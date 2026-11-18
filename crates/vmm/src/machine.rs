//! Builds the ARM64 board on the host hypervisor and runs it until the guest powers off or resets.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use arch::aarch64::psci::{self, Call};
use arch::aarch64::{boot, fdt, layout};
use devices::bus::MmioBus;
use devices::rtc::Rtc;
use devices::serial::Serial;
use devices::virtio::blk::{Block, DiskImage};
use devices::virtio::mmio::MmioTransport;
use devices::virtio::net::{MAC_LEN, Net};
use devices::{InterruptLine, MmioDevice};
use hv::{
    Exit, Gpa, Host, HypervisorBackend, IoHandler, Irqchip, IrqchipConfig, Kick, Prot, Regs, Vcpu,
    Vm, VmConfig,
};
use net::config::{ADB_PORT, AdbForward, NetConfig};
use net::policy::{DomainSet, Sinkhole};
use net::{HostResolver, Network, Resolver};
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::config::{
    BOOTCONFIG_ARG, Config, NetOptions, RNG_SEED_LEN, SERIAL_CONSOLE_ARG, SERIAL_INPUT_CHUNK,
    SERIAL_INPUT_RETRY,
};
use crate::console::{Console, MarkerWatch};
use crate::{VmmError, copy};

type HostVm = <Host as HypervisorBackend>::Vm;
type HostVcpu = <HostVm as Vm>::Vcpu;
type HostKick = <HostVcpu as Vcpu>::Kick;
type HostIrqchip = <HostVm as Vm>::Irqchip;

/// Why the guest stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    PowerOff,
    Reset,
}

#[derive(Debug)]
pub struct Outcome {
    pub stop: Stop,
    /// Time from `started` to the boot marker's first appearance on the console, if it appeared.
    pub boot_marker_after: Option<Duration>,
}

/// One SPI of the interrupt controller, as a device sees it.
struct SpiLine {
    chip: Arc<HostIrqchip>,
    spi: u32,
}

impl InterruptLine for SpiLine {
    fn set(&self, level: bool) {
        if let Err(err) = self.chip.set_irq(self.spi, level) {
            log::error!("{}", copy::irq_failed(self.spi, &err));
        }
    }
}

struct BusIo<'a>(&'a MmioBus);

impl IoHandler for BusIo<'_> {
    fn mmio_read(&self, gpa: Gpa, data: &mut [u8]) {
        self.0.read(gpa, data);
    }

    fn mmio_write(&self, gpa: Gpa, data: &[u8]) {
        self.0.write(gpa, data);
    }
}

/// Where a secondary CPU starts, from the PSCI `CPU_ON` that woke it.
struct Start {
    entry: u64,
    context: u64,
}

enum Role {
    Primary(Box<Regs>),
    Secondary(Receiver<Start>),
}

/// State the vCPU threads coordinate through.
struct Progress {
    /// vCPU threads that have finished creating their vCPU, successfully or not.
    created: u32,
    /// The first stop request wins.
    stop: Option<Result<Stop, VmmError>>,
}

struct Machine {
    vm: HostVm,
    bus: MmioBus,
    vcpus: u32,
    progress: Mutex<Progress>,
    progressed: Condvar,
    kickers: Mutex<Vec<HostKick>>,
    /// Per CPU: the way to start it, until the first `CPU_ON` that names it takes it.
    starters: Vec<Mutex<Option<Sender<Start>>>>,
    online: Vec<AtomicBool>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Machine {
    fn new(vm: HostVm, bus: MmioBus, vcpus: u32) -> Self {
        Self {
            vm,
            bus,
            vcpus,
            progress: Mutex::new(Progress {
                created: 0,
                stop: None,
            }),
            progressed: Condvar::new(),
            kickers: Mutex::new(Vec::new()),
            starters: (0..vcpus).map(|_| Mutex::new(None)).collect(),
            online: (0..vcpus).map(|_| AtomicBool::new(false)).collect(),
        }
    }

    fn stopping(&self) -> bool {
        lock(&self.progress).stop.is_some()
    }

    fn request_stop(&self, outcome: Result<Stop, VmmError>) {
        lock(&self.progress).stop.get_or_insert(outcome);
        self.progressed.notify_all();
        // Secondaries that were never started are waiting on their channel; dropping the sender wakes them.
        for starter in &self.starters {
            lock(starter).take();
        }
        for kick in lock(&self.kickers).iter() {
            kick.kick();
        }
    }

    /// The framework fixes the VM's CPU topology once any vCPU runs, so none runs before all exist.
    /// Returns false if a stop was requested in the meantime.
    fn wait_until_all_created(&self) -> bool {
        let mut progress = lock(&self.progress);
        progress.created += 1;
        self.progressed.notify_all();
        let waiting =
            |progress: &mut Progress| progress.stop.is_none() && progress.created < self.vcpus;
        self.progressed
            .wait_while(progress, waiting)
            .unwrap_or_else(PoisonError::into_inner)
            .stop
            .is_none()
    }

    fn cpu_slot<'a, T>(&self, slots: &'a [T], cpu: u32) -> Option<&'a T> {
        slots.get(usize::try_from(cpu).ok()?)
    }

    fn cpu_on(&self, cpu: u32, entry: u64, context: u64) -> u64 {
        let Some(starter) = self.cpu_slot(&self.starters, cpu) else {
            return psci::INVALID_PARAMETERS;
        };
        // shortcut: a CPU that turned itself off stays off (its starter is spent), so the guest sees
        // ALREADY_ON; make vCPU threads restartable when CPU hotplug matters.
        match lock(starter).take() {
            Some(start) if start.send(Start { entry, context }).is_ok() => psci::SUCCESS,
            Some(_) => psci::INVALID_PARAMETERS,
            None => psci::ALREADY_ON,
        }
    }

    fn affinity_info(&self, cpu: u32) -> u64 {
        match self.cpu_slot(&self.online, cpu) {
            Some(online) if online.load(Ordering::SeqCst) => psci::AFFINITY_ON,
            Some(_) => psci::AFFINITY_OFF,
            None => psci::INVALID_PARAMETERS,
        }
    }

    fn set_online(&self, cpu: u32, online: bool) {
        if let Some(flag) = self.cpu_slot(&self.online, cpu) {
            flag.store(online, Ordering::SeqCst);
        }
    }

    fn vcpu_loop(&self, index: u32, role: Role) -> Result<(), VmmError> {
        let created = self.vm.create_vcpu(index);
        if let Ok(vcpu) = &created {
            lock(&self.kickers).push(vcpu.kicker());
        }
        let proceed = self.wait_until_all_created();
        let mut vcpu = created?;
        if !proceed {
            return Ok(());
        }
        let regs = match role {
            Role::Primary(regs) => *regs,
            Role::Secondary(start) => match start.recv() {
                Ok(start) => boot::secondary_regs(start.entry, start.context),
                // The machine is stopping and this CPU was never turned on.
                Err(_) => return Ok(()),
            },
        };
        vcpu.set_regs(&regs)?;
        self.set_online(index, true);

        let io = BusIo(&self.bus);
        loop {
            match vcpu.run(&io)? {
                Exit::Kicked if self.stopping() => return Ok(()),
                Exit::Kicked => {}
                Exit::Hypercall => {
                    let mut regs = vcpu.regs()?;
                    regs.x[0] = match psci::decode(&regs) {
                        Call::Return(value) => value,
                        Call::CpuOn {
                            cpu,
                            entry,
                            context,
                        } => self.cpu_on(cpu, entry, context),
                        Call::AffinityInfo { cpu } => self.affinity_info(cpu),
                        Call::CpuOff => {
                            self.set_online(index, false);
                            return Ok(());
                        }
                        Call::SystemOff => {
                            self.request_stop(Ok(Stop::PowerOff));
                            return Ok(());
                        }
                        Call::SystemReset => {
                            self.request_stop(Ok(Stop::Reset));
                            return Ok(());
                        }
                    };
                    vcpu.set_regs(&regs)?;
                }
                Exit::Shutdown => {
                    self.request_stop(Ok(Stop::PowerOff));
                    return Ok(());
                }
                Exit::Reset => {
                    self.request_stop(Ok(Stop::Reset));
                    return Ok(());
                }
                _ => return Err(VmmError::Config(copy::UNKNOWN_EXIT)),
            }
        }
    }

    fn vcpu_thread(&self, index: u32, role: Role) {
        if let Err(err) = self.vcpu_loop(index, role) {
            log::error!("{}", copy::vcpu_stopped(index, &err));
            self.request_stop(Err(err));
        }
    }

    /// Runs every vCPU on its own thread and returns once the guest has stopped.
    fn run(&self, primary: Regs) -> Result<Stop, VmmError> {
        thread::scope(|scope| {
            let mut primary = Some(primary);
            for index in 0..self.vcpus {
                let role = match primary.take() {
                    Some(regs) => Role::Primary(Box::new(regs)),
                    None => {
                        let (start, started) = mpsc::channel();
                        if let Some(starter) = self.cpu_slot(&self.starters, index) {
                            *lock(starter) = Some(start);
                        }
                        Role::Secondary(started)
                    }
                };
                let name = format!("{}{index}", copy::VCPU_THREAD_PREFIX);
                let spawned = thread::Builder::new()
                    .name(name)
                    .spawn_scoped(scope, move || self.vcpu_thread(index, role));
                if let Err(err) = spawned {
                    // Threads already running wait for the full CPU count; this releases them.
                    self.request_stop(Err(VmmError::Thread(err)));
                    break;
                }
            }
            let progress = lock(&self.progress);
            let running = |progress: &mut Progress| progress.stop.is_none();
            drop(
                self.progressed
                    .wait_while(progress, running)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        });
        lock(&self.progress)
            .stop
            .take()
            .unwrap_or(Err(VmmError::Config(copy::VCPU_PANICKED)))
    }
}

/// Copies host console input into the guest UART until the input ends.
fn spawn_input_pump<W>(
    mut input: Box<dyn Read + Send>,
    serial: Arc<Serial<W>>,
) -> Result<(), VmmError>
where
    W: Write + Send + 'static,
{
    let pump = move || {
        let mut chunk = [0u8; SERIAL_INPUT_CHUNK];
        while let Ok(read @ 1..) = input.read(&mut chunk) {
            let mut pending = &chunk[..read];
            while !pending.is_empty() {
                let taken = serial.enqueue(pending);
                pending = &pending[taken..];
                if taken == 0 {
                    thread::sleep(SERIAL_INPUT_RETRY);
                }
            }
        }
    };
    thread::Builder::new()
        .name(copy::SERIAL_INPUT_THREAD.into())
        .spawn(pump)
        .map(drop)
        .map_err(VmmError::Thread)
}

/// The board places the GIC and numbers interrupts by its own constants; make sure the host's GIC agrees.
fn check_gic(chip: &HostIrqchip, highest_spi: u32) -> Result<(), VmmError> {
    let geometry = chip.geometry();
    let fits = geometry.distributor_size <= layout::GICD_SIZE
        && geometry.redistributor_size == layout::GICR_SIZE_PER_VCPU
        && geometry.spi_base == layout::SPI_INTID_BASE
        && highest_spi < geometry.spi_count;
    if fits {
        Ok(())
    } else {
        Err(VmmError::Config(copy::GIC_GEOMETRY))
    }
}

fn open(path: &Path) -> Result<File, VmmError> {
    File::open(path).map_err(|source| VmmError::File {
        path: path.to_owned(),
        source,
    })
}

/// The VMM's own arguments go first: anything after a ` -- ` in the user's command line is handed to
/// init instead of the kernel, and a later `console=` from the user should win over ours.
fn kernel_cmdline(config: &Config) -> String {
    let serial = config.serial.then_some(SERIAL_CONSOLE_ARG);
    let bootconfig = (!config.bootconfig.is_empty()).then_some(BOOTCONFIG_ARG);
    let parts = [serial, bootconfig, Some(config.cmdline.as_str())];
    parts
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn load_lists(paths: &[PathBuf]) -> Result<DomainSet, VmmError> {
    let mut domains = DomainSet::default();
    for path in paths {
        let text = std::fs::read_to_string(path).map_err(|source| VmmError::File {
            path: path.clone(),
            source,
        })?;
        domains.extend_from_list(&text);
    }
    Ok(domains)
}

/// The guest's network and the MAC address its device reports.
fn network(
    options: &NetOptions,
    resolver: Arc<dyn Resolver>,
) -> Result<(Network, [u8; MAC_LEN]), VmmError> {
    let policy = Sinkhole {
        blocked: load_lists(&options.blocklists)?,
        required: load_lists(&options.allowlists)?,
        drop_quic: !options.allow_quic,
        drop_encrypted_dns: true,
    };
    let cfg = NetConfig {
        adb: options.adb_port.map(|host_port| AdbForward {
            host_port,
            guest_port: ADB_PORT,
        }),
        host_loopback: options.host_loopback,
        ..NetConfig::default()
    };
    let mac = cfg.guest_mac;
    Ok((Network::new(cfg, Arc::new(policy), resolver), mac))
}

/// Builds the machine described by `config`, boots it and blocks until the guest stops.
/// `started` is the instant boot timing is measured from.
pub fn run(config: &Config, console: Console, started: Instant) -> Result<Outcome, VmmError> {
    run_with(config, console, started, Arc::new(HostResolver))
}

/// As `run`, with the guest's host-name lookups answered by `resolver` instead of the host's own.
pub fn run_with(
    config: &Config,
    console: Console,
    started: Instant,
    resolver: Arc<dyn Resolver>,
) -> Result<Outcome, VmmError> {
    let caps = Host::probe()?;
    if config.vcpus == 0 || config.vcpus > caps.max_vcpus.min(layout::MAX_VCPUS) {
        return Err(VmmError::Config(copy::TOO_MANY_VCPUS));
    }
    let disk_slots = u32::try_from(config.disks.len())
        .ok()
        .filter(|count| *count <= layout::VIRTIO_DISK_SLOTS)
        .ok_or(VmmError::Config(copy::TOO_MANY_DISKS))?;
    if config.initrd.is_none() && !config.bootconfig.is_empty() {
        return Err(VmmError::Config(copy::BOOTCONFIG_NEEDS_INITRD));
    }
    let ram_bytes = config
        .ram_bytes
        .next_multiple_of(caps.host_page_size as u64);
    boot::check_ram_size(ram_bytes)?;
    let ram_len = usize::try_from(ram_bytes).map_err(|_| VmmError::Config(copy::RAM_UNMAPPABLE))?;

    // Declared before the VM so that it is still alive when the VM, which maps it, is destroyed.
    let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(layout::RAM_BASE), ram_len)])
        .map_err(|err| VmmError::Device(err.to_string()))?;
    let vm = Host::create_vm(&VmConfig {
        gpa_bits: layout::GPA_BITS,
    })?;
    let host_ram = mem
        .get_host_address(GuestAddress(layout::RAM_BASE))
        .ok()
        .and_then(NonNull::new)
        .ok_or(VmmError::Config(copy::RAM_UNMAPPABLE))?;
    // SAFETY: `mem` owns `host_ram..host_ram+ram_len`, never moves or shrinks it, and outlives `vm`.
    unsafe { vm.map(layout::RAM_BASE, host_ram, ram_len, Prot::RWX) }?;

    let irqchip = vm.create_irqchip(&IrqchipConfig {
        gicd_base: layout::GICD_BASE,
        gicr_base: layout::GICR_BASE,
    })?;
    let highest_slot = match &config.net {
        Some(_) => Some(layout::VIRTIO_NET_SLOT),
        None => disk_slots.checked_sub(1),
    };
    let highest_spi = highest_slot.map_or(layout::RTC_SPI, layout::virtio_spi);
    check_gic(&irqchip, highest_spi)?;
    let line = |spi: u32| -> Arc<dyn InterruptLine> {
        Arc::new(SpiLine {
            chip: Arc::clone(&irqchip),
            spi,
        })
    };

    let mut bus = MmioBus::default();
    let (output, marker_seen) = MarkerWatch::new(console.output, config.boot_marker.as_deref());
    if config.serial {
        let serial = Arc::new(Serial::new(line(layout::UART_SPI), output));
        bus.insert(
            layout::UART_BASE,
            layout::UART_SIZE,
            Arc::clone(&serial) as Arc<dyn MmioDevice>,
        )?;
        if let Some(input) = console.input {
            spawn_input_pump(input, serial)?;
        }
    }
    bus.insert(layout::RTC_BASE, layout::RTC_SIZE, Arc::new(Rtc::default()))?;
    for (slot, disk) in (0u32..).zip(&config.disks) {
        let id = format!("{}{slot}", copy::DISK_ID_PREFIX);
        let image =
            DiskImage::open(&disk.path, disk.read_only, &id).map_err(|source| VmmError::File {
                path: disk.path.clone(),
                source,
            })?;
        let transport = MmioTransport::new(
            Box::new(Block::new(image)),
            mem.clone(),
            line(layout::virtio_spi(slot)),
        )
        .map_err(|err| VmmError::Device(err.to_string()))?;
        bus.insert(
            layout::virtio_mmio_base(slot),
            layout::VIRTIO_MMIO_STRIDE,
            Arc::new(transport),
        )?;
    }

    if let Some(options) = &config.net {
        let (network, mac) = network(options, resolver)?;
        let slot = layout::VIRTIO_NET_SLOT;
        let transport = MmioTransport::new(
            Box::new(Net::new(network, mac)),
            mem.clone(),
            line(layout::virtio_spi(slot)),
        )
        .map_err(|err| VmmError::Device(err.to_string()))?;
        bus.insert(
            layout::virtio_mmio_base(slot),
            layout::VIRTIO_MMIO_STRIDE,
            Arc::new(transport),
        )?;
    }

    let entry = boot::load_kernel(&mem, &mut open(&config.kernel)?)?;
    let initrd = match &config.initrd {
        Some(path) => Some(boot::load_initrd(
            &mem,
            &mut open(path)?,
            &config.bootconfig,
        )?),
        None => None,
    };
    let mut seed = [0u8; RNG_SEED_LEN + size_of::<u64>()];
    getrandom::fill(&mut seed).map_err(|_| VmmError::Config(copy::NO_ENTROPY))?;
    let (rng_seed, kaslr_seed) = seed.split_at(RNG_SEED_LEN);
    let net_slot = config.net.as_ref().map(|_| layout::VIRTIO_NET_SLOT);
    let virtio_slots: Vec<u32> = (0..disk_slots).chain(net_slot).collect();
    let blob = fdt::build(&fdt::FdtConfig {
        ram_size: ram_bytes,
        vcpus: config.vcpus,
        cmdline: &kernel_cmdline(config),
        initrd,
        serial: config.serial,
        virtio_slots: &virtio_slots,
        rng_seed,
        kaslr_seed: u64::from_le_bytes(kaslr_seed.try_into().expect("split leaves eight bytes")),
    })?;
    let fdt_addr = boot::load_fdt(&mem, &blob)?;

    let machine = Machine::new(vm, bus, config.vcpus);
    let stop = machine.run(boot::primary_regs(entry, fdt_addr))?;
    let boot_marker_after = marker_seen
        .get()
        .map(|seen| seen.saturating_duration_since(started));
    Ok(Outcome {
        stop,
        boot_marker_after,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn cmdline_joins_only_the_parts_that_apply() {
        let mut config = Config::new(PathBuf::from("k"));
        assert_eq!(kernel_cmdline(&config), "");
        config.serial = true;
        assert_eq!(kernel_cmdline(&config), SERIAL_CONSOLE_ARG);
        config.cmdline = "quiet".into();
        config.bootconfig.push(("a".into(), "b".into()));
        assert_eq!(
            kernel_cmdline(&config),
            format!("{SERIAL_CONSOLE_ARG} {BOOTCONFIG_ARG} quiet")
        );
    }

    #[test]
    fn vmm_arguments_stay_ahead_of_an_init_separator() {
        let mut config = Config::new(PathBuf::from("k"));
        config.serial = true;
        config.bootconfig.push(("a".into(), "b".into()));
        config.cmdline = "rdinit=/bin/sh -- -c true".into();
        let cmdline = kernel_cmdline(&config);
        let (kernel_part, _init_part) = cmdline.split_once(" -- ").unwrap();
        assert!(kernel_part.contains(SERIAL_CONSOLE_ARG) && kernel_part.contains(BOOTCONFIG_ARG));
    }
}

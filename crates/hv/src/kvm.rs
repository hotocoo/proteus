//! KVM backend (Linux, x86_64).
//!
//! The interrupt controllers (PIC, IOAPIC, local APICs) and the PIT live in the kernel, so idle
//! (`HLT`) never comes back here and interrupt lines are driven with one ioctl. A vCPU is kicked by
//! setting `immediate_exit` in its shared run area and then signalling its thread: either the
//! signal interrupts a running `KVM_RUN`, or the flag stops the next one from entering the guest.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex, Once, PoisonError};

use kvm_bindings::{
    CpuId, KVM_MAX_CPUID_ENTRIES, KVM_PIT_SPEAKER_DUMMY, KVM_SYSTEM_EVENT_RESET,
    KVM_SYSTEM_EVENT_SHUTDOWN, kvm_dtable, kvm_pit_config, kvm_regs, kvm_segment,
    kvm_userspace_memory_region,
};
use kvm_ioctls::{Cap, Kvm as KvmSystem, VcpuExit, VcpuFd, VmFd};

use crate::x86_64::{GPR_COUNT, Regs, Segment, SystemRegs, Table, apic_id};
use crate::{
    Caps, Exit, Gpa, HvError, HypervisorBackend, IoHandler, Irqchip, IrqchipConfig, Prot, Vcpu, Vm,
    VmConfig, copy,
};

/// KVM's stable API version; anything else is a kernel this code does not know.
const KVM_API_VERSION: i32 = 12;
/// x86 pages; KVM maps guest memory in these.
const PAGE_SIZE: usize = 4096;
/// Three pages KVM needs for its own task-state segment on Intel hosts, just below 4 GiB, where the
/// board keeps no RAM and no device.
const TSS_ADDRESS: usize = 0xfffb_d000;
/// One page after them, for the identity map Intel's real-mode emulation needs.
const IDENTITY_MAP_ADDRESS: u64 = 0xfffb_c000;
/// Local APIC registers (offsets into the APIC page) and the fields set in them.
const APIC_LVT0: usize = 0x350;
const APIC_LVT1: usize = 0x360;
const APIC_DELIVERY_MODE_SHIFT: u32 = 8;
const APIC_DELIVERY_MODE_MASK: u32 = 0b111 << APIC_DELIVERY_MODE_SHIFT;
const APIC_MODE_EXTINT: u32 = 0b111;
const APIC_MODE_NMI: u32 = 0b100;
/// CPUID leaves that carry a CPU's APIC ID, and where.
const CPUID_FEATURES: u32 = 1;
const CPUID_APIC_ID_SHIFT: u32 = 24;
const CPUID_TOPOLOGY: [u32; 2] = [0xb, 0x1f];

fn failed(call: &'static str) -> impl Fn(kvm_ioctls::Error) -> HvError {
    move |err| HvError::Backend {
        call,
        code: i64::from(err.errno()),
    }
}

/// The signal that kicks a vCPU thread out of `KVM_RUN`. Its handler does nothing; delivering it
/// is what interrupts the call.
fn kick_signal() -> libc::c_int {
    libc::SIGRTMIN()
}

fn install_kick_handler() {
    static INSTALLED: Once = Once::new();
    extern "C" fn ignore(_: libc::c_int) {}
    INSTALLED.call_once(|| {
        // SAFETY: a zeroed sigaction is a valid starting point; the handler is a plain function
        // that touches nothing, and SA_RESTART is left off so KVM_RUN returns EINTR.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = ignore as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(kick_signal(), &action, std::ptr::null_mut());
        }
    });
}

pub struct Kvm;

impl HypervisorBackend for Kvm {
    type Vm = KvmVm;

    fn probe() -> Result<Caps, HvError> {
        let kvm = open()?;
        Ok(Caps {
            host_page_size: PAGE_SIZE,
            gpa_bits: crate::x86_64::GPA_BITS,
            max_vcpus: u32::try_from(kvm.get_max_vcpus()).unwrap_or(u32::MAX),
        })
    }

    fn create_vm(_cfg: &VmConfig) -> Result<KvmVm, HvError> {
        let kvm = open()?;
        let fd = kvm.create_vm().map_err(failed("KVM_CREATE_VM"))?;
        fd.set_tss_address(TSS_ADDRESS)
            .map_err(failed("KVM_SET_TSS_ADDR"))?;
        fd.set_identity_map_address(IDENTITY_MAP_ADDRESS)
            .map_err(failed("KVM_SET_IDENTITY_MAP_ADDR"))?;
        let cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(failed("KVM_GET_SUPPORTED_CPUID"))?;
        Ok(KvmVm {
            fd: Arc::new(fd),
            cpuid,
            slots: Mutex::new(Vec::new()),
        })
    }
}

fn open() -> Result<KvmSystem, HvError> {
    let kvm = KvmSystem::new().map_err(|err| match err.errno() {
        libc::EACCES | libc::EPERM => HvError::Denied,
        libc::ENOENT => HvError::Unsupported(copy::NO_KVM),
        _ => failed("open /dev/kvm")(err),
    })?;
    if kvm.get_api_version() != KVM_API_VERSION {
        return Err(HvError::Unsupported(copy::KVM_API_VERSION));
    }
    for cap in [
        Cap::Irqchip,
        Cap::UserMemory,
        Cap::SetTssAddr,
        Cap::ImmediateExit,
    ] {
        if !kvm.check_extension(cap) {
            return Err(HvError::Unsupported(copy::KVM_CAPABILITY));
        }
    }
    Ok(kvm)
}

/// One memory slot: where it is in the guest, and how long.
#[derive(Clone, Copy)]
struct Slot {
    id: u32,
    gpa: Gpa,
    len: usize,
}

pub struct KvmVm {
    fd: Arc<VmFd>,
    cpuid: CpuId,
    slots: Mutex<Vec<Slot>>,
}

impl Vm for KvmVm {
    type Vcpu = KvmVcpu;
    type Irqchip = KvmIrqchip;

    unsafe fn map(
        &self,
        gpa: Gpa,
        host: NonNull<u8>,
        len: usize,
        prot: Prot,
    ) -> Result<(), HvError> {
        if !prot.contains(Prot::READ) {
            return Err(HvError::BadArgument(copy::KVM_UNREADABLE));
        }
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let id = (0..)
            .find(|id| slots.iter().all(|slot| slot.id != *id))
            .unwrap_or(0);
        let region = kvm_userspace_memory_region {
            slot: id,
            flags: if prot.contains(Prot::WRITE) {
                0
            } else {
                kvm_bindings::KVM_MEM_READONLY
            },
            guest_phys_addr: gpa,
            memory_size: len as u64,
            userspace_addr: host.as_ptr() as u64,
        };
        // SAFETY: the caller keeps `host..host+len` valid and unmoved until `unmap`.
        unsafe { self.fd.set_user_memory_region(region) }
            .map_err(failed("KVM_SET_USER_MEMORY_REGION"))?;
        slots.push(Slot { id, gpa, len });
        Ok(())
    }

    fn unmap(&self, gpa: Gpa, len: usize) -> Result<(), HvError> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let index = slots
            .iter()
            .position(|slot| slot.gpa == gpa && slot.len == len)
            .ok_or(HvError::BadArgument(copy::KVM_NOT_MAPPED))?;
        let region = kvm_userspace_memory_region {
            slot: slots[index].id,
            guest_phys_addr: gpa,
            ..Default::default()
        };
        // SAFETY: a size of zero removes the slot; no host memory is referenced.
        unsafe { self.fd.set_user_memory_region(region) }
            .map_err(failed("KVM_SET_USER_MEMORY_REGION"))?;
        slots.remove(index);
        Ok(())
    }

    fn create_irqchip(&self, _cfg: &IrqchipConfig) -> Result<Arc<KvmIrqchip>, HvError> {
        self.fd
            .create_irq_chip()
            .map_err(failed("KVM_CREATE_IRQCHIP"))?;
        let pit = kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        };
        self.fd
            .create_pit2(pit)
            .map_err(failed("KVM_CREATE_PIT2"))?;
        Ok(Arc::new(KvmIrqchip(Arc::clone(&self.fd))))
    }

    fn create_vcpu(&self, index: u32) -> Result<KvmVcpu, HvError> {
        install_kick_handler();
        let fd = self
            .fd
            .create_vcpu(u64::from(index))
            .map_err(failed("KVM_CREATE_VCPU"))?;
        let mut cpuid = self.cpuid.clone();
        for entry in cpuid.as_mut_slice() {
            let id = apic_id(index);
            if entry.function == CPUID_FEATURES {
                let others = entry.ebx & !(0xff << CPUID_APIC_ID_SHIFT);
                entry.ebx = others | (id << CPUID_APIC_ID_SHIFT);
            }
            if CPUID_TOPOLOGY.contains(&entry.function) {
                entry.edx = id;
            }
        }
        fd.set_cpuid2(&cpuid).map_err(failed("KVM_SET_CPUID2"))?;
        // Interrupts from the legacy PIC reach the boot CPU through LINT0, NMIs through LINT1, as on
        // a PC.
        let mut lapic = fd.get_lapic().map_err(failed("KVM_GET_LAPIC"))?;
        for (register, mode) in [(APIC_LVT0, APIC_MODE_EXTINT), (APIC_LVT1, APIC_MODE_NMI)] {
            let bytes = &mut lapic.regs[register..register + 4];
            let mut value = [0u8; 4];
            for (byte, reg) in value.iter_mut().zip(bytes.iter()) {
                *byte = *reg as u8;
            }
            let value = u32::from_le_bytes(value) & !APIC_DELIVERY_MODE_MASK;
            let value = value | (mode << APIC_DELIVERY_MODE_SHIFT);
            for (reg, byte) in bytes.iter_mut().zip(value.to_le_bytes()) {
                *reg = byte as libc::c_char;
            }
        }
        fd.set_lapic(&lapic).map_err(failed("KVM_SET_LAPIC"))?;
        let mut fd = fd;
        let run = std::ptr::addr_of_mut!(fd.get_kvm_run().immediate_exit) as usize;
        // SAFETY: pthread_self has no preconditions; this is the thread that will run the vCPU.
        let thread = unsafe { libc::pthread_self() };
        Ok(KvmVcpu {
            fd,
            kick: KvmKick(Arc::new(Mutex::new(Some(Target { run, thread })))),
        })
    }
}

pub struct KvmIrqchip(Arc<VmFd>);

impl Irqchip for KvmIrqchip {
    fn set_irq(&self, line: u32, level: bool) -> Result<(), HvError> {
        self.0
            .set_irq_line(line, level)
            .map_err(failed("KVM_IRQ_LINE"))
    }
}

/// Where a kick goes: the vCPU's `immediate_exit` byte and its thread.
#[derive(Clone, Copy)]
struct Target {
    run: usize,
    thread: libc::pthread_t,
}

/// Cleared when the vCPU is dropped, so a late kick touches nothing.
#[derive(Clone)]
pub struct KvmKick(Arc<Mutex<Option<Target>>>);

impl crate::Kick for KvmKick {
    fn kick(&self) {
        let target = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(target) = *target {
            // SAFETY: while the lock is held and the target is set, the vCPU, and with it its
            // mapped run area, exists. The byte is written by the kernel and by us only.
            unsafe {
                std::ptr::write_volatile(target.run as *mut u8, 1);
                libc::pthread_kill(target.thread, kick_signal());
            }
        }
    }
}

pub struct KvmVcpu {
    fd: VcpuFd,
    kick: KvmKick,
}

impl Drop for KvmVcpu {
    fn drop(&mut self) {
        *self.kick.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

fn segment_from(segment: &kvm_segment) -> Segment {
    Segment {
        base: segment.base,
        limit: segment.limit,
        selector: segment.selector,
        kind: segment.type_,
        present: segment.present != 0,
        dpl: segment.dpl,
        db: segment.db != 0,
        s: segment.s != 0,
        l: segment.l != 0,
        g: segment.g != 0,
        unusable: segment.unusable != 0,
    }
}

fn segment_to(segment: &Segment) -> kvm_segment {
    kvm_segment {
        base: segment.base,
        limit: segment.limit,
        selector: segment.selector,
        type_: segment.kind,
        present: segment.present.into(),
        dpl: segment.dpl,
        db: segment.db.into(),
        s: segment.s.into(),
        l: segment.l.into(),
        g: segment.g.into(),
        unusable: segment.unusable.into(),
        ..Default::default()
    }
}

fn table_to(table: &Table) -> kvm_dtable {
    kvm_dtable {
        base: table.base,
        limit: table.limit,
        ..Default::default()
    }
}

impl Vcpu for KvmVcpu {
    type Kick = KvmKick;

    fn run(&mut self, io: &dyn IoHandler) -> Result<Exit, HvError> {
        loop {
            let exit = self.fd.run();
            let exit = match exit {
                Ok(exit) => exit,
                Err(err) if err.errno() == libc::EINTR => {
                    self.fd.set_kvm_immediate_exit(0);
                    return Ok(Exit::Kicked);
                }
                Err(err) if err.errno() == libc::EAGAIN => continue,
                Err(err) => return Err(failed("KVM_RUN")(err)),
            };
            match exit {
                VcpuExit::IoIn(port, data) => io.pio_read(port, data),
                VcpuExit::IoOut(port, data) => io.pio_write(port, data),
                VcpuExit::MmioRead(gpa, data) => io.mmio_read(gpa, data),
                VcpuExit::MmioWrite(gpa, data) => io.mmio_write(gpa, data),
                // A triple fault: what a PC does to reset itself.
                VcpuExit::Shutdown => return Ok(Exit::Reset),
                VcpuExit::SystemEvent(KVM_SYSTEM_EVENT_SHUTDOWN, _) => return Ok(Exit::Shutdown),
                VcpuExit::SystemEvent(KVM_SYSTEM_EVENT_RESET, _) => return Ok(Exit::Reset),
                VcpuExit::Intr => {
                    self.fd.set_kvm_immediate_exit(0);
                    return Ok(Exit::Kicked);
                }
                other => return Err(HvError::UnhandledExit(format!("{other:?}"))),
            }
        }
    }

    fn regs(&self) -> Result<Regs, HvError> {
        let regs = self.fd.get_regs().map_err(failed("KVM_GET_REGS"))?;
        let gpr: [u64; GPR_COUNT] = [
            regs.rax, regs.rcx, regs.rdx, regs.rbx, regs.rsp, regs.rbp, regs.rsi, regs.rdi,
            regs.r8, regs.r9, regs.r10, regs.r11, regs.r12, regs.r13, regs.r14, regs.r15,
        ];
        Ok(Regs {
            gpr,
            rip: regs.rip,
            rflags: regs.rflags,
        })
    }

    fn set_regs(&mut self, regs: &Regs) -> Result<(), HvError> {
        let [
            rax,
            rcx,
            rdx,
            rbx,
            rsp,
            rbp,
            rsi,
            rdi,
            r8,
            r9,
            r10,
            r11,
            r12,
            r13,
            r14,
            r15,
        ] = regs.gpr;
        let regs = kvm_regs {
            rax,
            rcx,
            rdx,
            rbx,
            rsp,
            rbp,
            rsi,
            rdi,
            r8,
            r9,
            r10,
            r11,
            r12,
            r13,
            r14,
            r15,
            rip: regs.rip,
            rflags: regs.rflags,
        };
        self.fd.set_regs(&regs).map_err(failed("KVM_SET_REGS"))
    }

    fn system_regs(&self) -> Result<SystemRegs, HvError> {
        let sregs = self.fd.get_sregs().map_err(failed("KVM_GET_SREGS"))?;
        let table = |table: &kvm_dtable| Table {
            base: table.base,
            limit: table.limit,
        };
        Ok(SystemRegs {
            cs: segment_from(&sregs.cs),
            ds: segment_from(&sregs.ds),
            es: segment_from(&sregs.es),
            fs: segment_from(&sregs.fs),
            gs: segment_from(&sregs.gs),
            ss: segment_from(&sregs.ss),
            tr: segment_from(&sregs.tr),
            ldt: segment_from(&sregs.ldt),
            gdt: table(&sregs.gdt),
            idt: table(&sregs.idt),
            cr0: sregs.cr0,
            cr3: sregs.cr3,
            cr4: sregs.cr4,
            efer: sregs.efer,
        })
    }

    fn set_system_regs(&mut self, regs: &SystemRegs) -> Result<(), HvError> {
        let mut sregs = self.fd.get_sregs().map_err(failed("KVM_GET_SREGS"))?;
        sregs.cs = segment_to(&regs.cs);
        sregs.ds = segment_to(&regs.ds);
        sregs.es = segment_to(&regs.es);
        sregs.fs = segment_to(&regs.fs);
        sregs.gs = segment_to(&regs.gs);
        sregs.ss = segment_to(&regs.ss);
        sregs.tr = segment_to(&regs.tr);
        sregs.ldt = segment_to(&regs.ldt);
        sregs.gdt = table_to(&regs.gdt);
        sregs.idt = table_to(&regs.idt);
        sregs.cr0 = regs.cr0;
        sregs.cr3 = regs.cr3;
        sregs.cr4 = regs.cr4;
        sregs.efer = regs.efer;
        self.fd.set_sregs(&sregs).map_err(failed("KVM_SET_SREGS"))
    }

    fn kicker(&self) -> KvmKick {
        self.kick.clone()
    }
}

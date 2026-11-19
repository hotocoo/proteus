//! Boots a real Linux kernel to a shell over the serial console and exercises the board from inside:
//! SMP bring-up, virtio-blk read, write and identify, and power off.
//!
//! Needs kernels and a ramdisk the repository does not ship. Without them each test reports itself skipped.
//! - `EMU_TEST_INITRD`: a ramdisk whose init mounts `/proc` and `/sys` and ends in a BusyBox shell on
//!   the console.
//! - `EMU_TEST_KERNEL`: on ARM64 an uncompressed `Image` with virtio-mmio, 8250 and PL031 built in;
//!   on x86_64 a `vmlinux` or bzImage with virtio-mmio command-line devices and 8250 built in.
//! - `EMU_TEST_BOOTCONFIG_KERNEL`: an uncompressed ARM64 `Image` built with `CONFIG_BOOT_CONFIG`,
//!   such as a stock Android GKI kernel. It needs no virtio support.
#![cfg(host_board)]

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use net::Resolver;
use vmm::VmmError;
use vmm::config::{Config, DiskConfig, NetOptions};
use vmm::console::{Console, ConsoleInput};
use vmm::ctl::Controller;
use vmm::machine::{self, Outcome, Stop};

const KERNEL_ENV: &str = "EMU_TEST_KERNEL";
#[cfg(target_arch = "aarch64")]
const BOOTCONFIG_KERNEL_ENV: &str = "EMU_TEST_BOOTCONFIG_KERNEL";
const INITRD_ENV: &str = "EMU_TEST_INITRD";

/// The hypervisor allows one VM per process, and the test harness runs tests on parallel threads.
static ONE_VM: Mutex<()> = Mutex::new(());

/// Takes the one VM, and makes sure the emulator's warnings and errors reach the test's output: a
/// vCPU that fails stops the machine and says why only through the log. A test that failed while
/// holding the VM does not fail the next one.
fn one_vm() -> std::sync::MutexGuard<'static, ()> {
    static LOGGER: StderrLog = StderrLog;
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        if log::set_logger(&LOGGER).is_ok() {
            log::set_max_level(log::LevelFilter::Warn);
        }
    });
    ONE_VM
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("emu {}: {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

const RAM_BYTES: u64 = 512 << 20;
const VCPUS: u32 = 2;
const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// How the guest stops itself. An x86_64 guest without ACPI cannot switch the machine off; it can
/// only reset it, through the keyboard controller.
#[cfg(target_arch = "aarch64")]
const STOP_COMMAND: &str = "poweroff -f";
#[cfg(target_arch = "aarch64")]
const GUEST_STOP: Stop = Stop::PowerOff;
#[cfg(target_arch = "x86_64")]
const STOP_COMMAND: &str = "reboot -f";
#[cfg(target_arch = "x86_64")]
const GUEST_STOP: Stop = Stop::Reset;

const SECTOR: usize = 512;
const DISK_SECTORS: usize = 2048;
const READ_SECTOR: usize = 3;
const WRITE_SECTOR: usize = 5;
const WRITTEN: &[u8] = b"EMUWRITE";

/// Console output collected so far, searchable by other threads.
#[derive(Default)]
struct Transcript {
    bytes: Mutex<Vec<u8>>,
    grew: Condvar,
}

impl Transcript {
    /// Waits until `needle` shows up at or after `from`; returns the position just past it.
    fn expect(&self, needle: &str, from: usize) -> usize {
        let find = |bytes: &[u8]| {
            let hay = bytes.get(from..)?;
            hay.windows(needle.len())
                .position(|window| window == needle.as_bytes())
                .map(|at| from + at + needle.len())
        };
        let deadline = Instant::now() + STEP_TIMEOUT;
        let mut bytes = self.bytes.lock().unwrap();
        loop {
            if let Some(end) = find(&bytes) {
                return end;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "console never showed {needle:?}; it shows:\n{}",
                String::from_utf8_lossy(&bytes)
            );
            bytes = self.grew.wait_timeout(bytes, left).unwrap().0;
        }
    }
}

struct TranscriptWriter(Arc<Transcript>);

impl Write for TranscriptWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.bytes.lock().unwrap().extend_from_slice(buf);
        self.0.grew.notify_all();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Console input fed line by line from the test; closing the sender ends the stream.
struct Typed(Receiver<Vec<u8>>, Vec<u8>);

impl Read for Typed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.1.is_empty() {
            match self.0.recv() {
                Ok(line) => self.1 = line,
                Err(_) => return Ok(0),
            }
        }
        let take = buf.len().min(self.1.len());
        buf[..take].copy_from_slice(&self.1[..take]);
        self.1.drain(..take);
        Ok(take)
    }
}

/// A scratch disk where sector `n` is filled with byte `n % 251 + 1`, deleted on drop.
struct ScratchDisk(PathBuf);

static NEXT_DISK: AtomicU32 = AtomicU32::new(0);

fn sector_fill(sector: usize) -> u8 {
    (sector % 251) as u8 + 1
}

impl ScratchDisk {
    fn new() -> Self {
        let name = format!(
            "emu-linux-boot-{}-{}.img",
            std::process::id(),
            NEXT_DISK.fetch_add(1, Ordering::SeqCst)
        );
        let path = std::env::temp_dir().join(name);
        let bytes: Vec<u8> = (0..DISK_SECTORS)
            .flat_map(|sector| vec![sector_fill(sector); SECTOR])
            .collect();
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        Self(path)
    }

    fn contents(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        File::open(&self.0)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    }

    fn sector(&self, index: usize) -> Vec<u8> {
        self.contents()[index * SECTOR..(index + 1) * SECTOR].to_vec()
    }
}

impl Drop for ScratchDisk {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A running machine with its console wired to the test.
struct Session {
    transcript: Arc<Transcript>,
    typist: Sender<Vec<u8>>,
    outcome: Receiver<Result<Outcome, VmmError>>,
    /// The host ends of the machine's devices, and its power switch.
    controller: Arc<Controller>,
}

impl Session {
    fn boot(config: Config, initrd: PathBuf) -> Self {
        Self::boot_with(config, initrd, Arc::new(net::HostResolver))
    }

    fn boot_with(mut config: Config, initrd: PathBuf, resolver: Arc<dyn Resolver>) -> Self {
        config.initrd = Some(initrd);
        config.ram_bytes = RAM_BYTES;
        config.vcpus = VCPUS;
        config.serial = true;
        let transcript = Arc::new(Transcript::default());
        let (typist, typed) = mpsc::channel();
        let console = Console {
            output: Box::new(TranscriptWriter(Arc::clone(&transcript))),
            input: Some(ConsoleInput::from_reader(Typed(typed, Vec::new())).unwrap()),
        };
        let (finished, outcome) = mpsc::channel();
        let host = vmm::session::Session::new(&config).unwrap();
        let controller = Arc::clone(&host.controller);
        thread::spawn(move || {
            finished.send(machine::run_in(
                &host,
                &config,
                console,
                Instant::now(),
                resolver,
            ))
        });
        Self {
            transcript,
            typist,
            outcome,
            controller,
        }
    }

    fn type_line(&self, line: &str) {
        self.typist.send(format!("{line}\n").into_bytes()).unwrap();
    }

    fn power_off(self) -> Outcome {
        self.type_line(STOP_COMMAND);
        self.outcome
            .recv_timeout(STEP_TIMEOUT)
            .expect("the machine stopped after poweroff")
            .expect("machine ran to a clean stop")
    }
}

/// The paths named by the given environment variables, or `None` (after saying so) if any is unset.
fn fixtures<const N: usize>(names: [&str; N]) -> Option<[PathBuf; N]> {
    let mut paths = names.map(|name| std::env::var_os(name).map(PathBuf::from));
    if paths.iter().any(Option::is_none) {
        eprintln!("skipped: set {} to run this test", names.join(" and "));
        return None;
    }
    Some(
        paths
            .each_mut()
            .map(|path| path.take().expect("checked above")),
    )
}

#[test]
fn linux_boots_to_a_shell_uses_two_cpus_and_two_disks_then_powers_off() {
    let Some([kernel, initrd]) = fixtures([KERNEL_ENV, INITRD_ENV]) else {
        return;
    };
    let _one_vm = one_vm();
    let disk = ScratchDisk::new();
    let read_only_disk = ScratchDisk::new();
    let read_only_before = read_only_disk.contents();
    let mut config = Config::new(kernel);
    config.disks = vec![
        DiskConfig {
            path: disk.0.clone(),
            read_only: false,
        },
        DiskConfig {
            path: read_only_disk.0.clone(),
            read_only: true,
        },
    ];
    config.boot_marker = Some("/bin/sh".into());
    config.net = None;
    let session = Session::boot(config, initrd);
    let transcript = &session.transcript;
    let type_line = |line: &str| session.type_line(line);

    // The shell prompt appears once init has handed over.
    let mut at = transcript.expect("# ", 0);

    type_line("echo cpus=$(grep -c ^processor /proc/cpuinfo)");
    at = transcript.expect(&format!("cpus={VCPUS}\r\n"), at);

    // The first disk's interrupt arrives on the IOAPIC pin its slot is wired to.
    #[cfg(target_arch = "x86_64")]
    {
        let pin = arch::x86_64::layout::virtio_irq(0);
        type_line(&format!(
            "echo disk_irq=$(awk '$1==\"{pin}:\" && $NF ~ /virtio0/ && $2+$3>0 {{print \"ok\"}}' /proc/interrupts)"
        ));
        at = transcript.expect("disk_irq=ok\r\n", at);
    }

    type_line("echo sectors=$(cat /sys/block/vda/size) serial=$(cat /sys/block/vda/serial)");
    at = transcript.expect(&format!("sectors={DISK_SECTORS} serial=emu-disk-0\r\n"), at);

    // Read one sector back as hex; every byte of it is the sector's fill value.
    type_line(&format!(
        "dd if=/dev/vda bs={SECTOR} skip={READ_SECTOR} count=1 2>/dev/null | hexdump -v -e '1/1 \"%02x\"' | cut -c1-16; echo"
    ));
    at = transcript.expect(
        &format!(
            "\r\n{}\r\n",
            format!("{:02x}", sector_fill(READ_SECTOR)).repeat(8)
        ),
        at,
    );

    let written = String::from_utf8_lossy(WRITTEN);
    type_line(&format!(
        "printf {written} | dd of=/dev/vda bs={SECTOR} seek={WRITE_SECTOR} conv=notrunc 2>/dev/null; sync; echo wrote=$?"
    ));
    at = transcript.expect("wrote=0\r\n", at);

    // The second image sits in the next virtio slot, so it must be the next disk the guest names,
    // and the guest must see it as read-only and fail to write to it.
    type_line("echo second=$(cat /sys/block/vdb/serial) ro=$(cat /sys/block/vdb/ro)");
    at = transcript.expect("second=emu-disk-1 ro=1\r\n", at);
    type_line(&format!(
        "printf {written} | dd of=/dev/vdb bs={SECTOR} conv=notrunc 2>/dev/null; echo refused=$?"
    ));
    transcript.expect("refused=1\r\n", at);

    let outcome = session.power_off();
    assert_eq!(outcome.stop, GUEST_STOP);
    assert!(
        outcome.boot_marker_after.is_some(),
        "boot marker was on the console but not timed"
    );

    let sector = disk.sector(WRITE_SECTOR);
    assert_eq!(&sector[..WRITTEN.len()], WRITTEN);
    assert!(
        sector[WRITTEN.len()..]
            .iter()
            .all(|byte| *byte == sector_fill(WRITE_SECTOR))
    );
    assert_eq!(
        disk.sector(READ_SECTOR),
        vec![sector_fill(READ_SECTOR); SECTOR]
    );
    assert_eq!(read_only_disk.contents(), read_only_before);
}

/// The host can turn a running machine off without the guest's help, and a touch sent from the host
/// reaches the guest's input device.
#[test]
fn host_power_switch_stops_a_running_guest() {
    let Some([kernel, initrd]) = fixtures([KERNEL_ENV, INITRD_ENV]) else {
        return;
    };
    let _one_vm = one_vm();
    let mut config = Config::new(kernel);
    config.net = None;
    let session = Session::boot(config, initrd);
    session.transcript.expect("# ", 0);
    session.controller.power.off();
    let outcome = session
        .outcome
        .recv_timeout(STEP_TIMEOUT)
        .expect("the machine stopped when the host switched it off")
        .expect("the machine ran until then");
    assert_eq!(outcome.stop, Stop::PowerOff);
}

/// The bootconfig trailer is the only channel for `androidboot.*` parameters, so it has to be accepted by
/// a real kernel, not only by this repository's own reading of the format.
#[test]
#[cfg(target_arch = "aarch64")]
fn kernel_reads_the_bootconfig_appended_to_the_ramdisk() {
    let Some([kernel, initrd]) = fixtures([BOOTCONFIG_KERNEL_ENV, INITRD_ENV]) else {
        return;
    };
    let _one_vm = one_vm();
    let mut config = Config::new(kernel);
    config.bootconfig = vec![
        ("androidboot.hardware".into(), "emu".into()),
        ("androidboot.serialno".into(), "EMU0001".into()),
    ];
    config.net = None;
    let session = Session::boot(config, initrd);

    let at = session.transcript.expect("# ", 0);
    session.type_line("cat /proc/bootconfig; echo end-of-bootconfig");
    session.transcript.expect(
        "\r\nandroidboot.hardware = \"emu\"\r\nandroidboot.serialno = \"EMU0001\"\r\nend-of-bootconfig\r\n",
        at,
    );
    assert_eq!(session.power_off().stop, GUEST_STOP);
}

/// Answers every lookup with one fixed address and remembers what was asked.
#[derive(Default)]
struct FixedResolver {
    asked: Mutex<Vec<String>>,
}

/// From TEST-NET-3, reserved for documentation, so it can never be a real host.
const RESOLVED: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);

impl Resolver for FixedResolver {
    fn resolve(&self, name: &str) -> io::Result<Vec<Ipv4Addr>> {
        self.asked.lock().unwrap().push(name.to_owned());
        Ok(vec![RESOLVED])
    }
}

/// A host server that records the first bytes of every connection and answers `pong:` plus those bytes.
struct PongServer {
    port: u16,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl PongServer {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a loopback port");
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut first = vec![0u8; 4096];
                stream.set_read_timeout(Some(STEP_TIMEOUT)).unwrap();
                let Ok(len) = stream.read(&mut first) else {
                    continue;
                };
                first.truncate(len);
                let _ = stream.write_all(&[b"pong:", first.as_slice()].concat());
                record.lock().unwrap().push(first);
            }
        });
        Self { port, seen }
    }

    fn seen(&self) -> Vec<Vec<u8>> {
        self.seen.lock().unwrap().clone()
    }
}

// The guest's terminal echoes every typed command. Wherever a command prints a fixed marker, the marker
// is written with an empty quoted string inside it (`ga''teway=up`), so that the echo of the command
// cannot be mistaken for its output.

/// A minimal but well-formed TLS ClientHello naming `server`.
fn client_hello(server: &str) -> Vec<u8> {
    let len16 = |body: &[u8]| [&(body.len() as u16).to_be_bytes()[..], body].concat();
    let name_entry = [&[0u8][..], &len16(server.as_bytes())].concat();
    let extension = [&[0u8, 0][..], &len16(&len16(&name_entry))].concat();
    let mut hello = vec![3, 3];
    hello.extend([0xaa; 32]);
    hello.push(0);
    hello.extend(len16(&[0x13, 0x01]));
    hello.extend([1, 0]);
    hello.extend(len16(&extension));
    let mut handshake = vec![1];
    handshake.extend(&(hello.len() as u32).to_be_bytes()[1..]);
    handshake.extend(hello);
    [&[0x16u8, 3, 1][..], &len16(&handshake)].concat()
}

/// The bytes as a shell `printf` format: every byte a `\xHH` escape.
fn printf_escaped(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\x{byte:02x}")).collect()
}

fn free_loopback_port() -> u16 {
    let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a loopback port");
    probe.local_addr().unwrap().port()
}

const GUEST_ADB_REPLY: &str = "from-guest";
const ADB_ATTEMPTS: u32 = 50;
const ADB_RETRY: Duration = Duration::from_millis(100);

#[test]
fn guest_network_gets_dhcp_dns_tcp_adb_and_respects_the_sinkhole() {
    let Some([kernel, initrd]) = fixtures([KERNEL_ENV, INITRD_ENV]) else {
        return;
    };
    let _one_vm = one_vm();
    let server = PongServer::start();
    let resolver = Arc::new(FixedResolver::default());
    let adb_port = free_loopback_port();
    let blocklist = std::env::temp_dir().join(format!("emu-blocklist-{}.txt", std::process::id()));
    std::fs::write(&blocklist, "# test list\n0.0.0.0 tracker.test\n").unwrap();

    let mut config = Config::new(kernel);
    config.net = Some(NetOptions {
        blocklists: vec![blocklist.clone()],
        adb_port: Some(adb_port),
        host_loopback: true,
        ..NetOptions::default()
    });
    let session = Session::boot_with(config, initrd, Arc::clone(&resolver) as Arc<dyn Resolver>);
    let transcript = &session.transcript;
    let port = server.port;
    let mut at = transcript.expect("# ", 0);

    // DHCP: the lease must carry the guest address, router, DNS server and netmask.
    session.type_line(
        "ifconfig eth0 up && printf '#!/bin/sh\\necho lease=$ip router=$router dns=$dns mask=$subnet\\n' > /lease \
         && chmod +x /lease && udhcpc -i eth0 -n -q -s /lease",
    );
    at = transcript.expect(
        "lease=10.0.2.15 router=10.0.2.2 dns=10.0.2.2 mask=255.255.255.0",
        at,
    );

    // The gateway answers ARP and ping.
    session.type_line(
        "ifconfig eth0 10.0.2.15 netmask 255.255.255.0 && route add default gw 10.0.2.2 \
         && ping -c 1 -W 3 10.0.2.2 | grep -q '1 packets received' && echo ga''teway=up",
    );
    at = transcript.expect("gateway=up\r\n", at);

    // DNS: an ordinary name resolves through the host resolver; a blocklisted one never reaches it.
    session.type_line(
        "nslookup allowed.test 10.0.2.2 2>&1 | grep -q 203.0.113.7 && echo re''solved=yes",
    );
    at = transcript.expect("resolved=yes\r\n", at);
    session.type_line(
        "nslookup tracker.test 10.0.2.2 2>&1 | grep -q 203.0.113.7 || echo bl''ocked=yes",
    );
    at = transcript.expect("blocked=yes\r\n", at);
    let asked = resolver.asked.lock().unwrap().clone();
    assert!(asked.iter().any(|name| name == "allowed.test"), "{asked:?}");
    assert!(
        !asked.iter().any(|name| name.contains("tracker")),
        "{asked:?}"
    );

    // TCP: plain bytes are relayed both ways.
    session.type_line(&format!("echo ping | nc -w 5 10.0.2.2 {port}; echo"));
    at = transcript.expect("pong:ping", at);

    // TLS to a blocklisted server name is reset before any host connection exists.
    let blocked = client_hello("cdn.tracker.test");
    let connections_before = server.seen().len();
    session.type_line(&format!(
        "printf '{}' | nc -w 5 10.0.2.2 {port} | grep -q pong || echo tl''s=refused",
        printf_escaped(&blocked)
    ));
    at = transcript.expect("tls=refused\r\n", at);
    assert_eq!(
        server.seen().len(),
        connections_before,
        "a blocked flow reached the host"
    );

    // TLS to any other name passes through byte for byte.
    let allowed = client_hello("play.allowed.test");
    session.type_line(&format!(
        "printf '{}' | nc -w 5 10.0.2.2 {port} | grep -q pong && echo tl''s=relayed",
        printf_escaped(&allowed)
    ));
    at = transcript.expect("tls=relayed\r\n", at);
    assert_eq!(server.seen().last(), Some(&allowed));

    // adb forward: a host connection to the loopback port lands on guest port 5555.
    session.type_line(&format!("echo {GUEST_ADB_REPLY} | nc -l -p 5555 &"));
    transcript.expect("\r\n", at);
    let mut reply = String::new();
    for _ in 0..ADB_ATTEMPTS {
        // Until the guest is listening, the forward connects and is closed at once.
        if let Ok(mut stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, adb_port)) {
            stream.set_read_timeout(Some(STEP_TIMEOUT)).unwrap();
            reply.clear();
            if stream.read_to_string(&mut reply).is_ok() && !reply.is_empty() {
                break;
            }
        }
        thread::sleep(ADB_RETRY);
    }
    assert_eq!(reply.trim_end(), GUEST_ADB_REPLY);

    assert_eq!(session.power_off().stop, GUEST_STOP);
    let _ = std::fs::remove_file(&blocklist);
}

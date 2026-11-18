//! Boots a real Linux kernel to a shell over the serial console and exercises the board from inside:
//! SMP bring-up, virtio-blk read, write and identify, and power off.
//!
//! Needs kernels and a ramdisk the repository does not ship. Without them each test reports itself skipped.
//! - `EMU_TEST_INITRD`: a ramdisk whose init mounts `/proc` and `/sys` and ends in a BusyBox shell on
//!   the console.
//! - `EMU_TEST_KERNEL`: an uncompressed ARM64 `Image` with virtio-mmio, 8250 and PL031 built in.
//! - `EMU_TEST_BOOTCONFIG_KERNEL`: an uncompressed ARM64 `Image` built with `CONFIG_BOOT_CONFIG`,
//!   such as a stock Android GKI kernel. It needs no virtio support.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use vmm::VmmError;
use vmm::config::{Config, DiskConfig};
use vmm::console::Console;
use vmm::machine::{self, Outcome, Stop};

const KERNEL_ENV: &str = "EMU_TEST_KERNEL";
const BOOTCONFIG_KERNEL_ENV: &str = "EMU_TEST_BOOTCONFIG_KERNEL";
const INITRD_ENV: &str = "EMU_TEST_INITRD";

/// The hypervisor allows one VM per process, and the test harness runs tests on parallel threads.
static ONE_VM: Mutex<()> = Mutex::new(());

const RAM_BYTES: u64 = 512 << 20;
const VCPUS: u32 = 2;
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

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
}

impl Session {
    fn boot(mut config: Config, initrd: PathBuf) -> Self {
        config.initrd = Some(initrd);
        config.ram_bytes = RAM_BYTES;
        config.vcpus = VCPUS;
        config.serial = true;
        let transcript = Arc::new(Transcript::default());
        let (typist, typed) = mpsc::channel();
        let console = Console {
            output: Box::new(TranscriptWriter(Arc::clone(&transcript))),
            input: Some(Box::new(Typed(typed, Vec::new()))),
        };
        let (finished, outcome) = mpsc::channel();
        thread::spawn(move || finished.send(machine::run(&config, console, Instant::now())));
        Self {
            transcript,
            typist,
            outcome,
        }
    }

    fn type_line(&self, line: &str) {
        self.typist.send(format!("{line}\n").into_bytes()).unwrap();
    }

    fn power_off(self) -> Outcome {
        self.type_line("poweroff -f");
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
    let _one_vm = ONE_VM.lock().unwrap();
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
    let session = Session::boot(config, initrd);
    let transcript = &session.transcript;
    let type_line = |line: &str| session.type_line(line);

    // The shell prompt appears once init has handed over.
    let mut at = transcript.expect("# ", 0);

    type_line("echo cpus=$(grep -c ^processor /proc/cpuinfo)");
    at = transcript.expect(&format!("cpus={VCPUS}\r\n"), at);

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
    assert_eq!(outcome.stop, Stop::PowerOff);
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

/// The bootconfig trailer is the only channel for `androidboot.*` parameters, so it has to be accepted by
/// a real kernel, not only by this repository's own reading of the format.
#[test]
fn kernel_reads_the_bootconfig_appended_to_the_ramdisk() {
    let Some([kernel, initrd]) = fixtures([BOOTCONFIG_KERNEL_ENV, INITRD_ENV]) else {
        return;
    };
    let _one_vm = ONE_VM.lock().unwrap();
    let mut config = Config::new(kernel);
    config.bootconfig = vec![
        ("androidboot.hardware".into(), "emu".into()),
        ("androidboot.serialno".into(), "EMU0001".into()),
    ];
    let session = Session::boot(config, initrd);

    let at = session.transcript.expect("# ", 0);
    session.type_line("cat /proc/bootconfig; echo end-of-bootconfig");
    session.transcript.expect(
        "\r\nandroidboot.hardware = \"emu\"\r\nandroidboot.serialno = \"EMU0001\"\r\nend-of-bootconfig\r\n",
        at,
    );
    assert_eq!(session.power_off().stop, Stop::PowerOff);
}

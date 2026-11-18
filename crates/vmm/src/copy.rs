//! Every message the `emu` binary can show. Code refers to these by name.

use std::fmt::Display;
use std::path::Path;
use std::time::Duration;

pub const HELP: &str = "\
emu: lightweight Android emulator

Usage: emu --kernel <file> [options]

  --kernel <file>         Uncompressed kernel image (required)
  --initrd <file>         Ramdisk (uncompressed or kernel-supported cpio)
  --cmdline <text>        Kernel command line
  --ram-mib <n>           Guest RAM in MiB
  --vcpus <n>             Number of virtual CPUs
  --disk <file>           Attach a read-write raw disk image (repeatable, in slot order)
  --ro-disk <file>        Attach a read-only raw disk image (repeatable, in slot order)
  --bootconfig <k=v>      Add a bootconfig parameter, such as androidboot.hardware=emu (repeatable)
  --serial                Attach the debug serial console to this terminal
  --boot-timing           Report the time from start to the guest's first-stage init
  --boot-marker <text>    Console text that marks the end of boot for --boot-timing
  --log-level <level>     off, error, warn, info, debug or trace
  --help                  Show this text";

pub const USAGE_HINT: &str = "run `emu --help` for usage";

pub fn fatal(err: &dyn Display) -> String {
    format!("emu: {err}")
}

pub fn boot_time(elapsed: Duration) -> String {
    format!("emu: boot marker seen after {} ms", elapsed.as_millis())
}

pub const BOOT_MARKER_NOT_SEEN: &str = "emu: boot marker was never seen on the console";

pub fn bad_argument(err: &dyn Display) -> String {
    format!("command line: {err}")
}

pub fn unexpected_argument(arg: &str) -> String {
    format!("unexpected argument `{arg}`")
}

pub fn bad_number(option: &str) -> String {
    format!("--{option} needs a positive whole number")
}

pub const MISSING_KERNEL: &str = "--kernel is required";
pub const BAD_BOOTCONFIG: &str = "--bootconfig needs the form key=value";
pub const BAD_LOG_LEVEL: &str = "--log-level needs one of off, error, warn, info, debug, trace";
pub const BOOT_MARKER_NEEDS_SERIAL: &str =
    "--boot-timing needs --serial, because the marker is read from the console";

pub fn file(path: &Path, err: &dyn Display) -> String {
    format!("{}: {err}", path.display())
}

pub fn hypervisor(err: &dyn Display) -> String {
    format!("hypervisor: {err}")
}

pub fn board(err: &dyn Display) -> String {
    format!("board setup: {err}")
}

pub fn device(err: &dyn Display) -> String {
    format!("device setup: {err}")
}

pub fn thread(err: &dyn Display) -> String {
    format!("could not start a thread: {err}")
}

pub fn irq_failed(line: u32, err: &dyn Display) -> String {
    format!("interrupt line {line}: {err}")
}

pub fn vcpu_stopped(index: u32, err: &dyn Display) -> String {
    format!("vCPU {index} stopped: {err}")
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub const HOST_UNSUPPORTED: &str = "this build has no hypervisor backend for this host";
pub const BOOTCONFIG_NEEDS_INITRD: &str =
    "--bootconfig needs --initrd, because the kernel reads it from the end of the ramdisk";
pub const TOO_MANY_VCPUS: &str = "more vCPUs requested than the host or the board supports";
pub const TOO_MANY_DISKS: &str = "more disks than the board has virtio slots";
pub const GIC_GEOMETRY: &str =
    "the host's interrupt controller does not fit the board's address map";
pub const NO_ENTROPY: &str = "the host gave no random bytes to seed the guest";
pub const RAM_UNMAPPABLE: &str = "guest RAM could not be mapped";
pub const UNKNOWN_EXIT: &str = "the hypervisor reported an exit this build does not know";
pub const VCPU_PANICKED: &str = "a vCPU thread panicked";

pub const VCPU_THREAD_PREFIX: &str = "vcpu";
pub const SERIAL_INPUT_THREAD: &str = "serial-in";
/// Prefix of the serial number each disk reports to the guest, followed by its slot.
pub const DISK_ID_PREFIX: &str = "emu-disk-";

//! Every message the `emu` binary can show. Code refers to these by name.
// On a host that has no hypervisor backend yet, the machine is not built and its messages go unused.
#![cfg_attr(
    not(all(target_os = "macos", target_arch = "aarch64")),
    allow(dead_code)
)]

use std::fmt::Display;
use std::path::Path;
use std::time::Duration;

pub const HELP: &str = "\
emu: lightweight Android emulator

Usage: emu --kernel <file> [options]

  --kernel <file>         Uncompressed kernel image (required)
  --initrd <file>         Ramdisk (uncompressed or kernel-supported cpio)
  --android <dir>         Boot Android from a directory of AOSP virtual-device images
                          (init_boot.img, vendor_boot.img, super.img, vbmeta*.img)
  --data-dir <dir>        Where this Android instance keeps its data (default: <dir>/emu-data)
  --userdata-gib <n>      Size of the Android data partition when first created
  --cmdline <text>        Kernel command line
  --ram-mib <n>           Guest RAM in MiB
  --vcpus <n>             Number of virtual CPUs
  --disk <file>           Attach a read-write raw disk image (repeatable, in slot order)
  --ro-disk <file>        Attach a read-only raw disk image (repeatable, in slot order)
  --bootconfig <k=v>      Add a bootconfig parameter, such as androidboot.hardware=emu (repeatable)
  --serial                Attach the debug serial console to this terminal
  --display <WxH>         Size of the guest display in pixels (default 1280x720)
  --no-display            Give the guest no display device
  --no-net                Give the guest no network device
  --blocklist <file>      Block the domains in this list: trackers, ads (repeatable)
  --allowlist <file>      Keep these domains reachable even if a blocklist names them (repeatable)
  --adb-port <n>          Loopback port for adb (default 5555, 0 turns the forward off)
  --allow-quic            Let QUIC (UDP 443) through; by default it is dropped so TLS is used
  --host-loopback         Let the guest reach this machine's localhost at 10.0.2.2
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
pub const ANDROID_WITH_INITRD: &str =
    "--android brings its own ramdisk; do not pass --initrd with it";
pub const DATA_DIR_NEEDS_ANDROID: &str = "--data-dir only applies with --android";
pub const BAD_DISPLAY: &str = "--display needs a size such as 1280x720";
pub const BAD_ADB_PORT: &str = "--adb-port needs a port number from 0 to 65535";
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
pub const TOO_MANY_DISKS: &str = "more disks than the board has disk slots (three)";
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
/// Serial number the Android system disk reports to the guest.
pub const ANDROID_DISK_ID: &str = "emu-android";

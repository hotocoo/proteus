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
       emu ctl <socket> <command>

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
  --headless              Open no window; the display can still be captured
  --keymap <file>         Keys and mouse movements that stand for touches (see the README)
  --frame-dump <file>     Keep a PNG of the guest display in this file, refreshed about once a second
  --ctl <socket>          Listen on this local socket for `emu ctl` commands
  --logcat <file>         Write Android's system log to this file
  --no-sound              Give the guest no sound card
  --mute                  Keep the sound card but play nothing on this machine
  --no-net                Give the guest no network device
  --blocklist <file>      Block the domains in this list: trackers, ads (repeatable)
  --allowlist <file>      Keep these domains reachable even if a blocklist names them (repeatable)
  --adb-port <n>          Loopback port for adb (default 5555, 0 turns the forward off)
  --allow-quic            Let QUIC (UDP 443) through; by default it is dropped so TLS is used
  --host-loopback         Let the guest reach this machine's localhost at 10.0.2.2
  --boot-timing           Report the time from start to the guest's first-stage init
  --boot-marker <text>    Console text that marks the end of boot for --boot-timing
  --log-level <level>     off, error, warn, info, debug or trace
  --help                  Show this text

`emu ctl <socket> <command>` drives a running instance that was started with --ctl:
  tap <x> <y>                            Touch the screen briefly at a pixel
  swipe <x1> <y1> <x2> <y2> [<ms>]       Drag a finger from one pixel to another
  touch <finger> <x> <y>                 Put a finger down, or move it (fingers 0 to 9)
  lift <finger>                          Take that finger off the screen
  key <code> [down|up]                   Press a key by its Linux key code
  screenshot <file>                      Save what the display shows as a PNG
  location <lat> <lon> [<metres>]        Put the device at this position on Earth
  rotate <x> <y> <z>                     Turn the device, in degrees about its three axes;
                                         0 0 0 is upright, -90 0 0 is flat on a table
  shake                                  Shake the device
  stop                                   Turn the machine off at once, like pulling the plug;
                                         shut Android down from inside when you can
  record <file>                          Record the touches and keys that follow as a macro
  record-stop                            End the recording and write the file
  run <file> [<times>]                   Play a macro: a file of these commands, one per line,
                                         with `wait <ms>` for pauses and `#` for comments
  cancel                                 Stop the macro that is playing";

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
pub const LOGCAT_NEEDS_ANDROID: &str = "--logcat only applies with --android";
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
pub const POWER_THREAD: &str = "power";
pub const MACHINE_THREAD: &str = "machine";
pub fn keymap_unknown_key(name: &str) -> String {
    format!("key map: there is no key called `{name}`")
}

pub fn keymap_key_twice(name: &str) -> String {
    format!("key map: `{name}` is used more than once")
}

pub fn keymap_off_screen(x: u32, y: u32) -> String {
    format!("key map: position {x}, {y} is not on the guest's screen")
}

pub fn keymap_circle_off_screen(x: u32, y: u32, radius: u32) -> String {
    format!(
        "key map: a circle of radius {radius} around {x}, {y} does not fit on the guest's screen"
    )
}

pub const KEYMAP_TOO_MANY: &str =
    "key map: more touches than the touchscreen has fingers for (eight)";
pub const KEYMAP_BAD_SENSITIVITY: &str = "key map: sensitivity has to be a number above zero";
pub const KEYMAP_NEEDS_WINDOW: &str =
    "--keymap needs a window; do not combine it with --headless or --no-display";

pub fn keymap_refused(path: &Path, why: &dyn Display) -> String {
    format!("{}: {why}", path.display())
}

pub const AUDIO_THREAD: &str = "audio";
pub const AUDIO_NO_DEVICE: &str = "this machine has no sound output";
pub const AUDIO_OPENED: &str = "sound output opened: the guest has started playing";

pub fn audio_failed(err: &dyn Display) -> String {
    format!("sound output: {err}; the guest's sound will not be heard")
}
pub const WINDOW_TITLE: &str = "emu";

pub fn window_failed(err: &dyn Display) -> String {
    format!("the window could not be opened (use --headless to run without one): {err}")
}

pub fn window_draw_failed(err: &dyn Display) -> String {
    format!("the window could not be drawn: {err}")
}

pub fn window_grab_failed(err: &dyn Display) -> String {
    format!("the window could not keep the pointer for the key map: {err}")
}

pub fn window_input_dropped(why: &dyn Display) -> String {
    format!("input from the window was dropped: {why}")
}
pub const CONSOLE_INPUT_THREAD: &str = "console-in";
pub const REBOOTING: &str = "emu: guest asked to restart; booting again";
pub const RESET_LOOP: &str = "the guest keeps resetting right after it starts; giving up";
/// Prefix of the serial number each disk reports to the guest, followed by its slot.
pub const DISK_ID_PREFIX: &str = "emu-disk-";
/// Serial number the Android system disk reports to the guest.
pub const ANDROID_DISK_ID: &str = "emu-android";
/// Extension of the temporary file a frame dump is written to before it replaces the real one.
pub const FRAME_DUMP_PARTIAL_EXTENSION: &str = "partial";
pub const FRAME_DUMP_THREAD: &str = "frame-dump";

/// Names the guest sees for its input devices.
pub const TOUCHSCREEN_NAME: &str = "emu touchscreen";
pub const KEYBOARD_NAME: &str = "emu keyboard";
pub const INPUT_SERIAL: &str = "0";

pub const SENSORS_THREAD: &str = "sensors";

pub fn sensor_request(request: &str) -> String {
    format!("sensor service asked: {}", request.trim_end())
}

pub const CTL_THREAD: &str = "ctl";
/// The control socket's answer to a command that worked.
pub const CTL_OK: &str = "ok";

/// The control socket's answer to a command that did not.
pub fn ctl_error(why: &dyn Display) -> String {
    format!("error: {why}")
}

pub fn ctl_accept_failed(err: &dyn Display) -> String {
    format!("control socket stopped accepting connections: {err}")
}

pub const CTL_USAGE: &str =
    "unknown command; see `emu --help` for the commands and their arguments";
pub const CTL_NO_TOUCHSCREEN: &str = "this instance has no touchscreen";
pub const CTL_NO_KEYBOARD: &str = "this instance has no keyboard";
pub const CTL_NO_DISPLAY: &str = "this instance has no display to capture";
pub const CTL_NO_SENSORS: &str = "this instance has no motion sensors";
pub const CTL_MACRO_TOO_LARGE: &str = "that macro file is larger than a macro can be";
pub const CTL_MACRO_CANCELLED: &str = "the macro was cancelled";
pub const CTL_ALREADY_RECORDING: &str =
    "a macro is already being recorded; end it with record-stop";
pub const CTL_NOT_RECORDING: &str = "no macro is being recorded";
/// First line of a recorded macro file.
pub const MACRO_HEADER: &str = "# Recorded by emu. One command per line; see `emu --help`.";

pub fn ctl_bad_macro_line(path: &Path, line: usize) -> String {
    format!(
        "{}: line {line} is not a command a macro can contain",
        path.display()
    )
}
pub const CTL_NO_LOCATION: &str = "this instance has no location service";
pub const CTL_BAD_LOCATION: &str =
    "a location needs a latitude from -90 to 90 and a longitude from -180 to 180";
pub const CTL_BAD_TOUCH: &str =
    "that finger or position is outside the screen, or that finger is not down";
pub const CTL_BAD_KEY: &str = "the keyboard has no key with that code";
pub const CTL_GUEST_BEHIND: &str = "the guest is not reading its input; the event was dropped";
pub const CTL_NO_FRAME_YET: &str = "the guest has not shown anything yet";
pub const CTL_SOCKET_IN_USE: &str = "another instance is already listening on this control socket";
pub const CTL_NOT_A_SOCKET: &str = "the control socket path names a file that is not a socket";
pub const CTL_NO_REPLY: &str = "the instance closed the connection without answering";
pub const CTL_NEEDS_ARGUMENTS: &str = "`emu ctl` needs a socket path and a command";

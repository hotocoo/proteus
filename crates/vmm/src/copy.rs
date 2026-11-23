//! Every message the `proteus` binary can show. Code refers to these by name.
// On a host that has no hypervisor backend yet, the machine is not built and its messages go unused.
#![cfg_attr(not(host_board), allow(dead_code))]

use std::fmt::Display;
use std::path::Path;
use std::time::Duration;

pub const HELP: &str = "\
proteus: lightweight Android emulator

Usage: proteus --kernel <file> [options]
       proteus ctl <socket or instance> <command>
       proteus update                (install the newest release, through the GitHub CLI)
       proteus instance create <name> <options>   Make a named instance that starts with these options
       proteus instance start <name> [<options>]  Start it, with its own data, socket and adb port
       proteus instance list                      Name each instance, and say which are running
       proteus instance rm <name> [--yes]         Delete an instance and its Android data, for good

  --kernel <file>         Uncompressed kernel image (required)
  --initrd <file>         Ramdisk (uncompressed or kernel-supported cpio)
  --android <dir>         Boot Android from a directory of AOSP virtual-device images
                          (init_boot.img, vendor_boot.img, super.img, vbmeta*.img)
  --data-dir <dir>        Where this Android instance keeps its data (default: <dir>/proteus-data)
  --userdata-gib <n>      Storage, in GB as Android shows it, when the data is first made
  --cmdline <text>        Kernel command line
  --ram-mib <n>           Guest RAM in MiB
  --vcpus <n>             Number of virtual CPUs
  --disk <file>           Attach a read-write raw disk image (repeatable, in slot order)
  --ro-disk <file>        Attach a read-only raw disk image (repeatable, in slot order)
  --bootconfig <k=v>      Add a bootconfig parameter, such as androidboot.hardware=emu (repeatable)
  --serial                Attach the debug serial console to this terminal
  --display <WxH>         Size of the guest display in pixels (default 720x1280, a phone held upright)
  --refresh-hz <n>        Frames per second the display runs at: native (default: the fastest
                          multiple of 60 this screen shows), or 60, 90, 120, 144
  --writable-system       Give this Android its own writable copy of the system partitions, made
                          once in its data folder (for Google apps); otherwise they are read-only
  --screen-shape <shape>  Shape the window keeps when resized: free (default), 16:9, 18:9, 19:9,
                          19.5:9 or 20:9
  --no-display            Give the guest no display device
  --headless              Open no window; the display can still be captured
  --keymap <file>         Keys and mouse movements that stand for touches (see the README)
  --keymap-dir <dir>      Key maps named <package>.toml (and default.toml) for the app in front
  --frame-dump <file>     Keep a PNG of the guest display in this file, refreshed about once a second
  --ctl <socket>          Listen on this local socket for `proteus ctl` commands
  --logcat <file>         Write Android's system log to this file
  --no-sound              Give the guest no sound card
  --mute                  Keep the sound card but play nothing on this machine
  --gpu                   Draw with this machine's GPU (gfxstream over Vulkan) if this build has it
                          and a Vulkan driver works; otherwise the guest draws in software
  --gamepad               Give the guest a gamepad even with no host pad plugged in
  --no-gamepad            No gamepad, even with a host pad plugged in
  --sync <socket or instance>  Send this instance's touches, keys and pad moves to another
                          one as well (repeatable); give them the same display size
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

`proteus ctl <socket> <command>` drives a running instance that was started with --ctl:
  tap <x> <y>                            Touch the screen briefly at a pixel
  swipe <x1> <y1> <x2> <y2> [<ms>]       Drag a finger from one pixel to another
  touch <finger> <x> <y>                 Put a finger down, or move it (fingers 0 to 9)
  lift <finger>                          Take that finger off the screen
  key <code> [down|up]                   Press a key by its Linux key code
  pad <control> <value>                  Move a gamepad control: a b x y l1 r1 l2 r2 select
                                         start mode thumbl thumbr (down/up), sticks lx ly rx ry
                                         (-1 to 1), triggers lt rt (0 to 1), hatx haty (-1 0 1)
  screenshot <file>                      Save what the display shows as a PNG
  screenrecord <file> [<seconds>]        Record the screen as MP4, up to 180 seconds
  install <file or folder>               Install an APK, a folder of split APKs, or an
                                         .apks/.xapk archive with its OBB files
  location <lat> <lon> [<metres>]        Put the device at this position on Earth
  location <map link>                    The same, from a geo: or map link with coordinates
  walk <degrees> <km/h> | walk stop      Walk from there, degrees clockwise from north
  route <gpx file> [<km/h>]              Follow a GPX track or route (default 5 km/h)
  rotate <x> <y> <z>                     Turn the device, in degrees about its three axes;
                                         0 0 0 is upright, -90 0 0 is flat on a table
  shake                                  Shake the device
  shell <command>                        Run a command in Android's shell and print what it
                                         printed, such as `am force-stop <package>`
  window-screenshot <file>               Save what the window shows, controls included, as a PNG
  window-click <x> <y>                   Click the window at a point, in points from its top left
  window-resize <w> <h>                  Resize the window, in points, as dragging its edge would
  window-fullscreen                      Full screen (Game Mode on a Mac) or back to a window
  window-scroll <x> <y> <lines>          Turn the mouse wheel there (negative scrolls down)
  shutdown                               Shut Android down, then turn the machine off
  stop                                   Turn the machine off at once, like pulling the plug
  record <file>                          Record the touches and keys that follow as a macro
  record-stop                            End the recording and write the file
  run <file> [<times>]                   Play a macro: a file of these commands, one per line,
                                         with `wait <ms>` for pauses and `#` for comments
  cancel                                 Stop the macro that is playing";

pub const USAGE_HINT: &str = "run `proteus --help` for usage";

pub fn fatal(err: &dyn Display) -> String {
    format!("proteus: {err}")
}

pub fn boot_time(elapsed: Duration) -> String {
    format!("proteus: boot marker seen after {} ms", elapsed.as_millis())
}

pub const BOOT_MARKER_NOT_SEEN: &str = "proteus: boot marker was never seen on the console";

pub fn bad_argument(err: &dyn Display) -> String {
    format!("command line: {err}")
}

pub fn unexpected_argument(arg: &str) -> String {
    format!("unexpected argument `{arg}`")
}

pub fn bad_number(option: &str) -> String {
    format!("--{option} needs a positive whole number")
}

pub const MISSING_KERNEL: &str = if cfg!(target_arch = "x86_64") {
    "--kernel is required (with --android it can be left out when kernel-x86_64 is beside proteus)"
} else {
    "--kernel is required (with --android it can be left out when kernel-arm64 is beside proteus)"
};
pub const BAD_BOOTCONFIG: &str = "--bootconfig needs the form key=value";
pub const ANDROID_WITH_INITRD: &str =
    "--android brings its own ramdisk; do not pass --initrd with it";
pub const DATA_DIR_NEEDS_ANDROID: &str = "--data-dir only applies with --android";
pub const LOGCAT_NEEDS_ANDROID: &str = "--logcat only applies with --android";
pub const BAD_DISPLAY: &str = "--display needs a size such as 720x1280";
pub fn refresh_too_fast(most: u32) -> String {
    format!("--refresh-hz is too fast for this display size; at most {most}")
}
pub const BAD_ADB_PORT: &str = "--adb-port needs a port number from 0 to 65535";
/// The monitor was moved to a refresh rate whole frames fit, for fullscreen.
#[cfg(target_os = "macos")]
pub fn refresh_matched(hz: f64) -> String {
    format!("fullscreen: monitor runs at {hz:.0} Hz while fullscreen, so frames move evenly")
}
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

#[cfg(target_os = "macos")]
pub const HOST_RAM_THREAD: &str = "host-ram";
#[cfg(target_os = "macos")]
pub fn ram_alias_failed(kr: i32) -> String {
    format!(
        "could not keep a second mapping of guest memory (kern_return {kr}); Activity Monitor will count shared guest pages twice"
    )
}
#[cfg(target_os = "macos")]
pub fn ram_refresh_failed(kr: i32) -> String {
    format!("could not refresh the program's view of guest memory (kern_return {kr})")
}
#[cfg(target_os = "macos")]
pub fn ram_refresh_thread_failed(err: &std::io::Error) -> String {
    format!("could not start the guest memory refresh thread: {err}")
}
#[cfg(target_os = "macos")]
pub fn ram_release_failed(err: &dyn Display) -> String {
    format!("could not give freed guest memory back to macOS: {err}")
}

pub fn irq_failed(line: u32, err: &dyn Display) -> String {
    format!("interrupt line {line}: {err}")
}

pub fn vcpu_stopped(index: u32, err: &dyn Display) -> String {
    format!("vCPU {index} stopped: {err}")
}

#[cfg(not(host_board))]
pub const HOST_UNSUPPORTED: &str = "this build has no hypervisor backend for this host";
pub const BOOTCONFIG_NEEDS_INITRD: &str =
    "--bootconfig needs --initrd, because the kernel reads it from the end of the ramdisk";
pub const TOO_MANY_VCPUS: &str = "more vCPUs requested than the host or the board supports";
pub const TOO_MANY_DISKS: &str = "more disks than the board has disk slots (three)";
#[cfg(target_arch = "aarch64")]
pub const GIC_GEOMETRY: &str =
    "the host's interrupt controller does not fit the board's address map";
#[cfg(target_arch = "aarch64")]
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

pub fn window_sizes_not_saved(why: &std::io::Error) -> String {
    format!("the window's size could not be kept for next time: {why}")
}

pub fn keymap_presets_added(copied: usize, dir: &std::path::Path) -> String {
    format!(
        "key maps: {copied} game preset(s) added to {}",
        dir.display()
    )
}

pub fn keymap_presets_failed(why: &std::io::Error) -> String {
    format!("key maps: the game presets could not be added: {why}")
}

pub fn keymap_other_size(width: u32, height: u32, display: (u32, u32)) -> String {
    format!(
        "key map: made for a {width}x{height} screen, but the guest's long side is {}",
        display.0.max(display.1)
    )
}

pub fn keymap_circle_off_screen(x: u32, y: u32, radius: u32) -> String {
    format!(
        "key map: a circle of radius {radius} around {x}, {y} does not fit on the guest's screen"
    )
}

pub const KEYMAP_TOO_MANY: &str =
    "key map: more touches than the touchscreen has fingers for (eight)";
pub fn keymap_bad_tilt(max: i16) -> String {
    format!("key map: a tilt angle is from 1 to {max} degrees")
}
pub const KEYMAP_BAD_ZOOM: &str =
    "key map: zoom needs a step above zero and smaller than its spread";
pub const KEYMAP_BAD_SENSITIVITY: &str = "key map: sensitivity has to be a number above zero";
pub const KEYMAP_NEEDS_WINDOW: &str =
    "key maps need a window; do not combine them with --headless or --no-display";
pub const KEYMAP_TWO_SOURCES: &str = "give --keymap or --keymap-dir, not both";
pub const FOREGROUND_THREAD: &str = "foreground app";
pub const KEYMAP_THEN_THREAD: &str = "key map menu taps";
/// Why the process keeps macOS from napping it (shown by `pmset -g assertions`).
pub const VM_ACTIVITY_REASON: &std::ffi::CStr = c"Running an Android guest";
pub const ORIENTATION_THREAD: &str = "orientation";
pub const SETUP_THREAD: &str = "android setup";

pub fn setup_failed(err: &dyn Display) -> String {
    format!("could not finish setting Android up: {err}")
}

pub fn bundled_installed(name: &str) -> String {
    format!("proteus: installed {name}")
}

pub fn keymap_switched(package: &str, path: Option<&Path>) -> String {
    match path {
        Some(path) => format!("{package} is in front: key map {}", path.display()),
        None => format!("{package} is in front: no key map"),
    }
}

pub fn keymap_refused(path: &Path, why: &dyn Display) -> String {
    format!("{}: {why}", path.display())
}

pub const AUDIO_THREAD: &str = "audio";
pub const AUDIO_NO_DEVICE: &str = "this machine has no sound output";
pub const AUDIO_OPENED: &str = "sound output opened: the guest has started playing";

pub fn audio_failed(err: &dyn Display) -> String {
    format!("sound output: {err}; the guest's sound will not be heard")
}
pub const WINDOW_TITLE: &str = "Proteus";
/// The title while the guest draws in software because the GPU renderer did not start, so that
/// a slow picture is never mistaken for a hung machine.
pub const WINDOW_TITLE_SOFTWARE: &str =
    "Proteus (software drawing: the GPU renderer did not start)";
/// Names egui keeps the window's parts by.
pub const GUEST_TEXTURE: &str = "guest";
pub const SHARED_FRAMES_ON: &str =
    "guest frames are read straight into GPU-visible memory, with no second copy";
pub const SHARED_FRAMES_OFF: &str =
    "the GPU cannot read the guest's frames where they are; each one is copied to it";

pub fn frame_rate(frames: u32, over: std::time::Duration) -> String {
    format!(
        "guest frames: {:.1} per second",
        f64::from(frames) / over.as_secs_f64()
    )
}

pub fn gpu_wait_failed(err: &dyn Display) -> String {
    format!("the window's GPU work did not finish: {err}")
}

pub fn window_touch(window_x: f64, window_y: f64, x: u32, y: u32) -> String {
    format!("window pixel {window_x:.0},{window_y:.0} touches guest pixel {x},{y}")
}
pub const LAUNCHER_TITLE: &str = "Proteus";
pub const LAUNCHER_HEADER_ID: &str = "launcher header";
pub const TOOLBAR_ID: &str = "toolbar";
pub const PANEL_ID: &str = "panel";
pub const STATUS_ID: &str = "status";

pub fn window_failed(err: &dyn Display) -> String {
    format!("the window could not be opened (use --headless to run without one): {err}")
}

pub fn window_grab_failed(err: &dyn Display) -> String {
    format!("the window could not keep the pointer for the key map: {err}")
}

pub fn window_input_dropped(why: &dyn Display) -> String {
    format!("input from the window was dropped: {why}")
}
pub const CONSOLE_INPUT_THREAD: &str = "console-in";
pub const REBOOTING: &str = "proteus: guest asked to restart; booting again";
pub const RESET_LOOP: &str = "the guest keeps resetting right after it starts; giving up";
/// Prefix of the serial number each disk reports to the guest, followed by its slot.
pub const DISK_ID_PREFIX: &str = "emu-disk-";
/// Serial number the Android system disk reports to the guest.
pub const ANDROID_DISK_ID: &str = "proteus-android";
/// Extension of the temporary file a frame dump is written to before it replaces the real one.
pub const FRAME_DUMP_PARTIAL_EXTENSION: &str = "partial";
pub const FRAME_DUMP_THREAD: &str = "frame-dump";

/// Names the guest sees for its input devices.
pub const TOUCHSCREEN_NAME: &str = "emu touchscreen";
pub const KEYBOARD_NAME: &str = "proteus keyboard";
pub const GAMEPAD_NAME: &str = "proteus gamepad";
pub const GPU_NOT_BUILT: &str =
    "this build of Proteus has no GPU acceleration; the guest draws in software";
pub const GPU_ACCELERATED: &str = "proteus: the guest draws with this machine's GPU";
pub const GPU_CHECK_HUNG: &str = "it did not start in time";

/// The window's pop-up when the GPU renderer did not start.
pub fn gpu_unavailable_short(why: &dyn Display) -> String {
    format!(
        "The GPU renderer did not start ({why}). Games will be very slow; restart to try again."
    )
}

pub fn gpu_unavailable(why: &dyn Display) -> String {
    format!("proteus: the GPU renderer does not work here ({why}); the guest draws in software")
}

pub const GAMEPAD_THREAD: &str = "gamepads";

pub fn gamepads_unavailable(err: &dyn Display) -> String {
    format!("host gamepads cannot be read: {err}")
}
pub const INPUT_SERIAL: &str = "0";

pub const SENSORS_THREAD: &str = "sensors";

pub fn sensor_request(request: &str) -> String {
    format!("sensor service asked: {}", request.trim_end())
}

pub const CTL_THREAD: &str = "ctl";
/// The control socket's answer to a command that worked.
pub const CTL_OK: &str = "ok";
pub const CTL_NO_WINDOW: &str = "this emulator has no window";
pub const CTL_WINDOW_SILENT: &str = "the window did not answer";
pub const CTL_NO_PICTURE: &str = "the window has nothing to show yet";
pub const CTL_NO_ADB: &str = "adb is not up yet (or was turned off with --adb-port 0)";

pub fn ctl_shell_failed(err: &dyn Display) -> String {
    format!("the guest's shell did not answer: {err}")
}

/// The control socket's answer to a command that did not.
pub fn ctl_error(why: &dyn Display) -> String {
    format!("error: {why}")
}

pub fn ctl_accept_failed(err: &dyn Display) -> String {
    format!("control socket stopped accepting connections: {err}")
}

#[cfg(target_arch = "x86_64")]
pub const VIRTIO_MMIO_DEVICE_ARG: &str = "virtio_mmio.device=";

pub const CTL_USAGE: &str =
    "unknown command; see `proteus --help` for the commands and their arguments";
pub const CTL_NO_TOUCHSCREEN: &str = "this instance has no touchscreen";
pub const CTL_NO_KEYBOARD: &str = "this instance has no keyboard";
pub const ADB_TOO_LARGE: &str = "adb message too large";
pub const ADB_GARBLED: &str = "Android's adb sent something that is not adb";
pub const ADB_WANTS_KEY: &str = "Android's adb asks for a key, which this emulator cannot give yet";
pub const ADB_COMMAND_REFUSED: &str = "Android's adb refused the command";
pub const ADB_NONE: &str = "this instance has no adb; it needs the network and an --adb-port";
pub const ADB_NOT_ANSWERING: &str =
    "Android's adb is not answering; wait until Android has started";
pub const INSTALL_BAD_ARCHIVE: &str = "not an APK, a directory of APKs or an archive of them";
pub const INSTALL_ZIP64: &str = "archives over 4 GiB are not supported";
pub const INSTALL_TOO_LARGE: &str = "a compressed APK in this archive is too large to unpack";
pub const INSTALL_NOTHING: &str = "there is no APK there to install";
pub fn ctl_record_too_long(max: Duration) -> String {
    format!("a screen recording lasts at most {} seconds", max.as_secs())
}

pub fn ctl_record_failed(said: &str) -> String {
    format!("the screen could not be recorded: {said}")
}

pub const INSTALL_THREAD: &str = "install";
pub const GUI_FILL_THREAD: &str = "window shape";
pub const INSTALLED: &str = "installed the dropped app";

pub fn install_refused(said: &str) -> String {
    format!("Android refused the app: {said}")
}

pub fn install_failed(path: &Path, err: &dyn Display) -> String {
    format!("could not install {}: {err}", path.display())
}

pub const CTL_NO_GAMEPAD: &str = "this instance has no gamepad; start it with --gamepad";
pub const CTL_NO_DISPLAY: &str = "this instance has no display to capture";
pub const CTL_NO_SENSORS: &str = "this instance has no motion sensors";
pub const CTL_MACRO_TOO_LARGE: &str = "that macro file is larger than a macro can be";
pub const CTL_MACRO_CANCELLED: &str = "the macro was cancelled";
pub const CTL_ALREADY_RECORDING: &str =
    "a macro is already being recorded; end it with record-stop";
pub const CTL_NOT_RECORDING: &str = "no macro is being recorded";
/// First line of a recorded macro file.
pub const MACRO_HEADER: &str = "# Recorded by proteus. One command per line; see `proteus --help`.";

pub fn ctl_bad_macro_line(path: &Path, line: usize) -> String {
    format!(
        "{}: line {line} is not a command a macro can contain",
        path.display()
    )
}
pub const GPX_BROKEN: &str = "that GPX file has a point without a valid latitude and longitude";
pub const GPX_EMPTY: &str = "that GPX file has no track, route or waypoints";
pub const LOCATION_NOT_SET: &str = "set a location first; walking starts from it";
pub fn ctl_bad_speed(max: f64) -> String {
    format!("a speed is above 0 and at most {max} km/h")
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
pub const CTL_NEEDS_ARGUMENTS: &str =
    "`proteus ctl` needs a socket path or instance name, and a command";
pub const INSTANCE_USAGE: &str =
    "use `proteus instance create <name> <options>`, `start <name>`, `list` or `rm <name>`";
pub const INSTANCE_NO_HOME: &str = "no home folder for instances; set PROTEUS_HOME";
pub const INSTANCE_BAD_NAME: &str = "an instance name is 1 to 32 letters, digits, '-' or '_'";
pub const INSTANCE_OWN_OPTIONS: &str =
    "an instance has its own --data-dir and --ctl; leave them out";
pub const INSTANCE_BAD_ARGUMENT: &str = "an option of an instance cannot contain a line break";
pub const INSTANCE_PATH_TOO_LONG: &str =
    "the instance's control socket path would be too long; use a shorter name or PROTEUS_HOME";
pub const INSTANCE_EXISTS: &str = "there is already an instance with that name";
pub fn frame_path(shared: bool) -> &'static str {
    if shared {
        "frames reach the window without an upload"
    } else {
        "frames are uploaded to the window"
    }
}
pub fn bad_screen_shape(name: &str) -> String {
    format!("--screen-shape: unknown shape `{name}`; use free, 16:9, 18:9, 19:9, 19.5:9 or 20:9")
}
pub fn system_copy_made(path: &std::path::Path) -> String {
    format!(
        "making this instance's own copy of the system partitions: {}",
        path.display()
    )
}
pub const INSTANCE_UNKNOWN: &str = "there is no instance with that name";
pub const INSTANCE_RUNNING: &str = "that instance is running; stop it first";
pub const INSTANCE_KEPT: &str = "nothing was deleted";
pub const UPDATE_UNSUPPORTED: &str =
    "this build cannot update itself; download the newest release instead";
pub const UPDATE_NEEDS_CURL: &str =
    "updating needs curl, which comes with this system; it was not found";
pub const UPDATE_STOPPED: &str = "the update stopped unexpectedly";
pub const UPDATE_NO_RELEASE: &str = "the repository has no release yet";
pub const UPDATE_NO_ASSET: &str = "the newest release has no package for this system";
pub const UPDATE_UNPACK_FAILED: &str = "the downloaded package could not be unpacked";

pub fn update_bad_listing(why: &str) -> String {
    format!("GitHub's list of releases could not be read: {why}")
}

pub fn update_fetch_failed(why: &str) -> String {
    format!("could not reach GitHub: {why}")
}

pub fn up_to_date(tag: &str) -> String {
    format!("Proteus is up to date ({tag})")
}

pub fn updated(tag: &str) -> String {
    format!("updated to {tag}; restart Proteus to use it")
}
pub const INSTANCE_RUNNING_MARK: &str = "running";

pub fn instance_confirm_delete(name: &str) -> String {
    format!("Delete instance {name} and all of its Android data, for good? Type y to delete: ")
}

pub fn sync_failed(target: &Path, why: &dyn Display) -> String {
    format!("input sync to {}: {why}", target.display())
}

pub const SHUTDOWN_REQUEST_THREAD: &str = "stop request";
pub const SYNC_THREAD: &str = "input sync";
pub const ADB_INPUT_SHORT: &str = "the file ended before its full length was sent";
pub const DATA_DIR_LOCKED: &str =
    "another emulator is running on this data folder; stop it first, or use a different --data-dir";
pub const CTL_RECORDING_FULL: &str =
    "the recording got longer than a macro can be; it was saved up to there";
pub const CTL_BAD_SYNC: &str = "only input commands can be synced";

/// What the window's controls say.
pub mod gui {
    use std::path::Path;

    pub const BACK: &str = "Back";
    pub const BACK_HINT: &str = "Android's Back button (⌘⌫)";
    pub const HOME: &str = "Home";
    pub const HOME_HINT: &str = "Android's Home button (⌘⇧H)";
    pub const RECENTS: &str = "Recents";
    pub const RECENTS_HINT: &str = "Recent apps; swipe a card away there to close that app (⌘O)";
    pub const POWER: &str = "Power";
    pub const POWER_HINT: &str = "Power button: turns the screen off and on (⌘P)";
    pub const VOLUME_UP: &str = "Vol +";
    pub const VOLUME_UP_HINT: &str = "Volume up (⌘↑)";
    pub const VOLUME_DOWN: &str = "Vol −";
    pub const VOLUME_DOWN_HINT: &str = "Volume down (⌘↓)";
    pub const TURN_LEFT: &str = "Turn left";
    pub const TURN_RIGHT: &str = "Turn right";
    pub const TURN_HINT: &str =
        "Turn the device a quarter turn; Android follows when auto-rotate is on (⌘← ⌘→)";
    pub const SCREENSHOT: &str = "Screenshot";
    pub const SCREENSHOT_HINT: &str = "Save what the screen shows on the Desktop (⌘S)";

    pub const APPS: &str = "Apps";
    pub const LOCATION: &str = "Location";
    pub const SENSORS: &str = "Sensors";
    pub const MACROS: &str = "Macros";
    pub const KEYS: &str = "Key map";
    pub const CAPTURE: &str = "Capture";
    pub const SETTINGS: &str = "Settings";

    pub const WORKING: &str = "Working…";
    pub const SHUTTING_DOWN: &str = "Android is shutting down… (close again to cut the power)";
    pub const CHOOSE: &str = "Choose…";

    pub const LAUNCHER_TITLE: &str = "Android emulator";
    pub const UPDATE: &str = "Update Proteus";
    pub const UPDATE_HINT: &str = "Install the newest release from GitHub";
    pub const UPDATING: &str = "Updating…";
    pub const LAUNCHER_SUBTITLE: &str =
        "Each instance is a phone of its own: its own apps, data and settings.";
    pub const INSTANCES: &str = "Instances";
    pub const NO_INSTANCES: &str = "None yet. Make one below from an Android image folder.";
    pub const RUNNING: &str = "Running";
    pub const STOPPED: &str = "Stopped";
    pub const START: &str = "Start";
    pub const STOP: &str = "Stop";
    pub const DELETE: &str = "Delete";
    pub const DELETE_SURE: &str = "Delete it and its apps?";
    pub const NEW_INSTANCE: &str = "New instance";
    pub const NAME: &str = "Name";
    pub const CREATE_AND_START: &str = "Create and start";
    pub const NEEDS_IMAGE: &str = "Choose the Android image folder first.";
    pub const BACK_TO_LIST: &str = "← Instances";
    pub const GET_ANDROID: &str = "Getting an Android image";
    pub const GET_ANDROID_HINT: &str = if cfg!(target_arch = "x86_64") {
        "Open this page, pick a green build of aosp_cf_x86_64_only_phone-userdebug, download \
        aosp_cf_x86_64_only_phone-img-<build>.zip (about 1.1 GB) and unzip it into a folder. \
        Choose that folder above."
    } else {
        "Open this page, pick a green build of aosp_cf_arm64_only_phone-userdebug, download \
        aosp_cf_arm64_only_phone-img-<build>.zip (about 1.1 GB) and unzip it into a folder. \
        Choose that folder above."
    };
    pub const COPY: &str = "Copy";
    pub const RESTART_FROM_WINDOW: &str =
        "Saved. Stop and start the instance, or use Restart in its window, to apply.";
    pub const STOP_FIRST: &str = "Stop the instance first.";

    pub fn starting(name: &str) -> String {
        format!("Starting {name}…")
    }

    pub fn stopping(name: &str) -> String {
        format!("Stopping {name}.")
    }

    pub fn deleted(name: &str) -> String {
        format!("Deleted {name}.")
    }

    pub fn reset_done(name: &str) -> String {
        format!("{name} will start as a new phone.")
    }

    pub const ROM: &str = "Android image (ROM)";
    pub const ROM_HINT: &str = "The folder of Google's virtual-device image, and the kernel that \
        boots it. Leave the kernel empty to use the one that comes with proteus.";
    pub const IMAGE_FOLDER: &str = "Image folder";
    pub const IMAGE_FOLDER_HINT: &str = if cfg!(windows) {
        r"C:\path\to\aosp_cf_x86_64_only_phone-img"
    } else if cfg!(target_arch = "x86_64") {
        "/path/to/aosp_cf_x86_64_only_phone-img"
    } else {
        "/path/to/aosp_cf_arm64_only_phone-img"
    };
    pub const KERNEL: &str = "Kernel";
    pub const KERNEL_HINT: &str = "Built in";
    pub const PERFORMANCE: &str = "Performance";

    pub fn host_has(cpus: usize, memory_mib: u64) -> String {
        format!(
            "This computer has {cpus} CPUs and {} GB of memory.",
            memory_mib / 1024
        )
    }

    pub const CUSTOM: &str = "Custom";
    pub const DISPLAY_PHONE_TALL: &str = "Phone, tall";
    pub const SCREEN_HINT: &str =
        "Sizes are upright; the window turns when an app turns the screen.";
    pub const GRAPHICS_AND_SOUND: &str = "Graphics and sound";
    pub const GPU_HINT: &str = "Draws with this computer's GPU. Without it Android draws with the CPU, \
        which is slow for games.";
    pub const CONTROLS: &str = "Controls";
    pub const DATA: &str = "Data";
    pub const DATA_SIZE: &str = "Storage";
    pub const GIB: &str = " GB";
    pub const DATA_SIZE_HINT: &str = "Used when the data is made: on first start or after a reset.";
    pub const DATA_SIZE_NEEDS_RESET: &str = "A new size takes effect after a factory reset, which \
        erases this instance's apps and data: Android's data storage is encrypted and cannot be \
        grown in place.";

    pub const WRITABLE_SYSTEM: &str = "Writable system";
    pub const WRITABLE_SYSTEM_HINT: &str = "Copies Android's system into this instance so it \
        can be changed: needed to install Google apps or root.";
    pub const ADVANCED: &str = "Advanced";
    pub const LOG_LEVEL: &str = "Log";
    pub const LOG_DEFAULT: &str = "default";
    pub const OTHER_OPTIONS: &str = "Other options";
    pub const OTHER_OPTIONS_HINT: &str = "Any other command-line option, one word per line \
        (proteus --help lists them).";
    pub fn data_size_now(gib: u64) -> String {
        format!("This instance's storage is {gib} GB now.")
    }
    pub const FACTORY_RESET: &str = "Factory reset…";
    pub const RESET_SURE: &str = "Every app and file in this instance will be deleted.";
    pub const RESET_CONFIRM: &str = "Erase everything";
    pub const KEEP: &str = "Keep";
    pub const SAVE_AND_RESTART: &str = "Save and restart";
    pub const REVERT: &str = "Undo changes";
    pub const STARTING: &str = "Android is starting…";

    pub fn keymap_in_use(path: &Path) -> String {
        format!("Key map {} in use.", path.display())
    }

    pub const KEYMAP_NEEDS_PATH: &str = "Give the key map a file first, such as ~/game.toml.";
    pub const KEYMAP_EDIT_HINT: &str =
        "Add a tap or a stick, click where it goes on the screen, then press its key.";
    pub const CLICK_PLACE: &str = "Click on the screen where it goes.";
    pub const PRESS_KEY: &str = "Press the key for it.";
    pub const STICK_WAYS: [&str; 4] = ["up", "down", "left", "right"];

    pub fn press_way(way: &str) -> String {
        format!("Press the key that pushes the stick {way}.")
    }

    pub const ADD_TAP: &str = "Add tap";
    pub const ADD_STICK: &str = "Add stick";
    pub const REBIND: &str = "Key";
    pub const REMOVE: &str = "Remove";
    pub const RADIUS: &str = "radius";
    pub const SAVE_AND_USE: &str = "Save and use";
    pub const DISCARD: &str = "Close without saving";

    pub fn tap_line(key: &str, at: [u32; 2]) -> String {
        format!("Tap {key} at {}, {}", at[0], at[1])
    }

    pub fn stick_line(keys: [&String; 4], center: [u32; 2]) -> String {
        format!(
            "Stick {}/{}/{}/{} at {}, {}",
            keys[0], keys[1], keys[2], keys[3], center[0], center[1]
        )
    }
    pub const INSTALL_HINT: &str =
        "Drop APK, split-APK folder, .apks or .xapk files on the window, or give a path here.";
    pub const INSTALL_PATH_HINT: &str = "/path/to/game.xapk";
    pub const INSTALL_PATH_NAME: &str = "App file to install";
    pub const MAP_LINK_NAME: &str = "Map link";
    pub const PLACE_NAME: &str = "Name for this place";
    pub const MACRO_NAME: &str = "New macro's name";
    pub const MACRO_TEXT_NAME: &str = "Macro commands";
    pub const TIMES: &str = "Times to play";
    pub const CUSTOM_WIDTH: &str = "Custom width";
    pub const SYNC: &str = "Send my input to";
    pub const SYNC_HINT: &str =
        "Instance names, one per line: they get the same touches, keys and pad moves";
    pub const CUSTOM_HEIGHT: &str = "Custom height";
    pub const INSTALL: &str = "Install";
    pub const INSTALLED: &str = "Installed.";
    pub const LIST_APPS: &str = "List installed apps";
    pub const CLOSE_BACKGROUND: &str = "Close background apps";
    pub const CLOSE_BACKGROUND_HINT: &str =
        "Stops every app's background processes, freeing memory";
    pub const BACKGROUND_CLOSED: &str = "Background apps closed.";
    pub const OPEN: &str = "Open";
    pub const FORCE_STOP: &str = "Force stop";
    pub const RENDER_SCALE: &str = "Render:";
    pub const RENDER_SCALE_HINT: &str = "Draw this game smaller and let Android scale it up: \
        faster on a busy GPU, a little softer. Only for apps that say they are games; the game \
        is stopped so its next start uses it.";
    pub fn render_scale(scale: f32) -> String {
        format!("{:.0}%", scale * 100.0)
    }
    pub fn render_scale_set(package: &str, scale: f32) -> String {
        format!(
            "{package} draws at {:.0}% from its next start.",
            scale * 100.0
        )
    }
    pub const CLEAR_DATA: &str = "Clear data";
    pub const UNINSTALL: &str = "Uninstall";
    pub const UNINSTALL_SURE: &str = "Really uninstall?";

    pub fn packages_listed(count: usize) -> String {
        format!("{count} apps installed.")
    }

    pub fn stopped(package: &str) -> String {
        format!("{package} stopped.")
    }

    pub const LOCATION_HINT: &str = "Where the device is. Apps see an ordinary GPS fix.";
    pub const LATITUDE: &str = "Latitude";
    pub const LONGITUDE: &str = "Longitude";
    pub const SET_LOCATION: &str = "Go there";
    pub const BAD_COORDINATES: &str = "Latitude and longitude need to be numbers.";
    pub const MAP_LINK_HINT: &str = "Paste a map link or geo: link";
    pub const SET_FROM_LINK: &str = "Go to link";
    pub const BAD_LINK: &str = "That link has no coordinates in it.";
    pub const SAVED_PLACES: &str = "Saved places";
    pub const FORGET: &str = "Forget";
    pub const SAVE_PLACE: &str = "Save as place";
    pub const ROUTE: &str = "Follow a GPX route";
    pub const GPX_HINT: &str = "/path/to/walk.gpx";
    pub const KMH: &str = "km/h";
    pub const FOLLOW_ROUTE: &str = "Follow";
    pub const ROUTE_DONE: &str = "Route followed to its end.";
    pub const WALK: &str = "Walk from here";
    pub const BEARING: &str = "degrees from north";
    pub const WALK_START: &str = "Walk";
    pub const WALK_STOP: &str = "Stop";

    pub const SENSORS_HINT: &str = "How the device is held, in degrees about its three axes.";
    pub const AXES: [&str; 3] = ["x (tilt forward)", "y (tilt sideways)", "z (turn)"];
    pub const POSES: [&str; 4] = ["Upright", "Landscape", "Landscape, other way", "Flat"];
    pub const SHAKE: &str = "Shake";

    pub fn macros_hint(dir: &Path) -> String {
        format!(
            "Macros are kept in {}. One command per line, as for `proteus ctl`; `wait <ms>` pauses.",
            dir.display()
        )
    }

    pub const NEW_MACRO: &str = "New macro";
    pub const NEW_MACRO_TEMPLATE: &str = "# tap 360 640\n# wait 500\n";
    pub const BAD_FILE_NAME: &str = "Use letters, digits, spaces, - and _ for the name.";
    pub const SAVE: &str = "Save";
    pub const PLAY: &str = "Play";
    pub const CANCEL: &str = "Stop playing";
    pub const MACRO_DONE: &str = "Macro played.";
    pub const RECORD: &str = "Record into this macro";
    pub const RECORDING: &str = "Recording your touches and keys…";
    pub const STOP_RECORDING: &str = "Stop recording";

    pub const KEYS_HINT: &str = "A key map turns keys and the mouse into touches for games.";
    pub const KEYMAP_PATH_HINT: &str = "/path/to/game.toml";
    pub const USE_KEYMAP: &str = "Use this map";
    pub const NO_KEYMAP: &str = "No map";
    pub const EDIT_KEYMAP: &str = "Edit on screen";

    pub const SECONDS: &str = "seconds";
    pub const RECORD_SCREEN: &str = "Record the screen";
    pub const RECORD_HINT: &str = "Recordings are saved on the Desktop, without sound.";

    pub const NOT_AN_INSTANCE: &str = "This emulator was started with options on the command line. \
        Save them as an instance to change them here; the instance starts with new Android data.";
    pub const SAVE_AS_INSTANCE: &str = "Save as instance";
    pub const RELOAD: &str = "Read again";
    pub const MEMORY: &str = "Memory";
    pub const MIB: &str = " MiB";
    pub const CPUS: &str = "CPUs";
    pub const PRESETS: &str = "Quick setup";
    pub const FPS_HINT: &str = "Frames Android showed in the last second. It drops to what \
        changes: a still screen sends none.";
    pub fn fps(rate: f64) -> String {
        format!("{rate:.0} fps")
    }
    pub const LOW_FPS_HINT: &str = "The frame rate of the slowest 1% of recent frames: how bad the \
        stutters are. Close to the fps above means smooth.";
    pub fn low_fps(rate: f64) -> String {
        format!("1% low {rate:.0}")
    }
    pub const GPU_BUSY_HINT: &str = "How busy this computer's GPU was, counting every app, not only \
        this emulator.";
    pub fn gpu(percent: f64) -> String {
        format!("GPU {percent:.0}%")
    }
    pub const CPU_HINT: &str = "Host CPU this emulator used in the last second, as a share of one \
        core: 400% is four cores busy.";
    pub fn cpu(percent: f64) -> String {
        format!("CPU {percent:.0}%")
    }
    pub const MEMORY_HINT: &str = "Memory this emulator holds on this computer, as the system \
        counts it: the RAM Android has used plus the GPU memory its apps use.";
    pub fn memory(bytes: u64) -> String {
        format!("RAM {:.1} GB", bytes as f64 / 1e9)
    }
    pub const PRESET_GAMING: &str = "Gaming";
    pub const PRESET_BALANCED: &str = "Balanced";
    pub const PRESET_LOW_END: &str = "Low-end";
    pub const PRESETS_HINT: &str =
        "Sets memory, CPUs, frame rate and GPU in one click; change any of them after.";
    pub const HIDE_TOOLBAR: &str = "Hide ›";
    pub const HIDE_TOOLBAR_HINT: &str = "Hide the toolbar; the picture takes its room. ⌘\\ also.";
    pub const SHOW_TOOLBAR: &str = "‹";
    pub const SHOW_TOOLBAR_HINT: &str = "Show the toolbar. ⌘\\ also.";
    pub const SCREEN_SHAPE: &str = "Screen shape";
    pub const SCREEN_SHAPE_FREE: &str = "Free";
    pub const SCREEN_SHAPE_HINT: &str = "The window snaps to this shape when you stop resizing it, \
        and Android takes the same shape, so nothing is stretched. Free lets the window take any \
        shape. Used from the next start.";
    pub const FRAME_RATE: &str = "Frame rate";
    pub const FRAME_RATE_HINT: &str = "Android and games run up to this many frames per second. \
        Higher feels smoother and responds faster to touches, and uses more of the computer.";
    pub fn hz(rate: u32) -> String {
        if rate == crate::config::NATIVE_REFRESH {
            NATIVE_HZ.to_owned()
        } else {
            format!("{rate} Hz")
        }
    }
    pub const NATIVE_HZ: &str = "Same as this screen";
    pub const SCREEN: &str = "Screen";
    pub const DISPLAY_PHONE: &str = "Phone";
    pub const DISPLAY_PHONE_HD: &str = "Phone, full HD";
    pub const DISPLAY_TABLET: &str = "Tablet";
    pub const BY: &str = "×";
    pub const USE_SIZE: &str = "Use this size";
    pub const USE_GPU: &str = "Use this computer's GPU";
    pub const SOUND: &str = "Sound";
    pub const SOUND_ON: &str = "On";
    pub const SOUND_MUTED: &str = "Muted";
    pub const SOUND_OFF: &str = "No sound card";
    pub const GAMEPAD: &str = "Gamepad";
    pub const GAMEPAD_AUTO: &str = "If plugged in";
    pub const GAMEPAD_ALWAYS: &str = "Always";
    pub const GAMEPAD_NEVER: &str = "Never";
    pub const NETWORK: &str = "Network";
    pub const NETWORK_ON: &str = "Internet";
    pub const ALLOW_QUIC: &str = "Allow QUIC";
    pub const HOST_LOOPBACK: &str = "Let apps reach this machine's localhost";
    pub const HOST_LOOPBACK_HINT: &str =
        "Every app in Android could then reach services meant only for this machine.";
    pub const ADB_PORT: &str = "adb port";
    pub const BLOCKLISTS: &str = "Blocklists";
    pub const ALLOWLISTS: &str = "Allowlists";
    pub const ONE_FILE_PER_LINE: &str = "One file per line";
    pub const KEYMAP_FILE: &str = "Key map";
    pub const KEYMAP_FOLDER: &str = "Key maps per app";
    pub const KEYMAP_FOLDER_HINT: &str =
        "A folder of <package>.toml maps; the map follows the app in front";
    pub const SETTINGS_SAVED: &str = "Saved.";
    pub const APPLIES_ON_RESTART: &str = "Settings apply when the emulator starts.";

    pub fn preset(label: &str, size: (u32, u32)) -> String {
        format!("{label} ({}×{})", size.0, size.1)
    }

    pub fn settings_for(name: &str) -> String {
        format!("Instance \"{name}\"")
    }

    pub fn saved(path: &Path) -> String {
        format!("Saved {}", path.display())
    }

    pub fn instance_made(name: &str) -> String {
        format!("Instance \"{name}\" made. Start it with: proteus instance start {name}")
    }
}

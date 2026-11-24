//! Run configuration: defaults and the command line that overrides them.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lexopt::prelude::*;
use log::LevelFilter;

use crate::copy;

const MIB: u64 = 1 << 20;

pub const DEFAULT_RAM_MIB: u64 = 4096;
pub const DEFAULT_VCPUS: u32 = 4;
/// Size of the guest's display, in pixels.
/// A phone held upright: the shape Google's phone image is made for. Landscape apps turn it.
pub const DEFAULT_DISPLAY: (u32, u32) = (720, 1280);
/// `--refresh-hz native`: the guest's display runs as fast as this computer's screen.
pub const NATIVE_REFRESH: u32 = 0;
pub const NATIVE_REFRESH_WORD: &str = "native";
/// Frames per second the guest's display runs at unless told otherwise: the screen's own.
pub const DEFAULT_REFRESH_HZ: u32 = NATIVE_REFRESH;
/// The rate `native` means when the screen's cannot be read.
pub const FALLBACK_REFRESH_HZ: u32 = 60;
/// Games cap at 30 or 60 frames a second, and Android gives an app only whole fractions of the
/// display's one mode (its composer here uses no other), so `native` is the screen's fastest
/// multiple of this: a 144 Hz screen gets 120, where a 60 fps game runs at 60 rather than 48.
pub const GAME_FRAME_RATE: u32 = 60;

/// The display rate `native` stands for on a screen that shows `screen_hz` frames a second.
pub fn native_refresh(screen_hz: u32) -> u32 {
    (screen_hz - screen_hz % GAME_FRAME_RATE).max(GAME_FRAME_RATE)
}

/// How many frames per second this computer's main screen shows at most, if it can be read.
pub fn host_refresh_hz() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};
        // SAFETY: `+[NSScreen mainScreen]` returns an autoreleased screen or nil, and
        // `maximumFramesPerSecond` (macOS 12+) is a plain NSInteger property of it.
        let hz: isize = unsafe {
            let screen: *mut AnyObject = msg_send![class!(NSScreen), mainScreen];
            if screen.is_null() {
                return None;
            }
            msg_send![screen, maximumFramesPerSecond]
        };
        u32::try_from(hz).ok().filter(|hz| *hz > 0)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}
/// Refresh rates the settings offer: phone panels' usual ones.
pub const REFRESH_CHOICES: [u32; 4] = [60, 90, 120, 144];
/// Screen shapes the window keeps when it is resized, as long side to short side; `free` lets the
/// window take any shape and Android follow it.
pub const SCREEN_SHAPES: [(&str, Option<f32>); 6] = [
    (SCREEN_SHAPE_FREE, None),
    ("16:9", Some(16.0 / 9.0)),
    ("18:9", Some(18.0 / 9.0)),
    ("19:9", Some(19.0 / 9.0)),
    ("19.5:9", Some(19.5 / 9.0)),
    ("20:9", Some(20.0 / 9.0)),
];
pub const SCREEN_SHAPE_FREE: &str = "free";
/// The long-to-short ratio a `--screen-shape` name stands for: `Some(None)` for free.
pub fn screen_shape(name: &str) -> Option<Option<f32>> {
    SCREEN_SHAPES
        .iter()
        .find(|(label, _)| *label == name)
        .map(|(_, ratio)| *ratio)
}
/// Shares of the screen's size a game can be drawn at, then scaled up by Android (game mode).
/// Only the values Android's `cmd game set --downscale` accepts; 1.0 turns it off.
pub const RENDER_SCALES: [f32; 4] = [1.0, 0.85, 0.7, 0.5];
/// Size the guest's data partition is created with.
pub const DEFAULT_USERDATA_GIB: u64 = 8;
/// Android shows a phone's storage as the data and system partitions together, rounded up to a
/// power of two in decimal gigabytes, and counts the rounding as used. The data partition is made
/// this much under the size asked for, so the system partition fits and Android shows that size.
const STORAGE_SYSTEM_ALLOWANCE: u64 = 1_500_000_000;
const DECIMAL_GB: u64 = 1_000_000_000;

/// The storage size, in GB, a data partition of `bytes` was made for (`userdata_bytes` turned
/// back), rounded to the nearest GB.
pub fn storage_gb(bytes: u64) -> u64 {
    (bytes + STORAGE_SYSTEM_ALLOWANCE + DECIMAL_GB / 2) / DECIMAL_GB
}

/// The data partition's size, in bytes, for a phone that shows `gb` GB of storage. `None` if too
/// small to hold the system allowance, or too large to count.
pub fn userdata_bytes(gb: u64) -> Option<u64> {
    gb.checked_mul(DECIMAL_GB)?
        .checked_sub(STORAGE_SYSTEM_ALLOWANCE)
        .filter(|bytes| *bytes > 0)
}
/// Where an Android instance keeps what the guest writes, inside the images directory, unless told otherwise.
pub const DEFAULT_DATA_DIR: &str = "proteus-data";
const OLD_DATA_DIR: &str = "emu-data";
pub const DEFAULT_LOG_LEVEL: LevelFilter = LevelFilter::Warn;
/// What Android's first-stage init prints when it starts; the end point of the cold-boot measurement.
pub const DEFAULT_BOOT_MARKER: &str = "init first stage started";

/// Kernel arguments the VMM adds itself.
/// `8250.nr_uarts=1` matters on GKI, which is built with no 8250 ports unless the command line asks.
pub const SERIAL_CONSOLE_ARG: &str = "8250.nr_uarts=1 console=ttyS0";
pub const BOOTCONFIG_ARG: &str = "bootconfig";

/// Entropy handed to the guest kernel through the device tree, in bytes.
pub const RNG_SEED_LEN: usize = 64;
/// How long the serial input pump waits when the guest has not drained its receive FIFO.
pub const SERIAL_INPUT_RETRY: Duration = Duration::from_millis(1);
/// The frame dump file is rewritten at most this often.
pub const FRAME_DUMP_INTERVAL: Duration = Duration::from_secs(1);
/// Buffers frames are read into where the GPU samples them: the newest frame and the one before
/// it (a reader may hold either), one the GPU may still be drawing, and one to read the next into.
pub const SHARED_FRAME_SLOTS: usize = 4;
/// Hardware addresses of the network cards that have no cable (see the board's spare slots).
/// Locally administered, and different from the real card's.
pub const SPARE_NIC_MACS: [[u8; 6]; 2] = [
    [0x52, 0x54, 0x00, 0x12, 0x34, 0x60],
    [0x52, 0x54, 0x00, 0x12, 0x34, 0x61],
];
/// While the machine is stopping, how often vCPUs that are still running are told again.
pub const STOP_KICK_INTERVAL: Duration = Duration::from_millis(5);
/// How long `tap` keeps the finger down, and a key press keeps the key down.
pub const TAP_HOLD: Duration = Duration::from_millis(50);
/// A swipe moves the finger once per this interval.
pub const SWIPE_STEP: Duration = Duration::from_millis(10);
/// How long a swipe takes when the command does not say.
pub const DEFAULT_SWIPE: Duration = Duration::from_millis(300);
/// Frames of sound waiting between the guest and the host's output. This much, plus the host's own
/// buffer, is how late the guest's sound is heard.
pub const AUDIO_RING_FRAMES: usize = 2400;
/// How often a full ring is looked at again for room.
pub const AUDIO_RING_POLL: Duration = Duration::from_millis(1);
/// How much longer than the sound itself a full ring is waited on before the sound is dropped.
pub const AUDIO_STALL_MARGIN: Duration = Duration::from_millis(100);
/// How often the motion sensors are read, and sent to the guest if their readings changed.
pub const SENSOR_INTERVAL: Duration = Duration::from_millis(20);
/// How often unchanged readings are sent again anyway.
pub const SENSOR_KEEPALIVE: Duration = Duration::from_secs(1);
/// How fast the device turns towards a new orientation, in degrees per second about each axis.
pub const ROTATION_SPEED: f64 = 360.0;
/// How long a shake lasts, how often it changes direction, and how hard it pushes (m/s²).
pub const SHAKE_DURATION: Duration = Duration::from_millis(600);
pub const SHAKE_HZ: f64 = 8.0;
pub const SHAKE_ACCELERATION: f64 = 25.0;
/// How often a waiting macro looks whether it has been cancelled.
pub const MACRO_CANCEL_POLL: Duration = Duration::from_millis(20);
/// The largest macro file that is read.
pub const MACRO_MAX_BYTES: u64 = 1 << 20;
/// First word of the command line that sends a command to a running instance instead of starting one.
pub const CTL_SUBCOMMAND: &str = "ctl";
/// How long to listen for host gamepads being announced at start, before deciding there are none.
pub const GAMEPAD_SETTLE: Duration = Duration::from_millis(150);
/// How long the check for the app in front may take; it prints at once or not at all.
pub const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(5);
/// Speed a GPX route is followed at when none is given: a walk.
pub const ROUTE_DEFAULT_KMH: f64 = 5.0;
/// Fastest a walk or route may go; anything faster is a typing mistake (this is an airliner).
pub const MAX_KMH: f64 = 1000.0;
pub const INSTANCE_SUBCOMMAND: &str = "instance";
pub const INSTANCE_START: &str = "start";
/// Hidden: what `proteus` runs itself as to find out whether the GPU renderer starts.
pub const GPU_CHECK_ARG: &str = "--gpu-check";
pub const UPDATE_SUBCOMMAND: &str = "update";
/// How long the renderer may take to start before it is judged not to work.
pub const GPU_CHECK_TIMEOUT: Duration = Duration::from_secs(20);
/// How many times each driver is checked before the next is tried: one failed check can be a
/// passing hitch (a busy GPU, a driver just replaced), and software drawing is far slower.
pub const GPU_CHECK_ATTEMPTS: usize = 2;
/// File in the temporary folder that keeps the last check's own output, read back on failure.
pub const GPU_CHECK_LOG: &str = "proteus-gpu-check.log";
/// How often the check is looked at while it runs.
pub const GPU_CHECK_POLL: Duration = Duration::from_millis(50);
/// The Vulkan drivers a release may carry beside the program, best first, by gfxstream's name for
/// each and the file it reads: KosmicKrisp (Mesa, Vulkan 1.4), then MoltenVK. The layout is the one
/// gfxstream looks in itself.
pub const BUNDLED_VULKAN_DRIVERS: [(&str, &str); 2] = [
    ("kosmickrisp", "lib64/vulkan/libkosmickrisp_icd.json"),
    ("moltenvk", "lib64/vulkan/MoltenVK_icd.json"),
];
/// gfxstream's switch between its driver paths.
pub const VULKAN_ICD_VARIABLE: &str = "ANDROID_EMU_VK_ICD";
/// Where gfxstream looks for `lib64/vulkan` besides the program's own folder.
pub const VULKAN_LAUNCHER_DIR_VARIABLE: &str = "ANDROID_EMULATOR_LAUNCHER_DIR";
/// Any of these set means the user picked a Vulkan driver, so the bundled ones are left alone.
pub const VULKAN_DRIVER_VARIABLES: [&str; 3] =
    [VULKAN_ICD_VARIABLE, "VK_DRIVER_FILES", "VK_ICD_FILENAMES"];
/// Environment variable naming the emulator's home folder, where instances live.
pub const PROTEUS_HOME_VARIABLE: &str = "PROTEUS_HOME";
pub const INSTANCES_DIR: &str = "instances";
pub const INSTANCE_NAME_MAX: usize = 32;
/// Longest Unix socket path the system takes (macOS: 104 bytes with the terminating zero).
pub const SOCKET_PATH_MAX: usize = 103;

/// Where a release keeps what comes with the program: beside it, or in the app's Resources
/// folder when it is inside Proteus.app.
pub fn bundle_dirs() -> Vec<PathBuf> {
    let program = std::env::current_exe().and_then(std::fs::canonicalize);
    let Some(dir) = program.ok().and_then(|p| p.parent().map(Path::to_path_buf)) else {
        return Vec::new();
    };
    let resources = dir.join(APP_RESOURCES);
    vec![dir, resources]
}

/// The first of `bundle_dirs` that holds `name`: that folder, and the path to `name` in it.
pub fn bundled(name: &str) -> Option<(PathBuf, PathBuf)> {
    bundle_dirs()
        .into_iter()
        .map(|dir| {
            let path = dir.join(name);
            (dir, path)
        })
        .find(|(_, path)| path.exists())
}

/// The guest kernel a release carries.
pub fn bundled_kernel() -> Option<PathBuf> {
    bundled(BUNDLED_KERNEL).map(|(_, path)| path)
}
/// The kernel's file name in a release, and where a macOS app keeps it, from its program's folder.
pub const BUNDLED_KERNEL: &str = if cfg!(target_arch = "x86_64") {
    "kernel-x86_64"
} else {
    "kernel-arm64"
};
/// The GitHub repository `proteus update` installs releases from.
pub const UPSTREAM: &str = "hotocoo/proteus";
pub const APP_RESOURCES: &str = "../Resources";

/// The emulator's home folder when `PROTEUS_HOME` is not set: the platform's place for app data.
pub fn home_default() -> Option<PathBuf> {
    const MACOS_APP_DATA: &str = "Library/Application Support";
    const XDG_DATA_HOME: &str = "XDG_DATA_HOME";
    const XDG_DATA_DEFAULT: &str = ".local/share";
    const APP_FOLDER: &str = "proteus";
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if cfg!(target_os = "macos") {
            home.map(|home| home.join(MACOS_APP_DATA))
        } else {
            let data = std::env::var_os(XDG_DATA_HOME).map(PathBuf::from);
            data.or_else(|| home.map(|home| home.join(XDG_DATA_DEFAULT)))
        }
    }?;
    Some(renamed(base.join(APP_FOLDER), base.join(OLD_NAME)))
}
/// The name the program had before it was Proteus; its folders are moved over on first use.
pub const OLD_NAME: &str = "emu";
/// `new`, after moving `old` there if only `old` exists, so data kept under the old name stays.
pub fn renamed(new: PathBuf, old: PathBuf) -> PathBuf {
    if !new.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &new);
    }
    new
}
/// How often the app in front is checked, for key maps that follow it.
pub const FOREGROUND_POLL: Duration = Duration::from_secs(1);
/// How often the guest is asked which way up its screen is, and how long it may take to answer.
/// Most of the monitor the window takes when it first opens, so a tall phone screen fits.
/// Where ⌘S saves screenshots: the Desktop, named by the time, so they never overwrite each other.
pub fn screenshot_path() -> PathBuf {
    const DESKTOP: &str = "Desktop";
    const PREFIX: &str = "proteus-";
    const EXTENSION: &str = "png";
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_default();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    home.join(DESKTOP)
        .join(format!("{PREFIX}{stamp}.{EXTENSION}"))
}
/// Where ⌘S-style recordings go: beside screenshots.
pub fn recording_path() -> PathBuf {
    screenshot_path().with_extension(RECORDING_EXTENSION)
}
const RECORDING_EXTENSION: &str = "mp4";
/// The window's controls: the toolbar's width and button height, the space around buttons, the
/// open panel's width and its text fields' width, in points.
pub const TOOLBAR_WIDTH: f32 = 112.0;
/// How long a status message stays up, and how far above the window's bottom edge it sits.
pub const STATUS_SHOWN: Duration = Duration::from_secs(4);
pub const STATUS_OFFSET: [f32; 2] = [0.0, -16.0];
/// Where the button that brings the hidden toolbar back sits, from the window's top-right corner.
pub const SHOW_TOOLBAR_OFFSET: [f32; 2] = [-8.0, 8.0];
/// Whether the toolbar starts out (hidden, the picture gets the whole window), and how long it
/// stays out once the pointer leaves it and no panel is open, before it folds away by itself.
pub const TOOLBAR_SHOWN_AT_START: bool = false;
pub const TOOLBAR_AUTO_HIDE: Duration = Duration::from_secs(3);
/// Whether a fullscreen window moves its monitor to a refresh rate whole game frames fit (144 Hz
/// to 120 Hz), for even motion; the user's rate comes back on leaving fullscreen.
pub const MATCH_REFRESH_IN_FULLSCREEN: bool = true;
pub const TOOLBAR_MARGIN: f32 = 6.0;
pub const TOOLBAR_BUTTON_HEIGHT: f32 = 28.0;
pub const PANEL_WIDTH: f32 = 340.0;
/// Room a panel row keeps beside a text field for its label or button, and the narrowest field.
pub const PANEL_LABEL_ROOM: f32 = 110.0;
pub const PANEL_FIELD_MIN: f32 = 80.0;
/// Rows the macro editor shows.
pub const MACRO_EDITOR_ROWS: usize = 12;
/// Where macros and saved places are kept, under an instance's folder or the emulator's home.
pub const MACROS_DIR: &str = "macros";
pub const MACRO_EXTENSION: &str = ".txt";
pub const PLACES_FILE: &str = "places.toml";
pub const GUI_TASK_THREAD: &str = "gui task";
/// What the panels' sliders and fields allow.
pub const KMH_RANGE: std::ops::RangeInclusive<f64> = 0.5..=300.0;
pub const BEARING_RANGE: std::ops::RangeInclusive<f64> = 0.0..=359.0;
pub const DEGREES_RANGE: std::ops::RangeInclusive<f64> = -180.0..=180.0;
pub const MACRO_TIMES_RANGE: std::ops::RangeInclusive<u32> = 1..=10_000;
pub const CAPTURE_SECONDS_RANGE: std::ops::RangeInclusive<u32> = 1..=180;
pub const CAPTURE_DEFAULT_SECONDS: u32 = 30;
pub const RAM_MIB_RANGE: std::ops::RangeInclusive<u64> = 1024..=32_768;
/// How fast a drag changes the memory field, in MiB per point.
pub const RAM_MIB_STEP: f64 = 64.0;
pub const VCPUS_RANGE: std::ops::RangeInclusive<u32> = 1..=16;
pub const DISPLAY_RANGE: std::ops::RangeInclusive<u32> = 320..=3840;

/// A one-click setup in the settings form. Memory and CPUs are capped by what the Mac has.
pub struct Preset {
    pub ram_mib: u64,
    pub vcpus: u32,
    pub refresh_hz: u32,
    pub gpu: bool,
}
/// Games: many CPUs, memory for big games, a fast display, the GPU.
pub const PRESET_GAMING: Preset = Preset {
    ram_mib: 8192,
    vcpus: 8,
    refresh_hz: NATIVE_REFRESH,
    gpu: true,
};
/// Everyday apps without spending the Mac's battery.
pub const PRESET_BALANCED: Preset = Preset {
    ram_mib: 4096,
    vcpus: 4,
    refresh_hz: 60,
    gpu: true,
};
/// Older or busy Macs.
pub const PRESET_LOW_END: Preset = Preset {
    ram_mib: 3072,
    vcpus: 2,
    refresh_hz: 60,
    gpu: false,
};
/// CPUs a preset leaves to the Mac itself.
pub const PRESET_HOST_CPUS_KEPT: u32 = 2;
/// A preset uses at most this share of the Mac's memory.
pub const PRESET_HOST_MEMORY_SHARE: u64 = 2;
/// The key map editor's markers: radius and outline in points, label size, colour (RGBA), and a
/// new stick's radius in guest pixels and the range its slider allows.
pub const EDITOR_MARKER_RADIUS: f32 = 18.0;
pub const EDITOR_STROKE: f32 = 2.0;
pub const EDITOR_LABEL_SIZE: f32 = 13.0;
pub const EDITOR_MARKER_RGBA: [u8; 4] = [30, 110, 220, 190];
pub const EDITOR_STICK_RADIUS: u32 = 100;
pub const EDITOR_STICK_RADIUS_RANGE: std::ops::RangeInclusive<u32> = 20..=400;
/// The launcher: its window's size, how often it reads the instance list, a new instance's name.
pub const LAUNCHER_SIZE: (f64, f64) = (720.0, 760.0);
pub const LAUNCHER_REFRESH: Duration = Duration::from_secs(2);
pub const DEFAULT_INSTANCE: &str = "phone";
/// Where Google's prebuilt Android images are listed.
pub const ANDROID_IMAGES_URL: &str =
    "https://ci.android.com/builds/branches/aosp-android-latest-release/grid";
/// Options the launcher and settings write, by name.
pub const ANDROID_OPTION: &str = "--android";
pub const KERNEL_OPTION: &str = "--kernel";
pub const GPU_OPTION: &str = "--gpu";
pub const DATA_DIR_OPTION: &str = "--data-dir";
/// A data folder named only to check options; nothing is made there.
pub const CHECK_DATA_DIR: &str = "check";
/// What the settings' storage slider allows, in GiB, and how tall the list fields are.
pub const USERDATA_GIB_RANGE: std::ops::RangeInclusive<u64> = 4..=256;
pub const LIST_ROWS: usize = 2;
/// The `--log-level` values the settings form offers, quietest first.
pub const LOG_LEVELS: [&str; 6] = ["off", "error", "warn", "info", "debug", "trace"];
/// How often the toolbar's frame counter (and the trace log) counts the guest's frame rate.
pub const FRAME_RATE_PERIOD: Duration = Duration::from_secs(1);
/// Frames the toolbar's 1% low is worked out over: about 7 s at 144 fps, 17 s at 60 fps.
pub const LOW_FPS_FRAMES: usize = 1000;
/// A longer wait between frames is a still screen (Android sends no frame when nothing changes),
/// not a stutter, so it is left out of the 1% low.
pub const LOW_FPS_MAX_GAP: Duration = Duration::from_millis(250);
/// How long Android may take to answer the request to shut down, and to finish shutting down,
/// before the power is cut.
pub const SHUTDOWN_ASK: Duration = Duration::from_secs(5);
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the guest gets to write out what it holds before it is asked to shut down.
pub const SHUTDOWN_SYNC: Duration = Duration::from_secs(30);
pub const SHUTDOWN_THREAD: &str = "shutdown";
/// How many input commands wait for a slow follower before more are dropped.
pub const SYNC_QUEUE: usize = 256;
/// How long a follower's control socket may stall a read or write before it is dropped.
pub const SYNC_IO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `proteus ctl` waits for a line of an answer. Longer than the shutdown wait and a
/// screen recording, so that neither is cut short; a macro that is silent longer still runs on.
pub const CTL_REPLY_TIMEOUT: Duration = Duration::from_secs(900);
/// How long the control socket's accept loop rests after a failed accept (out of descriptors, say).
pub const CTL_ACCEPT_RETRY: Duration = Duration::from_millis(100);
/// How long the guest's `adb` may take to accept a write before the command is given up.
pub const ADB_WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the window may take to answer a request from the control socket.
/// Generous: a window whose display is asleep draws only about once a second.
pub const WINDOW_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `window-click` holds the button down: about as long as a quick click by hand.
pub const WINDOW_CLICK_HOLD: Duration = Duration::from_millis(80);
pub const WINDOW_SCREEN_SHARE: f64 = 0.85;
/// How long the window's size must stay still before Android is given its shape: a drag sends
/// many sizes, and each new logical size makes apps lay themselves out again.
pub const FILL_SETTLE: Duration = Duration::from_millis(400);
/// How long the window waits to ask again when Android could not take its shape yet (booting).
pub const FILL_RETRY: Duration = Duration::from_secs(2);
/// Shapes closer than this share (2%) count as the same, so rounding never asks Android again.
pub const FILL_TOLERANCE: f32 = 0.02;
/// Widths per unit of height Android's picture may take: from a third to three times as wide as
/// high, so a sliver of a window cannot make Android unusable.
pub const FILL_ASPECT_RANGE: std::ops::RangeInclusive<f32> = (1.0 / 3.0)..=3.0;
/// Apps a release carries beside the program, installed when a data folder first boots, and the
/// file in the data folder that notes which were.
pub const BUNDLED_APPS_DIR: &str = "apps";
/// Key maps for particular games a release carries, copied into an instance's key map folder.
pub const BUNDLED_KEYMAPS_DIR: &str = "keymaps";
/// How long a key map waits before each `then` tap: long enough for the menu the key's own tap
/// opened to be on screen (Wuthering Waves' Terminal takes over a second from the finger lifting).
pub const KEYMAP_THEN_WAIT: Duration = Duration::from_millis(1300);
pub const INSTALLED_APPS_FILE: &str = "bundled-apps-installed";
/// How often the guest is asked whether it has booted, and how long it may take to answer.
pub const SETUP_POLL: Duration = Duration::from_secs(2);
/// How often Android's file cache is checked and, when too big, dropped.
pub const CACHE_TRIM_PERIOD: Duration = Duration::from_secs(60);
/// Android's file cache is dropped once it holds more than one part in this many of guest RAM.
/// One sixteenth (512 MB of 8 GB): the host already caches the disk images, so the guest's copy
/// mostly doubles it, and only the inactive part is ever dropped.
pub const CACHE_TRIM_SHARE: u64 = 16;
/// Most inactive anonymous memory moved into zram per `CACHE_TRIM_PERIOD` (KiB): about 4 s of one
/// guest core compressing, so no single pass stalls the guest long.
pub const ANON_TRIM_MAX_KIB: u64 = 512 * 1024;
/// Less inactive anonymous memory than this (KiB) is left alone: not worth a pass.
pub const ANON_TRIM_MIN_KIB: u64 = 16 * 1024;
/// How often the program's own view of guest RAM is mapped afresh, so pages the emulator touched
/// stop counting twice in Activity Monitor (see `host_ram`). Each refresh costs a few ms; the
/// emulator's next touch of a page costs a soft page fault.
pub const HOST_RAM_REFRESH_PERIOD: Duration = Duration::from_secs(5);
/// Guest RAM is remapped this much at a time, so a release never waits long for a refresh.
pub const HOST_RAM_REFRESH_CHUNK: usize = 32 << 20;
/// Metal's GPU priority for the window's drawing: 0 is high (Metal's default is 1). The guest's
/// own GPU work gets the same from the bundled KosmicKrisp.
pub const WINDOW_GPU_PRIORITY: usize = 0;
/// What a running guest tells macOS it is doing (NSActivityOptions): user-initiated work that
/// allows idle system sleep (0x00EF_FFFF) and is latency-critical (0xFF_0000_0000), so App Nap
/// never throttles it.
pub const VM_ACTIVITY_OPTIONS: u64 = 0x00EF_FFFF | 0xFF_0000_0000;
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
pub const ORIENTATION_POLL: Duration = Duration::from_millis(500);
pub const ORIENTATION_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for the `adb` forward to accept a connection.
pub const ADB_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A guest command may print nothing for this long and still be running (a screen recording
/// prints nothing until it ends, at most `SCREENRECORD_MAX` later).
pub const ADB_IDLE_TIMEOUT: Duration = Duration::from_secs(240);
/// Most bytes sent to the guest in one `adb` write.
pub const ADB_CHUNK: usize = 64 * 1024;
/// The guest's shared storage, where OBB files go.
pub const GUEST_SHARED_STORAGE: &str = "/sdcard";
/// Largest compressed archive entry inflated while installing.
pub const ZIP_MAX_INFLATED: u64 = 1 << 30;
/// Where the guest writes a screen recording before it is copied out.
pub const GUEST_RECORDING: &str = "/data/local/tmp/proteus-screenrecord.mp4";
/// Longest recording the guest's `screenrecord` makes, and the default length.
pub const SCREENRECORD_MAX: Duration = Duration::from_secs(180);
/// How often the serial input pump checks whether its machine has stopped.
pub const SERIAL_INPUT_POLL: Duration = Duration::from_millis(100);
/// A guest that resets this soon after starting did not boot; this many in a row and the VMM gives up.
pub const QUICK_RESET_WINDOW: Duration = Duration::from_secs(30);
pub const MAX_QUICK_RESETS: u32 = 3;
/// Read size of the console input reader.
pub const SERIAL_INPUT_CHUNK: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskConfig {
    pub path: PathBuf,
    pub read_only: bool,
}

/// A directory of AOSP virtual-device images to boot, and where this instance keeps its own data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AndroidOptions {
    /// Holds `init_boot.img`, `vendor_boot.img`, `super.img` and the `vbmeta` images. Never written to.
    pub images: PathBuf,
    /// Holds the writable partitions; created on first use.
    pub data: PathBuf,
    /// Size of the data partition when it is first created.
    pub userdata_bytes: u64,
    /// Give this instance its own writable copy of the system partitions (`super`), so Android
    /// can be changed there (Google apps, say). Off: the image's own copy, read-only.
    pub writable_system: bool,
}

/// The guest's network. Everything here has a default; the command line only overrides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetOptions {
    /// Lists of domains to sinkhole: trackers, ad networks, public DNS-over-HTTPS resolvers.
    pub blocklists: Vec<PathBuf>,
    /// Lists of domains that stay reachable even if a blocklist names them.
    pub allowlists: Vec<PathBuf>,
    /// Loopback port `adb` connects to, or `None` for no forward.
    pub adb_port: Option<u16>,
    /// Let QUIC through. Off by default, so that clients fall back to TLS, where the server name shows.
    pub allow_quic: bool,
    /// Make the gateway address reach the host's own loopback interface.
    pub host_loopback: bool,
}

impl Default for NetOptions {
    fn default() -> Self {
        Self {
            blocklists: Vec::new(),
            allowlists: Vec::new(),
            adb_port: Some(net::config::ADB_PORT),
            allow_quic: false,
            host_loopback: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    pub cmdline: String,
    pub ram_bytes: u64,
    pub vcpus: u32,
    /// In virtio slot order.
    pub disks: Vec<DiskConfig>,
    pub serial: bool,
    /// `None` leaves the guest without a network device.
    pub net: Option<NetOptions>,
    /// Width and height of the guest's display, or `None` for no display device.
    pub display: Option<(u32, u32)>,
    /// Frames per second the guest's display claims (its EDID), which Android draws at.
    pub refresh_hz: u32,
    /// The name of the shape the window keeps when resized (one of `SCREEN_SHAPES`).
    pub screen_shape: &'static str,
    /// Keep this PNG file up to date with what the guest's display shows.
    pub frame_dump: Option<PathBuf>,
    /// Show the guest's display in a window.
    pub window: bool,
    /// Give the guest a sound card.
    pub sound: bool,
    /// Play nothing on the host; the guest's sound card stays.
    pub mute: bool,
    /// Control sockets of other instances that get this one's input too.
    pub sync: Vec<PathBuf>,
    /// Draw with the host's GPU through gfxstream, if this build and the host's Vulkan driver can.
    pub gpu: bool,
    /// Whether the guest gets a gamepad.
    pub gamepad: GamepadMode,
    /// Keys and mouse movements that stand for touches, read from this file.
    pub keymap: Option<PathBuf>,
    /// A folder of key maps named after the apps they are for; the one for the app in front is used.
    pub keymap_dir: Option<PathBuf>,
    /// Listen on this local socket for control commands.
    pub ctl: Option<PathBuf>,
    /// Write Android's system log to this file.
    pub logcat: Option<PathBuf>,
    /// Boot Android from an image directory instead of a plain ramdisk.
    pub android: Option<AndroidOptions>,
    /// The instance this is, when it was started as one; never set from the command line.
    pub instance: Option<String>,
    pub bootconfig: Vec<(String, String)>,
    /// Console text whose first appearance is timed, if boot timing was asked for.
    pub boot_marker: Option<String>,
    pub log_level: LevelFilter,
}

impl Config {
    pub fn new(kernel: PathBuf) -> Self {
        Self {
            kernel,
            initrd: None,
            cmdline: String::new(),
            ram_bytes: DEFAULT_RAM_MIB * MIB,
            vcpus: DEFAULT_VCPUS,
            disks: Vec::new(),
            serial: false,
            net: Some(NetOptions::default()),
            display: Some(DEFAULT_DISPLAY),
            refresh_hz: DEFAULT_REFRESH_HZ,
            screen_shape: SCREEN_SHAPE_FREE,
            frame_dump: None,
            window: true,
            sound: true,
            mute: false,
            sync: Vec::new(),
            gpu: false,
            gamepad: GamepadMode::Auto,
            keymap: None,
            keymap_dir: None,
            ctl: None,
            logcat: None,
            android: None,
            instance: None,
            bootconfig: Vec::new(),
            boot_marker: None,
            log_level: DEFAULT_LOG_LEVEL,
        }
    }
}

/// When the guest gets a gamepad. virtio devices are fixed at boot, so a pad plugged in later needs
/// a restart, or `Always`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadMode {
    /// If the window is shown and a host pad is plugged in at start.
    Auto,
    Always,
    Never,
}

/// What `proteus instance ...` asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum InstanceCommand {
    Create { name: String, args: Vec<String> },
    Start { name: String, extra: Vec<OsString> },
    List,
    Remove { name: String, confirmed: bool },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// Start and stop the GPU renderer once, to see whether it works here.
    GpuCheck,
    /// `proteus update`: what the launcher's Update button does, from a terminal.
    Update,
    Instance(InstanceCommand),
    Run(Box<Config>),
    /// Send `command` to the instance listening on `socket`.
    Ctl {
        socket: PathBuf,
        command: String,
    },
    Help,
    /// No arguments: open the launcher window.
    Launcher,
}

#[derive(Debug)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&copy::bad_argument(&self.0))
    }
}

impl std::error::Error for ConfigError {}

impl From<lexopt::Error> for ConfigError {
    fn from(err: lexopt::Error) -> Self {
        Self(err.to_string())
    }
}

fn positive<T: std::str::FromStr + PartialOrd + Default>(
    parser: &mut lexopt::Parser,
    option: &str,
) -> Result<T, ConfigError> {
    let text = parser.value()?.string()?;
    let value: T = text
        .parse()
        .map_err(|_| ConfigError(copy::bad_number(option)))?;
    if value > T::default() {
        Ok(value)
    } else {
        Err(ConfigError(copy::bad_number(option)))
    }
}

fn instance_command(args: Vec<OsString>) -> Result<InstanceCommand, ConfigError> {
    let text = |arg: &OsString| arg.to_string_lossy().into_owned();
    let usage = || ConfigError(copy::INSTANCE_USAGE.into());
    let words: Vec<String> = args.iter().take(2).map(text).collect();
    let rest = args.get(2..).unwrap_or_default();
    match words.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["create", name] if !rest.is_empty() => Ok(InstanceCommand::Create {
            name: name.to_owned(),
            args: rest.iter().map(text).collect(),
        }),
        [INSTANCE_START, name] => Ok(InstanceCommand::Start {
            name: name.to_owned(),
            extra: rest.to_vec(),
        }),
        ["list"] if rest.is_empty() => Ok(InstanceCommand::List),
        ["rm", name] => match rest {
            [] => Ok(InstanceCommand::Remove {
                name: name.to_owned(),
                confirmed: false,
            }),
            [yes] if yes == "--yes" => Ok(InstanceCommand::Remove {
                name: name.to_owned(),
                confirmed: true,
            }),
            _ => Err(usage()),
        },
        _ => Err(usage()),
    }
}

/// Parses arguments as they follow the program name.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Parsed, ConfigError> {
    let mut args = args.into_iter().peekable();
    if args.peek().is_none() {
        return Ok(Parsed::Launcher);
    }
    if args.peek().is_some_and(|first| first == CTL_SUBCOMMAND) {
        let socket = args.nth(1);
        let words: Vec<String> = args
            .map(|word| word.to_string_lossy().into_owned())
            .collect();
        return match socket {
            Some(socket) if !words.is_empty() => Ok(Parsed::Ctl {
                socket: socket.into(),
                command: words.join(" "),
            }),
            _ => Err(ConfigError(copy::CTL_NEEDS_ARGUMENTS.into())),
        };
    }
    if args.peek().is_some_and(|first| first == GPU_CHECK_ARG) {
        return Ok(Parsed::GpuCheck);
    }
    if args.peek().is_some_and(|first| first == UPDATE_SUBCOMMAND) {
        return Ok(Parsed::Update);
    }
    if args
        .peek()
        .is_some_and(|first| first == INSTANCE_SUBCOMMAND)
    {
        return instance_command(args.skip(1).collect()).map(Parsed::Instance);
    }
    let mut parser = lexopt::Parser::from_args(args);
    let mut kernel = None;
    let mut config = Config::new(PathBuf::new());
    let mut boot_timing = false;
    let mut boot_marker = None;
    let mut net = NetOptions::default();
    let mut no_net = false;
    let mut android_images: Option<PathBuf> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut userdata_gib = DEFAULT_USERDATA_GIB;
    let mut writable_system = false;

    while let Some(arg) = parser.next()? {
        match arg {
            Long("help") => return Ok(Parsed::Help),
            Long("kernel") => kernel = Some(PathBuf::from(parser.value()?)),
            Long("initrd") => config.initrd = Some(PathBuf::from(parser.value()?)),
            Long("cmdline") => config.cmdline = parser.value()?.string()?,
            Long("ram-mib") => {
                let mib: u64 = positive(&mut parser, "ram-mib")?;
                config.ram_bytes = mib
                    .checked_mul(MIB)
                    .ok_or_else(|| ConfigError(copy::bad_number("ram-mib")))?;
            }
            Long("vcpus") => config.vcpus = positive(&mut parser, "vcpus")?,
            Long("disk") => config.disks.push(DiskConfig {
                path: parser.value()?.into(),
                read_only: false,
            }),
            Long("ro-disk") => config.disks.push(DiskConfig {
                path: parser.value()?.into(),
                read_only: true,
            }),
            Long("bootconfig") => {
                let pair = parser.value()?.string()?;
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| ConfigError(copy::BAD_BOOTCONFIG.into()))?;
                config.bootconfig.push((key.to_owned(), value.to_owned()));
            }
            Long("serial") => config.serial = true,
            Long("android") => android_images = Some(parser.value()?.into()),
            Long("data-dir") => data_dir = Some(parser.value()?.into()),
            Long("writable-system") => writable_system = true,
            Long("userdata-gib") => userdata_gib = positive(&mut parser, "userdata-gib")?,
            Long("no-net") => no_net = true,
            Long("no-display") => config.display = None,
            Long("frame-dump") => config.frame_dump = Some(parser.value()?.into()),
            Long("ctl") => config.ctl = Some(parser.value()?.into()),
            Long("headless") => config.window = false,
            Long("keymap") => config.keymap = Some(parser.value()?.into()),
            Long("keymap-dir") => config.keymap_dir = Some(parser.value()?.into()),
            Long("no-sound") => config.sound = false,
            Long("mute") => config.mute = true,
            Long("gamepad") => config.gamepad = GamepadMode::Always,
            Long("gpu") => config.gpu = true,
            Long("sync") => config.sync.push(parser.value()?.into()),
            Long("no-gamepad") => config.gamepad = GamepadMode::Never,
            Long("logcat") => config.logcat = Some(parser.value()?.into()),
            Long("display") => {
                let text = parser.value()?.string()?;
                let size = text
                    .split_once('x')
                    .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)));
                config.display = Some(
                    size.filter(|(width, height)| *width > 0 && *height > 0)
                        .ok_or_else(|| ConfigError(copy::BAD_DISPLAY.into()))?,
                );
            }
            Long("refresh-hz") => {
                let text = parser.value()?.string()?;
                config.refresh_hz = if text == NATIVE_REFRESH_WORD {
                    NATIVE_REFRESH
                } else {
                    text.parse()
                        .ok()
                        .filter(|hz| *hz > 0)
                        .ok_or_else(|| ConfigError(copy::bad_number("refresh-hz")))?
                };
            }
            Long("screen-shape") => {
                let name = parser.value()?.string()?;
                config.screen_shape = SCREEN_SHAPES
                    .iter()
                    .map(|(label, _)| *label)
                    .find(|label| *label == name)
                    .ok_or_else(|| ConfigError(copy::bad_screen_shape(&name)))?;
            }
            Long("blocklist") => net.blocklists.push(parser.value()?.into()),
            Long("allowlist") => net.allowlists.push(parser.value()?.into()),
            Long("adb-port") => {
                let text = parser.value()?.string()?;
                let port: u16 = text
                    .parse()
                    .map_err(|_| ConfigError(copy::BAD_ADB_PORT.into()))?;
                net.adb_port = (port != 0).then_some(port);
            }
            Long("allow-quic") => net.allow_quic = true,
            Long("host-loopback") => net.host_loopback = true,
            Long("boot-timing") => boot_timing = true,
            Long("boot-marker") => boot_marker = Some(parser.value()?.string()?),
            Long("log-level") => {
                let level = parser.value()?.string()?;
                config.log_level = level
                    .parse()
                    .map_err(|_| ConfigError(copy::BAD_LOG_LEVEL.into()))?;
            }
            other => {
                return Err(ConfigError(copy::unexpected_argument(
                    &other.unexpected().to_string(),
                )));
            }
        }
    }

    if let Some((width, height)) = config.display {
        let most = devices::virtio::edid::max_refresh(width, height);
        if config.refresh_hz == NATIVE_REFRESH {
            let native = native_refresh(host_refresh_hz().unwrap_or(FALLBACK_REFRESH_HZ));
            config.refresh_hz = native.min(most);
        }
        if config.refresh_hz > most {
            return Err(ConfigError(copy::refresh_too_fast(most)));
        }
    }
    // Android needs no kernel named: the one a release carries beside the program boots it.
    let bundled = android_images.as_ref().and_then(|_| bundled_kernel());
    config.kernel = kernel
        .or(bundled)
        .ok_or_else(|| ConfigError(copy::MISSING_KERNEL.into()))?;
    if config.keymap.is_some() && config.keymap_dir.is_some() {
        return Err(ConfigError(copy::KEYMAP_TWO_SOURCES.into()));
    }
    config.net = (!no_net).then_some(net);
    config.android = match android_images {
        Some(images) => {
            if config.initrd.is_some() {
                return Err(ConfigError(copy::ANDROID_WITH_INITRD.into()));
            }
            let userdata_bytes = userdata_bytes(userdata_gib)
                .ok_or_else(|| ConfigError(copy::bad_number("userdata-gib")))?;
            let data = data_dir.unwrap_or_else(|| {
                renamed(images.join(DEFAULT_DATA_DIR), images.join(OLD_DATA_DIR))
            });
            Some(AndroidOptions {
                images,
                data,
                userdata_bytes,
                writable_system,
            })
        }
        None if data_dir.is_some() => return Err(ConfigError(copy::DATA_DIR_NEEDS_ANDROID.into())),
        None if config.logcat.is_some() => {
            return Err(ConfigError(copy::LOGCAT_NEEDS_ANDROID.into()));
        }
        None => None,
    };
    if boot_timing || boot_marker.is_some() {
        if !config.serial {
            return Err(ConfigError(copy::BOOT_MARKER_NEEDS_SERIAL.into()));
        }
        config.boot_marker = Some(boot_marker.unwrap_or_else(|| DEFAULT_BOOT_MARKER.to_owned()));
    }
    Ok(Parsed::Run(Box::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_shapes_are_named_long_side_to_short() {
        assert_eq!(screen_shape("free"), Some(None));
        assert_eq!(screen_shape("18:9"), Some(Some(2.0)));
        assert_eq!(screen_shape("19.5:9"), Some(Some(19.5 / 9.0)));
        assert_eq!(screen_shape("21:9"), None);
    }

    #[test]
    fn renamed_moves_the_old_folder_once() {
        let dir = std::env::temp_dir().join(format!("proteus-renamed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("old")).unwrap();
        std::fs::write(dir.join("old").join("kept"), "x").unwrap();
        let new = renamed(dir.join("new"), dir.join("old"));
        assert!(new.join("kept").exists() && !dir.join("old").exists());
        std::fs::create_dir_all(dir.join("old")).unwrap();
        renamed(dir.join("new"), dir.join("old"));
        assert!(
            dir.join("old").exists(),
            "an existing new folder is never replaced"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn parse_str(args: &[&str]) -> Result<Parsed, ConfigError> {
        parse(args.iter().map(OsString::from))
    }

    fn run(args: &[&str]) -> Config {
        match parse_str(args).unwrap() {
            Parsed::Run(config) => *config,
            other => panic!("expected a run configuration, got {other:?}"),
        }
    }

    #[test]
    fn kernel_alone_gets_every_default() {
        assert_eq!(
            run(&["--kernel", "Image"]),
            Config {
                // The display runs as fast as this computer's screen.
                refresh_hz: native_refresh(host_refresh_hz().unwrap_or(FALLBACK_REFRESH_HZ)),
                ..Config::new(PathBuf::from("Image"))
            }
        );
        assert_eq!(
            run(&["--kernel", "Image", "--refresh-hz", "native"]).refresh_hz,
            native_refresh(host_refresh_hz().unwrap_or(FALLBACK_REFRESH_HZ))
        );
        assert_eq!(
            run(&["--kernel", "Image", "--refresh-hz", "90"]).refresh_hz,
            90
        );
        assert!(parse_str(&["--kernel", "Image", "--refresh-hz", "0"]).is_err());
        assert_eq!(
            [60, 75, 90, 120, 144, 165, 240].map(native_refresh),
            [60, 60, 60, 120, 120, 120, 240]
        );
    }

    #[test]
    fn every_option_is_applied_and_disks_keep_their_order() {
        let config = run(&[
            "--kernel",
            "k",
            "--initrd",
            "r",
            "--cmdline",
            "quiet",
            "--ram-mib",
            "512",
            "--vcpus",
            "2",
            "--ro-disk",
            "system.img",
            "--disk",
            "userdata.img",
            "--bootconfig",
            "androidboot.hardware=emu",
            "--serial",
            "--boot-timing",
            "--log-level",
            "debug",
            "--blocklist",
            "ads.txt",
            "--blocklist",
            "trackers.txt",
            "--allowlist",
            "required.txt",
            "--adb-port",
            "5600",
            "--allow-quic",
            "--host-loopback",
        ]);
        assert_eq!(
            config.net,
            Some(NetOptions {
                blocklists: vec!["ads.txt".into(), "trackers.txt".into()],
                allowlists: vec!["required.txt".into()],
                adb_port: Some(5600),
                allow_quic: true,
                host_loopback: true,
            })
        );
        assert_eq!(config.initrd, Some(PathBuf::from("r")));
        assert_eq!(
            (config.cmdline.as_str(), config.ram_bytes, config.vcpus),
            ("quiet", 512 * MIB, 2)
        );
        assert_eq!(
            config.disks,
            [
                DiskConfig {
                    path: "system.img".into(),
                    read_only: true
                },
                DiskConfig {
                    path: "userdata.img".into(),
                    read_only: false
                },
            ]
        );
        assert_eq!(
            config.bootconfig,
            [("androidboot.hardware".to_owned(), "emu".to_owned())]
        );
        assert!(config.serial);
        assert_eq!(config.boot_marker.as_deref(), Some(DEFAULT_BOOT_MARKER));
        assert_eq!(config.log_level, LevelFilter::Debug);
    }

    #[test]
    fn custom_boot_marker_implies_timing() {
        let config = run(&["--kernel", "k", "--serial", "--boot-marker", "Welcome"]);
        assert_eq!(config.boot_marker.as_deref(), Some("Welcome"));
    }

    #[test]
    fn bootconfig_value_may_contain_an_equals_sign() {
        let config = run(&["--kernel", "k", "--bootconfig", "a=b=c"]);
        assert_eq!(config.bootconfig, [("a".to_owned(), "b=c".to_owned())]);
    }

    #[test]
    fn network_can_be_removed_and_the_adb_forward_switched_off() {
        assert_eq!(run(&["--kernel", "k", "--no-net"]).net, None);
        let net = run(&["--kernel", "k", "--adb-port", "0"]).net.unwrap();
        assert_eq!(net.adb_port, None);
        assert!(!net.allow_quic && !net.host_loopback);
    }

    #[test]
    fn android_directory_gets_a_data_directory_and_a_data_partition_size() {
        let android = run(&["--kernel", "k", "--android", "images"])
            .android
            .unwrap();
        assert_eq!(android.images, PathBuf::from("images"));
        assert_eq!(android.data, PathBuf::from("images").join(DEFAULT_DATA_DIR));
        assert_eq!(
            Some(android.userdata_bytes),
            userdata_bytes(DEFAULT_USERDATA_GIB)
        );
        // 64 GB asked for: data plus system stays under 64e9, so Android shows 64 GB, not 128.
        let data = userdata_bytes(64).unwrap();
        assert!(data + 918_964 * 1024 <= 64 * DECIMAL_GB);
        assert!(data > 60 * DECIMAL_GB);
        assert_eq!(storage_gb(data), 64);
        // A 64 GiB partition made by an older release is not what "64 GB" makes now.
        assert_ne!(storage_gb(64 << 30), 64);
        let custom = run(&[
            "--kernel",
            "k",
            "--android",
            "images",
            "--data-dir",
            "mine",
            "--userdata-gib",
            "32",
        ]);
        let custom = custom.android.unwrap();
        assert_eq!(
            (custom.data, custom.userdata_bytes),
            (PathBuf::from("mine"), userdata_bytes(32).unwrap())
        );
    }

    #[test]
    fn display_size_is_parsed_and_can_be_switched_off() {
        assert_eq!(run(&["--kernel", "k"]).display, Some(DEFAULT_DISPLAY));
        assert_eq!(
            run(&["--kernel", "k", "--display", "1920x1080"]).display,
            Some((1920, 1080))
        );
        assert_eq!(run(&["--kernel", "k", "--no-display"]).display, None);
    }

    #[test]
    fn ctl_takes_a_socket_and_joins_the_rest_into_one_command() {
        assert_eq!(
            parse_str(&["ctl", "/tmp/proteus.sock", "tap", "10", "20"]).unwrap(),
            Parsed::Ctl {
                socket: PathBuf::from("/tmp/proteus.sock"),
                command: "tap 10 20".to_owned(),
            }
        );
        assert!(parse_str(&["ctl"]).is_err());
        assert!(parse_str(&["ctl", "/tmp/proteus.sock"]).is_err());
        assert_eq!(
            run(&["--kernel", "k", "--ctl", "s"]).ctl,
            Some(PathBuf::from("s"))
        );
    }

    #[test]
    fn instance_subcommands_and_sync_targets_are_read() {
        let instance = |args: &[&str]| match parse_str(args).unwrap() {
            Parsed::Instance(command) => command,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            instance(&["instance", "create", "a", "--kernel", "k"]),
            InstanceCommand::Create {
                name: "a".to_owned(),
                args: vec!["--kernel".to_owned(), "k".to_owned()],
            }
        );
        assert_eq!(
            instance(&["instance", "start", "a", "--headless"]),
            InstanceCommand::Start {
                name: "a".to_owned(),
                extra: vec!["--headless".into()],
            }
        );
        assert_eq!(instance(&["instance", "list"]), InstanceCommand::List);
        assert_eq!(
            instance(&["instance", "rm", "a", "--yes"]),
            InstanceCommand::Remove {
                name: "a".to_owned(),
                confirmed: true,
            }
        );
        for wrong in [
            &["instance"][..],
            &["instance", "create", "a"],
            &["instance", "rm", "a", "now"],
            &["instance", "list", "x"],
            &["instance", "eat", "a"],
        ] {
            assert!(parse_str(wrong).is_err(), "{wrong:?}");
        }
        let synced = run(&["--kernel", "k", "--sync", "b", "--sync", "/tmp/c.sock"]);
        assert_eq!(
            synced.sync,
            [PathBuf::from("b"), PathBuf::from("/tmp/c.sock")]
        );
    }

    #[test]
    fn no_arguments_open_the_launcher() {
        assert_eq!(parse_str(&[]).unwrap(), Parsed::Launcher);
    }

    #[test]
    fn help_wins() {
        assert_eq!(parse_str(&["--help"]).unwrap(), Parsed::Help);
    }

    #[test]
    fn invalid_command_lines_are_refused() {
        let cases: &[&[&str]] = &[
            &["--initrd", "r"],
            &["--kernel"],
            &["--kernel", "k", "--vcpus", "0"],
            &["--kernel", "k", "--vcpus", "two"],
            &["--kernel", "k", "--ram-mib", "0"],
            &["--kernel", "k", "--ram-mib", "99999999999999999999"],
            &["--kernel", "k", "--ram-mib", "17592186044416"],
            &["--kernel", "k", "--bootconfig", "novalue"],
            &["--kernel", "k", "--log-level", "loud"],
            &["--kernel", "k", "--boot-timing"],
            &["--kernel", "k", "--frobnicate"],
            &["--kernel", "k", "--display", "1920"],
            &["--kernel", "k", "--display", "0x1080"],
            &["--kernel", "k", "--display", "widexhigh"],
            &["--kernel", "k", "--android", "images", "--initrd", "r"],
            &["--kernel", "k", "--data-dir", "mine"],
            &[
                "--kernel",
                "k",
                "--android",
                "images",
                "--userdata-gib",
                "0",
            ],
            &["--kernel", "k", "--adb-port", "70000"],
            &["--kernel", "k", "--adb-port", "adb"],
            &["--kernel", "k", "stray"],
        ];
        for args in cases {
            assert!(parse_str(args).is_err(), "{args:?} should be refused");
        }
    }
}

/// The look of the emulator's windows: colours (RGB), corner radii, gaps and sizes, in points.
pub mod style {
    pub const ACCENT: [u8; 3] = [0x3d, 0x8b, 0xfd];
    pub const DANGER: [u8; 3] = [0xc9, 0x3c, 0x3c];
    pub const GOOD: [u8; 3] = [0x4c, 0xc3, 0x6b];
    pub const PANEL: [u8; 3] = [0x16, 0x18, 0x1d];
    pub const CARD: [u8; 3] = [0x1f, 0x23, 0x2a];
    pub const FIELD: [u8; 3] = [0x11, 0x13, 0x17];
    pub const BUTTON: [u8; 3] = [0x2a, 0x2f, 0x38];
    pub const BUTTON_HOVER: [u8; 3] = [0x34, 0x3a, 0x45];
    pub const TITLE: [u8; 3] = [0xe8, 0xea, 0xee];
    pub const MUTED: [u8; 3] = [0x9a, 0xa1, 0xad];
    pub const STROKE: f32 = 1.0;
    pub const RADIUS: u8 = 6;
    pub const CARD_RADIUS: u8 = 10;
    pub const CARD_PAD: i8 = 12;
    pub const GAP: f32 = 8.0;
    pub const SECTION_GAP: f32 = 6.0;
    pub const BUTTON_PAD_X: f32 = 10.0;
    pub const BUTTON_PAD_Y: f32 = 5.0;
    pub const ROW_HEIGHT: f32 = 26.0;
    pub const SLIDER_WIDTH: f32 = 140.0;
    pub const LABEL_WIDTH: f32 = 96.0;
    pub const CHOOSE_WIDTH: f32 = 100.0;
    pub const HEADING_SIZE: f32 = 19.0;
    pub const BODY_SIZE: f32 = 13.5;
    pub const SMALL_SIZE: f32 = 11.5;
}

/// A window side this small or smaller means it is minimized or collapsed; such a size is not
/// worth remembering.
pub const MIN_REMEMBERED_SIDE: f64 = 50.0;

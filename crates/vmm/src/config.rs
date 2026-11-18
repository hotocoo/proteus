//! Run configuration: defaults and the command line that overrides them.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use lexopt::prelude::*;
use log::LevelFilter;

use crate::copy;

const MIB: u64 = 1 << 20;

pub const DEFAULT_RAM_MIB: u64 = 4096;
pub const DEFAULT_VCPUS: u32 = 4;
/// Size of the guest's display, in pixels.
pub const DEFAULT_DISPLAY: (u32, u32) = (1280, 720);
/// Size the guest's data partition is created with.
pub const DEFAULT_USERDATA_GIB: u64 = 8;
/// Where an Android instance keeps what the guest writes, inside the images directory, unless told otherwise.
pub const DEFAULT_DATA_DIR: &str = "emu-data";
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
/// Speed a GPX route is followed at when none is given: a walk.
pub const ROUTE_DEFAULT_KMH: f64 = 5.0;
/// Fastest a walk or route may go; anything faster is a typing mistake (this is an airliner).
pub const MAX_KMH: f64 = 1000.0;
pub const INSTANCE_SUBCOMMAND: &str = "instance";
/// Environment variable naming the emulator's home folder, where instances live.
pub const EMU_HOME_VARIABLE: &str = "EMU_HOME";
pub const INSTANCES_DIR: &str = "instances";
pub const INSTANCE_NAME_MAX: usize = 32;
/// Longest Unix socket path the system takes (macOS: 104 bytes with the terminating zero).
pub const SOCKET_PATH_MAX: usize = 103;

/// The emulator's home folder when `EMU_HOME` is not set: the platform's place for app data.
pub fn home_default() -> Option<PathBuf> {
    const MACOS_APP_DATA: &str = "Library/Application Support";
    const XDG_DATA_HOME: &str = "XDG_DATA_HOME";
    const XDG_DATA_DEFAULT: &str = ".local/share";
    const APP_FOLDER: &str = "emu";
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map(|home| home.join(MACOS_APP_DATA).join(APP_FOLDER));
    }
    let data = std::env::var_os(XDG_DATA_HOME).map(PathBuf::from);
    data.or_else(|| home.map(|home| home.join(XDG_DATA_DEFAULT)))
        .map(|data| data.join(APP_FOLDER))
}
/// How often the app in front is checked, for key maps that follow it.
pub const FOREGROUND_POLL: Duration = Duration::from_secs(1);
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
pub const GUEST_RECORDING: &str = "/data/local/tmp/emu-screenrecord.mp4";
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
            frame_dump: None,
            window: true,
            sound: true,
            mute: false,
            sync: Vec::new(),
            gamepad: GamepadMode::Auto,
            keymap: None,
            keymap_dir: None,
            ctl: None,
            logcat: None,
            android: None,
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

/// What `emu instance ...` asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum InstanceCommand {
    Create { name: String, args: Vec<String> },
    Start { name: String, extra: Vec<OsString> },
    List,
    Remove { name: String, confirmed: bool },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Instance(InstanceCommand),
    Run(Box<Config>),
    /// Send `command` to the instance listening on `socket`.
    Ctl {
        socket: PathBuf,
        command: String,
    },
    Help,
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
        ["start", name] => Ok(InstanceCommand::Start {
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

    config.kernel = kernel.ok_or_else(|| ConfigError(copy::MISSING_KERNEL.into()))?;
    if config.keymap.is_some() && config.keymap_dir.is_some() {
        return Err(ConfigError(copy::KEYMAP_TWO_SOURCES.into()));
    }
    config.net = (!no_net).then_some(net);
    config.android = match android_images {
        Some(images) => {
            if config.initrd.is_some() {
                return Err(ConfigError(copy::ANDROID_WITH_INITRD.into()));
            }
            let userdata_bytes = userdata_gib
                .checked_mul(MIB << 10)
                .ok_or_else(|| ConfigError(copy::bad_number("userdata-gib")))?;
            let data = data_dir.unwrap_or_else(|| images.join(DEFAULT_DATA_DIR));
            Some(AndroidOptions {
                images,
                data,
                userdata_bytes,
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
            Config::new(PathBuf::from("Image"))
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
        assert_eq!(android.userdata_bytes, DEFAULT_USERDATA_GIB << 30);
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
            (PathBuf::from("mine"), 32 << 30)
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
            parse_str(&["ctl", "/tmp/emu.sock", "tap", "10", "20"]).unwrap(),
            Parsed::Ctl {
                socket: PathBuf::from("/tmp/emu.sock"),
                command: "tap 10 20".to_owned(),
            }
        );
        assert!(parse_str(&["ctl"]).is_err());
        assert!(parse_str(&["ctl", "/tmp/emu.sock"]).is_err());
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
    fn help_wins() {
        assert_eq!(parse_str(&["--help"]).unwrap(), Parsed::Help);
    }

    #[test]
    fn invalid_command_lines_are_refused() {
        let cases: &[&[&str]] = &[
            &[],
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

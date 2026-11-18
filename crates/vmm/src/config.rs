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
/// Read size of the serial input pump.
pub const SERIAL_INPUT_CHUNK: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskConfig {
    pub path: PathBuf,
    pub read_only: bool,
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
            bootconfig: Vec::new(),
            boot_marker: None,
            log_level: DEFAULT_LOG_LEVEL,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Config),
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

/// Parses arguments as they follow the program name.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Parsed, ConfigError> {
    let mut parser = lexopt::Parser::from_args(args);
    let mut kernel = None;
    let mut config = Config::new(PathBuf::new());
    let mut boot_timing = false;
    let mut boot_marker = None;

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
    if boot_timing || boot_marker.is_some() {
        if !config.serial {
            return Err(ConfigError(copy::BOOT_MARKER_NEEDS_SERIAL.into()));
        }
        config.boot_marker = Some(boot_marker.unwrap_or_else(|| DEFAULT_BOOT_MARKER.to_owned()));
    }
    Ok(Parsed::Run(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Result<Parsed, ConfigError> {
        parse(args.iter().map(OsString::from))
    }

    fn run(args: &[&str]) -> Config {
        match parse_str(args).unwrap() {
            Parsed::Run(config) => config,
            Parsed::Help => panic!("expected a run configuration"),
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
        ]);
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
            &["--kernel", "k", "stray"],
        ];
        for args in cases {
            assert!(parse_str(args).is_err(), "{args:?} should be refused");
        }
    }
}

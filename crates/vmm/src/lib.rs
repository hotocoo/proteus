//! The `emu` virtual machine monitor: configuration, board wiring and the vCPU run loop.

use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

pub mod config;
pub mod console;
mod copy;
mod logger;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod machine;

/// Exit status for a command line the program could not understand.
const EXIT_USAGE: u8 = 2;

#[derive(Debug)]
pub enum VmmError {
    /// The requested configuration cannot be built on this host or board.
    Config(&'static str),
    File {
        path: PathBuf,
        source: std::io::Error,
    },
    Hypervisor(hv::HvError),
    Board(arch::ArchError),
    Device(String),
    Thread(std::io::Error),
}

impl fmt::Display for VmmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(why) => f.write_str(why),
            Self::File { path, source } => f.write_str(&copy::file(path, source)),
            Self::Hypervisor(err) => f.write_str(&copy::hypervisor(err)),
            Self::Board(err) => f.write_str(&copy::board(err)),
            Self::Device(err) => f.write_str(&copy::device(err)),
            Self::Thread(err) => f.write_str(&copy::thread(err)),
        }
    }
}

impl std::error::Error for VmmError {}

impl From<hv::HvError> for VmmError {
    fn from(err: hv::HvError) -> Self {
        Self::Hypervisor(err)
    }
}

impl From<arch::ArchError> for VmmError {
    fn from(err: arch::ArchError) -> Self {
        Self::Board(err)
    }
}

impl From<devices::bus::BusError> for VmmError {
    fn from(err: devices::bus::BusError) -> Self {
        Self::Device(err.to_string())
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run(config: &config::Config, started: Instant) -> Result<(), VmmError> {
    let outcome = machine::run(config, console::Console::stdio(), started)?;
    if config.boot_marker.is_some() {
        match outcome.boot_marker_after {
            Some(elapsed) => eprintln!("{}", copy::boot_time(elapsed)),
            None => eprintln!("{}", copy::BOOT_MARKER_NOT_SEEN),
        }
    }
    Ok(())
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn run(_config: &config::Config, _started: Instant) -> Result<(), VmmError> {
    Err(VmmError::Config(copy::HOST_UNSUPPORTED))
}

/// Entry point of the `emu` binary.
pub fn cli_main() -> ExitCode {
    let started = Instant::now();
    let config = match config::parse(std::env::args_os().skip(1)) {
        Ok(config::Parsed::Help) => {
            println!("{}", copy::HELP);
            return ExitCode::SUCCESS;
        }
        Ok(config::Parsed::Run(config)) => *config,
        Err(err) => {
            eprintln!("{}\n{}", copy::fatal(&err), copy::USAGE_HINT);
            return ExitCode::from(EXIT_USAGE);
        }
    };
    logger::init(config.log_level);
    match run(&config, started) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}", copy::fatal(&err));
            ExitCode::FAILURE
        }
    }
}

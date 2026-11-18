//! The `emu` virtual machine monitor: configuration, board wiring and the vCPU run loop.

use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

pub mod adb;
#[cfg(target_arch = "aarch64")]
mod android;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod audio;
pub mod config;
pub mod console;
mod copy;
pub mod ctl;
pub mod frames;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod gamepad;
pub mod input;
pub mod install;
pub mod keymap;
pub mod location;
mod logger;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod machine;
pub mod sensors;
pub mod session;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod window;

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

/// Runs the machine, with a window if the configuration asks for one, until it stops for good.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run(config: &config::Config, started: Instant) -> Result<(), VmmError> {
    use window::{Notice, WindowLoop};

    let session = session::Session::new(config)?;
    let Some(display) = config.display.filter(|_| config.window) else {
        if config.keymap.is_some() {
            return Err(VmmError::Config(copy::KEYMAP_NEEDS_WINDOW));
        }
        return boot_loop(config, &session, started);
    };
    let mapper = match &config.keymap {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|source| VmmError::File {
                path: path.clone(),
                source,
            })?;
            let parsed = keymap::Mapper::parse(&text, display);
            Some(parsed.map_err(|why| VmmError::Device(copy::keymap_refused(path, &why)))?)
        }
        None => None,
    };
    let window = WindowLoop::new()?;
    let notify = window.notifier();
    if let Some(screen) = &session.controller.screen {
        let notify = notify.clone();
        screen.on_frame(move || notify(Notice::Frame));
    }
    // The window system wants this thread, the program's first, so the machine gets another.
    std::thread::scope(|scope| {
        let machine = std::thread::Builder::new()
            .name(copy::MACHINE_THREAD.into())
            .spawn_scoped(scope, || {
                let ran = boot_loop(config, &session, started);
                notify(Notice::Stopped);
                ran
            })
            .map_err(VmmError::Thread)?;
        let shown = window.run(std::sync::Arc::clone(&session.controller), display, mapper);
        // However the window ended, the machine does not outlive it.
        session.controller.power.off();
        let ran = machine.join();
        ran.unwrap_or(Err(VmmError::Config(copy::VCPU_PANICKED)))
            .and(shown)
    })
}

/// Boots the machine, and boots it again each time the guest asks to restart.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn boot_loop(
    config: &config::Config,
    session: &session::Session,
    started: Instant,
) -> Result<(), VmmError> {
    use config::{MAX_QUICK_RESETS, QUICK_RESET_WINDOW};
    use machine::Stop;

    let input = config
        .serial
        .then(|| console::ConsoleInput::from_reader(std::io::stdin()))
        .transpose();
    let input = input.map_err(VmmError::Thread)?;
    let mut quick_resets = 0;
    let mut boot_started = started;
    loop {
        let outcome = machine::run_in(
            session,
            config,
            console::Console::stdout(input.clone()),
            boot_started,
            std::sync::Arc::new(net::HostResolver),
        )?;
        if boot_started == started && config.boot_marker.is_some() {
            match outcome.boot_marker_after {
                Some(elapsed) => eprintln!("{}", copy::boot_time(elapsed)),
                None => eprintln!("{}", copy::BOOT_MARKER_NOT_SEEN),
            }
        }
        if outcome.stop == Stop::PowerOff {
            return Ok(());
        }
        // A reset is a reboot request, unless the guest never got anywhere: then rebooting only loops.
        quick_resets = if boot_started.elapsed() < QUICK_RESET_WINDOW {
            quick_resets + 1
        } else {
            0
        };
        if quick_resets >= MAX_QUICK_RESETS {
            return Err(VmmError::Config(copy::RESET_LOOP));
        }
        eprintln!("{}", copy::REBOOTING);
        boot_started = Instant::now();
    }
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
        Ok(config::Parsed::Ctl { socket, command }) => {
            return match ctl::request(&socket, &command) {
                Ok(reply) if reply == copy::CTL_OK => ExitCode::SUCCESS,
                Ok(reply) => {
                    eprintln!("{}", copy::fatal(&reply));
                    ExitCode::FAILURE
                }
                Err(err) => {
                    eprintln!("{}", copy::fatal(&copy::file(&socket, &err)));
                    ExitCode::FAILURE
                }
            };
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

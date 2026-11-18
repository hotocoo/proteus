//! The `emu` virtual machine monitor: configuration, board wiring and the vCPU run loop.

use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

pub mod adb;
mod android;
#[cfg(host_board)]
pub mod audio;
pub mod config;
pub mod console;
mod copy;
pub mod ctl;
pub mod frames;
#[cfg(host_board)]
pub mod gamepad;
pub mod input;
pub mod install;
pub mod instance;
pub mod keymap;
pub mod location;
mod logger;
#[cfg(host_board)]
pub mod machine;
pub mod profiles;
pub mod sensors;
pub mod session;
#[cfg(host_board)]
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
#[cfg(host_board)]
fn run(config: &config::Config, started: Instant) -> Result<(), VmmError> {
    use window::{Notice, WindowLoop};

    let session = session::Session::new(config)?;
    let Some(display) = config.display.filter(|_| config.window) else {
        if config.keymap.is_some() || config.keymap_dir.is_some() {
            return Err(VmmError::Config(copy::KEYMAP_NEEDS_WINDOW));
        }
        return boot_loop(config, &session, started);
    };
    let mapper = config
        .keymap
        .as_ref()
        .map(|path| keymap::load(path, display));
    let mapper = mapper.transpose().map_err(VmmError::Device)?;
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
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watching = match &config.keymap_dir {
            Some(_) => {
                let notify = notify.clone();
                let watch = profiles::watch(
                    std::sync::Arc::clone(&session.controller.adb_port),
                    std::sync::Arc::clone(&done),
                    move |package| notify(Notice::Foreground(package)),
                );
                Some(watch.map_err(VmmError::Thread)?)
            }
            None => None,
        };
        let controller = std::sync::Arc::clone(&session.controller);
        let shown = window.run(controller, display, mapper, config.keymap_dir.clone());
        // However the window ended, the machine does not outlive it.
        session.controller.power.off();
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(watching) = watching {
            let _ = watching.join();
        }
        let ran = machine.join();
        ran.unwrap_or(Err(VmmError::Config(copy::VCPU_PANICKED)))
            .and(shown)
    })
}

/// Boots the machine, and boots it again each time the guest asks to restart.
#[cfg(host_board)]
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

#[cfg(not(host_board))]
fn run(_config: &config::Config, _started: Instant) -> Result<(), VmmError> {
    Err(VmmError::Config(copy::HOST_UNSUPPORTED))
}

/// Carries out `emu instance ...`. Starting one gives back its configuration, to run.
fn instance_main(command: config::InstanceCommand) -> Result<Option<config::Config>, String> {
    use config::InstanceCommand;

    let root = instance::root().map_err(|err| err.to_string())?;
    let failed = |err: std::io::Error| err.to_string();
    match command {
        InstanceCommand::Create { name, args } => {
            // Checked now, so that a mistake shows here and not when it is started.
            let mut check: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
            check.extend(["--data-dir".into(), "x".into()]);
            config::parse(check).map_err(|err| err.to_string())?;
            let cwd = std::env::current_dir().map_err(failed)?;
            instance::create(&root, &name, &args, &cwd).map_err(failed)?;
        }
        InstanceCommand::Start { name, extra } => {
            let args = instance::start_args(&root, &name, &extra).map_err(failed)?;
            return match config::parse(args) {
                Ok(config::Parsed::Run(config)) => Ok(Some(*config)),
                Ok(_) => Err(copy::INSTANCE_USAGE.to_owned()),
                Err(err) => Err(err.to_string()),
            };
        }
        InstanceCommand::List => {
            for (name, running) in instance::list(&root).map_err(failed)? {
                let mark = if running {
                    copy::INSTANCE_RUNNING_MARK
                } else {
                    ""
                };
                println!("{name}\t{mark}");
            }
        }
        InstanceCommand::Remove { name, confirmed } => {
            let mut answer = std::io::stdin().lock();
            let removed =
                instance::remove(&root, &name, confirmed, &mut std::io::stderr(), &mut answer);
            if !removed.map_err(failed)? {
                eprintln!("{}", copy::INSTANCE_KEPT);
            }
        }
    }
    Ok(None)
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
            let socket = instance::resolve(&socket);
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
        Ok(config::Parsed::Instance(command)) => match instance_main(command) {
            Ok(None) => return ExitCode::SUCCESS,
            Ok(Some(config)) => config,
            Err(err) => {
                eprintln!("{}", copy::fatal(&err));
                return ExitCode::FAILURE;
            }
        },
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

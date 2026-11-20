//! The `proteus` virtual machine monitor: configuration, board wiring and the vCPU run loop.

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
pub mod fill;
pub mod frames;
#[cfg(host_board)]
pub mod gamepad;
#[cfg(host_board)]
mod gui;
pub mod input;
pub mod install;
pub mod instance;
#[cfg(host_board)]
mod keyedit;
pub mod keymap;
#[cfg(host_board)]
mod launcher;
pub mod location;
mod logger;
#[cfg(host_board)]
pub mod machine;
pub mod modem;
pub mod orientation;
#[cfg(host_board)]
mod painter;
#[cfg(host_board)]
mod picture;
pub mod profiles;
pub mod sensors;
pub mod session;
pub mod settings;
#[cfg(host_board)]
mod settings_ui;
mod setup;
pub mod shared;
#[cfg(host_board)]
mod style;
#[cfg(host_board)]
mod update;
#[cfg(host_board)]
mod usage;
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
/// How the emulator ended, as far as the user asked for something next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Closed,
    /// Start the instance again, with the options it has saved now.
    Restart,
    /// Delete the instance's Android data, then start it again.
    FactoryReset,
}

/// Returns what the user asked for as the machine stopped.
#[cfg(host_board)]
fn run(config: &config::Config, started: Instant) -> Result<Exit, VmmError> {
    use window::{Notice, WindowLoop};

    let session = session::Session::new(config)?;
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let setting_up = match &config.android {
        Some(android) => Some(
            setup::spawn(
                std::sync::Arc::clone(&session.controller.adb_port),
                std::sync::Arc::clone(&session.controller.power),
                std::sync::Arc::clone(&done),
                config.logcat.is_some(),
                !config.window,
                Some(android.data.clone()),
            )
            .map_err(VmmError::Thread)?,
        ),
        None => None,
    };
    let Some(display) = config.display.filter(|_| config.window) else {
        if config.keymap.is_some() || config.keymap_dir.is_some() {
            return Err(VmmError::Config(copy::KEYMAP_NEEDS_WINDOW));
        }
        let watching = watch_orientation(&session, &done, |_| {})?;
        let ran = boot_loop(config, &session, started);
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        for watching in [watching, setting_up].into_iter().flatten() {
            let _ = watching.join();
        }
        return ran.map(|()| Exit::Closed);
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
        let rotated = notify.clone();
        let turning = watch_orientation(&session, &done, move |turns| {
            rotated(Notice::Rotated(turns));
        })?;
        let watching = match &config.keymap_dir {
            Some(dir) => {
                if let Some((_, presets)) = config::bundled(config::BUNDLED_KEYMAPS_DIR) {
                    match profiles::seed(dir, &presets) {
                        Ok(0) => {}
                        Ok(copied) => eprintln!("{}", copy::keymap_presets_added(copied, dir)),
                        Err(why) => eprintln!("{}", copy::keymap_presets_failed(&why)),
                    }
                }
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
        let shown = window.run(
            controller,
            display,
            config.keymap.clone(),
            config.keymap_dir.clone(),
            config.instance.clone(),
            config::screen_shape(config.screen_shape).flatten(),
        );
        // However the window ended, the machine does not outlive it.
        session.controller.power.off();
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        for watching in [watching, turning, setting_up].into_iter().flatten() {
            let _ = watching.join();
        }
        let ran = machine.join();
        ran.unwrap_or(Err(VmmError::Config(copy::VCPU_PANICKED)))
            .and(shown)
    })
}

/// Starts the instance `name` again in place of this program, with the options it has saved now.
#[cfg(host_board)]
fn restart_instance(name: &str) -> VmmError {
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(err) => return VmmError::Thread(err),
    };
    let mut command = std::process::Command::new(program);
    no_console_window(&mut command);
    command.args([config::INSTANCE_SUBCOMMAND, config::INSTANCE_START, name]);
    // Only comes back if the program could not be started.
    #[cfg(unix)]
    let err = std::os::unix::process::CommandExt::exec(&mut command);
    // Windows cannot replace a running program: start the new one and leave.
    #[cfg(windows)]
    let err = match command.spawn() {
        Ok(_) => std::process::exit(0),
        Err(err) => err,
    };
    VmmError::Thread(err)
}

/// Keeps the screen's way up in step with Android's, when there is a screen, until `done` is set.
#[cfg(host_board)]
fn watch_orientation(
    session: &session::Session,
    done: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    changed: impl Fn(u8) + Send + 'static,
) -> Result<Option<std::thread::JoinHandle<()>>, VmmError> {
    let Some(screen) = session.controller.screen.clone() else {
        return Ok(None);
    };
    let watch = orientation::watch(
        std::sync::Arc::clone(&session.controller.adb_port),
        std::sync::Arc::clone(done),
        move |turns| {
            screen.set_turns(turns);
            changed(turns);
        },
    );
    watch.map(Some).map_err(VmmError::Thread)
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
fn run(_config: &config::Config, _started: Instant) -> Result<Exit, VmmError> {
    Err(VmmError::Config(copy::HOST_UNSUPPORTED))
}

/// `proteus --gpu-check`: starts and stops the GPU renderer once. Exits with success if it worked.
fn gpu_check() -> ExitCode {
    #[cfg(feature = "gpu")]
    return match gfx::check() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("{why}");
            ExitCode::FAILURE
        }
    };
    #[cfg(not(feature = "gpu"))]
    ExitCode::FAILURE
}

/// Starts the GPU renderer's check with the Vulkan driver the user chose, or else with each one a
/// release carries beside the program, best first. The driver that passes stays set, so the
/// renderer uses the same one.
fn start_gpu() -> Result<(), String> {
    let chosen = config::VULKAN_DRIVER_VARIABLES
        .iter()
        .any(|name| std::env::var_os(name).is_some());
    let bundled: Vec<(&str, std::path::PathBuf)> = config::BUNDLED_VULKAN_DRIVERS
        .iter()
        .filter_map(|(driver, icd)| config::bundled(icd).map(|(dir, _)| (*driver, dir)))
        .collect();
    if chosen || bundled.is_empty() {
        return gpu_works();
    }
    // The first bundled driver that starts is used; each is tried in its own child process.
    let mut why = String::new();
    for (driver, dir) in bundled {
        // SAFETY: called from `main` before any thread is started.
        unsafe {
            std::env::set_var(config::VULKAN_ICD_VARIABLE, driver);
            std::env::set_var(config::VULKAN_LAUNCHER_DIR_VARIABLE, dir);
        }
        match gpu_works() {
            Ok(()) => return Ok(()),
            Err(failed) => why = failed,
        }
    }
    // SAFETY: as above.
    unsafe {
        std::env::remove_var(config::VULKAN_ICD_VARIABLE);
        std::env::remove_var(config::VULKAN_LAUNCHER_DIR_VARIABLE);
    }
    Err(why)
}

/// Whether the GPU renderer starts on this machine. It is tried in a child process with a time
/// limit, because a renderer with no working Vulkan driver never comes back.
fn gpu_works() -> Result<(), String> {
    if !gfx::AVAILABLE {
        return Err(copy::GPU_NOT_BUILT.to_owned());
    }
    let program = std::env::current_exe().map_err(|err| err.to_string())?;
    let mut child = no_console_window(&mut std::process::Command::new(program))
        .arg(config::GPU_CHECK_ARG)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|err| err.to_string())?;
    let deadline = Instant::now() + config::GPU_CHECK_TIMEOUT;
    loop {
        match child.try_wait().map_err(|err| err.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(status.to_string()),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(copy::GPU_CHECK_HUNG.to_owned());
            }
            None => std::thread::sleep(config::GPU_CHECK_POLL),
        }
    }
}

/// Carries out `proteus instance ...`. Starting one gives back its configuration, to run.
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
                Ok(config::Parsed::Run(mut config)) => {
                    config.instance = Some(name);
                    Ok(Some(*config))
                }
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

/// Keeps a program this one starts from opening a console window of its own when this one has
/// none (started from the launcher or a shortcut); from a terminal, it shares that terminal.
pub(crate) fn no_console_window(command: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    // SAFETY: takes no arguments and only reads this process's console state.
    if unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() }.is_null() {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::os::windows::process::CommandExt::creation_flags(command, CREATE_NO_WINDOW);
    }
    command
}

/// The coin logo as a window icon: 64x64 RGBA, made from `assets/logo.svg`.
#[cfg(host_board)]
pub(crate) fn app_icon() -> Option<winit::window::Icon> {
    const SIDE: u32 = 64;
    let rgba = include_bytes!("../../../assets/icon-64.rgba").to_vec();
    winit::window::Icon::from_rgba(rgba, SIDE, SIDE).ok()
}

/// Entry point of the `proteus` binary.
pub fn cli_main() -> ExitCode {
    let started = Instant::now();
    #[cfg(host_board)]
    update::remove_leftovers();
    let config = match config::parse(std::env::args_os().skip(1)) {
        Ok(config::Parsed::Help) => {
            println!("{}", copy::HELP);
            return ExitCode::SUCCESS;
        }
        Ok(config::Parsed::Ctl { socket, command }) => {
            let socket = instance::resolve(&socket);
            return match ctl::request(&socket, &command, &mut std::io::stdout()) {
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
        Ok(config::Parsed::GpuCheck) => return gpu_check(),
        #[cfg(host_board)]
        Ok(config::Parsed::Update) => {
            return match update::update() {
                Ok(update::Outcome::UpToDate(tag)) => {
                    println!("{}", copy::up_to_date(&tag));
                    ExitCode::SUCCESS
                }
                Ok(update::Outcome::Updated(tag)) => {
                    println!("{}", copy::updated(&tag));
                    ExitCode::SUCCESS
                }
                Err(why) => {
                    eprintln!("{}", copy::fatal(&why));
                    ExitCode::FAILURE
                }
            };
        }
        #[cfg(not(host_board))]
        Ok(config::Parsed::Update) => {
            eprintln!("{}", copy::fatal(&copy::UPDATE_UNSUPPORTED));
            return ExitCode::FAILURE;
        }
        #[cfg(host_board)]
        Ok(config::Parsed::Launcher) => {
            // Opened by a double click or shortcut, Windows gave this console program a console
            // window of its own, which would sit there empty: close it. A terminal's stays.
            #[cfg(windows)]
            // SAFETY: plain calls on this process's console; a too-small buffer only gets a count.
            unsafe {
                use windows_sys::Win32::System::Console::{
                    FreeConsole, GetConsoleProcessList, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
                    STD_OUTPUT_HANDLE, SetStdHandle,
                };
                let mut processes = [0u32; 2];
                if GetConsoleProcessList(processes.as_mut_ptr(), 2) == 1 {
                    FreeConsole();
                    // The old console handles are now dead; a child that inherited them would
                    // fail to start ("The handle is invalid" / "The request is not supported").
                    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                        SetStdHandle(which, std::ptr::null_mut());
                    }
                }
            }
            return match launcher::run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(why) => {
                    eprintln!("{}", copy::fatal(&why));
                    ExitCode::FAILURE
                }
            };
        }
        #[cfg(not(host_board))]
        Ok(config::Parsed::Launcher) => {
            eprintln!(
                "{}\n{}",
                copy::fatal(&copy::MISSING_KERNEL),
                copy::USAGE_HINT
            );
            return ExitCode::from(EXIT_USAGE);
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
    let mut config = config;
    if config.gpu {
        match start_gpu() {
            Ok(()) => eprintln!("{}", copy::GPU_ACCELERATED),
            Err(why) => {
                eprintln!("{}", copy::gpu_unavailable(&why));
                config.gpu = false;
            }
        }
    }
    match run(&config, started) {
        #[cfg(host_board)]
        Ok(exit @ (Exit::Restart | Exit::FactoryReset)) if let Some(name) = &config.instance => {
            if exit == Exit::FactoryReset {
                let wiped = instance::root().and_then(|root| instance::wipe_data(&root, name));
                if let Err(err) = wiped {
                    eprintln!("{}", copy::fatal(&err));
                    return ExitCode::FAILURE;
                }
            }
            eprintln!("{}", copy::fatal(&restart_instance(name)));
            ExitCode::FAILURE
        }
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}", copy::fatal(&err));
            ExitCode::FAILURE
        }
    }
}

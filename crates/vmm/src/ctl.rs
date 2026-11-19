//! The control socket: a local socket that takes one command per line and answers each with one line.
//!
//! This is how anything outside the emulator (the `emu ctl` command, a script, later the GUI) drives
//! a running instance. A command is plain words separated by spaces:
//!
//! ```text
//! tap <x> <y>
//! swipe <x1> <y1> <x2> <y2> [<milliseconds>]
//! touch <finger> <x> <y>
//! lift <finger>
//! key <code> [down|up]
//! pad <control> <value>
//! screenshot <file>
//! screenrecord <file> [<seconds>]
//! install <file or directory>
//! location <latitude> <longitude> [<metres above sea level>]
//! location <map link>
//! walk <degrees from north> <km/h>
//! walk stop
//! route <gpx file> [<km/h>]
//! rotate <x> <y> <z>
//! shake
//! stop
//! wait <milliseconds>
//! run <file> [<times>]
//! record <file>
//! record-stop
//! cancel
//! ```
//!
//! A macro is a file of these commands, one per line; `#` starts a comment. `record` writes one
//! from the touches and keys that follow, with the pauses between them, until `record-stop`. `run`
//! plays one, and `cancel` stops a macro that is playing.
//!
//! `install` takes an APK, a directory of split APKs, or an `.apks` / `.xapk` archive (whose OBB
//! files are copied too). `screenrecord` records the screen (no sound) for the given time, up to
//! three minutes, and saves it as MP4. Both go through the guest's `adb`.
//!
//! Gamepad controls are `a`, `b`, `x`, `y`, `l1`, `r1`, `l2`, `r2`, `select`, `start`, `mode`,
//! `thumbl` and `thumbr` (value `down`, `up`, 1 or 0), the sticks `lx`, `ly`, `rx` and `ry` (-1 to 1,
//! right and down positive), the triggers `lt` and `rt` (0 to 1) and the direction pad `hatx` and
//! `haty` (-1, 0 or 1).
//!
//! Positions are display pixels, key codes are Linux key codes, and angles are degrees. The answer is `ok`, or `error:`
//! followed by the reason. Unix sockets only; a Windows host will need a named pipe here.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use devices::virtio::input::{InputEvent, InputHandle};

use crate::config::{DEFAULT_SWIPE, MACRO_CANCEL_POLL, MACRO_MAX_BYTES, SWIPE_STEP, TAP_HOLD};
use crate::config::{GUEST_RECORDING, MAX_KMH, ROUTE_DEFAULT_KMH, SCREENRECORD_MAX};
use crate::copy;
use crate::frames::Screen;
use crate::input::{self, PadControl, PadKind, TOUCH_SLOTS, Touch};
use crate::location::{self, Fix, Location};
use crate::sensors::Motion;
use crate::session::PowerSwitch;

const WORD_TAP: &str = "tap";
const WORD_SWIPE: &str = "swipe";
const WORD_TOUCH: &str = "touch";
const WORD_LIFT: &str = "lift";
const WORD_KEY: &str = "key";
const WORD_PAD: &str = "pad";
const WORD_SCREENSHOT: &str = "screenshot";
const WORD_SCREENRECORD: &str = "screenrecord";
const WORD_INSTALL: &str = "install";
const WORD_LOCATION: &str = "location";
const WORD_WALK: &str = "walk";
const WORD_ROUTE: &str = "route";
const WORD_ROTATE: &str = "rotate";
const WORD_SHAKE: &str = "shake";
const WORD_STOP: &str = "stop";
const WORD_WAIT: &str = "wait";
const WORD_RUN: &str = "run";
const WORD_RECORD: &str = "record";
const WORD_RECORD_STOP: &str = "record-stop";
const WORD_CANCEL: &str = "cancel";
const WORD_SHELL: &str = "shell";
/// Starts each line of what a `shell` command printed, so the answer line after them stands out.
pub const SHELL_LINE: &str = "| ";
const MACRO_COMMENT: char = '#';
const WORD_DOWN: &str = "down";
const WORD_UP: &str = "up";

/// The finger `tap` and `swipe` use: the last one, so that they stay clear of `touch 0`, `touch 1`…
const GESTURE_FINGER: u32 = TOUCH_SLOTS - 1;
/// The socket file is created for its owner alone: anyone who can connect controls the guest.
const SOCKET_UMASK: libc::mode_t = 0o177;

#[derive(Debug, PartialEq)]
pub enum Command {
    Tap {
        x: u32,
        y: u32,
    },
    Swipe {
        from: (u32, u32),
        to: (u32, u32),
        duration: Duration,
    },
    Touch {
        finger: u32,
        x: u32,
        y: u32,
    },
    Lift {
        finger: u32,
    },
    /// `down` is `None` for a full press: down, then up.
    Key {
        code: u16,
        down: Option<bool>,
    },
    /// A gamepad control moved; see `input::pad_event` for the values.
    Pad {
        control: PadControl,
        value: f64,
    },
    Screenshot(PathBuf),
    /// Record the screen into an MP4 file for this long.
    Screenrecord {
        path: PathBuf,
        duration: Duration,
    },
    /// Install the app at this path.
    Install(PathBuf),
    Location {
        latitude: f64,
        longitude: f64,
        altitude: f64,
    },
    /// Walk at `(degrees from north, km/h)` from where the device is; `None` stops.
    Walk(Option<(f64, f64)>),
    /// Follow the route in a GPX file at this many km/h.
    Route {
        path: PathBuf,
        km_per_hour: f64,
    },
    /// Degrees about the device's x, y and z axes.
    Rotate([f64; 3]),
    Shake,
    /// Run a command in the guest's shell, through adb.
    Shell(String),
    /// Turn the machine off without asking the guest.
    Stop,
    Wait(Duration),
    /// Play the macro in a file this many times.
    Run {
        path: PathBuf,
        times: u32,
    },
    /// Start recording a macro into a file.
    Record(PathBuf),
    RecordStop,
    /// Stop the macro that is playing.
    Cancel,
}

impl Command {
    /// Whether this is something a macro recording keeps: what the player did to the guest.
    fn is_input(&self) -> bool {
        matches!(
            self,
            Self::Tap { .. }
                | Self::Swipe { .. }
                | Self::Touch { .. }
                | Self::Lift { .. }
                | Self::Key { .. }
                | Self::Pad { .. }
        )
    }
}

/// The command as it is typed. `parse` reads this back to the same command.
impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tap { x, y } => write!(f, "{WORD_TAP} {x} {y}"),
            Self::Swipe { from, to, duration } => {
                let millis = duration.as_millis();
                write!(
                    f,
                    "{WORD_SWIPE} {} {} {} {} {millis}",
                    from.0, from.1, to.0, to.1
                )
            }
            Self::Touch { finger, x, y } => write!(f, "{WORD_TOUCH} {finger} {x} {y}"),
            Self::Lift { finger } => write!(f, "{WORD_LIFT} {finger}"),
            Self::Key { code, down: None } => write!(f, "{WORD_KEY} {code}"),
            Self::Key {
                code,
                down: Some(down),
            } => {
                let state = if *down { WORD_DOWN } else { WORD_UP };
                write!(f, "{WORD_KEY} {code} {state}")
            }
            Self::Pad { control, value } => write!(f, "{WORD_PAD} {} {value}", control.name),
            Self::Screenrecord { path, duration } => {
                write!(
                    f,
                    "{WORD_SCREENRECORD} {} {}",
                    path.display(),
                    duration.as_secs()
                )
            }
            Self::Install(path) => write!(f, "{WORD_INSTALL} {}", path.display()),
            Self::Screenshot(path) => write!(f, "{WORD_SCREENSHOT} {}", path.display()),
            Self::Location {
                latitude,
                longitude,
                altitude,
            } => write!(f, "{WORD_LOCATION} {latitude} {longitude} {altitude}"),
            Self::Walk(Some((bearing, speed))) => write!(f, "{WORD_WALK} {bearing} {speed}"),
            Self::Walk(None) => write!(f, "{WORD_WALK} {WORD_STOP}"),
            Self::Route { path, km_per_hour } => {
                write!(f, "{WORD_ROUTE} {} {km_per_hour}", path.display())
            }
            Self::Rotate([x, y, z]) => write!(f, "{WORD_ROTATE} {x} {y} {z}"),
            Self::Shake => f.write_str(WORD_SHAKE),
            Self::Shell(command) => write!(f, "{WORD_SHELL} {command}"),
            Self::Stop => f.write_str(WORD_STOP),
            Self::Wait(duration) => write!(f, "{WORD_WAIT} {}", duration.as_millis()),
            Self::Run { path, times } => write!(f, "{WORD_RUN} {} {times}", path.display()),
            Self::Record(path) => write!(f, "{WORD_RECORD} {}", path.display()),
            Self::RecordStop => f.write_str(WORD_RECORD_STOP),
            Self::Cancel => f.write_str(WORD_CANCEL),
        }
    }
}

/// Reads one command line. `None` if it is not a command in the form the module text gives.
pub fn parse(line: &str) -> Option<Command> {
    let line = line.trim();
    let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let numbers: Option<Vec<u32>> = rest.split_whitespace().map(|n| n.parse().ok()).collect();
    let reals: Option<Vec<f64>> = rest.split_whitespace().map(|n| n.parse().ok()).collect();
    if word == WORD_SHELL && !rest.is_empty() {
        return Some(Command::Shell(rest.to_owned()));
    }
    match (word, reals.as_deref()) {
        (WORD_LOCATION, Some(&[latitude, longitude])) => {
            return Some(Command::Location {
                latitude,
                longitude,
                altitude: 0.0,
            });
        }
        (WORD_LOCATION, Some(&[latitude, longitude, altitude])) => {
            return Some(Command::Location {
                latitude,
                longitude,
                altitude,
            });
        }
        (WORD_ROTATE, Some(&[x, y, z])) => return Some(Command::Rotate([x, y, z])),
        (WORD_WALK, Some(&[bearing, speed])) if bearing.is_finite() && speed.is_finite() => {
            return Some(Command::Walk(Some((bearing, speed))));
        }
        (WORD_LOCATION, None) if !rest.is_empty() => {
            let fix = location::from_link(rest)?;
            return Some(Command::Location {
                latitude: fix.latitude,
                longitude: fix.longitude,
                altitude: fix.altitude,
            });
        }
        (WORD_SHAKE, Some(&[])) => return Some(Command::Shake),
        (WORD_STOP, Some(&[])) => return Some(Command::Stop),
        (WORD_WALK, None) if rest == WORD_STOP => return Some(Command::Walk(None)),
        (WORD_ROUTE, _) if !rest.is_empty() => {
            let speed = rest
                .rsplit_once(char::is_whitespace)
                .and_then(|(path, last)| {
                    let speed: f64 = last.parse().ok().filter(|speed: &f64| speed.is_finite())?;
                    Some((path.trim_end(), speed))
                });
            let (path, km_per_hour) = speed.unwrap_or((rest, ROUTE_DEFAULT_KMH));
            return Some(Command::Route {
                path: path.into(),
                km_per_hour,
            });
        }
        (WORD_RECORD_STOP, Some(&[])) => return Some(Command::RecordStop),
        (WORD_CANCEL, Some(&[])) => return Some(Command::Cancel),
        _ => {}
    }
    match (word, numbers.as_deref()) {
        (WORD_TAP, Some(&[x, y])) => Some(Command::Tap { x, y }),
        (WORD_SWIPE, Some(&[x1, y1, x2, y2])) => Some(Command::Swipe {
            from: (x1, y1),
            to: (x2, y2),
            duration: DEFAULT_SWIPE,
        }),
        (WORD_SWIPE, Some(&[x1, y1, x2, y2, millis])) => Some(Command::Swipe {
            from: (x1, y1),
            to: (x2, y2),
            duration: Duration::from_millis(millis.into()),
        }),
        (WORD_TOUCH, Some(&[finger, x, y])) => Some(Command::Touch { finger, x, y }),
        (WORD_LIFT, Some(&[finger])) => Some(Command::Lift { finger }),
        (WORD_KEY, _) => {
            let mut words = rest.split_whitespace();
            let code = words.next()?.parse().ok()?;
            let down = match words.next() {
                None => None,
                Some(WORD_DOWN) => Some(true),
                Some(WORD_UP) => Some(false),
                Some(_) => return None,
            };
            words
                .next()
                .is_none()
                .then_some(Command::Key { code, down })
        }
        (WORD_PAD, _) => {
            let (name, value) = rest.split_once(char::is_whitespace)?;
            let control = input::pad_control(name)?;
            let value = match (control.kind, value.trim()) {
                (PadKind::Button, WORD_DOWN) => 1.0,
                (PadKind::Button, WORD_UP) => 0.0,
                (_, number) => number
                    .parse()
                    .ok()
                    .filter(|value: &f64| value.is_finite())?,
            };
            Some(Command::Pad { control, value })
        }
        (WORD_SCREENSHOT, _) if !rest.is_empty() => Some(Command::Screenshot(rest.into())),
        (WORD_WAIT, Some(&[millis])) => Some(Command::Wait(Duration::from_millis(millis.into()))),
        (WORD_RECORD, _) if !rest.is_empty() => Some(Command::Record(rest.into())),
        (WORD_RUN, _) if !rest.is_empty() => {
            let (path, times) = counted(rest).unwrap_or((rest, 1));
            Some(Command::Run {
                path: path.into(),
                times,
            })
        }
        (WORD_SCREENRECORD, _) if !rest.is_empty() => {
            let (path, seconds) =
                counted(rest).unwrap_or((rest, SCREENRECORD_MAX.as_secs() as u32));
            Some(Command::Screenrecord {
                path: path.into(),
                duration: Duration::from_secs(seconds.into()),
            })
        }
        (WORD_INSTALL, _) if !rest.is_empty() => Some(Command::Install(rest.into())),
        _ => None,
    }
}

/// A file name, which may have spaces in it, and a last word that is a number, if there is one.
fn counted(rest: &str) -> Option<(&str, u32)> {
    let (path, last) = rest.rsplit_once(char::is_whitespace)?;
    Some((path.trim_end(), last.parse().ok()?))
}

/// A macro being recorded: the commands so far, as lines of a macro file.
pub struct Recording {
    path: PathBuf,
    lines: Vec<String>,
    /// When the last command was recorded, to write the pause before the next one.
    last: Option<Instant>,
}

/// Reads a macro file into its commands. Macros do not start other macros or recordings.
fn read_macro(path: &Path) -> Result<Vec<Command>, String> {
    let too_large = std::fs::metadata(path).is_ok_and(|file| file.len() > MACRO_MAX_BYTES);
    if too_large {
        return Err(copy::CTL_MACRO_TOO_LARGE.to_owned());
    }
    let text = std::fs::read_to_string(path).map_err(|err| copy::file(path, &err))?;
    let mut commands = Vec::new();
    for (number, line) in (1..).zip(text.lines()) {
        let line = line.split(MACRO_COMMENT).next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let command = parse(line).filter(|command| {
            !matches!(
                command,
                Command::Run { .. } | Command::Record(_) | Command::RecordStop | Command::Cancel
            )
        });
        commands.push(command.ok_or_else(|| copy::ctl_bad_macro_line(path, number))?);
    }
    Ok(commands)
}

fn send(device: &InputHandle, events: &[InputEvent]) -> Result<(), String> {
    if device.send(events) {
        Ok(())
    } else {
        Err(copy::CTL_GUEST_BEHIND.to_owned())
    }
}

/// A point `step` of `steps` of the way along a line.
fn along(from: (u32, u32), to: (u32, u32), step: u32, steps: u32) -> (u32, u32) {
    let lerp = |a: u32, b: u32| {
        let moved = (i64::from(b) - i64::from(a)) * i64::from(step) / i64::from(steps);
        (i64::from(a) + moved) as u32
    };
    (lerp(from.0, to.0), lerp(from.1, to.1))
}

/// Other instances that get this one's input too: one thread each, which keeps a connection to
/// that instance's control socket and sends it every command it is handed.
#[derive(Default)]
pub struct Followers(Vec<mpsc::Sender<String>>);

impl Followers {
    pub fn start(targets: &[PathBuf]) -> std::io::Result<Self> {
        let mut senders = Vec::new();
        for target in targets {
            let (sender, lines) = mpsc::channel::<String>();
            let target = target.clone();
            thread::Builder::new()
                .name(copy::SYNC_THREAD.into())
                .spawn(move || follow(&target, &lines))?;
            senders.push(sender);
        }
        Ok(Self(senders))
    }

    /// Hands a command to every follower. It does not wait for them.
    fn send(&self, line: &str) {
        for sender in &self.0 {
            // A follower thread only ends when this end is dropped.
            let _ = sender.send(line.to_owned());
        }
    }
}

/// Sends each line to the instance on `target`, connecting when needed. A follower that is not
/// running misses what happens until it is; that is reported once per connection attempt.
fn follow(target: &Path, lines: &mpsc::Receiver<String>) {
    let mut connection: Option<(BufReader<UnixStream>, UnixStream)> = None;
    for line in lines {
        if connection.is_none() {
            let opened = UnixStream::connect(target)
                .and_then(|stream| Ok((BufReader::new(stream.try_clone()?), stream)));
            connection = opened
                .inspect_err(|err| log::warn!("{}", copy::sync_failed(target, err)))
                .ok();
        }
        let Some((reader, writer)) = connection.as_mut() else {
            continue;
        };
        let mut reply = String::new();
        let answered = writeln!(writer, "{line}").and_then(|()| reader.read_line(&mut reply));
        match answered {
            Ok(read) if read > 0 => {
                if reply.trim_end() != copy::CTL_OK {
                    log::debug!("{}", copy::sync_failed(target, &reply.trim_end()));
                }
            }
            _ => connection = None,
        }
    }
}

/// The parts of a running machine that commands act on.
#[derive(Default)]
pub struct Controller {
    /// The touchscreen, and which fingers are on it.
    pub touch: Option<(InputHandle, Mutex<Touch>)>,
    pub keyboard: Option<InputHandle>,
    pub gamepad: Option<InputHandle>,
    pub screen: Option<Arc<Screen>>,
    pub location: Option<Arc<Location>>,
    pub motion: Option<Arc<Motion>>,
    pub power: Arc<PowerSwitch>,
    /// The loopback port the guest's `adb` is reached on; 0 while there is none.
    pub adb_port: Arc<AtomicU16>,
    /// The macro being recorded, if one is.
    pub recording: Mutex<Option<Recording>>,
    /// Set to stop the macro that is playing.
    pub cancel: AtomicBool,
    /// Instances that get this one's input as well.
    pub followers: Followers,
    /// Held for the length of a tap or swipe, so that two of them do not share the gesture finger.
    pub gesture: Mutex<()>,
}

impl Controller {
    /// Which way up the screen is shown, in quarter turns (see `orientation`).
    pub fn turns(&self) -> u8 {
        self.screen.as_ref().map_or(0, |screen| screen.turns())
    }

    /// Puts `finger` down, or moves it, at `(x, y)` of the screen as it is shown upright.
    fn place(&self, finger: u32, x: u32, y: u32) -> Result<(), String> {
        let turns = self.turns();
        self.touch(|touch| {
            let (x, y) = crate::orientation::to_panel(x, y, touch.size(), turns)?;
            touch.place(finger, x, y)
        })
    }

    fn touch(
        &self,
        change: impl FnOnce(&mut Touch) -> Option<Vec<InputEvent>>,
    ) -> Result<(), String> {
        let (device, state) = self.touch.as_ref().ok_or(copy::CTL_NO_TOUCHSCREEN)?;
        // The lock also covers the send, so that reports reach the guest in the order they were made.
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        let events = change(&mut state).ok_or(copy::CTL_BAD_TOUCH)?;
        send(device, &events)
    }

    fn key(&self, code: u16, down: bool) -> Result<(), String> {
        let keyboard = self.keyboard.as_ref().ok_or(copy::CTL_NO_KEYBOARD)?;
        if !input::is_key(code) {
            return Err(copy::CTL_BAD_KEY.to_owned());
        }
        send(keyboard, &input::key(code, down))
    }

    /// Runs `command` in the guest's shell and gives back what it printed.
    pub fn shell(&self, command: &str) -> Result<String, String> {
        let port = self.adb_port.load(Ordering::SeqCst);
        if port == 0 {
            return Err(copy::CTL_NO_ADB.to_owned());
        }
        crate::adb::output(port, command).map_err(|err| copy::ctl_shell_failed(&err))
    }

    /// Records the guest's screen for `duration`, then copies the recording to `path`.
    fn screenrecord(&self, path: &Path, duration: Duration) -> Result<(), String> {
        if duration.is_zero() || duration > SCREENRECORD_MAX {
            return Err(copy::ctl_record_too_long(SCREENRECORD_MAX));
        }
        let port = self.adb_port.load(Ordering::SeqCst);
        let guest_file = crate::adb::quote(GUEST_RECORDING);
        let record = format!(
            "screenrecord --time-limit {} {guest_file}",
            duration.as_secs()
        );
        let failed = |err: std::io::Error| copy::file(path, &err);
        let said = crate::adb::output(port, &record).map_err(failed)?;
        let mut file = std::fs::File::create(path).map_err(|err| copy::file(path, &err))?;
        let copied = crate::adb::exec(port, &format!("cat {guest_file}"), None, &mut file);
        let _ = crate::adb::output(port, &format!("rm -f {guest_file}"));
        copied.map_err(failed)?;
        // screenrecord says nothing when it works; an empty file means it did not.
        if file.metadata().map_err(|err| copy::file(path, &err))?.len() == 0 {
            let _ = std::fs::remove_file(path);
            return Err(copy::ctl_record_failed(said.trim()));
        }
        Ok(())
    }

    /// Carries out one command. Gestures, waits and macros take as long as they last.
    pub fn execute(&self, command: &Command) -> Result<(), String> {
        // The pause before a command is measured to when it starts, not to when it has finished.
        let started = Instant::now();
        self.perform(command)?;
        if command.is_input() {
            self.followers.send(&command.to_string());
        }
        let mut recording = self
            .recording
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(recording) = recording.as_mut().filter(|_| command.is_input()) {
            if let Some(last) = recording.last {
                let pause = started.saturating_duration_since(last);
                recording.lines.push(Command::Wait(pause).to_string());
            }
            recording.last = Some(started);
            recording.lines.push(command.to_string());
        }
        Ok(())
    }

    /// Waits, in short steps so that a cancelled macro stops soon.
    fn wait(&self, duration: Duration) -> Result<(), String> {
        let until = Instant::now() + duration;
        loop {
            if self.cancel.load(Ordering::SeqCst) {
                return Err(copy::CTL_MACRO_CANCELLED.to_owned());
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            thread::sleep(left.min(MACRO_CANCEL_POLL));
        }
    }

    fn perform(&self, command: &Command) -> Result<(), String> {
        match *command {
            Command::Touch { finger, x, y } => self.place(finger, x, y),
            Command::Lift { finger } => self.touch(|touch| touch.lift(finger)),
            Command::Tap { x, y } => {
                let _gesture = self.gesture.lock().unwrap_or_else(PoisonError::into_inner);
                self.place(GESTURE_FINGER, x, y)?;
                thread::sleep(TAP_HOLD);
                self.touch(|touch| touch.lift(GESTURE_FINGER))
            }
            Command::Swipe { from, to, duration } => {
                let _gesture = self.gesture.lock().unwrap_or_else(PoisonError::into_inner);
                let steps =
                    (duration.as_millis() / SWIPE_STEP.as_millis()).clamp(1, u128::from(u32::MAX));
                let steps = steps as u32;
                self.place(GESTURE_FINGER, from.0, from.1)?;
                let moved = (1..=steps).try_for_each(|step| {
                    thread::sleep(SWIPE_STEP);
                    let (x, y) = along(from, to, step, steps);
                    self.place(GESTURE_FINGER, x, y)
                });
                // The finger comes up even if the swipe left the screen part of the way.
                self.touch(|touch| touch.lift(GESTURE_FINGER)).and(moved)
            }
            Command::Key {
                code,
                down: Some(down),
            } => self.key(code, down),
            Command::Key { code, down: None } => {
                self.key(code, true)?;
                thread::sleep(TAP_HOLD);
                self.key(code, false)
            }
            Command::Pad { control, value } => {
                let gamepad = self.gamepad.as_ref().ok_or(copy::CTL_NO_GAMEPAD)?;
                send(gamepad, &input::pad_event(control, value))
            }
            Command::Shell(ref command) => self.shell(command).map(drop),
            Command::Install(ref path) => {
                let port = self.adb_port.load(Ordering::SeqCst);
                crate::install::install(port, path).map_err(|err| copy::install_failed(path, &err))
            }
            Command::Screenrecord { ref path, duration } => self.screenrecord(path, duration),
            Command::Screenshot(ref path) => {
                let screen = self.screen.as_ref().ok_or(copy::CTL_NO_DISPLAY)?;
                match screen.save(path) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(copy::CTL_NO_FRAME_YET.to_owned()),
                    Err(err) => Err(copy::file(path, &err)),
                }
            }
            Command::Location {
                latitude,
                longitude,
                altitude,
            } => {
                let location = self.location.as_ref().ok_or(copy::CTL_NO_LOCATION)?;
                let fix = Fix::new(latitude, longitude, altitude).ok_or(copy::CTL_BAD_LOCATION)?;
                location.set(fix);
                Ok(())
            }
            Command::Walk(heading) => {
                let location = self.location.as_ref().ok_or(copy::CTL_NO_LOCATION)?;
                if heading.is_some_and(|(_, speed)| !(0.0..=MAX_KMH).contains(&speed)) {
                    return Err(copy::ctl_bad_speed(MAX_KMH));
                }
                location.walk(heading)
            }
            Command::Route {
                ref path,
                km_per_hour,
            } => {
                let location = self.location.as_ref().ok_or(copy::CTL_NO_LOCATION)?;
                if !(0.0..=MAX_KMH).contains(&km_per_hour) || km_per_hour == 0.0 {
                    return Err(copy::ctl_bad_speed(MAX_KMH));
                }
                location.follow(path, km_per_hour)
            }
            Command::Rotate(degrees) => {
                let motion = self.motion.as_ref().ok_or(copy::CTL_NO_SENSORS)?;
                if !degrees.iter().all(|angle| angle.is_finite()) {
                    return Err(copy::CTL_USAGE.to_owned());
                }
                motion.rotate_to(degrees);
                Ok(())
            }
            Command::Shake => {
                let motion = self.motion.as_ref().ok_or(copy::CTL_NO_SENSORS)?;
                motion.shake();
                Ok(())
            }
            Command::Stop => {
                self.power.off();
                Ok(())
            }
            Command::Wait(duration) => self.wait(duration),
            Command::Run { ref path, times } => {
                let commands = read_macro(path)?;
                self.cancel.store(false, Ordering::SeqCst);
                for _ in 0..times {
                    for command in &commands {
                        if self.cancel.load(Ordering::SeqCst) {
                            return Err(copy::CTL_MACRO_CANCELLED.to_owned());
                        }
                        self.execute(command)?;
                    }
                }
                Ok(())
            }
            Command::Record(ref path) => {
                let mut recording = self
                    .recording
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if recording.is_some() {
                    return Err(copy::CTL_ALREADY_RECORDING.to_owned());
                }
                // Made now, so that a path that cannot be written is reported before the recording.
                std::fs::write(path, "").map_err(|err| copy::file(path, &err))?;
                *recording = Some(Recording {
                    path: path.clone(),
                    lines: vec![copy::MACRO_HEADER.to_owned()],
                    last: None,
                });
                Ok(())
            }
            Command::RecordStop => {
                let mut recording = self
                    .recording
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let done = recording.take().ok_or(copy::CTL_NOT_RECORDING)?;
                let text = done.lines.join("\n") + "\n";
                std::fs::write(&done.path, text).map_err(|err| copy::file(&done.path, &err))
            }
            Command::Cancel => {
                self.cancel.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
    }
}

/// Answers the commands arriving on one connection until its other end closes.
pub fn serve(controller: &Controller, reader: impl BufRead, mut writer: impl Write) {
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let done = match parse(&line) {
            // What the shell printed goes back too, a line at a time, before the answer.
            Some(Command::Shell(command)) => controller.shell(&command).and_then(|printed| {
                printed
                    .lines()
                    .try_for_each(|printed| writeln!(writer, "{SHELL_LINE}{printed}"))
                    .map_err(|err| err.to_string())
            }),
            Some(command) => controller.execute(&command),
            None => Err(copy::CTL_USAGE.to_owned()),
        };
        let reply = match done {
            Ok(()) => copy::CTL_OK.to_owned(),
            Err(why) => copy::ctl_error(&why),
        };
        if writeln!(writer, "{reply}").is_err() {
            break;
        }
    }
}

fn bind(path: &Path) -> std::io::Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err),
        Ok(found) if !found.file_type().is_socket() => {
            return Err(std::io::Error::new(
                ErrorKind::AlreadyExists,
                copy::CTL_NOT_A_SOCKET,
            ));
        }
        Ok(_) if UnixStream::connect(path).is_ok() => {
            return Err(std::io::Error::new(
                ErrorKind::AddrInUse,
                copy::CTL_SOCKET_IN_USE,
            ));
        }
        // A socket nobody answers on was left behind by an instance that did not shut down cleanly.
        Ok(_) => std::fs::remove_file(path)?,
    }
    // SAFETY: `umask` only swaps the process's file-creation mask; it touches no memory and cannot fail.
    let previous = unsafe { libc::umask(SOCKET_UMASK) };
    let listener = UnixListener::bind(path);
    // SAFETY: as above; this puts the caller's mask back.
    unsafe { libc::umask(previous) };
    listener
}

/// A listening control socket. Dropping it stops listening and removes the socket file.
pub struct Server {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    accepting: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start(path: &Path, controller: Arc<Controller>) -> std::io::Result<Self> {
        let listener = bind(path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let accept = move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let connection = stream.and_then(|stream| Ok((stream.try_clone()?, stream)));
                let (reader, writer) = match connection {
                    Ok(pair) => pair,
                    Err(err) => {
                        log::warn!("{}", copy::ctl_accept_failed(&err));
                        break;
                    }
                };
                let controller = Arc::clone(&controller);
                let serving = thread::Builder::new()
                    .name(copy::CTL_THREAD.into())
                    .spawn(move || serve(&controller, BufReader::new(reader), writer));
                if let Err(err) = serving {
                    log::warn!("{}", copy::thread(&err));
                }
            }
        };
        let accepting = thread::Builder::new()
            .name(copy::CTL_THREAD.into())
            .spawn(accept)?;
        Ok(Self {
            path: path.to_owned(),
            stop,
            accepting: Some(accepting),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The accept loop only looks at the flag when a connection arrives, so make one. If that fails
        // the loop cannot be woken, and waiting for it would hang.
        if UnixStream::connect(&self.path).is_ok()
            && let Some(accepting) = self.accepting.take()
        {
            let _ = accepting.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Sends one command to a running instance and returns its one-line answer.
/// Sends `command` and gives back the answer line. Lines a `shell` command printed go to `printed`.
pub fn request(socket: &Path, command: &str, printed: &mut impl Write) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    writeln!(stream, "{command}")?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut reply = String::new();
        if reader.read_line(&mut reply)? == 0 {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                copy::CTL_NO_REPLY,
            ));
        }
        match reply.strip_prefix(SHELL_LINE) {
            Some(line) => printed.write_all(line.as_bytes())?,
            None => return Ok(reply.trim_end().to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use devices::virtio::gpu::{Frame, FrameSink};
    use devices::virtio::input::Input;

    use super::*;

    const WIDTH: u32 = 64;
    const HEIGHT: u32 = 48;

    fn controller(screen: Option<Arc<Screen>>) -> Controller {
        let (_touchscreen, touch) = Input::new(input::touchscreen(WIDTH, HEIGHT));
        let (_keyboard, keys) = Input::new(input::keyboard());
        Controller {
            touch: Some((touch, Mutex::new(Touch::new(WIDTH, HEIGHT)))),
            keyboard: Some(keys),
            screen,
            ..Controller::default()
        }
    }

    fn replies(controller: &Controller, commands: &str) -> Vec<String> {
        let mut output = Vec::new();
        serve(controller, commands.as_bytes(), &mut output);
        let text = String::from_utf8(output).unwrap();
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn commands_are_read_word_by_word() {
        assert_eq!(parse("tap 10 20"), Some(Command::Tap { x: 10, y: 20 }));
        assert_eq!(
            parse("  swipe 1 2 3 4  "),
            Some(Command::Swipe {
                from: (1, 2),
                to: (3, 4),
                duration: DEFAULT_SWIPE
            })
        );
        assert_eq!(
            parse("swipe 1 2 3 4 750"),
            Some(Command::Swipe {
                from: (1, 2),
                to: (3, 4),
                duration: Duration::from_millis(750)
            })
        );
        assert_eq!(
            parse("touch 2 5 6"),
            Some(Command::Touch {
                finger: 2,
                x: 5,
                y: 6
            })
        );
        assert_eq!(parse("lift 2"), Some(Command::Lift { finger: 2 }));
        assert_eq!(
            parse("key 116"),
            Some(Command::Key {
                code: 116,
                down: None
            })
        );
        assert_eq!(
            parse("key 30 down"),
            Some(Command::Key {
                code: 30,
                down: Some(true)
            })
        );
        assert_eq!(
            parse("screenshot /tmp/a file.png"),
            Some(Command::Screenshot("/tmp/a file.png".into()))
        );
    }

    #[test]
    fn malformed_commands_are_refused() {
        for line in [
            "",
            "tap",
            "tap 1",
            "tap 1 2 3",
            "tap -1 2",
            "tap a b",
            "swipe 1 2 3",
            "lift",
            "key",
            "key x",
            "key 30 sideways",
            "key 30 down again",
            "pad a",
            "pad a sideways",
            "pad lx down",
            "pad lx NaN",
            "pad nub 1",
            "screenshot",
            "screenrecord",
            "install",
            "reboot",
        ] {
            assert_eq!(parse(line), None, "{line:?}");
        }
    }

    #[test]
    fn a_swipe_ends_exactly_at_its_target() {
        assert_eq!(along((10, 40), (30, 0), 0, 4), (10, 40));
        assert_eq!(along((10, 40), (30, 0), 2, 4), (20, 20));
        assert_eq!(along((10, 40), (30, 0), 4, 4), (30, 0));
    }

    #[test]
    fn each_command_gets_one_answer_line() {
        let controller = controller(None);
        let answers = replies(
            &controller,
            "tap 1 2\n\ntouch 0 5 5\nlift 0\nlift 0\ntap 64 0\nkey 30\nkey 272\nbogus\nscreenshot x.png\n",
        );
        let error = |why: &str| copy::ctl_error(&why);
        assert_eq!(
            answers,
            [
                copy::CTL_OK.to_owned(),
                copy::CTL_OK.to_owned(),
                copy::CTL_OK.to_owned(),
                // That finger is already up, and x=64 is one past the last column.
                error(copy::CTL_BAD_TOUCH),
                error(copy::CTL_BAD_TOUCH),
                copy::CTL_OK.to_owned(),
                // 272 is a mouse button.
                error(copy::CTL_BAD_KEY),
                error(copy::CTL_USAGE),
                error(copy::CTL_NO_DISPLAY),
            ]
        );
    }

    #[test]
    fn location_and_motion_commands_reach_their_services() {
        assert_eq!(
            parse("location 3.139 101.6869"),
            Some(Command::Location {
                latitude: 3.139,
                longitude: 101.6869,
                altitude: 0.0
            })
        );
        assert_eq!(
            parse("rotate -90 0 12.5"),
            Some(Command::Rotate([-90.0, 0.0, 12.5]))
        );
        assert_eq!(parse("shake"), Some(Command::Shake));
        assert_eq!(
            parse("shell am force-stop com.example.game"),
            Some(Command::Shell("am force-stop com.example.game".into()))
        );
        assert_eq!(parse("shell"), None);
        assert_eq!(parse("stop"), Some(Command::Stop));
        for line in ["location 1", "location a b", "rotate 1 2", "shake 1"] {
            assert_eq!(parse(line), None, "{line:?}");
        }

        let without = Controller::default();
        let with = Controller {
            location: Some(Arc::default()),
            motion: Some(Arc::default()),
            ..Controller::default()
        };
        let commands =
            "location 3.139 101.6869 60\nlocation 91 0\nrotate 0 0 90\nrotate 0 0 inf\nshake\n";
        let error = |why: &str| copy::ctl_error(&why);
        assert_eq!(
            replies(&with, commands),
            [
                copy::CTL_OK.to_owned(),
                error(copy::CTL_BAD_LOCATION),
                copy::CTL_OK.to_owned(),
                error(copy::CTL_USAGE),
                copy::CTL_OK.to_owned(),
            ]
        );
        assert_eq!(
            replies(&without, "location 1 2\nrotate 0 0 0\nshake\n"),
            [
                error(copy::CTL_NO_LOCATION),
                error(copy::CTL_NO_SENSORS),
                error(copy::CTL_NO_SENSORS)
            ]
        );
    }

    #[test]
    fn every_command_reads_back_from_how_it_is_written() {
        let commands = [
            Command::Tap { x: 1, y: 2 },
            Command::Swipe {
                from: (1, 2),
                to: (3, 4),
                duration: Duration::from_millis(250),
            },
            Command::Touch {
                finger: 3,
                x: 4,
                y: 5,
            },
            Command::Lift { finger: 3 },
            Command::Key {
                code: 30,
                down: None,
            },
            Command::Key {
                code: 30,
                down: Some(false),
            },
            Command::Pad {
                control: input::pad_control("ly").unwrap(),
                value: -0.25,
            },
            Command::Screenrecord {
                path: "clip one.mp4".into(),
                duration: Duration::from_secs(30),
            },
            Command::Install("my game.xapk".into()),
            Command::Screenshot("a b.png".into()),
            Command::Location {
                latitude: -3.5,
                longitude: 101.25,
                altitude: 60.0,
            },
            Command::Walk(Some((45.0, 5.5))),
            Command::Walk(None),
            Command::Route {
                path: "my walk.gpx".into(),
                km_per_hour: 12.5,
            },
            Command::Rotate([-90.0, 0.0, 12.5]),
            Command::Shake,
            Command::Shell("dumpsys window | grep -i rotation".into()),
            Command::Stop,
            Command::Wait(Duration::from_millis(1500)),
            Command::Run {
                path: "my macro.txt".into(),
                times: 3,
            },
            Command::Record("out.txt".into()),
            Command::RecordStop,
            Command::Cancel,
        ];
        for command in commands {
            assert_eq!(parse(&command.to_string()), Some(command));
        }
        let a = input::pad_control("a").unwrap();
        assert_eq!(
            parse("pad a down"),
            Some(Command::Pad {
                control: a,
                value: 1.0
            })
        );
        assert_eq!(
            parse("pad a up"),
            Some(Command::Pad {
                control: a,
                value: 0.0
            })
        );
        assert_eq!(
            parse("run farm.txt"),
            Some(Command::Run {
                path: "farm.txt".into(),
                times: 1
            })
        );
    }

    #[test]
    fn a_recording_plays_back_as_the_same_inputs_with_their_pauses() {
        let controller = controller(None);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("emu-macro-test-{}.txt", std::process::id()));
        let record = format!("record {}\n", path.display());
        let ok = copy::CTL_OK.to_owned();
        let error = |why: &str| copy::ctl_error(&why);
        assert_eq!(
            replies(&controller, "record-stop\n"),
            [error(copy::CTL_NOT_RECORDING)]
        );
        assert_eq!(replies(&controller, &record), std::slice::from_ref(&ok));
        assert_eq!(
            replies(&controller, &record),
            [error(copy::CTL_ALREADY_RECORDING)]
        );
        // The screenshot is not an input and is left out; the failed touch is too.
        let played =
            "touch 0 5 5\nwait 30\nlift 0\nscreenshot x.png\ntouch 0 99 99\nkey 30\nrecord-stop\n";
        let answers = replies(&controller, played);
        assert_eq!(answers[0], ok);
        assert_eq!(answers[4], error(copy::CTL_BAD_TOUCH));
        assert_eq!(answers[6], ok);

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], copy::MACRO_HEADER);
        assert_eq!(lines[1], "touch 0 5 5");
        let pause: u64 = lines[2].strip_prefix("wait ").unwrap().parse().unwrap();
        assert!((30..1000).contains(&pause), "{pause}");
        assert_eq!(lines[3], "lift 0");
        assert!(lines[4].starts_with("wait "));
        assert_eq!(lines[5], "key 30");
        assert_eq!(lines.len(), 6);

        // Played twice, it runs clean; and what it contains reads back as commands.
        let run = format!("run {} 2\n", path.display());
        assert_eq!(replies(&controller, &run), [ok]);
        assert_eq!(read_macro(&path).unwrap().len(), 5);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn macros_that_cannot_be_played_are_refused_and_a_playing_one_can_be_cancelled() {
        let controller = controller(None);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("emu-macro-bad-{}.txt", std::process::id()));
        std::fs::write(&path, "tap 1 2  # fine\n\nrun other.txt\n").unwrap();
        assert_eq!(
            read_macro(&path).err().unwrap(),
            copy::ctl_bad_macro_line(&path, 3)
        );
        std::fs::write(&path, "tap 1 2\nfly away\n").unwrap();
        assert_eq!(
            read_macro(&path).err().unwrap(),
            copy::ctl_bad_macro_line(&path, 2)
        );

        // A long wait ends early when the macro is cancelled from another connection.
        std::fs::write(&path, "wait 60000\n").unwrap();
        let run = format!("run {}\n", path.display());
        thread::scope(|scope| {
            let playing = scope.spawn(|| replies(&controller, &run));
            while !playing.is_finished() {
                controller.cancel.store(true, Ordering::SeqCst);
                thread::sleep(MACRO_CANCEL_POLL);
            }
            assert_eq!(
                playing.join().unwrap(),
                [copy::ctl_error(&copy::CTL_MACRO_CANCELLED)]
            );
        });
        std::fs::remove_file(&path).unwrap();
        assert!(read_macro(&path).is_err());
    }

    #[test]
    fn a_machine_without_input_devices_says_so() {
        let answers = replies(&Controller::default(), "tap 1 2\nkey 30\n");
        assert_eq!(
            answers,
            [
                copy::ctl_error(&copy::CTL_NO_TOUCHSCREEN),
                copy::ctl_error(&copy::CTL_NO_KEYBOARD)
            ]
        );
    }

    #[test]
    fn screenshot_saves_the_newest_frame_once_there_is_one() {
        let screen = Arc::new(Screen::default());
        let controller = controller(Some(Arc::clone(&screen)));
        let path = std::env::temp_dir().join(format!("emu-ctl-test-{}.png", std::process::id()));
        let command = format!("screenshot {}\n", path.display());
        assert_eq!(
            replies(&controller, &command),
            [copy::ctl_error(&copy::CTL_NO_FRAME_YET)]
        );

        const FORMAT_RGBA: u32 = 67;
        screen.present(&Frame {
            width: 2,
            height: 1,
            format: FORMAT_RGBA,
            pixels: &[1, 2, 3, 0, 4, 5, 6, 0],
        });
        assert_eq!(replies(&controller, &command), [copy::CTL_OK]);
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            saved,
            crate::frames::png(&Frame {
                width: 2,
                height: 1,
                format: FORMAT_RGBA,
                pixels: &[1, 2, 3, 0, 4, 5, 6, 0],
            })
            .unwrap()
        );
    }
}

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
//! screenshot <file>
//! ```
//!
//! Positions are display pixels and key codes are Linux key codes. The answer is `ok`, or `error:`
//! followed by the reason. Unix sockets only; a Windows host will need a named pipe here.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use devices::virtio::input::{InputEvent, InputHandle};

use crate::config::{DEFAULT_SWIPE, SWIPE_STEP, TAP_HOLD};
use crate::copy;
use crate::frames::Screen;
use crate::input::{self, TOUCH_SLOTS, Touch};

const WORD_TAP: &str = "tap";
const WORD_SWIPE: &str = "swipe";
const WORD_TOUCH: &str = "touch";
const WORD_LIFT: &str = "lift";
const WORD_KEY: &str = "key";
const WORD_SCREENSHOT: &str = "screenshot";
const WORD_DOWN: &str = "down";
const WORD_UP: &str = "up";

/// The finger `tap` and `swipe` use: the last one, so that they stay clear of `touch 0`, `touch 1`…
const GESTURE_FINGER: u32 = TOUCH_SLOTS - 1;
/// The socket file is created for its owner alone: anyone who can connect controls the guest.
const SOCKET_UMASK: libc::mode_t = 0o177;

#[derive(Debug, PartialEq, Eq)]
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
    Screenshot(PathBuf),
}

/// Reads one command line. `None` if it is not a command in the form the module text gives.
pub fn parse(line: &str) -> Option<Command> {
    let line = line.trim();
    let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let numbers: Option<Vec<u32>> = rest.split_whitespace().map(|n| n.parse().ok()).collect();
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
        (WORD_SCREENSHOT, _) if !rest.is_empty() => Some(Command::Screenshot(rest.into())),
        _ => None,
    }
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

/// The parts of a running machine that commands act on.
#[derive(Default)]
pub struct Controller {
    touch: Option<(InputHandle, Mutex<Touch>)>,
    keyboard: Option<InputHandle>,
    screen: Option<Arc<Screen>>,
    /// Held for the length of a tap or swipe, so that two of them do not share the gesture finger.
    gesture: Mutex<()>,
}

impl Controller {
    pub fn new(
        touch: Option<(InputHandle, Touch)>,
        keyboard: Option<InputHandle>,
        screen: Option<Arc<Screen>>,
    ) -> Self {
        Self {
            touch: touch.map(|(device, state)| (device, Mutex::new(state))),
            keyboard,
            screen,
            gesture: Mutex::default(),
        }
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

    /// Carries out one command. Gestures take as long as they last.
    pub fn execute(&self, command: &Command) -> Result<(), String> {
        match *command {
            Command::Touch { finger, x, y } => self.touch(|touch| touch.place(finger, x, y)),
            Command::Lift { finger } => self.touch(|touch| touch.lift(finger)),
            Command::Tap { x, y } => {
                let _gesture = self.gesture.lock().unwrap_or_else(PoisonError::into_inner);
                self.touch(|touch| touch.place(GESTURE_FINGER, x, y))?;
                thread::sleep(TAP_HOLD);
                self.touch(|touch| touch.lift(GESTURE_FINGER))
            }
            Command::Swipe { from, to, duration } => {
                let _gesture = self.gesture.lock().unwrap_or_else(PoisonError::into_inner);
                let steps =
                    (duration.as_millis() / SWIPE_STEP.as_millis()).clamp(1, u128::from(u32::MAX));
                let steps = steps as u32;
                self.touch(|touch| touch.place(GESTURE_FINGER, from.0, from.1))?;
                let moved = (1..=steps).try_for_each(|step| {
                    thread::sleep(SWIPE_STEP);
                    let (x, y) = along(from, to, step, steps);
                    self.touch(|touch| touch.place(GESTURE_FINGER, x, y))
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
            Command::Screenshot(ref path) => {
                let screen = self.screen.as_ref().ok_or(copy::CTL_NO_DISPLAY)?;
                match screen.save(path) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(copy::CTL_NO_FRAME_YET.to_owned()),
                    Err(err) => Err(copy::file(path, &err)),
                }
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
pub fn request(socket: &Path, command: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    writeln!(stream, "{command}")?;
    let mut reply = String::new();
    if BufReader::new(stream).read_line(&mut reply)? == 0 {
        return Err(std::io::Error::new(
            ErrorKind::UnexpectedEof,
            copy::CTL_NO_REPLY,
        ));
    }
    Ok(reply.trim_end().to_owned())
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
        Controller::new(Some((touch, Touch::new(WIDTH, HEIGHT))), Some(keys), screen)
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
            "screenshot",
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

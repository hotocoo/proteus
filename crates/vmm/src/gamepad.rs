//! Host gamepads: read with `gilrs` and passed to the guest's gamepad as control commands.
//!
//! Every host pad drives the one gamepad the guest has.
//! shortcut: all pads share one guest pad, upgrade to one guest pad per host pad when two-player
//! games need it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Instant;

use gilrs::{Axis, Button, EventType, Gilrs};

use crate::config::GAMEPAD_SETTLE;
use crate::copy;
use crate::ctl::{Command, Controller};
use crate::input::{self, PadControl};

fn control(name: &str) -> PadControl {
    // The names below are all in `input::PAD_CONTROLS`; the test checks that.
    input::pad_control(name).unwrap_or(input::PAD_CONTROLS[0])
}

fn button_name(button: Button) -> Option<&'static str> {
    Some(match button {
        Button::South => "a",
        Button::East => "b",
        Button::West => "x",
        Button::North => "y",
        Button::LeftTrigger => "l1",
        Button::RightTrigger => "r1",
        Button::LeftTrigger2 => "l2",
        Button::RightTrigger2 => "r2",
        Button::Select => "select",
        Button::Start => "start",
        Button::Mode => "mode",
        Button::LeftThumb => "thumbl",
        Button::RightThumb => "thumbr",
        _ => return None,
    })
}

/// The direction pad as buttons: which axis, and which way.
fn dpad(button: Button) -> Option<(&'static str, f64)> {
    Some(match button {
        Button::DPadUp => ("haty", -1.0),
        Button::DPadDown => ("haty", 1.0),
        Button::DPadLeft => ("hatx", -1.0),
        Button::DPadRight => ("hatx", 1.0),
        _ => return None,
    })
}

/// The parts of a host pad event the guest cares about.
#[derive(Clone, Copy, Debug)]
enum Moved {
    /// Pressed or released.
    Button(Button, bool),
    /// How far an analogue button is pushed, 0 to 1.
    Pushed(Button, f32),
    Axis(Axis, f32),
}

fn moved(event: EventType) -> Option<Moved> {
    Some(match event {
        EventType::ButtonPressed(button, _) => Moved::Button(button, true),
        EventType::ButtonReleased(button, _) => Moved::Button(button, false),
        EventType::ButtonChanged(button, value, _) => Moved::Pushed(button, value),
        EventType::AxisChanged(axis, value, _) => Moved::Axis(axis, value),
        _ => return None,
    })
}

/// What a host pad event means to the guest's gamepad.
pub fn translate(event: EventType) -> Vec<Command> {
    moved(event).map(commands).unwrap_or_default()
}

/// gilrs counts up as positive on the sticks; Linux counts down as positive, so vertical axes turn
/// over.
fn commands(moved: Moved) -> Vec<Command> {
    let pad = |name: &str, value: f64| Command::Pad {
        control: control(name),
        value,
    };
    match moved {
        Moved::Button(button, pressed) => {
            if let Some((axis, way)) = dpad(button) {
                return vec![pad(axis, if pressed { way } else { 0.0 })];
            }
            let name = button_name(button);
            name.map(|name| pad(name, f64::from(u8::from(pressed))))
                .into_iter()
                .collect()
        }
        // How far an analogue trigger is pulled; its press and release come as buttons too.
        Moved::Pushed(Button::LeftTrigger2, value) => vec![pad("lt", value.into())],
        Moved::Pushed(Button::RightTrigger2, value) => vec![pad("rt", value.into())],
        Moved::Pushed(..) => Vec::new(),
        Moved::Axis(axis, value) => {
            let value = f64::from(value);
            let moved = match axis {
                Axis::LeftStickX => pad("lx", value),
                Axis::LeftStickY => pad("ly", -value),
                Axis::RightStickX => pad("rx", value),
                Axis::RightStickY => pad("ry", -value),
                Axis::LeftZ => pad("lt", value),
                Axis::RightZ => pad("rt", value),
                Axis::DPadX => pad("hatx", value),
                Axis::DPadY => pad("haty", -value),
                Axis::Unknown => return Vec::new(),
            };
            vec![moved]
        }
    }
}

/// The controls last sent away from neutral, by name.
type Held = HashMap<&'static str, PadControl>;

/// What the guest's pad should get for a host event: the controls it stands for, or, when a pad
/// goes away, neutral for everything still held, since its releases will never come.
fn forward(held: &mut Held, event: EventType) -> Vec<Command> {
    if matches!(event, EventType::Disconnected | EventType::Dropped) {
        return held
            .drain()
            .map(|(_, control)| Command::Pad {
                control,
                value: 0.0,
            })
            .collect();
    }
    let commands = translate(event);
    for command in &commands {
        if let Command::Pad { control, value } = command {
            if *value == 0.0 {
                held.remove(control.name);
            } else {
                held.insert(control.name, *control);
            }
        }
    }
    commands
}

/// Watches the host's gamepads on a thread of its own.
pub struct HostPads {
    controller: mpsc::Sender<Arc<Controller>>,
}

impl HostPads {
    /// Starts watching. The events are held until `forward_to` says where they go. The receiver
    /// says, once it is known, whether a pad was plugged in at start.
    pub fn watch() -> std::io::Result<(Self, mpsc::Receiver<bool>)> {
        let (found, connected) = mpsc::channel();
        let (controller, target) = mpsc::channel::<Arc<Controller>>();
        std::thread::Builder::new()
            .name(copy::GAMEPAD_THREAD.into())
            .spawn(move || {
                // gilrs's handle stays on the thread that made it.
                let mut pads = match Gilrs::new() {
                    Ok(pads) => pads,
                    Err(err) => {
                        log::warn!("{}", copy::gamepads_unavailable(&err));
                        let _ = found.send(false);
                        return;
                    }
                };
                // Pads already plugged in are announced as connection events from another
                // thread, a moment after start, so they are waited for before counting; the first
                // one settles it.
                let settled = Instant::now() + GAMEPAD_SETTLE;
                while let Some(left) = settled.checked_duration_since(Instant::now()) {
                    match pads.next_event_blocking(Some(left)) {
                        Some(event) if event.event == EventType::Connected => break,
                        Some(_) => {}
                        None => break,
                    }
                }
                let _ = found.send(pads.gamepads().next().is_some());
                let Ok(controller) = target.recv() else {
                    return;
                };
                let mut held = Held::new();
                while let Some(event) = pads.next_event_blocking(None) {
                    for command in forward(&mut held, event.event) {
                        if let Err(why) = controller.execute(&command) {
                            log::debug!("{}", copy::window_input_dropped(&why));
                        }
                    }
                }
            })?;
        Ok((Self { controller }, connected))
    }

    /// Sends what the pads do to `controller` from now on.
    pub fn forward_to(self, controller: &Arc<Controller>) {
        // A watcher that already gave up has no use for it.
        let _ = self.controller.send(Arc::clone(controller));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(moved: Moved) -> Vec<(&'static str, f64)> {
        let commands = commands(moved);
        let pairs = commands.into_iter().map(|command| match command {
            Command::Pad { control, value } => (control.name, value),
            other => panic!("{other:?}"),
        });
        pairs.collect()
    }

    #[test]
    fn host_pad_events_become_the_guest_pad_controls_they_stand_for() {
        let all = [
            Button::South,
            Button::East,
            Button::West,
            Button::North,
            Button::LeftTrigger,
            Button::RightTrigger,
            Button::LeftTrigger2,
            Button::RightTrigger2,
            Button::Select,
            Button::Start,
            Button::Mode,
            Button::LeftThumb,
            Button::RightThumb,
        ];
        for button in all {
            let name = button_name(button).unwrap();
            assert_eq!(input::pad_control(name).map(|c| c.name), Some(name));
        }
        assert_eq!(values(Moved::Button(Button::South, true)), [("a", 1.0)]);
        assert_eq!(values(Moved::Button(Button::North, false)), [("y", 0.0)]);
        assert_eq!(
            values(Moved::Button(Button::DPadUp, true)),
            [("haty", -1.0)]
        );
        assert_eq!(
            values(Moved::Button(Button::DPadLeft, false)),
            [("hatx", 0.0)]
        );
        assert_eq!(
            values(Moved::Pushed(Button::RightTrigger2, 0.5)),
            [("rt", 0.5)]
        );
        // Pushed up on the host is negative on the guest.
        assert_eq!(values(Moved::Axis(Axis::LeftStickY, 1.0)), [("ly", -1.0)]);
        assert_eq!(values(Moved::Axis(Axis::RightStickX, -0.5)), [("rx", -0.5)]);
        assert!(values(Moved::Axis(Axis::Unknown, 1.0)).is_empty());
        assert!(values(Moved::Button(Button::C, true)).is_empty());
        assert!(translate(EventType::Connected).is_empty());
    }

    #[test]
    fn a_pad_that_goes_away_lets_go_of_what_it_held() {
        let mut held = Held::new();
        for button in [Button::South, Button::DPadUp] {
            for command in commands(Moved::Button(button, true)) {
                if let Command::Pad { control, .. } = command {
                    held.insert(control.name, control);
                }
            }
        }
        let mut neutral: Vec<_> = forward(&mut held, EventType::Disconnected)
            .into_iter()
            .map(|command| match command {
                Command::Pad { control, value } => (control.name, value),
                other => panic!("{other:?}"),
            })
            .collect();
        neutral.sort_by_key(|(name, _)| *name);
        assert_eq!(neutral, [("a", 0.0), ("haty", 0.0)]);
        assert!(held.is_empty());
    }
}

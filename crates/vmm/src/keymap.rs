//! Key maps: which keys and mouse movements stand for which touches on the guest's screen.
//!
//! A key map is a TOML file. Positions are pixels of the guest's display, and keys are named as in
//! `input::KEY_NAMES` ("KeyW", "Space", "ShiftLeft"), with "MouseLeft", "MouseRight" and
//! "MouseMiddle" for the mouse buttons.
//!
//! ```toml
//! toggle = "F12"            # switches the whole map off and on
//!
//! [[tap]]                   # a finger that is down while the key is
//! key = "Space"
//! at = [1100, 600]
//!
//! [[stick]]                 # four keys push one finger away from a centre
//! up = "KeyW"
//! left = "KeyA"
//! down = "KeyS"
//! right = "KeyD"
//! center = [200, 550]
//! radius = 100
//!
//! [look]                    # a key hands the mouse to a finger that drags the view around
//! key = "Backquote"
//! center = [900, 300]
//! radius = 250
//! sensitivity = 1.0
//!
//! [zoom]                    # each turn of the mouse wheel is a pinch of two fingers
//! center = [640, 360]
//! spread = 120              # how far from the centre each finger lands
//! step = 60                 # how far each finger moves per turn
//! ```
//!
//! The map is a plain state machine from key and pointer events to touches, so that it can be
//! tested without a window or a guest.

use serde::Deserialize;

use crate::copy;
use crate::input::{TOUCH_SLOTS, key_or_button_code};

/// Fingers the map may use. The first one is the mouse's and the last one the control socket's.
const FIRST_FINGER: u32 = 1;
const LAST_FINGER: u32 = TOUCH_SLOTS - 2;
const DEFAULT_SENSITIVITY: f64 = 1.0;
/// Moves each finger of a pinch makes between landing and lifting; a pinch that jumps in one move
/// is not always read as one.
const ZOOM_MOVES: u32 = 4;
/// Scrolling by pixels (a trackpad) this far is one turn of a wheel.
const ZOOM_PIXELS_PER_TURN: f64 = 40.0;

/// One thing the map does to the guest's touchscreen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touch {
    /// Put the finger down at a pixel, or move it there.
    Place {
        finger: u32,
        x: u32,
        y: u32,
    },
    Lift {
        finger: u32,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    toggle: Option<String>,
    #[serde(default)]
    tap: Vec<TapEntry>,
    #[serde(default)]
    stick: Vec<StickEntry>,
    look: Option<LookEntry>,
    zoom: Option<ZoomEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ZoomEntry {
    center: [u32; 2],
    spread: u32,
    step: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TapEntry {
    key: String,
    at: [u32; 2],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StickEntry {
    up: String,
    down: String,
    left: String,
    right: String,
    center: [u32; 2],
    radius: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LookEntry {
    key: String,
    center: [u32; 2],
    radius: u32,
    sensitivity: Option<f64>,
}

struct Tap {
    key: u16,
    at: (u32, u32),
    finger: u32,
}

struct Stick {
    /// Up, down, left, right.
    keys: [u16; 4],
    held: [bool; 4],
    center: (u32, u32),
    radius: u32,
    finger: u32,
}

struct Look {
    key: u16,
    center: (u32, u32),
    radius: u32,
    sensitivity: f64,
    finger: u32,
    /// Where the finger is while the mouse is the map's; `None` while it is not.
    at: Option<(f64, f64)>,
}

struct Zoom {
    center: (u32, u32),
    spread: u32,
    step: u32,
    fingers: [u32; 2],
    /// Pixels scrolled that do not yet add up to a turn.
    pixels: f64,
}

/// How far the mouse wheel turned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scroll {
    /// Turns of a wheel; positive is away from the user.
    Turns(f64),
    /// Pixels, from a trackpad; positive is away from the user.
    Pixels(f64),
}

/// A key map, and what it is doing right now.
pub struct Mapper {
    display: (u32, u32),
    toggle: Option<u16>,
    enabled: bool,
    taps: Vec<Tap>,
    /// Which taps have their finger down.
    tapping: Vec<bool>,
    sticks: Vec<Stick>,
    look: Option<Look>,
    zoom: Option<Zoom>,
}

fn key(name: &str, used: &mut Vec<u16>) -> Result<u16, String> {
    let code = key_or_button_code(name).ok_or_else(|| copy::keymap_unknown_key(name))?;
    if used.contains(&code) {
        return Err(copy::keymap_key_twice(name));
    }
    used.push(code);
    Ok(code)
}

fn position(at: [u32; 2], display: (u32, u32)) -> Result<(u32, u32), String> {
    if at[0] < display.0 && at[1] < display.1 {
        Ok((at[0], at[1]))
    } else {
        Err(copy::keymap_off_screen(at[0], at[1]))
    }
}

/// A circle that has to fit on the screen, so that a finger moved to its edge is still on it.
fn circle(center: [u32; 2], radius: u32, display: (u32, u32)) -> Result<(u32, u32), String> {
    let (x, y) = position(center, display)?;
    let fits = radius > 0
        && x >= radius
        && y >= radius
        && x + radius < display.0
        && y + radius < display.1;
    if fits {
        Ok((x, y))
    } else {
        Err(copy::keymap_circle_off_screen(x, y, radius))
    }
}

impl Mapper {
    /// Reads a key map for a guest display of `display` pixels.
    pub fn parse(text: &str, display: (u32, u32)) -> Result<Self, String> {
        let file: File = toml::from_str(text).map_err(|err| err.to_string())?;
        let mut used = Vec::new();
        let mut fingers = FIRST_FINGER..=LAST_FINGER;
        let mut finger = || fingers.next().ok_or(copy::KEYMAP_TOO_MANY.to_owned());
        let toggle = file.toggle.as_deref().map(|name| key(name, &mut used));
        let toggle = toggle.transpose()?;
        let mut taps = Vec::new();
        for entry in &file.tap {
            taps.push(Tap {
                key: key(&entry.key, &mut used)?,
                at: position(entry.at, display)?,
                finger: finger()?,
            });
        }
        let mut sticks = Vec::new();
        for entry in &file.stick {
            let names = [&entry.up, &entry.down, &entry.left, &entry.right];
            let mut keys = [0; 4];
            for (slot, name) in keys.iter_mut().zip(names) {
                *slot = key(name, &mut used)?;
            }
            sticks.push(Stick {
                keys,
                held: [false; 4],
                center: circle(entry.center, entry.radius, display)?,
                radius: entry.radius,
                finger: finger()?,
            });
        }
        let look = match &file.look {
            Some(entry) => {
                let sensitivity = entry.sensitivity.unwrap_or(DEFAULT_SENSITIVITY);
                if !sensitivity.is_finite() || sensitivity <= 0.0 {
                    return Err(copy::KEYMAP_BAD_SENSITIVITY.to_owned());
                }
                Some(Look {
                    key: key(&entry.key, &mut used)?,
                    center: circle(entry.center, entry.radius, display)?,
                    radius: entry.radius,
                    sensitivity,
                    finger: finger()?,
                    at: None,
                })
            }
            None => None,
        };
        let zoom = match &file.zoom {
            Some(entry) => {
                // Zooming out moves the fingers in by a step, so they must not meet or cross.
                if entry.step == 0 || entry.step >= entry.spread {
                    return Err(copy::KEYMAP_BAD_ZOOM.to_owned());
                }
                Some(Zoom {
                    center: circle(entry.center, entry.spread + entry.step, display)?,
                    spread: entry.spread,
                    step: entry.step,
                    fingers: [finger()?, finger()?],
                    pixels: 0.0,
                })
            }
            None => None,
        };
        Ok(Self {
            display,
            toggle,
            enabled: true,
            tapping: vec![false; taps.len()],
            taps,
            sticks,
            look,
            zoom,
        })
    }

    /// The mouse wheel turned. Each whole turn away from the user is a pinch that zooms in, each
    /// turn towards the user one that zooms out. Without a `[zoom]` the wheel does nothing.
    pub fn scroll(&mut self, scroll: Scroll) -> Vec<Touch> {
        let Some(zoom) = self.zoom.as_mut().filter(|_| self.enabled) else {
            return Vec::new();
        };
        let turns = match scroll {
            Scroll::Turns(turns) => turns.trunc(),
            Scroll::Pixels(pixels) => {
                zoom.pixels += pixels;
                let turns = (zoom.pixels / ZOOM_PIXELS_PER_TURN).trunc();
                zoom.pixels -= turns * ZOOM_PIXELS_PER_TURN;
                turns
            }
        };
        let outwards = turns > 0.0;
        let mut touches = Vec::new();
        for _ in 0..turns.abs() as u32 {
            touches.extend(zoom.pinch(outwards));
        }
        touches
    }

    /// Whether the mouse belongs to the map right now (it is dragging the view). The window should
    /// then keep the pointer and report its movement instead of its position.
    pub fn wants_pointer(&self) -> bool {
        self.look.as_ref().is_some_and(|look| look.at.is_some())
    }

    /// Lifts every finger the map has down.
    fn release(&mut self) -> Vec<Touch> {
        let mut touches = Vec::new();
        for (tap, tapping) in self.taps.iter().zip(&mut self.tapping) {
            if std::mem::take(tapping) {
                touches.push(Touch::Lift { finger: tap.finger });
            }
        }
        for stick in &mut self.sticks {
            if std::mem::take(&mut stick.held).contains(&true) {
                touches.push(Touch::Lift {
                    finger: stick.finger,
                });
            }
        }
        if let Some(look) = &mut self.look
            && look.at.take().is_some()
        {
            touches.push(Touch::Lift {
                finger: look.finger,
            });
        }
        touches
    }

    /// A key or mouse button went down or up. `None` means the map has no use for it and the guest
    /// should get it as a key; otherwise these are the touches it stands for, possibly none.
    pub fn key(&mut self, code: u16, down: bool) -> Option<Vec<Touch>> {
        if self.toggle == Some(code) {
            if down {
                self.enabled = !self.enabled;
            }
            return Some(if self.enabled {
                Vec::new()
            } else {
                self.release()
            });
        }
        if !self.enabled {
            return None;
        }
        if let Some(index) = self.taps.iter().position(|tap| tap.key == code) {
            let tap = &self.taps[index];
            let was = std::mem::replace(&mut self.tapping[index], down);
            return Some(match (was, down) {
                (false, true) => vec![Touch::Place {
                    finger: tap.finger,
                    x: tap.at.0,
                    y: tap.at.1,
                }],
                (true, false) => vec![Touch::Lift { finger: tap.finger }],
                _ => Vec::new(),
            });
        }
        if let Some(stick) = self
            .sticks
            .iter_mut()
            .find(|stick| stick.keys.contains(&code))
        {
            return Some(stick.key(code, down));
        }
        let look = self.look.as_mut().filter(|look| look.key == code)?;
        if !down {
            return Some(Vec::new());
        }
        // The key switches between the mouse dragging the view and the mouse being a pointer.
        Some(match look.at.take() {
            Some(_) => vec![Touch::Lift {
                finger: look.finger,
            }],
            None => {
                look.at = Some((f64::from(look.center.0), f64::from(look.center.1)));
                vec![Touch::Place {
                    finger: look.finger,
                    x: look.center.0,
                    y: look.center.1,
                }]
            }
        })
    }

    /// The mouse moved by this much while it belongs to the map.
    pub fn pointer(&mut self, dx: f64, dy: f64) -> Vec<Touch> {
        let Some(look) = &mut self.look else {
            return Vec::new();
        };
        let Some((x, y)) = look.at else {
            return Vec::new();
        };
        let (center_x, center_y) = (f64::from(look.center.0), f64::from(look.center.1));
        let (x, y) = (x + dx * look.sensitivity, y + dy * look.sensitivity);
        let (off_x, off_y) = (x - center_x, y - center_y);
        let finger = look.finger;
        let place = |x: f64, y: f64| Touch::Place {
            finger,
            x: x.round() as u32,
            y: y.round() as u32,
        };
        // A finger cannot drag past the edge of its circle; it is lifted and starts again from
        // the middle, carrying on in the same direction, so the view can turn without end.
        if off_x.hypot(off_y) > f64::from(look.radius) {
            let restart = (
                center_x + dx * look.sensitivity,
                center_y + dy * look.sensitivity,
            );
            let inside =
                (restart.0 - center_x).hypot(restart.1 - center_y) <= f64::from(look.radius);
            let restart = if inside {
                restart
            } else {
                (center_x, center_y)
            };
            look.at = Some(restart);
            let mut touches = vec![Touch::Lift { finger }, place(center_x, center_y)];
            if restart != (center_x, center_y) {
                touches.push(place(restart.0, restart.1));
            }
            return touches;
        }
        look.at = Some((x, y));
        debug_assert!(x >= 0.0 && y >= 0.0 && x < f64::from(self.display.0));
        vec![place(x, y)]
    }
}

impl Zoom {
    /// Two fingers on either side of the centre, moved apart or together by a step, then lifted.
    fn pinch(&self, outwards: bool) -> Vec<Touch> {
        let (x, y) = self.center;
        let [left, right] = self.fingers;
        let mut touches = Vec::new();
        for moved in 0..=ZOOM_MOVES {
            let travel = self.step * moved / ZOOM_MOVES;
            let reach = if outwards {
                self.spread + travel
            } else {
                self.spread - travel
            };
            touches.push(Touch::Place {
                finger: left,
                x: x - reach,
                y,
            });
            touches.push(Touch::Place {
                finger: right,
                x: x + reach,
                y,
            });
        }
        touches.push(Touch::Lift { finger: left });
        touches.push(Touch::Lift { finger: right });
        touches
    }
}

impl Stick {
    /// Where the finger goes for the keys that are held, or `None` if none is (or they cancel out).
    fn target(&self) -> Option<(u32, u32)> {
        let [up, down, left, right] = self.held.map(i32::from);
        let (x, y) = (right - left, down - up);
        if (x, y) == (0, 0) {
            return None;
        }
        // A diagonal is as long as a straight push, not longer.
        let reach = f64::from(self.radius) / f64::from(x * x + y * y).sqrt();
        let offset = |unit: i32| (f64::from(unit) * reach).round() as i64;
        Some((
            (i64::from(self.center.0) + offset(x)) as u32,
            (i64::from(self.center.1) + offset(y)) as u32,
        ))
    }

    fn key(&mut self, code: u16, down: bool) -> Vec<Touch> {
        let before = self.target();
        for (key, held) in self.keys.iter().zip(&mut self.held) {
            if *key == code {
                *held = down;
            }
        }
        let finger = self.finger;
        let place = |(x, y): (u32, u32)| Touch::Place { finger, x, y };
        match (before, self.target()) {
            (before, after) if before == after => Vec::new(),
            // The finger lands in the middle and is pushed out from there, as a thumb would.
            (None, Some(after)) => vec![place(self.center), place(after)],
            (Some(_), Some(after)) => vec![place(after)],
            (_, None) => vec![Touch::Lift { finger }],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: (u32, u32) = (1280, 720);
    const MAP: &str = r#"
        toggle = "F12"

        [[tap]]
        key = "Space"
        at = [1100, 600]

        [[tap]]
        key = "MouseLeft"
        at = [1000, 500]

        [[stick]]
        up = "KeyW"
        left = "KeyA"
        down = "KeyS"
        right = "KeyD"
        center = [200, 550]
        radius = 100

        [look]
        key = "Backquote"
        center = [900, 300]
        radius = 250

        [zoom]
        center = [640, 360]
        spread = 120
        step = 60
    "#;

    fn code(name: &str) -> u16 {
        key_or_button_code(name).unwrap()
    }

    fn mapper() -> Mapper {
        Mapper::parse(MAP, DISPLAY).unwrap()
    }

    fn place(finger: u32, x: u32, y: u32) -> Touch {
        Touch::Place { finger, x, y }
    }

    #[test]
    fn the_example_key_map_in_the_repository_is_a_valid_one() {
        let example = include_str!("../../../assets/keymaps/example.toml");
        let mut map = Mapper::parse(example, DISPLAY).unwrap();
        assert!(map.key(code("KeyW"), true).is_some());
        assert!(map.key(code("MouseLeft"), true).is_some());
    }

    #[test]
    fn tap_is_a_finger_that_is_down_while_the_key_is() {
        let mut map = mapper();
        assert_eq!(
            map.key(code("Space"), true),
            Some(vec![place(1, 1100, 600)])
        );
        // A repeat of "down" changes nothing.
        assert_eq!(map.key(code("Space"), true), Some(vec![]));
        assert_eq!(
            map.key(code("Space"), false),
            Some(vec![Touch::Lift { finger: 1 }])
        );
        // A mouse button can be bound like a key.
        assert_eq!(
            map.key(code("MouseLeft"), true),
            Some(vec![place(2, 1000, 500)])
        );
        // A key the map does not know goes to the guest.
        assert_eq!(map.key(code("KeyQ"), true), None);
    }

    #[test]
    fn stick_lands_in_the_middle_and_follows_the_keys_held() {
        let mut map = mapper();
        let (w, a, d) = (code("KeyW"), code("KeyA"), code("KeyD"));
        // Up: down in the middle, then pushed up by the radius.
        assert_eq!(
            map.key(w, true),
            Some(vec![place(3, 200, 550), place(3, 200, 450)])
        );
        // Up and left: a diagonal of the same length (100 / sqrt 2 is about 71).
        assert_eq!(map.key(a, true), Some(vec![place(3, 129, 479)]));
        // Left and right cancel out, leaving up.
        assert_eq!(map.key(d, true), Some(vec![place(3, 200, 450)]));
        assert_eq!(map.key(a, false), Some(vec![place(3, 271, 479)]));
        assert_eq!(map.key(d, false), Some(vec![place(3, 200, 450)]));
        assert_eq!(map.key(w, false), Some(vec![Touch::Lift { finger: 3 }]));
        // Opposite keys alone are no push at all.
        assert_eq!(
            map.key(a, true),
            Some(vec![place(3, 200, 550), place(3, 100, 550)])
        );
        assert_eq!(map.key(d, true), Some(vec![Touch::Lift { finger: 3 }]));
    }

    #[test]
    fn look_key_gives_the_mouse_to_a_finger_that_drags_from_the_centre() {
        let mut map = mapper();
        let look = code("Backquote");
        assert!(!map.wants_pointer());
        assert!(map.pointer(5.0, 5.0).is_empty());
        assert_eq!(map.key(look, true), Some(vec![place(4, 900, 300)]));
        assert_eq!(map.key(look, false), Some(vec![]));
        assert!(map.wants_pointer());
        assert_eq!(map.pointer(10.0, -4.0), vec![place(4, 910, 296)]);
        assert_eq!(map.pointer(0.4, 0.4), vec![place(4, 910, 296)]);
        // Past the edge of the circle: lifted, put back in the middle, and moved on from there.
        assert_eq!(
            map.pointer(245.0, 0.0),
            vec![
                Touch::Lift { finger: 4 },
                place(4, 900, 300),
                place(4, 1145, 300)
            ]
        );
        // One movement larger than the whole circle just starts again from the middle.
        assert_eq!(
            map.pointer(400.0, 0.0),
            vec![Touch::Lift { finger: 4 }, place(4, 900, 300)]
        );
        assert_eq!(map.key(look, true), Some(vec![Touch::Lift { finger: 4 }]));
        assert!(!map.wants_pointer());
    }

    #[test]
    fn each_wheel_turn_is_a_pinch_out_or_in_and_trackpad_pixels_add_up_to_turns() {
        let mut map = mapper();
        let pinch = map.scroll(Scroll::Turns(1.0));
        // Down at the spread, moved out by the step, lifted.
        assert_eq!(pinch.len(), 2 * (ZOOM_MOVES as usize + 1) + 2);
        assert_eq!(&pinch[..2], &[place(5, 520, 360), place(6, 760, 360)]);
        let last_move = 2 * ZOOM_MOVES as usize;
        assert_eq!(
            &pinch[last_move..],
            &[
                place(5, 460, 360),
                place(6, 820, 360),
                Touch::Lift { finger: 5 },
                Touch::Lift { finger: 6 }
            ]
        );
        let pinch = map.scroll(Scroll::Turns(-2.0));
        assert_eq!(pinch.len(), 2 * (2 * (ZOOM_MOVES as usize + 1) + 2));
        assert_eq!(
            &pinch[last_move..last_move + 2],
            &[place(5, 580, 360), place(6, 700, 360)]
        );
        // A part of a turn is nothing.
        assert!(map.scroll(Scroll::Turns(0.5)).is_empty());
        // Trackpad pixels count up until they make a turn, and the rest carries over.
        assert!(map.scroll(Scroll::Pixels(30.0)).is_empty());
        assert_eq!(map.scroll(Scroll::Pixels(30.0)).len(), pinch.len() / 2);
        assert_eq!(map.scroll(Scroll::Pixels(20.0)).len(), pinch.len() / 2);
        // Switched off, the wheel does nothing.
        map.key(code("F12"), true);
        assert!(map.scroll(Scroll::Turns(1.0)).is_empty());
    }

    #[test]
    fn toggle_lifts_every_finger_and_passes_keys_through_until_toggled_back() {
        let mut map = mapper();
        let toggle = code("F12");
        map.key(code("Space"), true);
        map.key(code("KeyW"), true);
        map.key(code("Backquote"), true);
        assert_eq!(
            map.key(toggle, true),
            Some(vec![
                Touch::Lift { finger: 1 },
                Touch::Lift { finger: 3 },
                Touch::Lift { finger: 4 }
            ])
        );
        assert!(!map.wants_pointer());
        assert_eq!(map.key(toggle, false), Some(vec![]));
        // Off: the map's keys are ordinary keys.
        assert_eq!(map.key(code("Space"), true), None);
        assert_eq!(map.key(code("KeyW"), false), None);
        assert_eq!(map.key(toggle, true), Some(vec![]));
        assert_eq!(
            map.key(code("Space"), true),
            Some(vec![place(1, 1100, 600)])
        );
    }

    #[test]
    fn maps_that_cannot_work_are_refused_with_the_reason() {
        let refused = |text: &str| Mapper::parse(text, DISPLAY).err().unwrap();
        assert_eq!(
            refused("[[tap]]\nkey = \"Spacebar\"\nat = [1, 1]"),
            copy::keymap_unknown_key("Spacebar")
        );
        assert_eq!(
            refused("toggle = \"F1\"\n[[tap]]\nkey = \"F1\"\nat = [1, 1]"),
            copy::keymap_key_twice("F1")
        );
        assert_eq!(
            refused("[[tap]]\nkey = \"F1\"\nat = [1280, 1]"),
            copy::keymap_off_screen(1280, 1)
        );
        let stick =
            "[[stick]]\nup = \"KeyW\"\ndown = \"KeyS\"\nleft = \"KeyA\"\nright = \"KeyD\"\n";
        assert_eq!(
            refused(&format!("{stick}center = [50, 300]\nradius = 100")),
            copy::keymap_circle_off_screen(50, 300, 100)
        );
        assert_eq!(
            refused("[look]\nkey = \"F1\"\ncenter = [600, 300]\nradius = 100\nsensitivity = 0"),
            copy::KEYMAP_BAD_SENSITIVITY
        );
        let zoom = "[zoom]\ncenter = [640, 360]\n";
        assert_eq!(
            refused(&format!("{zoom}spread = 50\nstep = 50")),
            copy::KEYMAP_BAD_ZOOM
        );
        assert_eq!(
            refused(&format!("{zoom}spread = 300\nstep = 100")),
            copy::keymap_circle_off_screen(640, 360, 400)
        );
        // More bindings than there are fingers.
        let keys = ["F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9"];
        let many: String = keys
            .iter()
            .map(|key| format!("[[tap]]\nkey = \"{key}\"\nat = [1, 1]\n"))
            .collect();
        assert_eq!(refused(&many), copy::KEYMAP_TOO_MANY);
        // A misspelt setting is an error, not something silently ignored.
        assert!(refused("togle = \"F1\"").contains("togle"));
        // An empty map is a map that does nothing.
        assert!(Mapper::parse("", DISPLAY).is_ok());
    }
}

//! Key maps: which keys and mouse movements stand for which touches on the guest's screen.
//!
//! A key map is a TOML file. Positions are pixels of the guest's display, and keys are named as in
//! `input::KEY_NAMES` ("KeyW", "Space", "ShiftLeft"), with "MouseLeft", "MouseRight" and
//! "MouseMiddle" for the mouse buttons.
//!
//! ```toml
//! size = [1280, 720]        # optional: the screen it was made for; refused on any other
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
//! start = true              # optional: the mouse turns the view from the start, as on a PC
//! hold = true               # optional: the key frees the pointer only while held (clicks
//!                           # then reach the screen instead of the map's mouse buttons)
//!
//! [walk]                    # four keys walk the device's location (set one first)
//! up = "ArrowUp"
//! down = "ArrowDown"
//! left = "ArrowLeft"
//! right = "ArrowRight"
//! speed = 5                 # km/h
//!
//! [tilt]                    # four keys tilt the device while they are held
//! up = "KeyI"               # top edge away from the player
//! down = "KeyK"
//! left = "KeyJ"             # turned like a steering wheel
//! right = "KeyL"
//! angle = 30                # degrees
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
use crate::input::{TOUCH_SLOTS, button_code, key_or_button_code};

/// Fingers the map may use. The first one is the mouse's and the last one the control socket's.
const FIRST_FINGER: u32 = 1;
const LAST_FINGER: u32 = TOUCH_SLOTS - 2;
const DEFAULT_SENSITIVITY: f64 = 1.0;
/// Longest move the look finger makes in one go, as a share of its circle's radius.
const LOOK_STEP_SHARE: f64 = 0.5;
/// Most moves one mouse report is split into; beyond that each move is cut to the longest.
const LOOK_MAX_STEPS: f64 = 32.0;
const FULL_TURN_DEGREES: f64 = 360.0;
/// Furthest a tilt key may turn the device: on its side, or flat.
const MAX_TILT_DEGREES: i16 = 90;
/// Moves each finger of a pinch makes between landing and lifting; a pinch that jumps in one move
/// is not always read as one.
const ZOOM_MOVES: u32 = 4;
/// Scrolling by pixels (a trackpad) this far is one turn of a wheel.
const ZOOM_PIXELS_PER_TURN: f64 = 40.0;
/// Most wheel turns one scroll event acts on; a huge line count must not make a huge list.
const ZOOM_MAX_TURNS: f64 = 10.0;

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
    /// Hold the device turned by these degrees about its x axis (across the screen) and its z
    /// axis (out of the screen); 0 and 0 is upright.
    Tilt {
        x: i16,
        z: i16,
    },
    /// Walk the location this many degrees from north (up on the screen is north), or stop.
    Walk {
        bearing: Option<u16>,
        km_per_hour: u16,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    /// The screen the positions were measured on; a map for another size is refused.
    size: Option<[u32; 2]>,
    toggle: Option<String>,
    #[serde(default)]
    tap: Vec<TapEntry>,
    #[serde(default)]
    stick: Vec<StickEntry>,
    look: Option<LookEntry>,
    zoom: Option<ZoomEntry>,
    walk: Option<WalkEntry>,
    tilt: Option<TiltEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TiltEntry {
    up: String,
    down: String,
    left: String,
    right: String,
    angle: i16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WalkEntry {
    up: String,
    down: String,
    left: String,
    right: String,
    speed: u16,
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
    /// Also frees the pointer (a menu opens) or gives it back to the view (it closes).
    #[serde(default)]
    pointer: bool,
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
    /// The mouse turns the view as soon as the map is loaded, as in PC games.
    #[serde(default)]
    start: bool,
    /// The key frees the pointer only while it is held, instead of switching.
    #[serde(default)]
    hold: bool,
}

/// Whether `code` is a mouse button rather than a key.
fn is_button(code: u16) -> bool {
    (0..).map_while(button_code).any(|button| button == code)
}

/// A tap borrows a finger while its key is down, so a map may bind more keys than the
/// touchscreen has fingers, as long as not all of them are held at once.
struct Tap {
    key: u16,
    at: (u32, u32),
    pointer: bool,
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
    /// Whether the mouse is the map's (turning the view) rather than a pointer.
    on: bool,
    hold: bool,
    /// Whether a menu key freed the pointer (rather than the look key).
    menu: bool,
    /// Where the finger is, once it is down; it goes down with the mouse's first move.
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

struct Tilt {
    /// Up, down, left, right.
    keys: [u16; 4],
    held: [bool; 4],
    angle: i16,
}

impl Tilt {
    fn pose(&self) -> Touch {
        let [up, down, left, right] = self.held.map(i16::from);
        Touch::Tilt {
            x: (down - up) * self.angle,
            z: (left - right) * self.angle,
        }
    }

    fn key(&mut self, code: u16, down: bool) -> Vec<Touch> {
        let before = self.pose();
        for (key, held) in self.keys.iter().zip(&mut self.held) {
            if *key == code {
                *held = down;
            }
        }
        let after = self.pose();
        if before == after {
            Vec::new()
        } else {
            vec![after]
        }
    }
}

struct Walk {
    /// Up, down, left, right.
    keys: [u16; 4],
    held: [bool; 4],
    km_per_hour: u16,
}

impl Walk {
    /// Degrees from north for the keys held, or `None` if none are (or they cancel out).
    fn bearing(&self) -> Option<u16> {
        let [up, down, left, right] = self.held.map(i32::from);
        let (east, north) = (right - left, up - down);
        if (east, north) == (0, 0) {
            return None;
        }
        let degrees = f64::from(east).atan2(f64::from(north)).to_degrees();
        Some(degrees.rem_euclid(FULL_TURN_DEGREES).round() as u16)
    }

    fn key(&mut self, code: u16, down: bool) -> Vec<Touch> {
        let before = self.bearing();
        for (key, held) in self.keys.iter().zip(&mut self.held) {
            if *key == code {
                *held = down;
            }
        }
        let after = self.bearing();
        if before == after {
            return Vec::new();
        }
        vec![Touch::Walk {
            bearing: after,
            km_per_hour: self.km_per_hour,
        }]
    }
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
    /// The finger each tap has down, if it has one.
    tapping: Vec<Option<u32>>,
    /// Fingers no tap is using, lowest last.
    spare: Vec<u32>,
    sticks: Vec<Stick>,
    look: Option<Look>,
    zoom: Option<Zoom>,
    walk: Option<Walk>,
    tilt: Option<Tilt>,
}

/// Reads the four direction keys of a section.
fn four_keys(names: [&String; 4], used: &mut Vec<u16>) -> Result<[u16; 4], String> {
    let mut keys = [0; 4];
    for (slot, name) in keys.iter_mut().zip(names) {
        *slot = key(name, used)?;
    }
    Ok(keys)
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

/// Reads the key map in the file at `path`, for a guest display of `display` pixels.
pub fn load(path: &std::path::Path, display: (u32, u32)) -> Result<Mapper, String> {
    let text = std::fs::read_to_string(path).map_err(|err| copy::file(path, &err))?;
    Mapper::parse(&text, display).map_err(|why| copy::keymap_refused(path, &why))
}

impl Mapper {
    /// Reads a key map for a guest display of `display` pixels.
    pub fn parse(text: &str, display: (u32, u32)) -> Result<Self, String> {
        let file: File = toml::from_str(text).map_err(|err| err.to_string())?;
        // shortcut: `display` is the square that holds the screen either way up, so only the
        // long side is compared; pass the panel's own size if screens of one length but another
        // shape need telling apart.
        if let Some([width, height]) = file.size
            && width.max(height) != display.0.max(display.1)
        {
            return Err(copy::keymap_other_size(width, height, display));
        }
        let mut used = Vec::new();
        // Sticks, the look and the zoom keep their fingers, taken from the top; taps borrow
        // the rest.
        let mut fingers = (FIRST_FINGER..=LAST_FINGER).rev();
        let mut finger = || fingers.next().ok_or(copy::KEYMAP_TOO_MANY.to_owned());
        let toggle = file.toggle.as_deref().map(|name| key(name, &mut used));
        let toggle = toggle.transpose()?;
        let mut taps = Vec::new();
        for entry in &file.tap {
            taps.push(Tap {
                key: key(&entry.key, &mut used)?,
                at: position(entry.at, display)?,
                pointer: entry.pointer,
            });
        }
        let mut sticks = Vec::new();
        for entry in &file.stick {
            let names = [&entry.up, &entry.down, &entry.left, &entry.right];
            sticks.push(Stick {
                keys: four_keys(names, &mut used)?,
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
                    on: entry.start,
                    hold: entry.hold,
                    menu: false,
                    at: None,
                })
            }
            None => None,
        };
        let zoom = match &file.zoom {
            Some(entry) => {
                // Zooming out moves the fingers in by a step, so they must not meet or cross.
                let reach = entry.spread.checked_add(entry.step);
                let Some(reach) = reach.filter(|_| entry.step != 0 && entry.step < entry.spread)
                else {
                    return Err(copy::KEYMAP_BAD_ZOOM.to_owned());
                };
                Some(Zoom {
                    center: circle(entry.center, reach, display)?,
                    spread: entry.spread,
                    step: entry.step,
                    fingers: [finger()?, finger()?],
                    pixels: 0.0,
                })
            }
            None => None,
        };
        let walk = match &file.walk {
            Some(entry) => {
                let names = [&entry.up, &entry.down, &entry.left, &entry.right];
                Some(Walk {
                    keys: four_keys(names, &mut used)?,
                    held: [false; 4],
                    km_per_hour: entry.speed,
                })
            }
            None => None,
        };
        let tilt = match &file.tilt {
            Some(entry) => {
                if !(1..=MAX_TILT_DEGREES).contains(&entry.angle) {
                    return Err(copy::keymap_bad_tilt(MAX_TILT_DEGREES));
                }
                let names = [&entry.up, &entry.down, &entry.left, &entry.right];
                Some(Tilt {
                    keys: four_keys(names, &mut used)?,
                    held: [false; 4],
                    angle: entry.angle,
                })
            }
            None => None,
        };
        let spare: Vec<u32> = fingers.collect();
        if !taps.is_empty() && spare.is_empty() {
            return Err(copy::KEYMAP_TOO_MANY.to_owned());
        }
        Ok(Self {
            display,
            toggle,
            enabled: true,
            tapping: vec![None; taps.len()],
            spare,
            taps,
            sticks,
            look,
            zoom,
            walk,
            tilt,
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
        let turns = turns.clamp(-ZOOM_MAX_TURNS, ZOOM_MAX_TURNS);
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
        self.enabled && self.look.as_ref().is_some_and(|look| look.on)
    }

    /// Lifts every finger the map has down.
    pub fn release(&mut self) -> Vec<Touch> {
        let mut touches = Vec::new();
        for tapping in &mut self.tapping {
            if let Some(finger) = tapping.take() {
                touches.push(Touch::Lift { finger });
                self.spare.push(finger);
            }
        }
        self.spare.sort_unstable_by(|a, b| b.cmp(a));
        for stick in &mut self.sticks {
            if std::mem::take(&mut stick.held).contains(&true) {
                touches.push(Touch::Lift {
                    finger: stick.finger,
                });
            }
        }
        if let Some(tilt) = &mut self.tilt
            && std::mem::take(&mut tilt.held).contains(&true)
        {
            touches.push(tilt.pose());
        }
        if let Some(walk) = &mut self.walk
            && std::mem::take(&mut walk.held).contains(&true)
        {
            touches.push(Touch::Walk {
                bearing: None,
                km_per_hour: walk.km_per_hour,
            });
        }
        if let Some(look) = &mut self.look {
            if look.at.take().is_some() {
                touches.push(Touch::Lift {
                    finger: look.finger,
                });
            }
            // A held key whose release is missed (focus went elsewhere) must not keep the pointer free.
            if look.hold {
                look.on = true;
            }
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
        // While the pointer is free, mouse buttons click the screen; a button already holding a
        // finger down still gets its release.
        let pointer_free = self.look.as_ref().is_some_and(|look| !look.on);
        // A menu a key opened is often closed with a click the map never sees. Moving or acting
        // again means play resumed, so the mouse turns the view again.
        if down && pointer_free && !is_button(code) {
            let resumes = self.sticks.iter().any(|stick| stick.keys.contains(&code))
                || self.taps.iter().any(|tap| tap.key == code && !tap.pointer);
            if let Some(look) = self.look.as_mut().filter(|look| look.menu && resumes) {
                look.on = true;
                look.menu = false;
            }
        }
        let pointer_free = self.look.as_ref().is_some_and(|look| !look.on);
        let index = self.taps.iter().position(|tap| tap.key == code);
        let holding = index.is_some_and(|index| self.tapping[index].is_some());
        if pointer_free && is_button(code) && !holding {
            return None;
        }
        if let Some(index) = index {
            let Tap {
                at: (x, y),
                pointer,
                ..
            } = self.taps[index];
            let tapping = &mut self.tapping[index];
            return Some(match (*tapping, down) {
                // With every finger busy the press is dropped, and so is its release.
                (None, true) => match self.spare.pop() {
                    Some(finger) => {
                        *tapping = Some(finger);
                        let mut touches = vec![Touch::Place { finger, x, y }];
                        if pointer && let Some(look) = &mut self.look {
                            look.on = !look.on;
                            look.menu = !look.on;
                            if look.at.take().is_some() {
                                touches.push(Touch::Lift {
                                    finger: look.finger,
                                });
                            }
                        }
                        touches
                    }
                    None => Vec::new(),
                },
                (Some(finger), false) => {
                    *tapping = None;
                    self.spare.push(finger);
                    self.spare.sort_unstable_by(|a, b| b.cmp(a));
                    vec![Touch::Lift { finger }]
                }
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
        if let Some(walk) = self.walk.as_mut().filter(|walk| walk.keys.contains(&code)) {
            return Some(walk.key(code, down));
        }
        if let Some(tilt) = self.tilt.as_mut().filter(|tilt| tilt.keys.contains(&code)) {
            return Some(tilt.key(code, down));
        }
        let look = self.look.as_mut().filter(|look| look.key == code)?;
        // The key switches between the mouse dragging the view and the mouse being a pointer;
        // with `hold`, the pointer is free only while the key is down.
        let on = match (look.hold, down) {
            (true, _) => !down,
            (false, true) => !look.on,
            (false, false) => return Some(Vec::new()),
        };
        look.on = on;
        look.menu = false;
        Some(match (on, look.at.take()) {
            (false, Some(_)) => vec![Touch::Lift {
                finger: look.finger,
            }],
            (true, _) => {
                look.at = Some((f64::from(look.center.0), f64::from(look.center.1)));
                vec![Touch::Place {
                    finger: look.finger,
                    x: look.center.0,
                    y: look.center.1,
                }]
            }
            (false, None) => Vec::new(),
        })
    }

    /// The mouse moved by this much while it belongs to the map.
    pub fn pointer(&mut self, dx: f64, dy: f64) -> Vec<Touch> {
        let Some(look) = self.look.as_mut().filter(|look| look.on && self.enabled) else {
            return Vec::new();
        };
        let center = (f64::from(look.center.0), f64::from(look.center.1));
        // A view the mouse turns from the start puts its finger down on the first move.
        let mut touches = Vec::new();
        let mut at = match look.at {
            Some(at) => at,
            None => {
                touches.push(Touch::Place {
                    finger: look.finger,
                    x: look.center.0,
                    y: look.center.1,
                });
                center
            }
        };
        let radius = f64::from(look.radius);
        let (dx, dy) = (dx * look.sensitivity, dy * look.sensitivity);
        // A fast flick can move further in one report than the whole circle; it is walked in
        // steps shorter than the circle so none of it is lost.
        let longest = radius * LOOK_STEP_SHARE;
        let steps = (dx.hypot(dy) / longest).ceil().clamp(1.0, LOOK_MAX_STEPS);
        let (mut step_x, mut step_y) = (dx / steps, dy / steps);
        let length = step_x.hypot(step_y);
        if length > longest {
            (step_x, step_y) = (step_x * longest / length, step_y * longest / length);
        }
        let finger = look.finger;
        let place = |(x, y): (f64, f64)| Touch::Place {
            finger,
            x: x.round() as u32,
            y: y.round() as u32,
        };
        for _ in 0..steps as u32 {
            let next = (at.0 + step_x, at.1 + step_y);
            // A finger cannot drag past the edge of its circle; it is lifted and starts again
            // from the middle, carrying on in the same direction, so the view turns without end.
            at = if (next.0 - center.0).hypot(next.1 - center.1) > radius {
                touches.extend([Touch::Lift { finger }, place(center)]);
                (center.0 + step_x, center.1 + step_y)
            } else {
                next
            };
            touches.push(place(at));
        }
        look.at = Some(at);
        debug_assert!(at.0 >= 0.0 && at.1 >= 0.0 && at.0 < f64::from(self.display.0));
        // Moves under a pixel change nothing the guest sees.
        touches.dedup();
        touches
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
    #[test]
    fn a_menu_key_frees_the_pointer_and_gives_it_back() {
        let text = "[look]\nkey = \"AltLeft\"\ncenter = [900, 300]\nradius = 250\nstart = true\n\
                    hold = true\n[[tap]]\nkey = \"Escape\"\nat = [1200, 40]\npointer = true\n\
                    [[stick]]\nup = \"KeyW\"\nleft = \"KeyA\"\ndown = \"KeyS\"\nright = \"KeyD\"\n\
                    center = [300, 500]\nradius = 100\n";
        let mut map = Mapper::parse(text, DISPLAY).unwrap();
        map.pointer(1.0, 0.0);
        assert_eq!(
            map.key(code("Escape"), true),
            Some(vec![place(1, 1200, 40), Touch::Lift { finger: 7 }])
        );
        assert!(!map.wants_pointer());
        map.key(code("Escape"), false);
        assert_eq!(
            map.key(code("Escape"), true),
            Some(vec![place(1, 1200, 40)])
        );
        assert!(map.wants_pointer());
        // A menu closed with a click instead: moving gives the mouse back to the view.
        map.key(code("Escape"), false);
        map.key(code("Escape"), true);
        map.key(code("Escape"), false);
        assert!(!map.wants_pointer());
        assert!(map.key(code("KeyW"), true).is_some());
        assert!(map.wants_pointer()); // Held keys and buttons the map uses never stop the mouse turning the view.
        map.key(code("KeyW"), false);
        map.key(code("ShiftLeft"), true);
        assert!(map.wants_pointer());
        assert!(!map.pointer(5.0, 0.0).is_empty());
    }

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

        [tilt]
        up = "KeyI"
        down = "KeyK"
        left = "KeyJ"
        right = "KeyL"
        angle = 30

        [walk]
        up = "ArrowUp"
        down = "ArrowDown"
        left = "ArrowLeft"
        right = "ArrowRight"
        speed = 5
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
        assert!(map.key(code("KeyW"), false).is_some());
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
        // While the pointer is free, a mouse button clicks the screen.
        assert_eq!(map.key(code("MouseLeft"), true), None);
        assert_eq!(map.key(code("MouseLeft"), false), None);
        // With the mouse turning the view, it is bound like a key; the finger let go is lent to it.
        assert_eq!(
            map.key(code("Backquote"), true),
            Some(vec![place(7, 900, 300)])
        );
        assert_eq!(
            map.key(code("MouseLeft"), true),
            Some(vec![place(1, 1000, 500)])
        );
        // Freed again while held, the button still gets its release.
        assert_eq!(
            map.key(code("Backquote"), true),
            Some(vec![Touch::Lift { finger: 7 }])
        );
        assert_eq!(
            map.key(code("MouseLeft"), false),
            Some(vec![Touch::Lift { finger: 1 }])
        );
        // A key the map does not know goes to the guest.
        assert_eq!(map.key(code("KeyQ"), true), None);
    }

    #[test]
    fn bundled_presets_are_valid_maps_for_their_screen_and_refused_on_others() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/keymaps/presets");
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
            let file: File = toml::from_str(&text).unwrap();
            let [width, height] = file
                .size
                .expect("a preset says which screen it was made for");
            let square = (width.max(height), width.max(height));
            assert!(Mapper::parse(&text, square).is_ok());
            assert!(Mapper::parse(&text, (square.0 / 2, square.0 / 2)).is_err());
            checked += 1;
        }
        assert!(checked > 0);
    }

    #[test]
    fn a_view_turned_from_the_start_puts_its_finger_down_on_the_first_move_and_hold_frees_it() {
        let text = "[look]\nkey = \"AltLeft\"\ncenter = [900, 300]\nradius = 250\n\
                    start = true\nhold = true\n[[tap]]\nkey = \"MouseLeft\"\nat = [1000, 500]\n";
        let mut map = Mapper::parse(text, DISPLAY).unwrap();
        assert!(map.wants_pointer());
        assert_eq!(
            map.pointer(10.0, 0.0),
            vec![place(8, 900, 300), place(8, 910, 300)]
        );
        assert_eq!(
            map.key(code("MouseLeft"), true),
            Some(vec![place(1, 1000, 500)])
        );
        assert_eq!(
            map.key(code("MouseLeft"), false),
            Some(vec![Touch::Lift { finger: 1 }])
        );
        // Holding the key frees the pointer, and clicks reach the screen.
        assert_eq!(
            map.key(code("AltLeft"), true),
            Some(vec![Touch::Lift { finger: 8 }])
        );
        assert!(!map.wants_pointer());
        assert_eq!(map.key(code("MouseLeft"), true), None);
        assert_eq!(map.pointer(5.0, 5.0), vec![]);
        assert_eq!(map.key(code("MouseLeft"), false), None);
        assert_eq!(
            map.key(code("AltLeft"), false),
            Some(vec![place(8, 900, 300)])
        );
        assert!(map.wants_pointer());
        // A release missed while the window was away does not leave the pointer free.
        map.key(code("AltLeft"), true);
        map.release();
        assert!(map.wants_pointer());
    }

    #[test]
    fn taps_borrow_fingers_so_a_map_may_bind_more_keys_than_there_are_fingers() {
        let keys = ["F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10"];
        let text: String = keys
            .iter()
            .map(|key| format!("[[tap]]\nkey = \"{key}\"\nat = [1, 1]\n"))
            .collect();
        let mut map = Mapper::parse(&text, DISPLAY).unwrap();
        let fingers = LAST_FINGER - FIRST_FINGER + 1;
        for (finger, key) in (FIRST_FINGER..).zip(&keys[..fingers as usize]) {
            assert_eq!(map.key(code(key), true), Some(vec![place(finger, 1, 1)]));
        }
        // Every finger is down: the next press is dropped, and its release lifts nothing.
        assert_eq!(map.key(code("F9"), true), Some(vec![]));
        assert_eq!(map.key(code("F9"), false), Some(vec![]));
        // A finger let go is lent to the next key, lowest first.
        assert_eq!(
            map.key(code("F3"), false),
            Some(vec![Touch::Lift { finger: 3 }])
        );
        assert_eq!(map.key(code("F10"), true), Some(vec![place(3, 1, 1)]));
        assert_eq!(map.release().len(), fingers as usize);
    }

    #[test]
    fn stick_lands_in_the_middle_and_follows_the_keys_held() {
        let mut map = mapper();
        let (w, a, d) = (code("KeyW"), code("KeyA"), code("KeyD"));
        // Up: down in the middle, then pushed up by the radius.
        assert_eq!(
            map.key(w, true),
            Some(vec![place(8, 200, 550), place(8, 200, 450)])
        );
        // Up and left: a diagonal of the same length (100 / sqrt 2 is about 71).
        assert_eq!(map.key(a, true), Some(vec![place(8, 129, 479)]));
        // Left and right cancel out, leaving up.
        assert_eq!(map.key(d, true), Some(vec![place(8, 200, 450)]));
        assert_eq!(map.key(a, false), Some(vec![place(8, 271, 479)]));
        assert_eq!(map.key(d, false), Some(vec![place(8, 200, 450)]));
        assert_eq!(map.key(w, false), Some(vec![Touch::Lift { finger: 8 }]));
        // Opposite keys alone are no push at all.
        assert_eq!(
            map.key(a, true),
            Some(vec![place(8, 200, 550), place(8, 100, 550)])
        );
        assert_eq!(map.key(d, true), Some(vec![Touch::Lift { finger: 8 }]));
    }

    #[test]
    fn look_key_gives_the_mouse_to_a_finger_that_drags_from_the_centre() {
        let mut map = mapper();
        let look = code("Backquote");
        assert!(!map.wants_pointer());
        assert!(map.pointer(5.0, 5.0).is_empty());
        assert_eq!(map.key(look, true), Some(vec![place(7, 900, 300)]));
        assert_eq!(map.key(look, false), Some(vec![]));
        assert!(map.wants_pointer());
        assert_eq!(map.pointer(10.0, -4.0), vec![place(7, 910, 296)]);
        assert_eq!(map.pointer(0.4, 0.4), vec![place(7, 910, 296)]);
        // Past the edge of the circle: lifted, put back in the middle, and moved on from there.
        let touches = map.pointer(245.0, 0.0);
        assert_eq!(touches.first(), Some(&place(7, 1033, 296)));
        assert_eq!(
            &touches[touches.len() - 3..],
            &[
                Touch::Lift { finger: 7 },
                place(7, 900, 300),
                place(7, 1023, 300)
            ]
        );
        // A flick larger than the whole circle keeps all of its distance: steps, not a jump.
        // From 1023, four moves of 100, two of them from a fresh start in the middle.
        assert_eq!(
            map.pointer(400.0, 0.0),
            vec![
                place(7, 1123, 300),
                Touch::Lift { finger: 7 },
                place(7, 900, 300),
                place(7, 1000, 300),
                place(7, 1100, 300),
                Touch::Lift { finger: 7 },
                place(7, 900, 300),
                place(7, 1000, 300),
            ]
        );
        assert_eq!(map.key(look, true), Some(vec![Touch::Lift { finger: 7 }]));
        assert!(!map.wants_pointer());
    }

    #[test]
    fn each_wheel_turn_is_a_pinch_out_or_in_and_trackpad_pixels_add_up_to_turns() {
        let mut map = mapper();
        let pinch = map.scroll(Scroll::Turns(1.0));
        // Down at the spread, moved out by the step, lifted.
        assert_eq!(pinch.len(), 2 * (ZOOM_MOVES as usize + 1) + 2);
        assert_eq!(&pinch[..2], &[place(6, 520, 360), place(5, 760, 360)]);
        let last_move = 2 * ZOOM_MOVES as usize;
        assert_eq!(
            &pinch[last_move..],
            &[
                place(6, 460, 360),
                place(5, 820, 360),
                Touch::Lift { finger: 6 },
                Touch::Lift { finger: 5 }
            ]
        );
        let pinch = map.scroll(Scroll::Turns(-2.0));
        assert_eq!(pinch.len(), 2 * (2 * (ZOOM_MOVES as usize + 1) + 2));
        assert_eq!(
            &pinch[last_move..last_move + 2],
            &[place(6, 580, 360), place(5, 700, 360)]
        );
        // A part of a turn is nothing.
        assert!(map.scroll(Scroll::Turns(0.5)).is_empty());
        // A huge scroll is cut to a few turns.
        let huge = map.scroll(Scroll::Turns(1e9)).len();
        assert_eq!(huge, ZOOM_MAX_TURNS as usize * pinch.len() / 2);
        // Trackpad pixels count up until they make a turn, and the rest carries over.
        assert!(map.scroll(Scroll::Pixels(30.0)).is_empty());
        assert_eq!(map.scroll(Scroll::Pixels(30.0)).len(), pinch.len() / 2);
        assert_eq!(map.scroll(Scroll::Pixels(20.0)).len(), pinch.len() / 2);
        // Switched off, the wheel does nothing.
        map.key(code("F12"), true);
        assert!(map.scroll(Scroll::Turns(1.0)).is_empty());
    }

    #[test]
    fn walk_keys_turn_into_a_bearing_from_north_and_stop_when_let_go() {
        let mut map = mapper();
        let walk = |bearing| {
            Some(vec![Touch::Walk {
                bearing,
                km_per_hour: 5,
            }])
        };
        assert_eq!(map.key(code("ArrowUp"), true), walk(Some(0)));
        assert_eq!(map.key(code("ArrowRight"), true), walk(Some(45)));
        assert_eq!(map.key(code("ArrowUp"), false), walk(Some(90)));
        // A repeat changes nothing.
        assert_eq!(map.key(code("ArrowRight"), true), Some(vec![]));
        assert_eq!(map.key(code("ArrowDown"), true), walk(Some(135)));
        assert_eq!(map.key(code("ArrowRight"), false), walk(Some(180)));
        assert_eq!(map.key(code("ArrowLeft"), true), walk(Some(225)));
        assert_eq!(map.key(code("ArrowDown"), false), walk(Some(270)));
        // Switching the map off stops the walk.
        assert_eq!(map.key(code("F12"), true), walk(None));
    }

    #[test]
    fn tilt_keys_hold_the_device_turned_and_let_it_back_up() {
        let mut map = mapper();
        let tilt = |x, z| Some(vec![Touch::Tilt { x, z }]);
        assert_eq!(map.key(code("KeyI"), true), tilt(-30, 0));
        assert_eq!(map.key(code("KeyJ"), true), tilt(-30, 30));
        assert_eq!(map.key(code("KeyI"), false), tilt(0, 30));
        assert_eq!(map.key(code("KeyL"), true), tilt(0, 0));
        assert_eq!(map.key(code("KeyJ"), false), tilt(0, -30));
        assert_eq!(map.key(code("F12"), true), tilt(0, 0));
        let bad = "[tilt]\nup=\"KeyI\"\ndown=\"KeyK\"\nleft=\"KeyJ\"\nright=\"KeyL\"\nangle=91";
        assert_eq!(
            Mapper::parse(bad, DISPLAY).err(),
            Some(copy::keymap_bad_tilt(MAX_TILT_DEGREES))
        );
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
                Touch::Lift { finger: 8 },
                Touch::Lift { finger: 7 }
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
            refused(&format!("{zoom}spread = 4294967295\nstep = 2")),
            copy::KEYMAP_BAD_ZOOM
        );
        assert_eq!(
            refused(&format!("{zoom}spread = 300\nstep = 100")),
            copy::keymap_circle_off_screen(640, 360, 400)
        );
        // Sticks, a look and a zoom that hold every finger leave none for a tap to borrow.
        let fixed = "[look]\nkey = \"Backquote\"\ncenter = [900, 300]\nradius = 250\n\
                     [zoom]\ncenter = [640, 360]\nspread = 120\nstep = 60\n";
        let sticks: String = (0..5u8)
            .map(|n| {
                let [up, down, left, right] = [0, 1, 2, 3].map(|k| (b'A' + n * 4 + k) as char);
                format!(
                    "[[stick]]\nup = \"Key{up}\"\ndown = \"Key{down}\"\nleft = \"Key{left}\"\n\
                     right = \"Key{right}\"\ncenter = [300, 300]\nradius = 10\n"
                )
            })
            .collect();
        assert_eq!(
            refused(&format!(
                "{sticks}[[tap]]\nkey = \"Space\"\nat = [1, 1]\n{fixed}"
            )),
            copy::KEYMAP_TOO_MANY
        );
        // A misspelt setting is an error, not something silently ignored.
        assert!(refused("togle = \"F1\"").contains("togle"));
        // An empty map is a map that does nothing.
        assert!(Mapper::parse("", DISPLAY).is_ok());
    }
}

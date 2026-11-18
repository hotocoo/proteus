//! The input devices the guest sees, and the events that drive them.
//!
//! Event types and codes are the Linux evdev numbers; the guest's input stack reads them unchanged.

use std::ops::RangeInclusive;

use devices::virtio::input::{AbsInfo, DeviceIds, InputDescription, InputEvent};

use crate::copy;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const BTN_TOUCH: u16 = 0x14a;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
/// The touch surface is the screen itself, not a separate pad that moves a pointer.
const INPUT_PROP_DIRECT: u16 = 1;
const BUS_VIRTUAL: u16 = 6;
/// The virtio vendor number, reused so that no real device's key layout matches these devices.
const VENDOR: u16 = 0x1af4;
const PRODUCT_TOUCHSCREEN: u16 = 1;
const PRODUCT_KEYBOARD: u16 = 2;
const PRODUCT_GAMEPAD: u16 = 3;
const DEVICE_VERSION: u16 = 1;

/// Fingers the touchscreen tracks at once.
pub const TOUCH_SLOTS: u32 = 10;
/// A contact's tracking number while it is down; the guest only needs it to differ between contacts.
const TRACKING_ID_MAX: u32 = 0xffff;
/// Value of `ABS_MT_TRACKING_ID` that ends a contact.
const TRACKING_ID_NONE: i32 = -1;
/// Every key code of an ordinary keyboard, `KEY_ESC` to `KEY_MICMUTE`. Button codes start above it,
/// and a device that reported those would be taken for a mouse or a gamepad as well.
const KEYBOARD_CODES: RangeInclusive<u16> = 1..=248;
/// `KEY_APPSELECT`, which Android reads as its "recent apps" button.
pub const KEY_APPSELECT: u16 = 0x244;

/// How far a stick reaches from its centre, in the units the gamepad reports.
const STICK_REACH: i32 = 32767;
/// A trigger pulled all the way.
const TRIGGER_FULL: i32 = 255;
/// A stick this close to its centre reads as centred; real sticks never rest exactly there.
const STICK_FLAT: u32 = 1024;

/// What a gamepad control is: a button, or one of the kinds of axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadKind {
    Button,
    /// From one side through the centre to the other.
    Stick,
    /// From released to fully pulled.
    Trigger,
    /// The direction pad, which is only ever one way, centred or the other.
    Hat,
}

/// One control of the gamepad: its name in control commands, its Linux code and what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadControl {
    pub name: &'static str,
    pub code: u16,
    pub kind: PadKind,
}

const fn pad(name: &'static str, code: u16, kind: PadKind) -> PadControl {
    PadControl { name, code, kind }
}

/// The gamepad's controls. Buttons are the Linux gamepad buttons, which Android's generic gamepad
/// layout already knows. The right stick is Z and RZ and the triggers brake and gas, because that is
/// where Android looks for them on a pad with no layout of its own.
pub const PAD_CONTROLS: [PadControl; 21] = [
    pad("a", 0x130, PadKind::Button),
    pad("b", 0x131, PadKind::Button),
    pad("x", 0x133, PadKind::Button),
    pad("y", 0x134, PadKind::Button),
    pad("l1", 0x136, PadKind::Button),
    pad("r1", 0x137, PadKind::Button),
    pad("l2", 0x138, PadKind::Button),
    pad("r2", 0x139, PadKind::Button),
    pad("select", 0x13a, PadKind::Button),
    pad("start", 0x13b, PadKind::Button),
    pad("mode", 0x13c, PadKind::Button),
    pad("thumbl", 0x13d, PadKind::Button),
    pad("thumbr", 0x13e, PadKind::Button),
    pad("lx", 0x00, PadKind::Stick),
    pad("ly", 0x01, PadKind::Stick),
    pad("rx", 0x02, PadKind::Stick),
    pad("ry", 0x05, PadKind::Stick),
    pad("lt", 0x0a, PadKind::Trigger),
    pad("rt", 0x09, PadKind::Trigger),
    pad("hatx", 0x10, PadKind::Hat),
    pad("haty", 0x11, PadKind::Hat),
];

/// The gamepad control with this name.
pub fn pad_control(name: &str) -> Option<PadControl> {
    PAD_CONTROLS
        .iter()
        .copied()
        .find(|control| control.name == name)
}

const fn event(kind: u16, code: u16, value: i32) -> InputEvent {
    InputEvent { kind, code, value }
}

const REPORT: InputEvent = event(EV_SYN, SYN_REPORT, 0);

fn ids(product: u16) -> DeviceIds {
    DeviceIds {
        bustype: BUS_VIRTUAL,
        vendor: VENDOR,
        product,
        version: DEVICE_VERSION,
    }
}

fn axis(max: u32) -> AbsInfo {
    AbsInfo {
        max,
        ..AbsInfo::default()
    }
}

/// A multitouch screen (protocol B) whose coordinates are the display's pixels.
pub fn touchscreen(width: u32, height: u32) -> InputDescription {
    InputDescription {
        name: copy::TOUCHSCREEN_NAME.to_owned(),
        serial: copy::INPUT_SERIAL.to_owned(),
        ids: ids(PRODUCT_TOUCHSCREEN),
        properties: vec![INPUT_PROP_DIRECT],
        events: vec![
            (EV_KEY, vec![BTN_TOUCH]),
            (
                EV_ABS,
                vec![
                    ABS_MT_SLOT,
                    ABS_MT_POSITION_X,
                    ABS_MT_POSITION_Y,
                    ABS_MT_TRACKING_ID,
                ],
            ),
        ],
        axes: vec![
            (ABS_MT_SLOT, axis(TOUCH_SLOTS - 1)),
            (ABS_MT_POSITION_X, axis(width.saturating_sub(1))),
            (ABS_MT_POSITION_Y, axis(height.saturating_sub(1))),
            (ABS_MT_TRACKING_ID, axis(TRACKING_ID_MAX)),
        ],
    }
}

pub fn keyboard() -> InputDescription {
    InputDescription {
        name: copy::KEYBOARD_NAME.to_owned(),
        serial: copy::INPUT_SERIAL.to_owned(),
        ids: ids(PRODUCT_KEYBOARD),
        properties: Vec::new(),
        events: vec![(EV_KEY, KEYBOARD_CODES.chain([KEY_APPSELECT]).collect())],
        axes: Vec::new(),
    }
}

/// A gamepad with two sticks, two analogue triggers, a direction pad and the usual buttons.
pub fn gamepad() -> InputDescription {
    // evdev's axis limits are signed; virtio carries them as their 32-bit patterns.
    let range = |min: i32, max: i32, flat: u32| AbsInfo {
        min: min as u32,
        max: max as u32,
        flat,
        ..AbsInfo::default()
    };
    let of = |kind: PadKind| {
        PAD_CONTROLS
            .iter()
            .filter(move |control| control.kind == kind)
    };
    let axes = of(PadKind::Stick)
        .map(|stick| (stick.code, range(-STICK_REACH, STICK_REACH, STICK_FLAT)))
        .chain(of(PadKind::Trigger).map(|trigger| (trigger.code, range(0, TRIGGER_FULL, 0))))
        .chain(of(PadKind::Hat).map(|hat| (hat.code, range(-1, 1, 0))));
    let axes: Vec<_> = axes.collect();
    InputDescription {
        name: copy::GAMEPAD_NAME.to_owned(),
        serial: copy::INPUT_SERIAL.to_owned(),
        ids: ids(PRODUCT_GAMEPAD),
        properties: Vec::new(),
        events: vec![
            (
                EV_KEY,
                of(PadKind::Button).map(|button| button.code).collect(),
            ),
            (EV_ABS, axes.iter().map(|(code, _)| *code).collect()),
        ],
        axes,
    }
}

/// A gamepad control moved, as a complete report. `value` is 0 or 1 for a button (anything else
/// counts as pressed), -1 to 1 for a stick (right and down are positive) or the direction pad, and
/// 0 to 1 for a trigger. Values past the ends are held at the end.
pub fn pad_event(control: PadControl, value: f64) -> [InputEvent; 2] {
    let scaled =
        |low: f64, high: f64, full: i32| (value.clamp(low, high) * f64::from(full)).round();
    let (kind, value) = match control.kind {
        PadKind::Button => (EV_KEY, i32::from(value != 0.0)),
        PadKind::Stick => (EV_ABS, scaled(-1.0, 1.0, STICK_REACH) as i32),
        PadKind::Trigger => (EV_ABS, scaled(0.0, 1.0, TRIGGER_FULL) as i32),
        PadKind::Hat => (EV_ABS, scaled(-1.0, 1.0, 1) as i32),
    };
    [event(kind, control.code, value), REPORT]
}

/// Keys by name and Linux key code. The names are the ones the W3C gives physical keys ("KeyW",
/// "Space", "ArrowUp"), which is also what the window system calls them; key maps use them too.
const KEY_NAMES: [(&str, u16); 99] = [
    ("Escape", 1),
    ("Digit1", 2),
    ("Digit2", 3),
    ("Digit3", 4),
    ("Digit4", 5),
    ("Digit5", 6),
    ("Digit6", 7),
    ("Digit7", 8),
    ("Digit8", 9),
    ("Digit9", 10),
    ("Digit0", 11),
    ("Minus", 12),
    ("Equal", 13),
    ("Backspace", 14),
    ("Tab", 15),
    ("KeyQ", 16),
    ("KeyW", 17),
    ("KeyE", 18),
    ("KeyR", 19),
    ("KeyT", 20),
    ("KeyY", 21),
    ("KeyU", 22),
    ("KeyI", 23),
    ("KeyO", 24),
    ("KeyP", 25),
    ("BracketLeft", 26),
    ("BracketRight", 27),
    ("Enter", 28),
    ("ControlLeft", 29),
    ("KeyA", 30),
    ("KeyS", 31),
    ("KeyD", 32),
    ("KeyF", 33),
    ("KeyG", 34),
    ("KeyH", 35),
    ("KeyJ", 36),
    ("KeyK", 37),
    ("KeyL", 38),
    ("Semicolon", 39),
    ("Quote", 40),
    ("Backquote", 41),
    ("ShiftLeft", 42),
    ("Backslash", 43),
    ("KeyZ", 44),
    ("KeyX", 45),
    ("KeyC", 46),
    ("KeyV", 47),
    ("KeyB", 48),
    ("KeyN", 49),
    ("KeyM", 50),
    ("Comma", 51),
    ("Period", 52),
    ("Slash", 53),
    ("ShiftRight", 54),
    ("NumpadMultiply", 55),
    ("AltLeft", 56),
    ("Space", 57),
    ("CapsLock", 58),
    ("F1", 59),
    ("F2", 60),
    ("F3", 61),
    ("F4", 62),
    ("F5", 63),
    ("F6", 64),
    ("F7", 65),
    ("F8", 66),
    ("F9", 67),
    ("F10", 68),
    ("Numpad7", 71),
    ("Numpad8", 72),
    ("Numpad9", 73),
    ("NumpadSubtract", 74),
    ("Numpad4", 75),
    ("Numpad5", 76),
    ("Numpad6", 77),
    ("NumpadAdd", 78),
    ("Numpad1", 79),
    ("Numpad2", 80),
    ("Numpad3", 81),
    ("Numpad0", 82),
    ("NumpadDecimal", 83),
    ("F11", 87),
    ("F12", 88),
    ("NumpadEnter", 96),
    ("ControlRight", 97),
    ("NumpadDivide", 98),
    ("AltRight", 100),
    ("Home", 102),
    ("ArrowUp", 103),
    ("PageUp", 104),
    ("ArrowLeft", 105),
    ("ArrowRight", 106),
    ("End", 107),
    ("ArrowDown", 108),
    ("PageDown", 109),
    ("Insert", 110),
    ("Delete", 111),
    ("SuperLeft", 125),
    ("SuperRight", 126),
];
/// Mouse buttons, for key maps. The guest's keyboard does not have these.
const BUTTON_NAMES: [(&str, u16); 3] = [
    ("MouseLeft", 0x110),
    ("MouseRight", 0x111),
    ("MouseMiddle", 0x112),
];

/// The Linux key code of the keyboard key with this name.
pub fn key_code(name: &str) -> Option<u16> {
    let found = KEY_NAMES.iter().find(|(known, _)| *known == name);
    found.map(|(_, code)| *code)
}

/// The code of the key or mouse button with this name, for key maps.
pub fn key_or_button_code(name: &str) -> Option<u16> {
    let found = BUTTON_NAMES.iter().find(|(known, _)| *known == name);
    found.map(|(_, code)| *code).or_else(|| key_code(name))
}

/// The code a key map knows this mouse button by: 0 is the left button, 1 the right, 2 the middle.
pub fn button_code(button: usize) -> Option<u16> {
    BUTTON_NAMES.get(button).map(|(_, code)| *code)
}

/// Whether the keyboard has this key.
pub fn is_key(code: u16) -> bool {
    KEYBOARD_CODES.contains(&code) || code == KEY_APPSELECT
}

/// One key going down or up, as a complete report.
pub fn key(code: u16, down: bool) -> [InputEvent; 2] {
    [event(EV_KEY, code, i32::from(down)), REPORT]
}

/// Which fingers are on the touchscreen. Turns "finger N is at (x, y)" and "finger N lifted" into the
/// event sequences the multitouch protocol wants.
pub struct Touch {
    width: u32,
    height: u32,
    /// One bit per slot that has a finger down.
    down: u32,
    next_tracking_id: u32,
}

impl Touch {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            down: 0,
            next_tracking_id: 0,
        }
    }

    /// Puts finger `slot` at a pixel, or moves it there if it is already down.
    /// `None` if the slot or the position is outside the screen.
    pub fn place(&mut self, slot: u32, x: u32, y: u32) -> Option<Vec<InputEvent>> {
        if slot >= TOUCH_SLOTS || x >= self.width || y >= self.height {
            return None;
        }
        let mut events = vec![event(EV_ABS, ABS_MT_SLOT, slot as i32)];
        if self.down & (1 << slot) == 0 {
            events.push(event(
                EV_ABS,
                ABS_MT_TRACKING_ID,
                self.next_tracking_id as i32,
            ));
            self.next_tracking_id = (self.next_tracking_id + 1) % (TRACKING_ID_MAX + 1);
            if self.down == 0 {
                events.push(event(EV_KEY, BTN_TOUCH, 1));
            }
            self.down |= 1 << slot;
        }
        events.extend([
            event(EV_ABS, ABS_MT_POSITION_X, x as i32),
            event(EV_ABS, ABS_MT_POSITION_Y, y as i32),
            REPORT,
        ]);
        Some(events)
    }

    /// Lifts finger `slot`. `None` if no such finger is down.
    pub fn lift(&mut self, slot: u32) -> Option<Vec<InputEvent>> {
        if slot >= TOUCH_SLOTS || self.down & (1 << slot) == 0 {
            return None;
        }
        self.down &= !(1 << slot);
        let mut events = vec![
            event(EV_ABS, ABS_MT_SLOT, slot as i32),
            event(EV_ABS, ABS_MT_TRACKING_ID, TRACKING_ID_NONE),
        ];
        if self.down == 0 {
            events.push(event(EV_KEY, BTN_TOUCH, 0));
        }
        events.push(REPORT);
        Some(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_finger_presses_the_touch_button_and_the_last_one_releases_it() {
        let mut touch = Touch::new(100, 50);
        assert_eq!(
            touch.place(0, 10, 20).unwrap(),
            [
                event(EV_ABS, ABS_MT_SLOT, 0),
                event(EV_ABS, ABS_MT_TRACKING_ID, 0),
                event(EV_KEY, BTN_TOUCH, 1),
                event(EV_ABS, ABS_MT_POSITION_X, 10),
                event(EV_ABS, ABS_MT_POSITION_Y, 20),
                REPORT,
            ]
        );
        // A second finger gets its own tracking number and leaves the button alone.
        assert_eq!(
            touch.place(1, 99, 49).unwrap(),
            [
                event(EV_ABS, ABS_MT_SLOT, 1),
                event(EV_ABS, ABS_MT_TRACKING_ID, 1),
                event(EV_ABS, ABS_MT_POSITION_X, 99),
                event(EV_ABS, ABS_MT_POSITION_Y, 49),
                REPORT,
            ]
        );
        assert_eq!(
            touch.lift(0).unwrap(),
            [
                event(EV_ABS, ABS_MT_SLOT, 0),
                event(EV_ABS, ABS_MT_TRACKING_ID, TRACKING_ID_NONE),
                REPORT,
            ]
        );
        assert_eq!(
            touch.lift(1).unwrap(),
            [
                event(EV_ABS, ABS_MT_SLOT, 1),
                event(EV_ABS, ABS_MT_TRACKING_ID, TRACKING_ID_NONE),
                event(EV_KEY, BTN_TOUCH, 0),
                REPORT,
            ]
        );
    }

    #[test]
    fn moving_a_finger_that_is_down_only_reports_its_position() {
        let mut touch = Touch::new(100, 50);
        touch.place(3, 1, 1).unwrap();
        assert_eq!(
            touch.place(3, 2, 3).unwrap(),
            [
                event(EV_ABS, ABS_MT_SLOT, 3),
                event(EV_ABS, ABS_MT_POSITION_X, 2),
                event(EV_ABS, ABS_MT_POSITION_Y, 3),
                REPORT,
            ]
        );
    }

    #[test]
    fn positions_off_the_screen_and_unknown_fingers_are_refused() {
        let mut touch = Touch::new(100, 50);
        assert!(touch.place(0, 100, 0).is_none());
        assert!(touch.place(0, 0, 50).is_none());
        assert!(touch.place(TOUCH_SLOTS, 0, 0).is_none());
        assert!(touch.lift(0).is_none());
        assert!(touch.lift(TOUCH_SLOTS).is_none());
    }

    #[test]
    fn keyboard_has_letters_and_android_buttons_but_no_mouse_or_gamepad_buttons() {
        let description = keyboard();
        let (_, codes) = &description.events[0];
        const KEY_Q: u16 = 16;
        const KEY_POWER: u16 = 116;
        const KEY_BACK: u16 = 158;
        const BTN_MOUSE: u16 = 0x110;
        const BTN_GAMEPAD: u16 = 0x130;
        for code in [KEY_Q, KEY_POWER, KEY_BACK, KEY_APPSELECT] {
            assert!(codes.contains(&code) && is_key(code));
        }
        for code in [0, BTN_MOUSE, BTN_GAMEPAD, BTN_TOUCH] {
            assert!(!codes.contains(&code) && !is_key(code));
        }
    }

    #[test]
    fn every_named_key_is_one_the_keyboard_has_and_no_name_or_code_repeats() {
        for (index, (name, code)) in KEY_NAMES.iter().enumerate() {
            assert!(is_key(*code), "{name}");
            assert_eq!(key_code(name), Some(*code));
            let later = &KEY_NAMES[index + 1..];
            assert!(later.iter().all(|(n, c)| n != name && c != code), "{name}");
        }
        assert_eq!(key_code("KeyW"), Some(17));
        assert_eq!(key_code("MouseLeft"), None);
        assert_eq!(key_or_button_code("MouseLeft"), button_code(0));
        assert_eq!(key_or_button_code("KeyW"), Some(17));
        assert_eq!(key_or_button_code("NoSuchKey"), None);
        // Mouse buttons are not keyboard keys, so they are never sent to the guest's keyboard.
        assert!(BUTTON_NAMES.iter().all(|(_, code)| !is_key(*code)));
    }

    #[test]
    fn gamepad_reports_every_control_and_scales_values_into_its_ranges() {
        let description = gamepad();
        let (_, buttons) = &description.events[0];
        let (_, axes) = &description.events[1];
        assert_eq!(buttons.len() + axes.len(), PAD_CONTROLS.len());
        assert_eq!(description.axes.len(), axes.len());
        // Nothing a keyboard has, so the guest never takes the pad for one.
        assert!(buttons.iter().all(|code| !is_key(*code)));
        let hat = pad_control("hatx").unwrap();
        let limits = description
            .axes
            .iter()
            .find(|(code, _)| *code == hat.code)
            .unwrap()
            .1;
        assert_eq!((limits.min as i32, limits.max as i32), (-1, 1));

        let a = pad_control("a").unwrap();
        assert_eq!(pad_event(a, 1.0), [event(EV_KEY, 0x130, 1), REPORT]);
        assert_eq!(pad_event(a, 0.0)[0].value, 0);
        let stick = pad_control("ly").unwrap();
        assert_eq!(pad_event(stick, -1.0)[0], event(EV_ABS, 0x01, -STICK_REACH));
        assert_eq!(pad_event(stick, 0.5)[0].value, 16384);
        assert_eq!(pad_event(stick, 3.0)[0].value, STICK_REACH);
        let trigger = pad_control("rt").unwrap();
        assert_eq!(pad_event(trigger, -1.0)[0].value, 0);
        assert_eq!(pad_event(trigger, 1.0)[0].value, TRIGGER_FULL);
        assert_eq!(pad_event(hat, -0.7)[0].value, -1);
        assert_eq!(pad_control("z"), None);
        // No name or code is used twice.
        for (index, control) in PAD_CONTROLS.iter().enumerate() {
            let later = &PAD_CONTROLS[index + 1..];
            assert!(
                later
                    .iter()
                    .all(|c| c.name != control.name && c.code != control.code)
            );
        }
    }

    #[test]
    fn touchscreen_axes_end_at_the_last_pixel() {
        let description = touchscreen(1280, 720);
        let max = |wanted: u16| {
            let found = description.axes.iter().find(|(axis, _)| *axis == wanted);
            found.unwrap().1.max
        };
        assert_eq!(max(ABS_MT_POSITION_X), 1279);
        assert_eq!(max(ABS_MT_POSITION_Y), 719);
        assert_eq!(max(ABS_MT_SLOT), TOUCH_SLOTS - 1);
        assert_eq!(description.properties, [INPUT_PROP_DIRECT]);
    }
}

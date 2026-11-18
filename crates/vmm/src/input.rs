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

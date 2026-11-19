//! Which way up the guest's screen is shown. Android turns what it draws when the device turns (or
//! an app asks for landscape), but the display panel keeps its own shape; the emulator turns the
//! picture back so it reads upright, and turns positions given upright (taps, key maps, the
//! window's mouse) into the panel's own before they reach the touchscreen, which Android turns
//! again itself.
//!
//! Turns are quarter turns of Android's display rotation: 0 is the panel's natural way up, 1 is
//! `ROTATION_90`, whose picture lies on the panel turned a quarter clockwise.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::thread::JoinHandle;

use devices::virtio::gpu::{BYTES_PER_PIXEL, Frame};

use crate::adb;
use crate::config::{ORIENTATION_POLL, ORIENTATION_TIMEOUT};
use crate::copy;

/// Prints the display's rotation as `mCurrentOrientation=<turns>`. Other output is dropped: `grep`
/// stopping early makes `dumpsys` complain of a broken pipe.
const ORIENTATION_COMMAND: &str =
    "sh -c 'dumpsys display 2>/dev/null | grep -m1 mCurrentOrientation='";
const ORIENTATION_FIELD: &str = "mCurrentOrientation=";
/// Quarter turns in a whole turn.
const TURNS: u8 = 4;

/// The rotation in what `ORIENTATION_COMMAND` printed.
pub fn parse(printed: &str) -> Option<u8> {
    let (_, rest) = printed.split_once(ORIENTATION_FIELD)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|turns| *turns < TURNS)
}

/// Size of the picture shown upright, for a panel of `panel` pixels.
pub fn upright_size(panel: (u32, u32), turns: u8) -> (u32, u32) {
    if turns % 2 == 1 {
        (panel.1, panel.0)
    } else {
        panel
    }
}

/// A box that holds the picture whichever way up it is shown: for checking positions given upright
/// (key maps) before it is known how the app will hold the screen.
pub fn any_way_up(panel: (u32, u32)) -> (u32, u32) {
    let long = panel.0.max(panel.1);
    (long, long)
}

/// The panel pixel under the upright picture's pixel `(x, y)`. `None` outside the picture.
pub fn to_panel(x: u32, y: u32, panel: (u32, u32), turns: u8) -> Option<(u32, u32)> {
    let (width, height) = upright_size(panel, turns);
    if x >= width || y >= height {
        return None;
    }
    let (last_x, last_y) = (panel.0 - 1, panel.1 - 1);
    Some(match turns % TURNS {
        0 => (x, y),
        1 => (last_x - y, x),
        2 => (last_x - x, last_y - y),
        _ => (y, last_y - x),
    })
}

/// `frame` turned upright, in the same pixel format. Borrowed as it is when nothing turns.
pub fn upright<'a>(frame: &Frame<'a>, turns: u8) -> (u32, u32, Cow<'a, [u8]>) {
    let panel = (frame.width, frame.height);
    let (width, height) = upright_size(panel, turns);
    let stride = frame.width as usize * BYTES_PER_PIXEL;
    // A frame whose size and pixels disagree is passed on as it is, for its user to refuse.
    if turns.is_multiple_of(TURNS) || frame.pixels.len() != stride * frame.height as usize {
        return (frame.width, frame.height, Cow::Borrowed(frame.pixels));
    }
    let mut pixels = Vec::with_capacity(frame.pixels.len());
    for y in 0..height {
        for x in 0..width {
            let (px, py) = to_panel(x, y, panel, turns).unwrap_or_default();
            let at = py as usize * stride + px as usize * BYTES_PER_PIXEL;
            pixels.extend_from_slice(&frame.pixels[at..at + BYTES_PER_PIXEL]);
        }
    }
    (width, height, Cow::Owned(pixels))
}

/// Asks the guest which way up its display is every `ORIENTATION_POLL`, and calls `changed` with
/// the new turns whenever that changes, until `done` is set.
pub fn watch(
    adb_port: Arc<AtomicU16>,
    done: Arc<AtomicBool>,
    changed: impl Fn(u8) + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(copy::ORIENTATION_THREAD.into())
        .spawn(move || {
            let mut last = 0;
            while !done.load(Ordering::SeqCst) {
                let port = adb_port.load(Ordering::SeqCst);
                // Until Android is up there is no answer; that is not worth reporting.
                if port != 0
                    && let Ok(printed) =
                        adb::output_within(port, ORIENTATION_COMMAND, ORIENTATION_TIMEOUT)
                    && let Some(turns) = parse(&printed)
                    && turns != last
                {
                    last = turns;
                    changed(turns);
                }
                std::thread::sleep(ORIENTATION_POLL);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_is_read_from_the_display_service() {
        assert_eq!(parse("    mCurrentOrientation=1\n"), Some(1));
        assert_eq!(parse("mCurrentOrientation=3"), Some(3));
        assert_eq!(parse("mCurrentOrientation=4"), None);
        assert_eq!(parse("nothing here"), None);
    }

    #[test]
    fn upright_positions_land_where_android_drew_them() {
        // A portrait panel 720x1280. At ROTATION_90 Android draws its top-left corner (the clock)
        // at the panel's top-right, so the upright picture's top-left is the panel's top-right.
        let panel = (720, 1280);
        assert_eq!(upright_size(panel, 1), (1280, 720));
        assert_eq!(to_panel(0, 0, panel, 1), Some((719, 0)));
        assert_eq!(to_panel(1279, 719, panel, 1), Some((0, 1279)));
        assert_eq!(to_panel(0, 0, panel, 2), Some((719, 1279)));
        assert_eq!(to_panel(0, 0, panel, 3), Some((0, 1279)));
        assert_eq!(to_panel(5, 7, panel, 0), Some((5, 7)));
        assert_eq!(to_panel(1280, 0, panel, 1), None);
        assert_eq!(to_panel(0, 720, panel, 1), None);
    }

    #[test]
    fn every_turn_reaches_every_panel_pixel_once() {
        let panel = (3, 2);
        for turns in 0..TURNS {
            let (width, height) = upright_size(panel, turns);
            let mut seen = [false; 6];
            for y in 0..height {
                for x in 0..width {
                    let (px, py) = to_panel(x, y, panel, turns).unwrap();
                    let at = (py * panel.0 + px) as usize;
                    assert!(!seen[at], "turns {turns}");
                    seen[at] = true;
                }
            }
            assert!(seen.iter().all(|seen| *seen));
        }
    }

    #[test]
    fn frame_is_turned_upright() {
        // Panel 2x1: pixel A then B. A quarter turn shows them as a 1x2 column, B above A
        // (the panel's right end is the picture's top).
        let a = [1, 1, 1, 1];
        let b = [2, 2, 2, 2];
        let pixels = [a, b].concat();
        let frame = Frame {
            width: 2,
            height: 1,
            format: 0,
            pixels: &pixels,
        };
        let (width, height, turned) = upright(&frame, 1);
        assert_eq!((width, height), (1, 2));
        assert_eq!(turned, [b, a].concat());
    }
}

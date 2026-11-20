//! Matches the monitor's refresh rate to the guest's frames while the window is fullscreen. A
//! 60 fps game on a 144 Hz monitor shows its frames for two and three refreshes in turn, which
//! looks like stutter however even the game's frames are; at 120 Hz each frame shows for two.
//! The change is made for this program only: macOS undoes it when the program ends, crash or not.

use crate::config::GAME_FRAME_RATE;

/// The rate to switch a monitor running at `now` Hz to, from the rates it offers: the fastest
/// whole multiple of a game's frame rate. `None` when `now` already is one, or none is offered.
pub fn better_rate(now: f64, offered: &[f64]) -> Option<f64> {
    let frame = f64::from(GAME_FRAME_RATE);
    let whole = |hz: f64| hz >= frame && ((hz / frame).round() * frame - hz).abs() < 0.5;
    if whole(now) {
        return None;
    }
    offered
        .iter()
        .copied()
        .filter(|hz| whole(*hz))
        .max_by(f64::total_cmp)
}

mod cg {
    use std::ffi::c_void;

    pub type Display = u32;
    type Mode = *mut c_void;
    type Array = *const c_void;
    type Config = *mut c_void;
    /// `kCGConfigureForAppOnly`: undone when the program ends.
    const FOR_APP_ONLY: u32 = 0;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGDisplayCopyDisplayMode(display: Display) -> Mode;
        fn CGDisplayCopyAllDisplayModes(display: Display, options: *const c_void) -> Array;
        fn CGDisplayModeGetWidth(mode: Mode) -> usize;
        fn CGDisplayModeGetHeight(mode: Mode) -> usize;
        fn CGDisplayModeGetPixelWidth(mode: Mode) -> usize;
        fn CGDisplayModeGetRefreshRate(mode: Mode) -> f64;
        fn CGDisplayModeRelease(mode: Mode);
        fn CGBeginDisplayConfiguration(config: *mut Config) -> i32;
        fn CGConfigureDisplayWithDisplayMode(
            config: Config,
            display: Display,
            mode: Mode,
            options: *const c_void,
        ) -> i32;
        fn CGCompleteDisplayConfiguration(config: Config, option: u32) -> i32;
        fn CGCancelDisplayConfiguration(config: Config) -> i32;
        fn CGRestorePermanentDisplayConfiguration();
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(array: Array) -> isize;
        fn CFArrayGetValueAtIndex(array: Array, index: isize) -> Mode;
        fn CFRelease(object: *const c_void);
    }

    /// Switches `display` to the mode of the same size whose rate `pick` chooses from those
    /// offered. Returns the new rate, or `None` if nothing changed.
    pub fn switch(display: Display, pick: impl Fn(f64, &[f64]) -> Option<f64>) -> Option<f64> {
        // SAFETY: CoreGraphics calls on a display id; every copied object is released once, and
        // modes taken from the array are used only while the array is alive.
        unsafe {
            let current = CGDisplayCopyDisplayMode(display);
            if current.is_null() {
                return None;
            }
            let size = |mode: Mode| {
                (
                    CGDisplayModeGetWidth(mode),
                    CGDisplayModeGetHeight(mode),
                    CGDisplayModeGetPixelWidth(mode),
                )
            };
            let (now, want) = (CGDisplayModeGetRefreshRate(current), size(current));
            CGDisplayModeRelease(current);
            let all = CGDisplayCopyAllDisplayModes(display, std::ptr::null());
            if all.is_null() {
                return None;
            }
            let modes: Vec<Mode> = (0..CFArrayGetCount(all))
                .map(|index| CFArrayGetValueAtIndex(all, index))
                .filter(|mode| size(*mode) == want)
                .collect();
            let rates: Vec<f64> = modes
                .iter()
                .map(|mode| CGDisplayModeGetRefreshRate(*mode))
                .collect();
            let chosen = pick(now, &rates).and_then(|rate| {
                let mode = *modes.get(rates.iter().position(|hz| *hz == rate)?)?;
                let mut config: Config = std::ptr::null_mut();
                if CGBeginDisplayConfiguration(&mut config) != 0 {
                    return None;
                }
                if CGConfigureDisplayWithDisplayMode(config, display, mode, std::ptr::null()) != 0 {
                    CGCancelDisplayConfiguration(config);
                    return None;
                }
                (CGCompleteDisplayConfiguration(config, FOR_APP_ONLY) == 0).then_some(rate)
            });
            CFRelease(all);
            chosen
        }
    }

    /// Puts every display back the way the user set it.
    pub fn restore() {
        // SAFETY: no arguments; undoes this program's display changes.
        unsafe { CGRestorePermanentDisplayConfiguration() }
    }
}

/// The window went fullscreen on `display` (a CoreGraphics display id): runs it at a rate whole
/// frames fit. Returns the new rate if it changed.
pub fn enter(display: u32) -> Option<f64> {
    cg::switch(display, better_rate)
}

/// The window left fullscreen: the monitor goes back to the user's rate.
pub fn leave() {
    cg::restore();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_monitor_moves_to_the_fastest_rate_whole_frames_fit() {
        assert_eq!(
            better_rate(144.0, &[144.0, 120.0, 100.0, 60.0]),
            Some(120.0)
        );
        assert_eq!(better_rate(119.88, &[119.88, 60.0]), None);
        assert_eq!(better_rate(120.0, &[144.0, 120.0]), None);
        assert_eq!(better_rate(75.0, &[75.0, 60.0]), Some(60.0));
        assert_eq!(better_rate(144.0, &[144.0, 100.0]), None);
    }
}

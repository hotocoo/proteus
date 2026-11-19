//! Android's picture takes the window's shape: the window asks Android for a logical display size
//! (`wm size`) of the space's shape, and Android draws it, centred, into its fixed-size panel.
//! The window then shows only that part, so there are no bars, whatever shape the window is.

/// Asks Android for a logical display size (followed by `WxH`), or to drop the one it was given.
pub const WM_SIZE: &str = "wm size";
pub const WM_SIZE_RESET: &str = "wm size reset";
/// Runs what follows only once Android has finished booting: a size set earlier would be undone.
pub const WHEN_BOOTED: &str = "[ \"$(getprop sys.boot_completed)\" = 1 ] &&";
const PHYSICAL: &str = "Physical size:";
const OVERRIDE: &str = "Override size:";

/// What `wm size` printed: `Some(None)` for no logical size, `Some(Some(size))` for one, `None`
/// if it did not answer (Android's window manager not up yet, or an error).
pub fn parse_wm_size(printed: &str) -> Option<Option<(u32, u32)>> {
    if !printed.contains(PHYSICAL) {
        return None;
    }
    Some(printed.lines().find_map(|line| {
        let size = line.trim().strip_prefix(OVERRIDE)?.trim();
        crate::settings::parse_size(size)
    }))
}

/// The part of an upright panel of `physical` pixels that Android draws a logical display of
/// `logical` pixels (also upright) into: x, y, width, height. Android's own rule
/// (`LogicalDisplay.configureDisplayLocked`): the largest centred rectangle of the logical shape.
pub fn shown_rect(physical: (u32, u32), logical: (u32, u32)) -> (u32, u32, u32, u32) {
    let (pw, ph) = (u64::from(physical.0), u64::from(physical.1));
    let (lw, lh) = (u64::from(logical.0.max(1)), u64::from(logical.1.max(1)));
    let (width, height) = if pw * lh < ph * lw {
        (pw, lh * pw / lw)
    } else {
        (lw * ph / lh, ph)
    };
    let narrow = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
    (
        narrow((pw - width) / 2),
        narrow((ph - height) / 2),
        narrow(width),
        narrow(height),
    )
}

/// The logical size, upright, that fills a space `aspect` wide for each unit high on an upright
/// panel of `physical` pixels, at the panel's own pixel size. `None` when the panel's own shape is
/// close enough. The shape is kept to `range`, so a sliver of a window cannot make Android unusable,
/// and to the panel's own orientation: a portrait panel stays portrait (at most square) and a
/// landscape one landscape, so the same logical size, turned, still suits the other orientation,
/// and Android is never asked for a short, wide portrait display it cannot lay its launcher out in.
pub fn logical_for(
    physical: (u32, u32),
    aspect: f32,
    range: std::ops::RangeInclusive<f32>,
    tolerance: f32,
) -> Option<(u32, u32)> {
    let (width, height) = (physical.0 as f32, physical.1 as f32);
    if !aspect.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let panel = width / height;
    let (low, high) = match panel.partial_cmp(&1.0) {
        Some(std::cmp::Ordering::Less) => (*range.start(), 1.0),
        Some(std::cmp::Ordering::Greater) => (1.0, *range.end()),
        _ => (*range.start(), *range.end()),
    };
    let aspect = aspect.clamp(low, high);
    if (aspect / (width / height) - 1.0).abs() <= tolerance {
        return None;
    }
    // Even sizes: Android halves them to centre the picture.
    let even = |value: f32| ((value / 2.0).round() as u32 * 2).max(2);
    Some(if aspect * height <= width {
        (even(aspect * height), physical.1)
    } else {
        (physical.0, even(width / aspect))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGE: std::ops::RangeInclusive<f32> = (1.0 / 3.0)..=3.0;

    #[test]
    fn android_centres_a_shorter_display() {
        // Seen on the Cuttlefish image: `wm size 1080x1600` on a 1080x2400 panel draws rows 400
        // to 1999.
        assert_eq!(shown_rect((1080, 2400), (1080, 1600)), (0, 400, 1080, 1600));
        assert_eq!(shown_rect((2400, 1080), (1920, 1080)), (240, 0, 1920, 1080));
        assert_eq!(shown_rect((1080, 2400), (1080, 2400)), (0, 0, 1080, 2400));
    }

    #[test]
    fn a_wider_window_gets_a_wider_display_at_the_same_pixel_size() {
        // A 16:9 space on a landscape 2400x1080 panel.
        let logical = logical_for((2400, 1080), 16.0 / 9.0, RANGE, 0.01).unwrap();
        assert_eq!(logical, (1920, 1080));
        assert_eq!(shown_rect((2400, 1080), logical).2, 1920);
        // Wider than the panel: full width, shorter.
        let logical = logical_for((2400, 1080), 3.0, RANGE, 0.01).unwrap();
        assert_eq!(logical, (2400, 800));
    }

    #[test]
    fn a_portrait_panel_in_a_wide_window_becomes_square_at_most() {
        assert_eq!(
            logical_for((1080, 2400), 1.6, RANGE, 0.01),
            Some((1080, 1080))
        );
        assert_eq!(
            logical_for((2400, 1080), 0.5, RANGE, 0.01),
            Some((1080, 1080))
        );
    }

    #[test]
    fn what_wm_size_prints_is_read_back() {
        let set = "Physical size: 1080x2400\nOverride size: 1080x1976\n";
        assert_eq!(parse_wm_size(set), Some(Some((1080, 1976))));
        assert_eq!(parse_wm_size("Physical size: 1080x2400\n"), Some(None));
        assert_eq!(parse_wm_size("Error: Can't find service: window\n"), None);
    }

    #[test]
    fn the_panels_own_shape_needs_no_change() {
        assert_eq!(logical_for((1080, 2400), 0.451, RANGE, 0.01), None);
        assert_eq!(logical_for((1080, 2400), f32::NAN, RANGE, 0.01), None);
    }

    #[test]
    fn a_sliver_of_a_window_is_kept_to_the_range() {
        let logical = logical_for((1080, 2400), 0.05, RANGE, 0.01).unwrap();
        assert_eq!(logical, (800, 2400));
    }
}

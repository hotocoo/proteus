//! An EDID 1.4 block describing the display: its size in pixels, its refresh rate and its
//! physical size. Linux's virtio-gpu driver takes its display modes from it; Android's composer
//! takes its refresh rate from that mode (`clock / (htotal * vtotal)`), so this is what lets the
//! guest draw faster than the 60 Hz the driver falls back to without one.

/// Length of an EDID base block.
pub const EDID_LEN: usize = 128;
const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
/// "EMU" as three five-bit letters (A = 1), big-endian.
const MANUFACTURER: [u8; 2] = [0x15, 0xb5];
/// EDID 1.4.
const VERSION: [u8; 2] = [1, 4];
/// Digital input, colour depth and interface left undefined.
const DIGITAL_INPUT: u8 = 0x80;
/// Gamma 2.2, stored as (gamma * 100) - 100.
const GAMMA_2_2: u8 = 120;
/// RGB 4:4:4, sRGB is the default colour space, the first timing is the preferred one.
const FEATURES: u8 = 0x06;
/// sRGB's primaries and white point, as EDID stores chromaticity.
const SRGB_CHROMATICITY: [u8; 10] = [0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54];
/// A standard-timing slot that is not used.
const UNUSED_STANDARD_TIMING: [u8; 2] = [0x01, 0x01];
const STANDARD_TIMINGS_AT: usize = 38;
const STANDARD_TIMINGS: usize = 8;
const DESCRIPTORS_AT: usize = 54;
const DESCRIPTOR_LEN: usize = 18;
/// Display product name descriptor, then a dummy descriptor for the slots left.
const NAME_TAG: u8 = 0xfc;
const DUMMY_TAG: u8 = 0x10;
const NAME: &[u8] = b"emu";
const NAME_END: u8 = b'\n';
const NAME_PAD: u8 = b' ';
const EXTENSIONS_AT: usize = 126;
/// Digital separate sync, both syncs positive.
const SYNC_FLAGS: u8 = 0x1e;
/// Blanking around the picture. There is no real beam to wait for, so it is kept small, which
/// keeps the pixel clock low enough to fit EDID's 16-bit field at high refresh rates.
const H_FRONT_PORCH: u32 = 8;
const H_SYNC: u32 = 32;
const H_BACK_PORCH: u32 = 40;
const V_FRONT_PORCH: u32 = 3;
const V_SYNC: u32 = 5;
const V_BACK_PORCH: u32 = 22;
/// EDID stores the pixel clock in units of 10 kHz, in 16 bits.
const CLOCK_UNIT_HZ: u64 = 10_000;
const MAX_CLOCK_UNITS: u64 = u16::MAX as u64;
/// Sizes (pixels, blanking, millimetres) are 12-bit fields.
const MAX_FIELD: u32 = 0xfff;
const MM_PER_INCH: f64 = 25.4;

fn h_blank() -> u32 {
    H_FRONT_PORCH + H_SYNC + H_BACK_PORCH
}

fn v_blank() -> u32 {
    V_FRONT_PORCH + V_SYNC + V_BACK_PORCH
}

/// The pixel clock, in 10 kHz units, of a `width` x `height` display refreshed `hz` times a second.
/// `None` if it does not fit EDID's field.
pub fn clock_units(width: u32, height: u32, hz: u32) -> Option<u16> {
    let total = (u64::from(width) + u64::from(h_blank()))
        .saturating_mul(u64::from(height) + u64::from(v_blank()));
    let units = (total
        .saturating_mul(u64::from(hz))
        .saturating_add(CLOCK_UNIT_HZ / 2))
        / CLOCK_UNIT_HZ;
    (units > 0 && units <= MAX_CLOCK_UNITS).then_some(units as u16)
}

/// The fastest refresh rate a `width` x `height` display can have and still be described.
pub fn max_refresh(width: u32, height: u32) -> u32 {
    let total = (u64::from(width) + u64::from(h_blank()))
        .saturating_mul(u64::from(height) + u64::from(v_blank()));
    u32::try_from(MAX_CLOCK_UNITS * CLOCK_UNIT_HZ / total.max(1)).unwrap_or(u32::MAX)
}

/// The EDID of a `width` x `height` display at `hz`, claiming `dpi` pixels per inch.
/// `None` if a size or the pixel clock does not fit its field.
pub fn build(width: u32, height: u32, hz: u32, dpi: u32) -> Option<[u8; EDID_LEN]> {
    let (h_blank, v_blank) = (h_blank(), v_blank());
    if width == 0 || height == 0 || width > MAX_FIELD || height > MAX_FIELD || dpi == 0 {
        return None;
    }
    let clock = clock_units(width, height, hz)?;
    let mm = |pixels: u32| (f64::from(pixels) * MM_PER_INCH / f64::from(dpi)).round() as u32;
    let (width_mm, height_mm) = (mm(width).min(MAX_FIELD), mm(height).min(MAX_FIELD));

    let mut edid = [0u8; EDID_LEN];
    edid[..8].copy_from_slice(&HEADER);
    edid[8..10].copy_from_slice(&MANUFACTURER);
    edid[18..20].copy_from_slice(&VERSION);
    edid[20] = DIGITAL_INPUT;
    // Whole centimetres, at least one, so the size is not read as "unknown".
    edid[21] = (width_mm / 10).clamp(1, u32::from(u8::MAX)) as u8;
    edid[22] = (height_mm / 10).clamp(1, u32::from(u8::MAX)) as u8;
    edid[23] = GAMMA_2_2;
    edid[24] = FEATURES;
    edid[25..35].copy_from_slice(&SRGB_CHROMATICITY);
    for slot in 0..STANDARD_TIMINGS {
        let at = STANDARD_TIMINGS_AT + slot * 2;
        edid[at..at + 2].copy_from_slice(&UNUSED_STANDARD_TIMING);
    }

    let low = |value: u32| (value & 0xff) as u8;
    let high4 = |value: u32| ((value >> 8) & 0xf) as u8;
    let timing = &mut edid[DESCRIPTORS_AT..DESCRIPTORS_AT + DESCRIPTOR_LEN];
    timing[0..2].copy_from_slice(&clock.to_le_bytes());
    timing[2] = low(width);
    timing[3] = low(h_blank);
    timing[4] = (high4(width) << 4) | high4(h_blank);
    timing[5] = low(height);
    timing[6] = low(v_blank);
    timing[7] = (high4(height) << 4) | high4(v_blank);
    timing[8] = low(H_FRONT_PORCH);
    timing[9] = low(H_SYNC);
    timing[10] = (((V_FRONT_PORCH & 0xf) << 4) | (V_SYNC & 0xf)) as u8;
    timing[11] = 0;
    timing[12] = low(width_mm);
    timing[13] = low(height_mm);
    timing[14] = (high4(width_mm) << 4) | high4(height_mm);
    timing[17] = SYNC_FLAGS;

    let name = &mut edid[DESCRIPTORS_AT + DESCRIPTOR_LEN..DESCRIPTORS_AT + 2 * DESCRIPTOR_LEN];
    name[3] = NAME_TAG;
    name[5..].fill(NAME_PAD);
    name[5..5 + NAME.len()].copy_from_slice(NAME);
    name[5 + NAME.len()] = NAME_END;
    for slot in 2..4 {
        edid[DESCRIPTORS_AT + slot * DESCRIPTOR_LEN + 3] = DUMMY_TAG;
    }

    edid[EXTENSIONS_AT] = 0;
    let sum = edid[..EDID_LEN - 1]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    edid[EDID_LEN - 1] = 0u8.wrapping_sub(sum);
    Some(edid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Linux makes of the first detailed timing, and the rate Android's composer computes.
    fn decode(edid: &[u8; EDID_LEN]) -> (u32, u32, f64, u32, u32) {
        let t = &edid[DESCRIPTORS_AT..DESCRIPTORS_AT + DESCRIPTOR_LEN];
        let clock_khz = f64::from(u16::from_le_bytes([t[0], t[1]])) * 10.0;
        let width = u32::from(t[2]) | (u32::from(t[4] >> 4) << 8);
        let h_blank = u32::from(t[3]) | (u32::from(t[4] & 0xf) << 8);
        let height = u32::from(t[5]) | (u32::from(t[7] >> 4) << 8);
        let v_blank = u32::from(t[6]) | (u32::from(t[7] & 0xf) << 8);
        let width_mm = u32::from(t[12]) | (u32::from(t[14] >> 4) << 8);
        let height_mm = u32::from(t[13]) | (u32::from(t[14] & 0xf) << 8);
        let refresh =
            1000.0 * clock_khz / (f64::from(width + h_blank) * f64::from(height + v_blank));
        (width, height, refresh, width_mm, height_mm)
    }

    #[test]
    fn every_offered_rate_comes_back_from_the_timing() {
        for (width, height) in [(1080, 2400), (2400, 1080), (720, 1280), (1440, 3200)] {
            for hz in [60, 90, 120, 144] {
                if hz > max_refresh(width, height) {
                    assert!(build(width, height, hz, 320).is_none());
                    continue;
                }
                let edid = build(width, height, hz, 320).expect("fits");
                let (w, h, refresh, _, _) = decode(&edid);
                assert_eq!((w, h), (width, height));
                assert_eq!(
                    (refresh + 0.5) as u32,
                    hz,
                    "{width}x{height}@{hz}: {refresh}"
                );
            }
        }
    }

    #[test]
    fn the_block_is_valid() {
        let edid = build(1080, 2400, 120, 320).expect("fits");
        assert_eq!(edid[..8], HEADER);
        let sum = edid.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        assert_eq!(sum, 0);
        // The composer's parser needs a detailed timing first.
        assert!(edid[DESCRIPTORS_AT] != 0 || edid[DESCRIPTORS_AT + 1] != 0);
    }

    #[test]
    fn physical_size_gives_back_the_density() {
        let edid = build(1080, 2400, 60, 320).expect("fits");
        let (w, h, _, width_mm, height_mm) = decode(&edid);
        let dpi = |pixels: u32, mm: u32| f64::from(pixels) / f64::from(mm) * MM_PER_INCH;
        assert!((dpi(w, width_mm) - 320.0).abs() < 4.0);
        assert!((dpi(h, height_mm) - 320.0).abs() < 4.0);
    }

    #[test]
    fn a_clock_too_fast_for_the_field_is_refused() {
        assert!(clock_units(4000, 4000, 240).is_none());
        assert!(build(4000, 4000, 240, 320).is_none());
        assert!(build(5000, 100, 60, 320).is_none());
        assert!(build(1080, 2400, 0, 320).is_none());
    }
}

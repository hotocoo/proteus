//! Linux bootconfig, the channel Android 12+ requires for `androidboot.*` parameters.
//!
//! The kernel finds it at the very end of the ramdisk:
//! `[ramdisk][bootconfig text][zero padding][size: u32 LE][checksum: u32 LE]["#BOOTCONFIG\n"]`,
//! where `size` counts text plus padding, the checksum is the byte sum of the text, and padding makes the
//! whole ramdisk a multiple of four bytes. See `Documentation/admin-guide/bootconfig.rst`.

use crate::{ArchError, copy};

const MAGIC: &[u8; 12] = b"#BOOTCONFIG\n";
const ALIGN: usize = 4;
/// `XBC_DATA_MAX` in the kernel.
const MAX_TEXT_LEN: usize = 32 * 1024 - 1;
const FOOTER_LEN: usize = 2 * size_of::<u32>() + MAGIC.len();

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn valid_value(value: &str) -> bool {
    !value.bytes().any(|b| matches!(b, b'"' | b'\n' | 0))
}

/// Bytes to place directly after a ramdisk of `ramdisk_len` bytes.
pub fn trailer(params: &[(String, String)], ramdisk_len: usize) -> Result<Vec<u8>, ArchError> {
    let mut text = String::new();
    for (key, value) in params {
        if !valid_key(key) {
            return Err(ArchError::Bootconfig(copy::BOOTCONFIG_BAD_KEY));
        }
        if !valid_value(value) {
            return Err(ArchError::Bootconfig(copy::BOOTCONFIG_BAD_VALUE));
        }
        text.push_str(&format!("{key} = \"{value}\"\n"));
    }
    // The kernel treats the text as a C string.
    let mut out = text.into_bytes();
    out.push(0);
    if out.len() > MAX_TEXT_LEN {
        return Err(ArchError::Bootconfig(copy::BOOTCONFIG_TOO_LARGE));
    }
    let checksum = out
        .iter()
        .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
    let unpadded = ramdisk_len + out.len() + FOOTER_LEN;
    out.resize(out.len() + unpadded.next_multiple_of(ALIGN) - unpadded, 0);
    let size = u32::try_from(out.len()).expect("bounded by MAX_TEXT_LEN plus padding");
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(MAGIC);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// What the kernel does in `get_boot_config_from_initrd`: walk back from the end of the ramdisk.
    fn parse(ramdisk: &[u8]) -> Option<&[u8]> {
        let (rest, magic) = ramdisk.split_at_checked(ramdisk.len().checked_sub(MAGIC.len())?)?;
        if magic != MAGIC {
            return None;
        }
        let (rest, checksum) = rest.split_at_checked(rest.len().checked_sub(4)?)?;
        let (rest, size) = rest.split_at_checked(rest.len().checked_sub(4)?)?;
        let size = u32::from_le_bytes(size.try_into().unwrap()) as usize;
        let data = &rest[rest.len().checked_sub(size)?..];
        let sum = data
            .iter()
            .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
        (sum == u32::from_le_bytes(checksum.try_into().unwrap())).then_some(data)
    }

    #[test]
    fn round_trips_for_every_ramdisk_alignment() {
        let params = params(&[
            ("androidboot.hardware", "emu"),
            ("androidboot.serialno", "EMU0001"),
        ]);
        for ramdisk_len in 0..8 {
            let mut ramdisk = vec![0xaa; ramdisk_len];
            ramdisk.extend(trailer(&params, ramdisk_len).unwrap());
            assert_eq!(ramdisk.len() % ALIGN, 0, "ramdisk of {ramdisk_len} bytes");
            let data = parse(&ramdisk).expect("kernel-side parse");
            let text = std::str::from_utf8(data).unwrap().trim_end_matches('\0');
            assert_eq!(
                text,
                "androidboot.hardware = \"emu\"\nandroidboot.serialno = \"EMU0001\"\n"
            );
        }
    }

    #[test]
    fn rejects_values_that_would_break_out_of_the_quotes() {
        for bad in ["a\"b", "a\nb", "a\0b"] {
            assert!(matches!(
                trailer(&params(&[("k", bad)]), 0),
                Err(ArchError::Bootconfig(_))
            ));
        }
    }

    #[test]
    fn rejects_malformed_keys() {
        for bad in ["", "a b", "a=b", "a\nb"] {
            assert!(matches!(
                trailer(&params(&[(bad, "v")]), 0),
                Err(ArchError::Bootconfig(_))
            ));
        }
    }

    #[test]
    fn rejects_oversized_text() {
        let big = "x".repeat(MAX_TEXT_LEN);
        assert!(matches!(
            trailer(&params(&[("k", &big)]), 0),
            Err(ArchError::Bootconfig(_))
        ));
    }
}

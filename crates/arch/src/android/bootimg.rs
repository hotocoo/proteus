//! Android boot image headers, versions 3 and 4 (`system/tools/mkbootimg/include/bootimg/bootimg.h`).
//!
//! Nothing is unpacked: the VMM only needs to know which byte ranges of the files hold ramdisks, and
//! copies those ranges into guest memory as they are. Headers come from files the user supplies and are
//! checked against the file size before any range is returned.

use std::ops::Range;

use crate::{ArchError, copy};

const BOOT_MAGIC: &[u8; 8] = b"ANDROID!";
const VENDOR_BOOT_MAGIC: &[u8; 8] = b"VNDRBOOT";
/// Page size of every boot image from header version 3 on.
const BOOT_PAGE_SIZE: u64 = 4096;
const MIN_VERSION: u32 = 3;
const MAX_VERSION: u32 = 4;

// `boot_img_hdr_v3` field offsets.
const BOOT_KERNEL_SIZE_AT: usize = 8;
const BOOT_RAMDISK_SIZE_AT: usize = 12;
const BOOT_HEADER_VERSION_AT: usize = 40;
/// Bytes of a boot image header this parser reads.
pub const BOOT_HEADER_LEN: usize = 44;

// `vendor_boot_img_hdr_v3` / `_v4` field offsets.
const VENDOR_HEADER_VERSION_AT: usize = 8;
const VENDOR_PAGE_SIZE_AT: usize = 12;
const VENDOR_RAMDISK_SIZE_AT: usize = 24;
const VENDOR_CMDLINE_AT: usize = 28;
const VENDOR_CMDLINE_LEN: usize = 2048;
const VENDOR_HEADER_SIZE_AT: usize = 2096;
const VENDOR_DTB_SIZE_AT: usize = 2100;
const VENDOR_TABLE_SIZE_AT: usize = 2112;
const VENDOR_BOOTCONFIG_SIZE_AT: usize = 2124;
/// Bytes of a vendor boot image header this parser reads (the whole version 4 header).
pub const VENDOR_HEADER_LEN: usize = 2128;

fn u32_at(bytes: &[u8], at: usize) -> u64 {
    let word: [u8; 4] = bytes[at..at + 4].try_into().expect("slice is four bytes");
    u64::from(u32::from_le_bytes(word))
}

fn pages(len: u64, page: u64) -> u64 {
    len.div_ceil(page) * page
}

fn section(start: u64, len: u64, file_len: u64) -> Result<Range<u64>, ArchError> {
    match start.checked_add(len) {
        Some(end) if end <= file_len => Ok(start..end),
        _ => Err(ArchError::BootImage(copy::BOOT_IMAGE_TRUNCATED)),
    }
}

fn check(
    header: &[u8],
    needed: usize,
    magic: &[u8; 8],
    version_at: usize,
) -> Result<(), ArchError> {
    if header.len() < needed || &header[..magic.len()] != magic {
        return Err(ArchError::BootImage(copy::BOOT_IMAGE_BAD_MAGIC));
    }
    let version = u32_at(header, version_at);
    if !(u64::from(MIN_VERSION)..=u64::from(MAX_VERSION)).contains(&version) {
        return Err(ArchError::BootImage(copy::BOOT_IMAGE_BAD_VERSION));
    }
    Ok(())
}

/// Where the ramdisk sits in a `boot.img` or `init_boot.img`. Empty if the image carries none.
pub fn boot_ramdisk(header: &[u8], file_len: u64) -> Result<Range<u64>, ArchError> {
    check(header, BOOT_HEADER_LEN, BOOT_MAGIC, BOOT_HEADER_VERSION_AT)?;
    let kernel_len = u32_at(header, BOOT_KERNEL_SIZE_AT);
    let start = BOOT_PAGE_SIZE + pages(kernel_len, BOOT_PAGE_SIZE);
    section(start, u32_at(header, BOOT_RAMDISK_SIZE_AT), file_len)
}

#[derive(Debug, PartialEq, Eq)]
pub struct VendorBoot {
    /// Every vendor ramdisk fragment, back to back.
    pub ramdisk: Range<u64>,
    /// Bootconfig text the vendor baked into the image (version 4 only; empty otherwise).
    pub bootconfig: Range<u64>,
    /// Kernel arguments the vendor wants.
    pub cmdline: String,
}

pub fn vendor_boot(header: &[u8], file_len: u64) -> Result<VendorBoot, ArchError> {
    check(
        header,
        VENDOR_HEADER_LEN,
        VENDOR_BOOT_MAGIC,
        VENDOR_HEADER_VERSION_AT,
    )?;
    let page = u32_at(header, VENDOR_PAGE_SIZE_AT);
    if !page.is_power_of_two() {
        return Err(ArchError::BootImage(copy::BOOT_IMAGE_BAD_PAGE_SIZE));
    }
    let ramdisk_start = pages(u32_at(header, VENDOR_HEADER_SIZE_AT), page);
    let ramdisk = section(
        ramdisk_start,
        u32_at(header, VENDOR_RAMDISK_SIZE_AT),
        file_len,
    )?;
    let dtb_start = pages(ramdisk.end, page);
    let dtb = section(dtb_start, u32_at(header, VENDOR_DTB_SIZE_AT), file_len)?;
    let has_v4_fields = u32_at(header, VENDOR_HEADER_VERSION_AT) == u64::from(MAX_VERSION);
    let (table_len, bootconfig_len) = if has_v4_fields {
        (
            u32_at(header, VENDOR_TABLE_SIZE_AT),
            u32_at(header, VENDOR_BOOTCONFIG_SIZE_AT),
        )
    } else {
        (0, 0)
    };
    let table = section(pages(dtb.end, page), table_len, file_len)?;
    let bootconfig = section(pages(table.end, page), bootconfig_len, file_len)?;
    let cmdline = &header[VENDOR_CMDLINE_AT..VENDOR_CMDLINE_AT + VENDOR_CMDLINE_LEN];
    let cmdline = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
    Ok(VendorBoot {
        ramdisk,
        bootconfig,
        cmdline: String::from_utf8_lossy(cmdline).trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(header: &mut [u8], at: usize, value: u32) {
        header[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn boot_header(kernel_len: u32, ramdisk_len: u32, version: u32) -> Vec<u8> {
        let mut header = vec![0u8; BOOT_HEADER_LEN];
        header[..8].copy_from_slice(BOOT_MAGIC);
        put(&mut header, BOOT_KERNEL_SIZE_AT, kernel_len);
        put(&mut header, BOOT_RAMDISK_SIZE_AT, ramdisk_len);
        put(&mut header, BOOT_HEADER_VERSION_AT, version);
        header
    }

    fn vendor_header(
        version: u32,
        page: u32,
        ramdisk: u32,
        dtb: u32,
        table: u32,
        bootconfig: u32,
    ) -> Vec<u8> {
        let mut header = vec![0u8; VENDOR_HEADER_LEN];
        header[..8].copy_from_slice(VENDOR_BOOT_MAGIC);
        put(&mut header, VENDOR_HEADER_VERSION_AT, version);
        put(&mut header, VENDOR_PAGE_SIZE_AT, page);
        put(&mut header, VENDOR_RAMDISK_SIZE_AT, ramdisk);
        put(&mut header, VENDOR_HEADER_SIZE_AT, VENDOR_HEADER_LEN as u32);
        put(&mut header, VENDOR_DTB_SIZE_AT, dtb);
        put(&mut header, VENDOR_TABLE_SIZE_AT, table);
        put(&mut header, VENDOR_BOOTCONFIG_SIZE_AT, bootconfig);
        header[VENDOR_CMDLINE_AT..VENDOR_CMDLINE_AT + 14].copy_from_slice(b"panic=-1 quiet");
        header
    }

    #[test]
    fn init_boot_ramdisk_follows_the_header_page() {
        assert_eq!(
            boot_ramdisk(&boot_header(0, 5000, 4), 1 << 20).unwrap(),
            4096..9096
        );
    }

    #[test]
    fn boot_ramdisk_follows_the_page_padded_kernel() {
        assert_eq!(
            boot_ramdisk(&boot_header(4097, 10, 3), 1 << 20).unwrap(),
            12288..12298
        );
        assert_eq!(
            boot_ramdisk(&boot_header(8192, 0, 4), 1 << 20).unwrap(),
            12288..12288
        );
    }

    #[test]
    fn vendor_boot_sections_are_page_aligned_in_order() {
        // Header fits one 4096-byte page; ramdisk 5000 bytes ends in page 2; dtb, table, bootconfig follow.
        let layout = vendor_boot(&vendor_header(4, 4096, 5000, 100, 108, 77), 1 << 20).unwrap();
        assert_eq!(layout.ramdisk, 4096..9096);
        assert_eq!(layout.bootconfig, 20480..20557);
        assert_eq!(layout.cmdline, "panic=-1 quiet");
        // Version 3 has neither a ramdisk table nor bootconfig.
        let v3 = vendor_boot(&vendor_header(3, 4096, 5000, 100, 108, 77), 1 << 20).unwrap();
        assert!(v3.bootconfig.is_empty());
    }

    #[test]
    fn page_size_other_than_4096_is_honoured() {
        let layout = vendor_boot(&vendor_header(4, 2048, 10, 0, 0, 5), 1 << 20).unwrap();
        assert_eq!(layout.ramdisk, 4096..4106);
        assert_eq!(layout.bootconfig, 6144..6149);
    }

    #[test]
    fn foreign_old_and_truncated_images_are_refused() {
        let mut wrong_magic = boot_header(0, 10, 4);
        wrong_magic[0] = b'X';
        assert!(boot_ramdisk(&wrong_magic, 1 << 20).is_err());
        assert!(boot_ramdisk(&boot_header(0, 10, 2), 1 << 20).is_err());
        assert!(boot_ramdisk(&boot_header(0, 10, 5), 1 << 20).is_err());
        assert!(boot_ramdisk(&boot_header(0, 10, 4)[..BOOT_HEADER_LEN - 1], 1 << 20).is_err());
        // Sizes that point past the end of the file.
        assert!(boot_ramdisk(&boot_header(0, 5000, 4), 9095).is_err());
        assert!(boot_ramdisk(&boot_header(u32::MAX, u32::MAX, 4), u64::MAX).is_ok());
        assert!(vendor_boot(&vendor_header(4, 4096, 5000, 0, 0, 77), 12000).is_err());
        assert!(vendor_boot(&vendor_header(4, 0, 10, 0, 0, 0), 1 << 20).is_err());
        assert!(vendor_boot(&vendor_header(4, 3000, 10, 0, 0, 0), 1 << 20).is_err());
        assert!(vendor_boot(&boot_header(0, 10, 4), 1 << 20).is_err());
    }
}

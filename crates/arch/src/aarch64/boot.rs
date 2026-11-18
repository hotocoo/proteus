//! Direct kernel boot: place an uncompressed ARM64 `Image`, the ramdisk and the device tree in guest RAM
//! and describe the register state the boot protocol expects (`Documentation/arch/arm64/booting.rst`).

use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;

use hv::Regs;
use hv::aarch64::PSTATE_EL1H_DAIF_MASKED;
use vm_memory::{Bytes, GuestAddress, GuestMemory, ReadVolatile};

use super::layout;
use crate::{ArchError, bootconfig, copy};

const HEADER_LEN: usize = 64;
const TEXT_OFFSET_AT: usize = 8;
const IMAGE_SIZE_AT: usize = 16;
const MAGIC_AT: usize = 56;
const MAGIC: &[u8; 4] = b"ARM\x64";

#[derive(Debug, PartialEq, Eq)]
pub struct ImageHeader {
    /// Where the image wants to sit, relative to the start of RAM.
    pub text_offset: u64,
    /// Memory the kernel occupies once running, including BSS. Zero in images older than v3.17.
    pub image_size: u64,
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(
        bytes[at..at + size_of::<u64>()]
            .try_into()
            .expect("slice is eight bytes"),
    )
}

pub fn parse_header(header: &[u8]) -> Result<ImageHeader, ArchError> {
    if header.len() < HEADER_LEN {
        return Err(ArchError::KernelImage(copy::KERNEL_TOO_SHORT));
    }
    if &header[MAGIC_AT..MAGIC_AT + MAGIC.len()] != MAGIC {
        return Err(ArchError::KernelImage(copy::KERNEL_BAD_MAGIC));
    }
    let text_offset = le_u64(header, TEXT_OFFSET_AT);
    if !text_offset.is_multiple_of(layout::KERNEL_ALIGN) {
        return Err(ArchError::KernelImage(copy::KERNEL_BAD_OFFSET));
    }
    Ok(ImageHeader {
        text_offset,
        image_size: le_u64(header, IMAGE_SIZE_AT),
    })
}

fn stream_len(stream: &mut impl Seek) -> Result<u64, ArchError> {
    Ok(stream.seek(SeekFrom::End(0))?)
}

fn copy_in<M, F>(mem: &M, at: u64, source: &mut F, len: u64) -> Result<(), ArchError>
where
    M: GuestMemory,
    F: ReadVolatile + Seek,
{
    source.rewind()?;
    let len = usize::try_from(len).map_err(|_| ArchError::Layout(copy::RAM_TOO_SMALL))?;
    Ok(mem.read_exact_volatile_from(GuestAddress(at), source, len)?)
}

/// Copies the kernel to its load address and returns the entry point.
pub fn load_kernel<M, F>(mem: &M, image: &mut F) -> Result<u64, ArchError>
where
    M: GuestMemory,
    F: Read + ReadVolatile + Seek,
{
    let file_len = stream_len(image)?;
    image.rewind()?;
    let mut header = [0u8; HEADER_LEN];
    image
        .read_exact(&mut header)
        .map_err(|_| ArchError::KernelImage(copy::KERNEL_TOO_SHORT))?;
    let header = parse_header(&header)?;
    let footprint = header.image_size.max(file_len);
    let end = header.text_offset.checked_add(footprint);
    if end.is_none_or(|end| end > layout::INITRD_OFFSET) {
        return Err(ArchError::Layout(copy::KERNEL_OVERLAPS_INITRD));
    }
    let entry = layout::RAM_BASE + header.text_offset;
    copy_in(mem, entry, image, file_len)?;
    Ok(entry)
}

/// Copies the ramdisk in and appends the bootconfig trailer when there are parameters.
/// Returns the guest range the device tree must advertise.
pub fn load_initrd<M, F>(
    mem: &M,
    initrd: &mut F,
    bootconfig: &[(String, String)],
) -> Result<Range<u64>, ArchError>
where
    M: GuestMemory,
    F: ReadVolatile + Seek,
{
    let len = stream_len(initrd)?;
    let host_len =
        usize::try_from(len).map_err(|_| ArchError::Layout(copy::INITRD_OVERLAPS_FDT))?;
    let trailer = if bootconfig.is_empty() {
        Vec::new()
    } else {
        bootconfig::trailer(bootconfig, host_len)?
    };
    let total = len + trailer.len() as u64;
    if total > layout::FDT_OFFSET - layout::INITRD_OFFSET {
        return Err(ArchError::Layout(copy::INITRD_OVERLAPS_FDT));
    }
    let start = layout::RAM_BASE + layout::INITRD_OFFSET;
    copy_in(mem, start, initrd, len)?;
    mem.write_slice(&trailer, GuestAddress(start + len))?;
    Ok(start..start + total)
}

/// Copies the device tree in and returns its guest address.
pub fn load_fdt<M: GuestMemory>(mem: &M, blob: &[u8]) -> Result<u64, ArchError> {
    if blob.len() as u64 > layout::FDT_MAX_SIZE {
        return Err(ArchError::Layout(copy::FDT_TOO_LARGE));
    }
    let at = layout::RAM_BASE + layout::FDT_OFFSET;
    mem.write_slice(blob, GuestAddress(at))?;
    Ok(at)
}

/// Register state for the primary CPU: EL1h, interrupts masked, MMU off, `x0` = device tree.
pub fn primary_regs(entry: u64, fdt: u64) -> Regs {
    let mut regs = entry_regs(entry);
    regs.x[0] = fdt;
    regs
}

/// Register state for a secondary CPU started by PSCI `CPU_ON`: same mode, `x0` = caller's context.
pub fn secondary_regs(entry: u64, context: u64) -> Regs {
    let mut regs = entry_regs(entry);
    regs.x[0] = context;
    regs
}

fn entry_regs(entry: u64) -> Regs {
    Regs {
        pc: entry,
        pstate: PSTATE_EL1H_DAIF_MASKED,
        ..Regs::default()
    }
}

/// Checks a RAM size against the fixed placement above.
pub fn check_ram_size(ram_size: u64) -> Result<(), ArchError> {
    if ram_size < layout::RAM_MIN_SIZE {
        return Err(ArchError::Layout(copy::RAM_TOO_SMALL));
    }
    if ram_size > layout::RAM_MAX_SIZE {
        return Err(ArchError::Layout(copy::RAM_TOO_LARGE));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use vm_memory::GuestMemoryMmap;

    use super::*;

    const TEST_RAM: usize = layout::RAM_MIN_SIZE as usize;

    fn ram() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(layout::RAM_BASE), TEST_RAM)]).unwrap()
    }

    fn image(text_offset: u64, image_size: u64, body_len: usize) -> Vec<u8> {
        let mut bytes = vec![0x5a; HEADER_LEN + body_len];
        bytes[TEXT_OFFSET_AT..TEXT_OFFSET_AT + 8].copy_from_slice(&text_offset.to_le_bytes());
        bytes[IMAGE_SIZE_AT..IMAGE_SIZE_AT + 8].copy_from_slice(&image_size.to_le_bytes());
        bytes[MAGIC_AT..MAGIC_AT + 4].copy_from_slice(MAGIC);
        bytes
    }

    #[test]
    fn header_fields_are_read() {
        let header = parse_header(&image(layout::KERNEL_ALIGN, 0x1234, 0)).unwrap();
        assert_eq!(
            header,
            ImageHeader {
                text_offset: layout::KERNEL_ALIGN,
                image_size: 0x1234
            }
        );
    }

    #[test]
    fn compressed_or_foreign_kernels_are_refused() {
        let mut gzip = image(0, 0, 0);
        gzip[MAGIC_AT] = 0;
        assert!(matches!(
            parse_header(&gzip),
            Err(ArchError::KernelImage(_))
        ));
        assert!(matches!(
            parse_header(&[0; HEADER_LEN - 1]),
            Err(ArchError::KernelImage(_))
        ));
        assert!(matches!(
            parse_header(&image(4096, 0, 0)),
            Err(ArchError::KernelImage(_))
        ));
    }

    #[test]
    fn kernel_lands_at_ram_base_plus_offset() {
        let mem = ram();
        let bytes = image(layout::KERNEL_ALIGN, 0, 100);
        let entry = load_kernel(&mem, &mut Cursor::new(bytes.clone())).unwrap();
        assert_eq!(entry, layout::RAM_BASE + layout::KERNEL_ALIGN);
        let mut loaded = vec![0; bytes.len()];
        mem.read_slice(&mut loaded, GuestAddress(entry)).unwrap();
        assert_eq!(loaded, bytes);
    }

    #[test]
    fn kernel_whose_bss_reaches_the_ramdisk_is_refused() {
        let bytes = image(0, layout::INITRD_OFFSET + 1, 0);
        assert!(matches!(
            load_kernel(&ram(), &mut Cursor::new(bytes)),
            Err(ArchError::Layout(_))
        ));
    }

    #[test]
    fn ramdisk_range_covers_the_bootconfig_trailer() {
        let mem = ram();
        let ramdisk = vec![7u8; 1001];
        let params = vec![("androidboot.hardware".to_string(), "emu".to_string())];
        let range = load_initrd(&mem, &mut Cursor::new(ramdisk.clone()), &params).unwrap();
        assert_eq!(range.start, layout::RAM_BASE + layout::INITRD_OFFSET);
        let total = (range.end - range.start) as usize;
        assert!(total > ramdisk.len());
        assert_eq!(total % 4, 0);
        let mut tail = [0u8; 12];
        mem.read_slice(&mut tail, GuestAddress(range.end - 12))
            .unwrap();
        assert_eq!(&tail, b"#BOOTCONFIG\n");
        // Without parameters nothing is appended.
        let plain = load_initrd(&mem, &mut Cursor::new(ramdisk.clone()), &[]).unwrap();
        assert_eq!(plain.end - plain.start, ramdisk.len() as u64);
    }

    #[test]
    fn ram_size_limits() {
        assert!(check_ram_size(layout::RAM_MIN_SIZE).is_ok());
        assert!(check_ram_size(layout::RAM_MIN_SIZE - 1).is_err());
        assert!(check_ram_size(layout::RAM_MAX_SIZE + 1).is_err());
    }

    #[test]
    fn primary_enters_at_el1_with_the_device_tree_in_x0() {
        let regs = primary_regs(0x8000_0000, 0x8fe0_0000);
        assert_eq!(
            (regs.pc, regs.x[0], regs.pstate),
            (0x8000_0000, 0x8fe0_0000, PSTATE_EL1H_DAIF_MASKED)
        );
    }
}

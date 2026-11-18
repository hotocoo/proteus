//! Direct kernel boot on x86_64, by the 64-bit Linux boot protocol (`Documentation/arch/x86/boot.rst`):
//! the kernel, ramdisk and command line in RAM, `boot_params` with the memory map, identity-mapped
//! page tables, and the CPU already in long mode at the kernel's 64-bit entry point.

use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;

use hv::x86_64::{
    CR0_ET, CR0_PE, CR0_PG, CR4_PAE, EFER_LMA, EFER_LME, RBP, RFLAGS_RESERVED, RSI, RSP, Regs,
    Segment, SystemRegs, Table,
};
use vm_memory::{Bytes, GuestAddress, GuestMemory, ReadVolatile};

use super::layout::{
    BOOT_STACK, CMDLINE_ADDR, CMDLINE_MAX, EBDA_START, GDT_ADDR, HIMEM_START, IDT_ADDR,
    MMIO_GAP_END, MMIO_GAP_START, PDE_ADDR, PDPTE_ADDR, PML4_ADDR, ZERO_PAGE,
};
use crate::{ArchError, copy};

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const ELF_CLASS_64: u8 = 2;
const ELF_HEADER_LEN: usize = 64;
const ELF_PHDR_LEN: usize = 56;
const PT_LOAD: u32 = 1;

// boot_params and its setup header, by offset into the zero page.
const BP_E820_ENTRIES: usize = 0x1e8;
const BP_SETUP_SECTS: usize = 0x1f1;
const BP_BOOT_FLAG: usize = 0x1fe;
const BP_JUMP: usize = 0x200;
const BP_HEADER: usize = 0x202;
const BP_VERSION: usize = 0x206;
const BP_TYPE_OF_LOADER: usize = 0x210;
const BP_LOADFLAGS: usize = 0x211;
const BP_RAMDISK_IMAGE: usize = 0x218;
const BP_RAMDISK_SIZE: usize = 0x21c;
const BP_HEAP_END_PTR: usize = 0x224;
const BP_CMD_LINE_PTR: usize = 0x228;
const BP_INITRD_ADDR_MAX: usize = 0x22c;
const BP_KERNEL_ALIGNMENT: usize = 0x230;
const BP_XLOADFLAGS: usize = 0x236;
const BP_INIT_SIZE: usize = 0x260;
const BP_E820_TABLE: usize = 0x2d0;
const ZERO_PAGE_LEN: usize = 0x1000;
const E820_ENTRY_LEN: usize = 20;

const BOOT_FLAG: u16 = 0xaa55;
const HEADER_MAGIC: &[u8; 4] = b"HdrS";
/// Boot protocol 2.12, the first with `xloadflags`.
const MIN_VERSION: u16 = 0x020c;
/// "Undefined" loader: one with no ID assigned.
const LOADER_UNDEFINED: u8 = 0xff;
const LOADFLAGS_CAN_USE_HEAP: u8 = 0x80;
/// End of the setup heap, relative to the real-mode code; the value boot.rst suggests.
const HEAP_END: u16 = 0xfe00;
const XLF_KERNEL_64: u16 = 1;
/// The 64-bit entry point sits this far into a bzImage's protected-mode kernel.
const ENTRY_64_OFFSET: u64 = 0x200;
const SECTOR: u64 = 512;
/// A bzImage that says it has no setup sectors has four (old kernels).
const DEFAULT_SETUP_SECTS: u64 = 4;
/// What the protocol says older kernels accept as the highest ramdisk address.
const DEFAULT_INITRD_ADDR_MAX: u64 = 0x37ff_ffff;
const KERNEL_ALIGNMENT: u32 = 0x100_0000;
const PAGE: u64 = 0x1000;
const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;

// Page-table entries: present, writable, and for the directory a 2 MiB page.
const PTE_PRESENT_WRITABLE: u64 = 0b11;
const PTE_HUGE: u64 = 1 << 7;
const HUGE_PAGE: u64 = 2 << 20;
const ENTRIES_PER_TABLE: u64 = 512;

// The GDT: null, 64-bit code, data, task state.
const GDT: [u64; 4] = [
    0,
    0x00af_9b00_0000_ffff,
    0x00cf_9300_0000_ffff,
    0x008f_8b00_0000_ffff,
];
const CODE_SELECTOR: u16 = 1 << 3;
const DATA_SELECTOR: u16 = 2 << 3;
const TSS_SELECTOR: u16 = 3 << 3;

fn le_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("slice is four bytes"))
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("slice is eight bytes"))
}

/// A kernel in guest memory.
#[derive(Debug, PartialEq, Eq)]
pub struct Kernel {
    /// Where the 64-bit kernel is entered.
    pub entry: u64,
    /// A bzImage's setup header, from `BP_SETUP_SECTS` on, to go into `boot_params` as it is.
    pub setup_header: Option<Vec<u8>>,
}

/// Copies the kernel in: an uncompressed ELF `vmlinux` at its physical addresses, or a bzImage's
/// protected-mode part at one megabyte. `ram_end` is where the RAM below the device hole ends.
pub fn load_kernel<M, F>(mem: &M, image: &mut F, ram_end: u64) -> Result<Kernel, ArchError>
where
    M: GuestMemory,
    F: Read + ReadVolatile + Seek,
{
    let file_len = image.seek(SeekFrom::End(0))?;
    image.rewind()?;
    let mut start = [0u8; ZERO_PAGE_LEN];
    let read = image.read(&mut start)?;
    if start[..4] == *ELF_MAGIC {
        return load_elf(mem, image, &start[..read], ram_end);
    }
    if read < BP_INIT_SIZE + 4 || start[BP_HEADER..BP_HEADER + 4] != *HEADER_MAGIC {
        return Err(ArchError::KernelImage(copy::X86_NOT_A_KERNEL));
    }
    if le_u16(&start, BP_VERSION) < MIN_VERSION
        || le_u16(&start, BP_XLOADFLAGS) & XLF_KERNEL_64 == 0
    {
        return Err(ArchError::KernelImage(copy::X86_NO_64_BIT_ENTRY));
    }
    let setup_sects = match u64::from(start[BP_SETUP_SECTS]) {
        0 => DEFAULT_SETUP_SECTS,
        sectors => sectors,
    };
    let offset = (setup_sects + 1) * SECTOR;
    let init_size = u64::from(le_u32(&start, BP_INIT_SIZE));
    let len = file_len.saturating_sub(offset);
    if HIMEM_START + init_size.max(len) > ram_end {
        return Err(ArchError::Layout(copy::RAM_TOO_SMALL));
    }
    image.seek(SeekFrom::Start(offset))?;
    mem.read_exact_volatile_from(GuestAddress(HIMEM_START), image, len as usize)?;
    // The header runs to the end the jump at its start points past.
    let header_end = BP_HEADER + usize::from(start[BP_JUMP + 1]);
    Ok(Kernel {
        entry: HIMEM_START + ENTRY_64_OFFSET,
        setup_header: Some(start[BP_SETUP_SECTS..header_end.min(read)].to_vec()),
    })
}

fn load_elf<M, F>(mem: &M, image: &mut F, start: &[u8], ram_end: u64) -> Result<Kernel, ArchError>
where
    M: GuestMemory,
    F: Read + ReadVolatile + Seek,
{
    let bad = || ArchError::KernelImage(copy::X86_BAD_ELF);
    if start.len() < ELF_HEADER_LEN || start[4] != ELF_CLASS_64 {
        return Err(bad());
    }
    let entry = le_u64(start, 0x18);
    let phoff = le_u64(start, 0x20);
    let phnum = usize::from(le_u16(start, 0x38));
    if usize::from(le_u16(start, 0x36)) != ELF_PHDR_LEN {
        return Err(bad());
    }
    let mut headers = vec![0u8; phnum * ELF_PHDR_LEN];
    image.seek(SeekFrom::Start(phoff))?;
    image.read_exact(&mut headers).map_err(|_| bad())?;
    for header in headers.as_chunks::<ELF_PHDR_LEN>().0 {
        if le_u32(header, 0) != PT_LOAD {
            continue;
        }
        let (offset, paddr) = (le_u64(header, 0x08), le_u64(header, 0x18));
        let (filesz, memsz) = (le_u64(header, 0x20), le_u64(header, 0x28));
        let fits =
            paddr >= HIMEM_START && paddr.checked_add(memsz).is_some_and(|end| end <= ram_end);
        if !fits || filesz > memsz {
            return Err(ArchError::Layout(copy::RAM_TOO_SMALL));
        }
        image.seek(SeekFrom::Start(offset))?;
        mem.read_exact_volatile_from(GuestAddress(paddr), image, filesz as usize)?;
    }
    Ok(Kernel {
        entry,
        setup_header: None,
    })
}

pub use crate::ramdisk::RamdiskPart;

/// Copies the ramdisk parts and the bootconfig trailer in, as high as the kernel accepts below
/// `ram_end`, page-aligned. Returns where they landed.
pub fn load_ramdisk<M, F>(
    mem: &M,
    parts: &mut [RamdiskPart<'_, F>],
    bootconfig: &[(String, String)],
    kernel: &Kernel,
    ram_end: u64,
) -> Result<Range<u64>, ArchError>
where
    M: GuestMemory,
    F: ReadVolatile + Seek,
{
    let len = crate::ramdisk::total_len(parts, bootconfig)?;
    let header_max = kernel.setup_header.as_ref().and_then(|header| {
        let at = BP_INITRD_ADDR_MAX - BP_SETUP_SECTS;
        (header.len() >= at + 4).then(|| u64::from(le_u32(header, at)))
    });
    // Zero means the header does not say.
    let header_max = header_max.filter(|max| *max != 0);
    let limit = header_max
        .unwrap_or(DEFAULT_INITRD_ADDR_MAX)
        .saturating_add(1)
        .min(ram_end);
    let start = limit.checked_sub(len).map(|start| start & !(PAGE - 1));
    let start = start
        .filter(|start| *start >= HIMEM_START)
        .ok_or(ArchError::Layout(copy::X86_RAMDISK_TOO_LARGE))?;
    crate::ramdisk::write(
        mem,
        start,
        limit - start,
        parts,
        bootconfig,
        copy::X86_RAMDISK_TOO_LARGE,
    )
}

/// As `load_ramdisk`, for a ramdisk that is one whole file.
pub fn load_initrd<M, F>(
    mem: &M,
    initrd: &mut F,
    bootconfig: &[(String, String)],
    kernel: &Kernel,
    ram_end: u64,
) -> Result<Range<u64>, ArchError>
where
    M: GuestMemory,
    F: ReadVolatile + Seek,
{
    let len = initrd.seek(SeekFrom::End(0))?;
    let mut parts = [RamdiskPart::Stream {
        stream: initrd,
        range: 0..len,
    }];
    load_ramdisk(mem, &mut parts, bootconfig, kernel, ram_end)
}

/// RAM as the guest sees it: below the device hole, and above 4 GiB for what does not fit under it.
pub fn ram_regions(ram_size: u64) -> Vec<Range<u64>> {
    let low = ram_size.min(MMIO_GAP_START);
    let high = (ram_size > low).then(|| MMIO_GAP_END..MMIO_GAP_END + (ram_size - low));
    std::iter::once(0..low).chain(high).collect()
}

/// The memory map the kernel gets: conventional memory, the BIOS area reserved, then RAM.
fn e820(ram_size: u64) -> Vec<(u64, u64, u32)> {
    let regions = ram_regions(ram_size);
    let mut map = vec![
        (0, EBDA_START, E820_RAM),
        (EBDA_START, HIMEM_START - EBDA_START, E820_RESERVED),
    ];
    for region in regions {
        let start = region.start.max(HIMEM_START);
        if region.end > start {
            map.push((start, region.end - start, E820_RAM));
        }
    }
    map
}

/// Writes `boot_params` and the command line.
pub fn write_boot_params<M: GuestMemory>(
    mem: &M,
    kernel: &Kernel,
    cmdline: &str,
    initrd: Option<Range<u64>>,
    ram_size: u64,
) -> Result<(), ArchError> {
    let mut command = cmdline.as_bytes().to_vec();
    command.push(0);
    if command.len() as u64 > CMDLINE_MAX {
        return Err(ArchError::Layout(copy::X86_CMDLINE_TOO_LONG));
    }
    mem.write_slice(&command, GuestAddress(CMDLINE_ADDR))?;

    let mut page = vec![0u8; ZERO_PAGE_LEN];
    match &kernel.setup_header {
        Some(header) => page[BP_SETUP_SECTS..BP_SETUP_SECTS + header.len()].copy_from_slice(header),
        None => {
            page[BP_BOOT_FLAG..BP_BOOT_FLAG + 2].copy_from_slice(&BOOT_FLAG.to_le_bytes());
            page[BP_HEADER..BP_HEADER + 4].copy_from_slice(HEADER_MAGIC);
            page[BP_KERNEL_ALIGNMENT..BP_KERNEL_ALIGNMENT + 4]
                .copy_from_slice(&KERNEL_ALIGNMENT.to_le_bytes());
        }
    }
    page[BP_TYPE_OF_LOADER] = LOADER_UNDEFINED;
    page[BP_LOADFLAGS] |= LOADFLAGS_CAN_USE_HEAP;
    page[BP_HEAP_END_PTR..BP_HEAP_END_PTR + 2].copy_from_slice(&HEAP_END.to_le_bytes());
    page[BP_CMD_LINE_PTR..BP_CMD_LINE_PTR + 4]
        .copy_from_slice(&(CMDLINE_ADDR as u32).to_le_bytes());
    if let Some(initrd) = initrd {
        let (start, len) = (initrd.start as u32, (initrd.end - initrd.start) as u32);
        page[BP_RAMDISK_IMAGE..BP_RAMDISK_IMAGE + 4].copy_from_slice(&start.to_le_bytes());
        page[BP_RAMDISK_SIZE..BP_RAMDISK_SIZE + 4].copy_from_slice(&len.to_le_bytes());
    }
    let map = e820(ram_size);
    page[BP_E820_ENTRIES] = map.len() as u8;
    for (index, (addr, len, kind)) in map.iter().enumerate() {
        let at = BP_E820_TABLE + index * E820_ENTRY_LEN;
        page[at..at + 8].copy_from_slice(&addr.to_le_bytes());
        page[at + 8..at + 16].copy_from_slice(&len.to_le_bytes());
        page[at + 16..at + 20].copy_from_slice(&kind.to_le_bytes());
    }
    mem.write_slice(&page, GuestAddress(ZERO_PAGE))?;
    Ok(())
}

/// Writes the GDT and page tables that identity-map the first gigabyte, which holds the kernel,
/// `boot_params` and the command line.
pub fn write_tables<M: GuestMemory>(mem: &M) -> Result<(), ArchError> {
    for (index, entry) in GDT.iter().enumerate() {
        mem.write_obj(*entry, GuestAddress(GDT_ADDR + index as u64 * 8))?;
    }
    mem.write_obj(0u64, GuestAddress(IDT_ADDR))?;
    mem.write_obj(PDPTE_ADDR | PTE_PRESENT_WRITABLE, GuestAddress(PML4_ADDR))?;
    mem.write_obj(PDE_ADDR | PTE_PRESENT_WRITABLE, GuestAddress(PDPTE_ADDR))?;
    for index in 0..ENTRIES_PER_TABLE {
        let entry = (index * HUGE_PAGE) | PTE_PRESENT_WRITABLE | PTE_HUGE;
        mem.write_obj(entry, GuestAddress(PDE_ADDR + index * 8))?;
    }
    Ok(())
}

/// Registers for the boot CPU: the 64-bit entry point, `rsi` at `boot_params`.
pub fn boot_regs(entry: u64) -> Regs {
    let mut regs = Regs {
        rip: entry,
        rflags: RFLAGS_RESERVED,
        ..Regs::default()
    };
    regs.gpr[RSI] = ZERO_PAGE;
    regs.gpr[RSP] = BOOT_STACK;
    regs.gpr[RBP] = BOOT_STACK;
    regs
}

/// A flat segment from the GDT entry at `index`.
fn segment(index: usize, selector: u16) -> Segment {
    let entry = GDT[index];
    let flags = (entry >> 52) & 0xf;
    let access = (entry >> 40) & 0xff;
    Segment {
        base: 0,
        limit: 0xfffff,
        selector,
        kind: (access & 0xf) as u8,
        present: access & 0x80 != 0,
        dpl: ((access >> 5) & 0b11) as u8,
        db: flags & 0b0100 != 0,
        s: access & 0x10 != 0,
        l: flags & 0b0010 != 0,
        g: flags & 0b1000 != 0,
        unusable: false,
    }
}

/// Long mode with paging on the identity map, flat segments from the GDT.
pub fn boot_system_regs(current: SystemRegs) -> SystemRegs {
    let code = segment(1, CODE_SELECTOR);
    let data = segment(2, DATA_SELECTOR);
    SystemRegs {
        cs: code,
        ds: data,
        es: data,
        fs: data,
        gs: data,
        ss: data,
        tr: segment(3, TSS_SELECTOR),
        gdt: Table {
            base: GDT_ADDR,
            limit: (GDT.len() * 8 - 1) as u16,
        },
        idt: Table {
            base: IDT_ADDR,
            limit: 7,
        },
        cr0: current.cr0 | CR0_PE | CR0_PG | CR0_ET,
        cr3: PML4_ADDR,
        cr4: current.cr4 | CR4_PAE,
        efer: current.efer | EFER_LME | EFER_LMA,
        ..current
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use vm_memory::GuestMemoryMmap;

    use super::*;

    const RAM: u64 = 64 << 20;

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), RAM as usize)]).unwrap()
    }

    fn bzimage(setup_sects: u8, version: u16, xloadflags: u16) -> Vec<u8> {
        let mut image = vec![0u8; (usize::from(setup_sects) + 1) * 512 + 4096];
        image[BP_SETUP_SECTS] = setup_sects;
        image[BP_BOOT_FLAG..BP_BOOT_FLAG + 2].copy_from_slice(&BOOT_FLAG.to_le_bytes());
        image[BP_JUMP] = 0xeb;
        image[BP_JUMP + 1] = 0x66;
        image[BP_HEADER..BP_HEADER + 4].copy_from_slice(HEADER_MAGIC);
        image[BP_VERSION..BP_VERSION + 2].copy_from_slice(&version.to_le_bytes());
        image[BP_XLOADFLAGS..BP_XLOADFLAGS + 2].copy_from_slice(&xloadflags.to_le_bytes());
        image[BP_INIT_SIZE..BP_INIT_SIZE + 4].copy_from_slice(&0x10000u32.to_le_bytes());
        let kernel_at = (usize::from(setup_sects) + 1) * 512;
        image[kernel_at..kernel_at + 4].copy_from_slice(b"KERN");
        image
    }

    #[test]
    fn a_bzimage_loads_at_one_megabyte_and_keeps_its_setup_header() {
        let mem = memory();
        let kernel = load_kernel(&mem, &mut Cursor::new(bzimage(8, 0x020f, 1)), RAM).unwrap();
        assert_eq!(kernel.entry, HIMEM_START + ENTRY_64_OFFSET);
        let mut loaded = [0u8; 4];
        mem.read_slice(&mut loaded, GuestAddress(HIMEM_START))
            .unwrap();
        assert_eq!(&loaded, b"KERN");
        let header = kernel.setup_header.unwrap();
        assert_eq!(header.len(), BP_HEADER + 0x66 - BP_SETUP_SECTS);
        assert_eq!(header[0], 8);
        let refused = |image: Vec<u8>| load_kernel(&mem, &mut Cursor::new(image), RAM).unwrap_err();
        assert!(matches!(
            refused(bzimage(8, 0x020b, 1)),
            ArchError::KernelImage(_)
        ));
        assert!(matches!(
            refused(bzimage(8, 0x020f, 0)),
            ArchError::KernelImage(_)
        ));
        assert!(matches!(refused(vec![0u8; 100]), ArchError::KernelImage(_)));
    }

    #[test]
    fn an_elf_kernel_loads_at_its_physical_addresses() {
        let mut image = vec![0u8; 0x2000];
        image[..4].copy_from_slice(ELF_MAGIC);
        image[4] = ELF_CLASS_64;
        image[0x18..0x20].copy_from_slice(&0x100_0000u64.to_le_bytes());
        image[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        image[0x36..0x38].copy_from_slice(&(ELF_PHDR_LEN as u16).to_le_bytes());
        image[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());
        let phdr = 64;
        image[phdr..phdr + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        image[phdr + 8..phdr + 16].copy_from_slice(&0x1000u64.to_le_bytes());
        image[phdr + 0x18..phdr + 0x20].copy_from_slice(&0x100_0000u64.to_le_bytes());
        image[phdr + 0x20..phdr + 0x28].copy_from_slice(&4u64.to_le_bytes());
        image[phdr + 0x28..phdr + 0x30].copy_from_slice(&0x2000u64.to_le_bytes());
        image[0x1000..0x1004].copy_from_slice(b"ELF!");
        let mem = memory();
        let kernel = load_kernel(&mem, &mut Cursor::new(image), RAM).unwrap();
        assert_eq!(
            kernel,
            Kernel {
                entry: 0x100_0000,
                setup_header: None
            }
        );
        let mut loaded = [0u8; 4];
        mem.read_slice(&mut loaded, GuestAddress(0x100_0000))
            .unwrap();
        assert_eq!(&loaded, b"ELF!");
    }

    #[test]
    fn boot_params_carry_the_memory_map_command_line_and_ramdisk() {
        let mem = memory();
        let kernel = load_kernel(&mem, &mut Cursor::new(bzimage(4, 0x020f, 1)), RAM).unwrap();
        let initrd =
            load_initrd(&mem, &mut Cursor::new(vec![7u8; 5000]), &[], &kernel, RAM).unwrap();
        assert_eq!(initrd.end - initrd.start, 5000);
        let params = [("androidboot.hardware".to_owned(), "emu".to_owned())];
        let with_config = load_initrd(
            &mem,
            &mut Cursor::new(vec![7u8; 5000]),
            &params,
            &kernel,
            RAM,
        )
        .unwrap();
        let mut magic = [0u8; 12];
        mem.read_slice(&mut magic, GuestAddress(with_config.end - 12))
            .unwrap();
        assert_eq!(&magic, b"#BOOTCONFIG\n");
        assert_eq!(initrd.start % PAGE, 0);
        assert!(initrd.end <= RAM);
        write_boot_params(&mem, &kernel, "console=ttyS0", Some(initrd.clone()), RAM).unwrap();
        let mut page = vec![0u8; ZERO_PAGE_LEN];
        mem.read_slice(&mut page, GuestAddress(ZERO_PAGE)).unwrap();
        assert_eq!(page[BP_TYPE_OF_LOADER], LOADER_UNDEFINED);
        assert_eq!(u64::from(le_u32(&page, BP_CMD_LINE_PTR)), CMDLINE_ADDR);
        assert_eq!(u64::from(le_u32(&page, BP_RAMDISK_IMAGE)), initrd.start);
        assert_eq!(page[BP_E820_ENTRIES], 3);
        let last = BP_E820_TABLE + 2 * E820_ENTRY_LEN;
        assert_eq!(le_u64(&page, last), HIMEM_START);
        assert_eq!(le_u64(&page, last + 8), RAM - HIMEM_START);
        let mut command = [0u8; 14];
        mem.read_slice(&mut command, GuestAddress(CMDLINE_ADDR))
            .unwrap();
        assert_eq!(&command, b"console=ttyS0\0");
    }

    #[test]
    fn ram_past_the_device_hole_continues_above_four_gigabytes() {
        assert_eq!(ram_regions(1 << 30).len(), 1);
        assert_eq!(ram_regions(1 << 30)[0], 0..1 << 30);
        let big = ram_regions(4 << 30);
        assert_eq!(
            big,
            [
                0..MMIO_GAP_START,
                MMIO_GAP_END..MMIO_GAP_END + (4 << 30) - MMIO_GAP_START
            ]
        );
    }

    #[test]
    fn the_cpu_starts_in_long_mode_on_the_identity_map() {
        let regs = boot_system_regs(SystemRegs::default());
        assert!(regs.cs.l && !regs.cs.db && regs.cs.present);
        assert!(!regs.ds.l && regs.ds.db);
        assert_eq!(regs.efer & (EFER_LME | EFER_LMA), EFER_LME | EFER_LMA);
        assert_eq!(regs.cr3, PML4_ADDR);
        let mem = memory();
        write_tables(&mem).unwrap();
        let pde: u64 = mem.read_obj(GuestAddress(PDE_ADDR + 8)).unwrap();
        assert_eq!(pde, HUGE_PAGE | PTE_PRESENT_WRITABLE | PTE_HUGE);
        assert_eq!(boot_regs(0x200).gpr[RSI], ZERO_PAGE);
    }
}

//! The x86_64 board's address map. Low memory follows the PC layout Linux expects; devices sit in a
//! hole below 4 GiB, and RAM that does not fit under the hole continues above 4 GiB.

pub use crate::slots::*;

/// Guest physical address width the map needs.
pub const GPA_BITS: u8 = hv::x86_64::GPA_BITS;
/// The MP table holds at most this many processors in the one-kilobyte BIOS data area.
pub const MAX_VCPUS: u32 = 32;
/// Guest RAM starts at physical zero, as on a PC.
pub const RAM_BASE: u64 = 0;
/// The GDT and IDT the kernel is entered with.
pub const GDT_ADDR: u64 = 0x500;
pub const IDT_ADDR: u64 = 0x520;
/// `boot_params`, the "zero page" of the Linux boot protocol.
pub const ZERO_PAGE: u64 = 0x7000;
/// Top of the stack the kernel is entered with.
pub const BOOT_STACK: u64 = 0x8ff0;
/// Page tables that identity-map the first gigabyte with 2 MiB pages.
pub const PML4_ADDR: u64 = 0x9000;
pub const PDPTE_ADDR: u64 = 0xa000;
pub const PDE_ADDR: u64 = 0xb000;
/// The kernel command line, and the most of it there is room for.
pub const CMDLINE_ADDR: u64 = 0x20000;
pub const CMDLINE_MAX: u64 = 0x10000;
/// End of conventional memory; the extended BIOS data area follows. The MP table goes at its start,
/// where Linux searches for it.
pub const EBDA_START: u64 = 0x9_fc00;
/// Where a bzImage's protected-mode kernel is loaded: one megabyte.
pub const HIMEM_START: u64 = 0x10_0000;
/// The device hole below 4 GiB.
pub const MMIO_GAP_START: u64 = 0xd000_0000;
pub const MMIO_GAP_END: u64 = 0x1_0000_0000;
/// virtio-gpu's host-visible shared-memory window (shmid 1), where blobs are mapped at run time.
pub const SHM_BASE: u64 = 0x10_0000_0000;
pub const SHM_SIZE: u64 = 8 << 30;
/// The IOAPIC and the local APICs, at their architectural addresses inside the hole.
pub const IOAPIC_ADDR: u64 = 0xfec0_0000;
pub const LAPIC_ADDR: u64 = 0xfee0_0000;
/// IOAPIC input pins.
pub const IOAPIC_PINS: u32 = 24;
/// Kernel arguments the board needs: no PCI to probe, and reboot through the keyboard controller,
/// the only reset this board has.
pub const BOARD_CMDLINE: &str = "pci=off reboot=k";
/// The first serial port and its interrupt, as on every PC.
pub const SERIAL_PORT: u16 = 0x3f8;
pub const SERIAL_PORT_LEN: u16 = 8;
pub const SERIAL_IRQ: u32 = 4;
/// The CMOS clock's index and data ports.
pub const CMOS_PORT: u16 = 0x70;
pub const CMOS_PORT_LEN: u16 = 2;
/// The keyboard controller's command port; writing `I8042_RESET` to it resets the machine.
pub const I8042_COMMAND_PORT: u16 = 0x64;
pub const I8042_RESET: u8 = 0xfe;

/// virtio-mmio transports: slot `n` at `VIRTIO_MMIO_BASE + n * VIRTIO_MMIO_STRIDE`, on IOAPIC pin
/// `VIRTIO_FIRST_IRQ + n`. Pin 8 is left free because the CMOS clock driver may claim it unshared,
/// and the pins below it are the legacy ones; so 15 slots, 9 to 23.
pub const VIRTIO_MMIO_BASE: u64 = MMIO_GAP_START;
pub const VIRTIO_MMIO_STRIDE: u64 = 0x1000;
pub const VIRTIO_FIRST_IRQ: u32 = 9;
pub const VIRTIO_MMIO_SLOTS: u32 = IOAPIC_PINS - VIRTIO_FIRST_IRQ;

pub const fn virtio_mmio_base(slot: u32) -> u64 {
    VIRTIO_MMIO_BASE + slot as u64 * VIRTIO_MMIO_STRIDE
}

pub const fn virtio_irq(slot: u32) -> u32 {
    VIRTIO_FIRST_IRQ + slot
}

const _: () = {
    assert!(ZERO_PAGE > IDT_ADDR && BOOT_STACK < PML4_ADDR);
    assert!(PDE_ADDR < CMDLINE_ADDR && CMDLINE_ADDR + CMDLINE_MAX < EBDA_START);
    assert!(virtio_mmio_base(VIRTIO_MMIO_SLOTS) <= IOAPIC_ADDR);
    assert!(virtio_irq(VIRTIO_MMIO_SLOTS - 1) < IOAPIC_PINS);
    assert!(VIRTIO_VSOCK_SLOT < VIRTIO_MMIO_SLOTS);
    assert!(SHM_BASE + SHM_SIZE <= 1 << GPA_BITS);
};

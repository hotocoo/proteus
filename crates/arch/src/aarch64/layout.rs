//! ARM64 guest physical address map. Every address, size and interrupt number of the board lives here.
//!
//! Everything sits below 64 GiB so a 36-bit IPA space is enough. RAM and blob mappings must be multiples of
//! the host page size (16 KiB on Apple Silicon) even though the guest uses 4 KiB pages.

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// Guest physical address width this map needs.
pub const GPA_BITS: u8 = 36;

/// GICv3 distributor.
pub const GICD_BASE: u64 = 0x0800_0000;
pub const GICD_SIZE: u64 = 64 * KIB;
/// GICv3 redistributors, one frame pair per vCPU, packed upward from here.
pub const GICR_BASE: u64 = 0x080a_0000;
pub const GICR_SIZE_PER_VCPU: u64 = 128 * KIB;

/// First interrupt ID that is an SPI; SPI `n` is interrupt ID `SPI_INTID_BASE + n`.
pub const SPI_INTID_BASE: u32 = 32;

/// ns16550 debug UART.
pub const UART_BASE: u64 = 0x0900_0000;
pub const UART_SIZE: u64 = 4 * KIB;
pub const UART_SPI: u32 = 0;
/// Input clock of a standard 16550, in Hz.
pub const UART_CLOCK_HZ: u32 = 1_843_200;

/// PL031 real-time clock.
pub const RTC_BASE: u64 = 0x0901_0000;
pub const RTC_SIZE: u64 = 4 * KIB;
pub const RTC_SPI: u32 = 1;
/// Nominal bus clock the PL031 binding requires a reference to, in Hz. Nothing is derived from it.
pub const APB_CLOCK_HZ: u32 = 24_000_000;

/// virtio-mmio transports: slot `n` is at `VIRTIO_MMIO_BASE + n * VIRTIO_MMIO_STRIDE`, on SPI `VIRTIO_FIRST_SPI + n`.
pub const VIRTIO_MMIO_BASE: u64 = 0x0a00_0000;
pub const VIRTIO_MMIO_STRIDE: u64 = 4 * KIB;
pub const VIRTIO_MMIO_SLOTS: u32 = 32;
pub const VIRTIO_FIRST_SPI: u32 = 16;

/// Guest RAM.
pub const RAM_BASE: u64 = 0x8000_0000;
pub const RAM_MAX_SIZE: u64 = 30 * GIB;

/// The kernel `Image` loads at `RAM_BASE` plus the offset its header asks for, which must be 2 MiB aligned.
pub const KERNEL_ALIGN: u64 = 2 * MIB;
/// Ramdisk, followed directly by the bootconfig trailer.
pub const INITRD_OFFSET: u64 = 128 * MIB;
/// Flattened device tree. The boot protocol caps it at 2 MiB.
pub const FDT_OFFSET: u64 = 254 * MIB;
pub const FDT_MAX_SIZE: u64 = 2 * MIB;
/// Smallest RAM that holds the fixed kernel, ramdisk and device tree placement.
pub const RAM_MIN_SIZE: u64 = FDT_OFFSET + FDT_MAX_SIZE;

/// virtio-gpu shared-memory window (shmid 1, HOST_VISIBLE), mapped at runtime.
pub const SHM_BASE: u64 = 0x8_0000_0000;
pub const SHM_SIZE: u64 = 8 * GIB;

/// Which device sits in which slot. Disks come first so that the guest names them `vda`, `vdb`, `vdc`.
pub const VIRTIO_DISK_SLOTS: u32 = 3;
pub const VIRTIO_GPU_SLOT: u32 = 3;
pub const VIRTIO_TOUCH_SLOT: u32 = 4;
pub const VIRTIO_KEYBOARD_SLOT: u32 = 5;
pub const VIRTIO_SOUND_SLOT: u32 = 7;
pub const VIRTIO_NET_SLOT: u32 = 8;
pub const VIRTIO_CONSOLE_SLOT: u32 = 9;

pub const fn virtio_mmio_base(slot: u32) -> u64 {
    VIRTIO_MMIO_BASE + slot as u64 * VIRTIO_MMIO_STRIDE
}

pub const fn virtio_spi(slot: u32) -> u32 {
    VIRTIO_FIRST_SPI + slot
}

// The map is only valid if its pieces do not collide; check that at compile time.
const _: () = {
    assert!(GICD_BASE + GICD_SIZE <= GICR_BASE);
    assert!(UART_BASE + UART_SIZE <= RTC_BASE);
    assert!(RTC_BASE + RTC_SIZE <= VIRTIO_MMIO_BASE);
    assert!(virtio_mmio_base(VIRTIO_MMIO_SLOTS) <= RAM_BASE);
    assert!(RAM_BASE + RAM_MAX_SIZE <= SHM_BASE);
    assert!(SHM_BASE + SHM_SIZE <= 1 << GPA_BITS);
    assert!(INITRD_OFFSET < FDT_OFFSET);
    assert!(VIRTIO_DISK_SLOTS <= VIRTIO_GPU_SLOT && VIRTIO_GPU_SLOT < VIRTIO_TOUCH_SLOT);
    assert!(VIRTIO_TOUCH_SLOT < VIRTIO_KEYBOARD_SLOT && VIRTIO_KEYBOARD_SLOT < VIRTIO_SOUND_SLOT);
    assert!(VIRTIO_SOUND_SLOT < VIRTIO_NET_SLOT);
    assert!(VIRTIO_NET_SLOT < VIRTIO_CONSOLE_SLOT && VIRTIO_CONSOLE_SLOT < VIRTIO_MMIO_SLOTS);
};

/// Most vCPUs whose redistributors fit below the UART.
pub const MAX_VCPUS: u32 = ((UART_BASE - GICR_BASE) / GICR_SIZE_PER_VCPU) as u32;

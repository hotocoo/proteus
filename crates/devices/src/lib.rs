//! Device models. Nothing here knows which hypervisor runs the guest: devices see guest memory,
//! an MMIO window and an interrupt line.

pub mod bus;
mod copy;
pub mod disk;
pub mod rtc;
pub mod serial;
pub mod virtio;

/// A device mapped into guest physical address space. Offsets are relative to its window.
pub trait MmioDevice: Send + Sync {
    fn read(&self, offset: u64, data: &mut [u8]);
    fn write(&self, offset: u64, data: &[u8]);
}

/// One wired interrupt line: a GIC SPI on ARM64, an IOAPIC pin on x86_64.
pub trait InterruptLine: Send + Sync {
    fn set(&self, level: bool);
}

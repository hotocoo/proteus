//! Device models. Nothing here knows which hypervisor runs the guest: devices see guest memory,
//! an MMIO window and an interrupt line.

pub mod bus;
pub mod cmos;
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

/// Asks macOS to run the calling thread at user-interactive QoS: performance cores first and the
/// quickest wake-ups, as Virtualization.framework does for its vCPUs. At the default QoS a busy
/// Mac (another program on the GPU or CPU) can park the guest's render path on efficiency cores.
pub fn interactive_thread() {
    #[cfg(target_os = "macos")]
    // SAFETY: changes only the calling thread's scheduling class; no memory is touched.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

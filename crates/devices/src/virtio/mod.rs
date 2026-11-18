//! virtio devices and their MMIO transport.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use virtio_bindings::virtio_mmio::{VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING};
pub use virtio_queue::Queue;
use vm_memory::GuestMemoryMmap;

use crate::{InterruptLine, copy};

pub mod blk;
pub mod gpu;
pub mod mmio;
pub mod net;

/// A shared-memory region a device exposes through the transport's SHM registers.
#[derive(Clone, Copy, Debug)]
pub struct ShmRegion {
    pub id: u8,
    pub gpa: u64,
    pub len: u64,
}

#[derive(Debug)]
pub enum DeviceError {
    Io(std::io::Error),
    AlreadyActive,
    MissingQueue,
    /// The host side of the device could not start.
    Backend(String),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{}", copy::disk_open_failed(err)),
            Self::AlreadyActive => f.write_str(copy::DISK_ALREADY_ACTIVE),
            Self::MissingQueue => f.write_str(copy::DISK_NO_QUEUE),
            Self::Backend(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for DeviceError {}

impl From<std::io::Error> for DeviceError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Everything a device gets when the driver sets DRIVER_OK.
pub struct Activate {
    pub mem: GuestMemoryMmap,
    /// One per entry of `VirtioDevice::queue_max_sizes`, configured by the driver.
    pub queues: Vec<Queue>,
    pub irq: Arc<VirtioInterrupt>,
    /// Feature bits the driver accepted.
    pub features: u64,
}

pub trait VirtioDevice: Send {
    fn device_type(&self) -> u32;
    fn queue_max_sizes(&self) -> &[u16];
    /// Device-specific feature bits; the transport adds the ones it owns.
    fn features(&self) -> u64;
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, offset: u64, data: &[u8]);
    fn shm_regions(&self) -> &[ShmRegion] {
        &[]
    }
    fn activate(&mut self, ctx: Activate) -> Result<(), DeviceError>;
    /// Runs on a vCPU thread. Must only wake the device's worker, never do I/O.
    fn queue_notify(&mut self, queue: u16);
    fn reset(&mut self);
}

/// The interrupt status register and the level-triggered line it drives.
///
/// The line is high exactly while a status bit is set, so an interrupt raised between the driver's
/// read and its acknowledgement is never lost. Both are updated under one lock for that reason.
pub struct VirtioInterrupt {
    status: Mutex<u32>,
    line: Arc<dyn InterruptLine>,
}

impl VirtioInterrupt {
    pub fn new(line: Arc<dyn InterruptLine>) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(0),
            line,
        })
    }

    /// The device put buffers on a used ring.
    pub fn signal_used(&self) {
        self.raise(VIRTIO_MMIO_INT_VRING);
    }

    /// The device's configuration space changed.
    pub fn signal_config(&self) {
        self.raise(VIRTIO_MMIO_INT_CONFIG);
    }

    fn raise(&self, bits: u32) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        *status |= bits;
        self.line.set(true);
    }

    fn status(&self) -> u32 {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn acknowledge(&self, bits: u32) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        *status &= !bits;
        if *status == 0 {
            self.line.set(false);
        }
    }

    fn clear(&self) {
        self.acknowledge(u32::MAX);
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Mutex;

    use crate::InterruptLine;

    /// Records every level change of an interrupt line.
    #[derive(Default)]
    pub struct LineLog(pub Mutex<Vec<bool>>);

    impl LineLog {
        pub fn level(&self) -> Option<bool> {
            self.0.lock().unwrap().last().copied()
        }
    }

    impl InterruptLine for LineLog {
        fn set(&self, level: bool) {
            self.0.lock().unwrap().push(level);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::LineLog;
    use super::*;

    #[test]
    fn line_stays_high_until_every_status_bit_is_acknowledged() {
        let line = Arc::new(LineLog::default());
        let irq = VirtioInterrupt::new(line.clone());
        irq.signal_used();
        irq.signal_config();
        assert_eq!(irq.status(), VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG);
        assert_eq!(line.level(), Some(true));

        irq.acknowledge(VIRTIO_MMIO_INT_VRING);
        assert_eq!(irq.status(), VIRTIO_MMIO_INT_CONFIG);
        assert_eq!(line.level(), Some(true));

        irq.acknowledge(VIRTIO_MMIO_INT_CONFIG);
        assert_eq!(irq.status(), 0);
        assert_eq!(line.level(), Some(false));
    }

    #[test]
    fn interrupt_raised_after_acknowledge_raises_the_line_again() {
        let line = Arc::new(LineLog::default());
        let irq = VirtioInterrupt::new(line.clone());
        irq.signal_used();
        irq.acknowledge(VIRTIO_MMIO_INT_VRING);
        irq.signal_used();
        assert_eq!(*line.0.lock().unwrap(), [true, false, true]);
    }
}

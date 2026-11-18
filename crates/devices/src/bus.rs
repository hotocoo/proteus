//! Routes guest MMIO accesses to the device whose window contains the address.

use std::fmt;
use std::sync::Arc;

use crate::{MmioDevice, copy};

#[derive(Debug, PartialEq, Eq)]
pub enum BusError {
    Overlap,
    EmptyWindow,
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Overlap => copy::BUS_OVERLAP,
            Self::EmptyWindow => copy::BUS_EMPTY_WINDOW,
        })
    }
}

impl std::error::Error for BusError {}

struct Window {
    base: u64,
    /// Last address inside the window.
    last: u64,
    device: Arc<dyn MmioDevice>,
}

/// Built once at start-up, then shared read-only by every vCPU thread.
#[derive(Default)]
pub struct MmioBus {
    /// Sorted by base, never overlapping.
    windows: Vec<Window>,
}

impl MmioBus {
    pub fn insert(
        &mut self,
        base: u64,
        size: u64,
        device: Arc<dyn MmioDevice>,
    ) -> Result<(), BusError> {
        let last = size
            .checked_sub(1)
            .and_then(|span| base.checked_add(span))
            .ok_or(BusError::EmptyWindow)?;
        let at = self.windows.partition_point(|window| window.base < base);
        let clashes_before = at
            .checked_sub(1)
            .is_some_and(|before| self.windows[before].last >= base);
        let clashes_after = self.windows.get(at).is_some_and(|after| after.base <= last);
        if clashes_before || clashes_after {
            return Err(BusError::Overlap);
        }
        self.windows.insert(at, Window { base, last, device });
        Ok(())
    }

    /// The device holding `gpa..gpa+len` and the offset into its window.
    fn find(&self, gpa: u64, len: usize) -> Option<(&dyn MmioDevice, u64)> {
        let at = self
            .windows
            .partition_point(|window| window.base <= gpa)
            .checked_sub(1)?;
        let window = &self.windows[at];
        let last = gpa.checked_add((len as u64).checked_sub(1)?)?;
        (last <= window.last).then(|| (&*window.device, gpa - window.base))
    }

    /// Reads from the device at `gpa`. An access no device claims reads as zeros.
    pub fn read(&self, gpa: u64, data: &mut [u8]) {
        match self.find(gpa, data.len()) {
            Some((device, offset)) => device.read(offset, data),
            None => {
                log::debug!(
                    "{}",
                    copy::unmapped_access(copy::ACCESS_READ, gpa, data.len())
                );
                data.fill(0);
            }
        }
    }

    /// Writes to the device at `gpa`. An access no device claims is dropped.
    pub fn write(&self, gpa: u64, data: &[u8]) {
        match self.find(gpa, data.len()) {
            Some((device, offset)) => device.write(offset, data),
            None => log::debug!(
                "{}",
                copy::unmapped_access(copy::ACCESS_WRITE, gpa, data.len())
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Remembers the last write and answers reads with its tag.
    struct Probe {
        tag: u8,
        last_write: Mutex<Option<(u64, Vec<u8>)>>,
    }

    impl Probe {
        fn new(tag: u8) -> Arc<Self> {
            Arc::new(Self {
                tag,
                last_write: Mutex::new(None),
            })
        }
    }

    impl MmioDevice for Probe {
        fn read(&self, offset: u64, data: &mut [u8]) {
            data.fill(self.tag.wrapping_add(offset as u8));
        }

        fn write(&self, offset: u64, data: &[u8]) {
            *self.last_write.lock().unwrap() = Some((offset, data.to_vec()));
        }
    }

    #[test]
    fn accesses_reach_the_right_device_with_window_relative_offsets() {
        let (a, b) = (Probe::new(0x10), Probe::new(0x20));
        let mut bus = MmioBus::default();
        // Inserted out of order on purpose.
        bus.insert(0x2000, 0x100, b.clone()).unwrap();
        bus.insert(0x1000, 0x100, a.clone()).unwrap();

        let mut byte = [0u8];
        bus.read(0x1004, &mut byte);
        assert_eq!(byte, [0x14]);
        bus.read(0x20ff, &mut byte);
        assert_eq!(byte, [0x20u8.wrapping_add(0xff)]);

        bus.write(0x2008, &[1, 2, 3, 4]);
        assert_eq!(*b.last_write.lock().unwrap(), Some((8, vec![1, 2, 3, 4])));
        assert_eq!(*a.last_write.lock().unwrap(), None);
    }

    #[test]
    fn unclaimed_and_straddling_accesses_read_zero_and_write_nowhere() {
        let a = Probe::new(0x10);
        let mut bus = MmioBus::default();
        bus.insert(0x1000, 0x100, a.clone()).unwrap();

        let mut word = [0xffu8; 4];
        bus.read(0x0fff, &mut word);
        assert_eq!(word, [0; 4]);
        // Starts inside the window, ends outside it.
        let mut word = [0xffu8; 4];
        bus.read(0x10fe, &mut word);
        assert_eq!(word, [0; 4]);
        bus.write(0x10fe, &[9; 4]);
        bus.write(0x3000, &[9]);
        assert_eq!(*a.last_write.lock().unwrap(), None);
    }

    #[test]
    fn overlapping_and_empty_windows_are_refused() {
        let mut bus = MmioBus::default();
        bus.insert(0x1000, 0x100, Probe::new(0)).unwrap();
        assert_eq!(bus.insert(0x10ff, 1, Probe::new(0)), Err(BusError::Overlap));
        assert_eq!(
            bus.insert(0x0f00, 0x101, Probe::new(0)),
            Err(BusError::Overlap)
        );
        assert_eq!(
            bus.insert(0x0800, 0x1000, Probe::new(0)),
            Err(BusError::Overlap)
        );
        assert_eq!(
            bus.insert(0x1100, 0, Probe::new(0)),
            Err(BusError::EmptyWindow)
        );
        assert_eq!(
            bus.insert(u64::MAX, 2, Probe::new(0)),
            Err(BusError::EmptyWindow)
        );
        // Directly adjacent on both sides is fine.
        assert_eq!(bus.insert(0x1100, 0x100, Probe::new(0)), Ok(()));
        assert_eq!(bus.insert(0x0f00, 0x100, Probe::new(0)), Ok(()));
    }
}

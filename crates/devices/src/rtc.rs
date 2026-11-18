//! PL031 real-time clock, a thin MMIO wrapper over `vm-superio`. It gives the guest wall-clock time at boot.

use std::sync::{Mutex, PoisonError};

use vm_superio::Rtc as Pl031;
use vm_superio::rtc_pl031::NoEvents;

use crate::MmioDevice;

pub struct Rtc {
    pl031: Mutex<Pl031<NoEvents>>,
}

impl Default for Rtc {
    fn default() -> Self {
        Self {
            pl031: Mutex::new(Pl031::new()),
        }
    }
}

impl MmioDevice for Rtc {
    fn read(&self, offset: u64, data: &mut [u8]) {
        // Every PL031 register is 32 bits wide; other access sizes read as zero.
        match (u16::try_from(offset), <&mut [u8; 4]>::try_from(&mut *data)) {
            (Ok(register), Ok(word)) => {
                self.pl031
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .read(register, word);
            }
            _ => data.fill(0),
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if let (Ok(register), Ok(word)) = (u16::try_from(offset), <&[u8; 4]>::try_from(data)) {
            self.pl031
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .write(register, word);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    const REG_DATA: u64 = 0x000;
    /// PrimeCell identification register; PL031 reads 0x31 here.
    const REG_PERIPH_ID0: u64 = 0xfe0;
    const PL031_PART_LOW: u32 = 0x31;
    /// Slack for the clock ticking between the device read and the host read.
    const TOLERANCE_SECS: u64 = 2;

    fn read_word(rtc: &Rtc, reg: u64) -> u32 {
        let mut word = [0u8; 4];
        rtc.read(reg, &mut word);
        u32::from_le_bytes(word)
    }

    #[test]
    fn reports_host_wall_clock_and_identifies_as_pl031() {
        let rtc = Rtc::default();
        let host = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(u64::from(read_word(&rtc, REG_DATA)).abs_diff(host) <= TOLERANCE_SECS);
        assert_eq!(read_word(&rtc, REG_PERIPH_ID0), PL031_PART_LOW);
    }

    #[test]
    fn odd_sized_accesses_are_harmless() {
        let rtc = Rtc::default();
        let mut byte = [0xffu8];
        rtc.read(REG_DATA, &mut byte);
        assert_eq!(byte, [0]);
        rtc.write(REG_DATA, &[1, 2]);
        rtc.write(u64::MAX, &[0; 4]);
    }
}

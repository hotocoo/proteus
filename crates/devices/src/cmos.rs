//! The PC's CMOS real-time clock (MC146818), at I/O ports 0x70 (register index) and 0x71 (data).
//! It gives an x86_64 guest the wall-clock time; nothing in it ever changes but the time, and no
//! interrupt is raised. Registers are binary, not BCD, and the hour runs to 24.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::MmioDevice;

/// Offsets from the first port.
const INDEX: u64 = 0;
const DATA: u64 = 1;
/// The top bit of an index write switches NMIs off on a PC; it is not part of the index.
const INDEX_MASK: u8 = 0x7f;

const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const WEEKDAY: u8 = 0x06;
const DAY: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0a;
const STATUS_B: u8 = 0x0b;
const STATUS_D: u8 = 0x0d;
const CENTURY: u8 = 0x32;
/// 32.768 kHz time base, 1024 Hz rate, and no update in progress.
const STATUS_A_VALUE: u8 = 0x26;
/// 24-hour clock, binary values.
const STATUS_B_VALUE: u8 = 0x02 | 0x04;
/// The battery is good, so the time is valid.
const STATUS_D_VALUE: u8 = 0x80;

const SECONDS_PER_DAY: u64 = 86_400;
const DAYS_PER_WEEK: u64 = 7;
/// 1 January 1970 was a Thursday; the clock counts Sunday as 1.
const EPOCH_WEEKDAY: u64 = 4;

/// The civil date of a day counted from 1 January 1970: (year, month, day).
/// Howard Hinnant's `civil_from_days`, for the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The value of clock register `index` at `unix_seconds`.
fn register(index: u8, unix_seconds: u64) -> u8 {
    let days = unix_seconds / SECONDS_PER_DAY;
    let in_day = unix_seconds % SECONDS_PER_DAY;
    let (year, month, day) = civil_from_days(days as i64);
    match index {
        SECONDS => (in_day % 60) as u8,
        MINUTES => (in_day / 60 % 60) as u8,
        HOURS => (in_day / 3600) as u8,
        WEEKDAY => ((days + EPOCH_WEEKDAY) % DAYS_PER_WEEK + 1) as u8,
        DAY => day as u8,
        MONTH => month as u8,
        YEAR => year.rem_euclid(100) as u8,
        CENTURY => year.div_euclid(100) as u8,
        STATUS_A => STATUS_A_VALUE,
        STATUS_B => STATUS_B_VALUE,
        STATUS_D => STATUS_D_VALUE,
        _ => 0,
    }
}

#[derive(Default)]
pub struct Cmos {
    index: AtomicU8,
}

impl MmioDevice for Cmos {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let value = match offset {
            DATA => {
                let now = SystemTime::now().duration_since(UNIX_EPOCH);
                register(
                    self.index.load(Ordering::SeqCst),
                    now.map_or(0, |now| now.as_secs()),
                )
            }
            _ => self.index.load(Ordering::SeqCst),
        };
        data.fill(0);
        if let Some(first) = data.first_mut() {
            *first = value;
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if let (INDEX, Some(index)) = (offset, data.first()) {
            self.index.store(index & INDEX_MASK, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_read_the_time_as_a_calendar_date() {
        // 2026-11-18 15:49:28 UTC, a Wednesday.
        let at = 1_795_016_968;
        assert_eq!(register(SECONDS, at), 28);
        assert_eq!(register(MINUTES, at), 49);
        assert_eq!(register(HOURS, at), 15);
        assert_eq!(register(DAY, at), 18);
        assert_eq!(register(MONTH, at), 11);
        assert_eq!(register(YEAR, at), 26);
        assert_eq!(register(CENTURY, at), 20);
        assert_eq!(register(WEEKDAY, at), 4);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        // Never an update in progress, and the time is valid.
        assert_eq!(register(STATUS_A, at) & 0x80, 0);
        assert_eq!(register(STATUS_D, at), STATUS_D_VALUE);
    }

    #[test]
    fn the_index_port_selects_what_the_data_port_reads() {
        let cmos = Cmos::default();
        cmos.write(INDEX, &[0x80 | STATUS_B]);
        let mut value = [0u8];
        cmos.read(DATA, &mut value);
        assert_eq!(value[0], STATUS_B_VALUE);
        cmos.write(DATA, &[0xff]);
        cmos.read(DATA, &mut value);
        assert_eq!(value[0], STATUS_B_VALUE);
    }
}

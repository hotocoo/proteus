//! Where the guest believes it is.
//!
//! The Android image's location service asks one of its terminals for a position about once a
//! second by writing a command to it, and reads back two NMEA sentences. Until a position has been
//! set here nothing is answered, and the image reports its built-in default.

use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use devices::virtio::console::ConsoleHandle;

/// Terminal the image asks for its position on (`/dev/hvc7`).
pub const PORT: usize = 7;

const REQUEST: &[u8] = b"CMD_GET_LOCATION";
/// What the image reads as the end of an answer.
const END_OF_ANSWER: &str = "\n\n\n\n";
const SECONDS_PER_DAY: u64 = 86_400;
const SECONDS_PER_HOUR: u64 = 3600;
const SECONDS_PER_MINUTE: u64 = 60;
const MINUTES_PER_DEGREE: f64 = 60.0;
const MAX_LATITUDE: f64 = 90.0;
const MAX_LONGITUDE: f64 = 180.0;

/// A position: degrees north, degrees east, metres above sea level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fix {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: f64,
}

impl Fix {
    /// `None` unless the numbers are a place on Earth.
    pub fn new(latitude: f64, longitude: f64, altitude: f64) -> Option<Self> {
        let valid = latitude.abs() <= MAX_LATITUDE
            && longitude.abs() <= MAX_LONGITUDE
            && altitude.is_finite();
        valid.then_some(Self {
            latitude,
            longitude,
            altitude,
        })
    }
}

/// Year, month, day of a count of days since 1970-01-01 (the usual civil-calendar arithmetic).
fn date(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// An angle as NMEA writes it: whole degrees, then minutes with a fraction, in one number.
fn angle(degrees: f64, degree_digits: usize) -> String {
    let degrees = degrees.abs();
    let whole = degrees.trunc();
    let minutes = (degrees - whole) * MINUTES_PER_DEGREE;
    format!("{whole:0degree_digits$}{minutes:09.6}")
}

fn sentence(body: &str) -> String {
    let checksum = body.bytes().fold(0u8, |sum, byte| sum ^ byte);
    format!("${body}*{checksum:02X}\n")
}

/// The answer to one position request: a GGA and an RMC sentence for `fix` at `unix_seconds`.
fn answer(fix: &Fix, unix_seconds: u64) -> String {
    let time_of_day = unix_seconds % SECONDS_PER_DAY;
    let time = format!(
        "{:02}{:02}{:02}.00",
        time_of_day / SECONDS_PER_HOUR,
        time_of_day % SECONDS_PER_HOUR / SECONDS_PER_MINUTE,
        time_of_day % SECONDS_PER_MINUTE
    );
    let (year, month, day) = date(unix_seconds / SECONDS_PER_DAY);
    let latitude = angle(fix.latitude, 2);
    let north = if fix.latitude >= 0.0 { 'N' } else { 'S' };
    let longitude = angle(fix.longitude, 3);
    let east = if fix.longitude >= 0.0 { 'E' } else { 'W' };
    let position = format!("{latitude},{north},{longitude},{east}");
    // Fix quality 1 (a satellite fix), eight satellites, horizontal dilution 0.9, no geoid offset.
    let gga = format!(
        "GPGGA,{time},{position},1,08,0.9,{:.1},M,0.0,M,,",
        fix.altitude
    );
    // Status A (valid), standing still, no heading.
    let rmc = format!(
        "GPRMC,{time},A,{position},000.0,000.0,{day:02}{month:02}{:02},,,A",
        year % 100
    );
    format!("{}{}{END_OF_ANSWER}", sentence(&gga), sentence(&rmc))
}

/// The position to report, shared between whoever sets it and the terminal that answers with it.
#[derive(Default)]
pub struct Location(Mutex<Option<Fix>>);

impl Location {
    pub fn set(&self, fix: Fix) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(fix);
    }

    fn get(&self) -> Option<Fix> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Takes what the guest writes to the position terminal and answers each request.
pub struct Requests {
    location: Arc<Location>,
    answers: ConsoleHandle,
    /// The end of the bytes seen so far, in case a request is split across two writes.
    tail: Vec<u8>,
}

impl Requests {
    pub fn new(location: Arc<Location>, answers: ConsoleHandle) -> Self {
        Self {
            location,
            answers,
            tail: Vec::new(),
        }
    }
}

impl Write for Requests {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.tail.extend_from_slice(bytes);
        // The guest's terminal also echoes the answers back; only the request text counts.
        let mut asked = 0;
        while let Some(at) = self
            .tail
            .windows(REQUEST.len())
            .position(|window| window == REQUEST)
        {
            self.tail.drain(..at + REQUEST.len());
            asked += 1;
        }
        let keep = self.tail.len().min(REQUEST.len() - 1);
        self.tail.drain(..self.tail.len() - keep);
        if let (true, Some(fix)) = (asked > 0, self.location.get()) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH);
            let text = answer(&fix, now.map_or(0, |since| since.as_secs()));
            self.answers.send(PORT, text.as_bytes());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use devices::virtio::console::Console;

    use super::*;

    /// 2019-08-29 21:32:04 UTC, the moment in the NMEA example the image's parser documents.
    const EXAMPLE_TIME: u64 = 1_567_114_324;

    #[test]
    fn dates_follow_the_calendar_across_leap_years() {
        assert_eq!(date(0), (1970, 1, 1));
        assert_eq!(date(EXAMPLE_TIME / SECONDS_PER_DAY), (2019, 8, 29));
        // 2024-02-29 and the day after.
        assert_eq!(date(19_782), (2024, 2, 29));
        assert_eq!(date(19_783), (2024, 3, 1));
    }

    #[test]
    fn answer_is_the_two_sentences_the_image_parses() {
        let fix = Fix::new(37.422_854, -122.093_153_983, 12.34).unwrap();
        let text = answer(&fix, EXAMPLE_TIME);
        let mut lines = text.split('\n');
        let gga = lines.next().unwrap();
        let rmc = lines.next().unwrap();
        assert_eq!(
            gga,
            "$GPGGA,213204.00,3725.371240,N,12205.589239,W,1,08,0.9,12.3,M,0.0,M,,*71"
        );
        assert_eq!(
            rmc,
            "$GPRMC,213204.00,A,3725.371240,N,12205.589239,W,000.0,000.0,290819,,,A*49"
        );
        assert!(text.ends_with(END_OF_ANSWER));
        // The image needs at least thirteen comma-separated fields in each.
        assert!(gga.split(',').count() >= 13 && rmc.split(',').count() >= 13);
    }

    #[test]
    fn southern_and_eastern_positions_change_the_hemisphere_letters() {
        let fix = Fix::new(-33.5, 151.25, 0.0).unwrap();
        let text = answer(&fix, 0);
        assert!(text.contains(",3330.000000,S,15115.000000,E,"));
    }

    #[test]
    fn positions_that_are_not_on_earth_are_refused() {
        assert!(Fix::new(90.1, 0.0, 0.0).is_none());
        assert!(Fix::new(0.0, -180.5, 0.0).is_none());
        assert!(Fix::new(f64::NAN, 0.0, 0.0).is_none());
        assert!(Fix::new(0.0, 0.0, f64::INFINITY).is_none());
        assert!(Fix::new(-90.0, 180.0, -400.0).is_some());
    }

    #[test]
    fn requests_are_answered_only_once_a_position_is_set() {
        let (console, answers) = Console::new((0..=PORT).map(|_| None).collect());
        let location = Arc::new(Location::default());
        let mut requests = Requests::new(Arc::clone(&location), answers);
        requests.write_all(REQUEST).unwrap();
        assert_eq!(console.pending(PORT), 0);

        location.set(Fix::new(1.0, 2.0, 3.0).unwrap());
        // A request split across writes, with echoed text around it.
        requests.write_all(b"$GPGGA,echo\nCMD_GET_LO").unwrap();
        assert_eq!(console.pending(PORT), 0);
        requests.write_all(b"CATION").unwrap();
        let one_answer = console.pending(PORT);
        assert!(one_answer > 0);
        // Two requests in one write still get one answer: the position is the same.
        requests
            .write_all(b"CMD_GET_LOCATIONCMD_GET_LOCATION")
            .unwrap();
        assert_eq!(console.pending(PORT), 2 * one_answer);
        assert!(requests.tail.len() < REQUEST.len());
    }
}

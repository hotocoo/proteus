//! Where the guest believes it is.
//!
//! The Android image's location service asks one of its terminals for a position about once a
//! second by writing a command to it, and reads back one line describing a fix. Until a position
//! has been set here nothing is answered, and the image reports its built-in default.

use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use devices::virtio::console::ConsoleHandle;

/// Terminal the image asks for its position on (`/dev/hvc7`).
pub const PORT: usize = 7;

const REQUEST: &[u8] = b"CMD_GET_LOCATION";
/// What the image reads as the end of an answer.
const END_OF_ANSWER: &str = "\n\n\n\n";
const PROVIDER: &str = "GPS";
/// How exact the guest is told the position is.
const ACCURACY_METRES: f64 = 3.0;
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

/// The answer to one position request, at `unix_millis`: one line of twelve comma-separated fields.
/// In order: the word "Fix", the provider, latitude, longitude, altitude, speed, accuracy in metres,
/// bearing, the time, speed accuracy, bearing accuracy, and a clock reading the image ignores.
fn answer(fix: &Fix, unix_millis: u128) -> String {
    let Fix {
        latitude,
        longitude,
        altitude,
    } = fix;
    format!(
        "Fix,{PROVIDER},{latitude},{longitude},{altitude},0,{ACCURACY_METRES},0,{unix_millis},0,0,0{END_OF_ANSWER}"
    )
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
            let text = answer(&fix, now.map_or(0, |since| since.as_millis()));
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

    #[test]
    fn answer_is_the_one_line_the_image_parses() {
        let fix = Fix::new(3.139_003, -101.686_855, 60.5).unwrap();
        let text = answer(&fix, 1_795_000_000_123);
        assert_eq!(
            text,
            "Fix,GPS,3.139003,-101.686855,60.5,0,3,0,1795000000123,0,0,0\n\n\n\n"
        );
        // The image needs at least twelve fields on the first line.
        assert_eq!(text.lines().next().unwrap().split(',').count(), 12);
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
        requests.write_all(b"Fix,GPS,echo\nCMD_GET_LO").unwrap();
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

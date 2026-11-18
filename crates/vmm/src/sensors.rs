//! The host end of the Android image's sensor service.
//!
//! The image's sensor service talks to a helper outside the guest over two of its terminals: one for
//! requests, one for readings. It will not start, and Android will not finish booting, until its
//! first request ("which sensors do you have?") is answered. Messages in both directions are a
//! header of two little-endian `u32`s, a command number with the top bit marking a response and the
//! payload length, followed by the payload, which is text.

use std::io::Write;

use devices::virtio::console::ConsoleHandle;

/// Terminal the image sends its sensor requests to (`/dev/hvc18`).
pub const CONTROL_PORT: usize = 18;

const HEADER_LEN: usize = 8;
/// Set in the first header word of a message that answers a request.
const IS_RESPONSE: u32 = 1 << 31;
/// The command number the image expects on everything the host sends about sensors.
const UPDATE_HAL: u32 = 2;
/// Requests are short commands; a longer length means the stream is not what it should be.
const MAX_PAYLOAD: usize = 256;
const LIST_SENSORS: &[u8] = b"list-sensors";
const END_OF_MESSAGE: char = '\n';

// The image's numbers for the sensors this machine has; the answer to "list" has one bit for each.
const ACCELERATION: u32 = 0;
const GYROSCOPE: u32 = 1;
const MAGNETIC: u32 = 2;
const TEMPERATURE: u32 = 4;
const PROXIMITY: u32 = 5;
const LIGHT: u32 = 6;
const PRESSURE: u32 = 7;
const HUMIDITY: u32 = 8;
const SENSORS: u32 = 1 << ACCELERATION
    | 1 << GYROSCOPE
    | 1 << MAGNETIC
    | 1 << TEMPERATURE
    | 1 << PROXIMITY
    | 1 << LIGHT
    | 1 << PRESSURE
    | 1 << HUMIDITY;

/// One message for the guest: a response carrying `text`.
fn message(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_LEN + text.len());
    bytes.extend((UPDATE_HAL | IS_RESPONSE).to_le_bytes());
    bytes.extend((text.len() as u32).to_le_bytes());
    bytes.extend(text.as_bytes());
    bytes
}

/// Takes the guest's request bytes as they arrive, and answers the requests that need an answer.
pub struct Requests {
    answers: ConsoleHandle,
    /// Bytes of a message that has not arrived completely yet.
    partial: Vec<u8>,
}

impl Requests {
    /// Answers go back to the guest on `CONTROL_PORT` of the console `answers` belongs to.
    pub fn new(answers: ConsoleHandle) -> Self {
        Self {
            answers,
            partial: Vec::new(),
        }
    }

    /// The next complete payload, if one has arrived.
    fn next(&mut self) -> Option<Vec<u8>> {
        let header = self.partial.get(..HEADER_LEN)?;
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if len > MAX_PAYLOAD {
            // Not a message this side knows; there is no way to find the next one, so start over.
            self.partial.clear();
            return None;
        }
        if self.partial.len() < HEADER_LEN + len {
            return None;
        }
        let payload = self.partial[HEADER_LEN..HEADER_LEN + len].to_vec();
        self.partial.drain(..HEADER_LEN + len);
        Some(payload)
    }
}

impl Write for Requests {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.partial.extend_from_slice(bytes);
        while let Some(request) = self.next() {
            // Everything else (switching a sensor on, setting its rate) needs no answer.
            if request.starts_with(LIST_SENSORS) {
                let answer = message(&format!("{SENSORS}{END_OF_MESSAGE}"));
                self.answers.send(CONTROL_PORT, &answer);
            }
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

    fn request(text: &[u8]) -> Vec<u8> {
        let mut bytes = 0u32.to_le_bytes().to_vec();
        bytes.extend((text.len() as u32).to_le_bytes());
        bytes.extend(text);
        bytes
    }

    #[test]
    fn answer_names_the_sensors_as_a_response_message() {
        assert_eq!(
            message("503\n"),
            [2, 0, 0, 0x80, 4, 0, 0, 0, b'5', b'0', b'3', b'\n']
        );
        assert_eq!(SENSORS, 503);
    }

    #[test]
    fn list_request_is_answered_however_its_bytes_are_split() {
        let (console, answers) = Console::new((0..=CONTROL_PORT).map(|_| None).collect());
        let mut requests = Requests::new(answers);
        let mut stream = request(b"set:acceleration:1");
        stream.extend(request(LIST_SENSORS));
        let (first, rest) = stream.split_at(HEADER_LEN + 3);
        requests.write_all(first).unwrap();
        assert_eq!(console.pending(CONTROL_PORT), 0);
        requests.write_all(rest).unwrap();
        assert_eq!(console.pending(CONTROL_PORT), message("503\n").len());
        assert!(requests.partial.is_empty());
    }

    #[test]
    fn a_length_no_request_has_resets_the_stream() {
        let (console, answers) = Console::new((0..=CONTROL_PORT).map(|_| None).collect());
        let mut requests = Requests::new(answers);
        let mut garbage = 0u32.to_le_bytes().to_vec();
        garbage.extend(u32::MAX.to_le_bytes());
        requests.write_all(&garbage).unwrap();
        assert!(requests.partial.is_empty());
        requests.write_all(&request(LIST_SENSORS)).unwrap();
        assert_ne!(console.pending(CONTROL_PORT), 0);
    }
}

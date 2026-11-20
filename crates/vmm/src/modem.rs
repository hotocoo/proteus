//! A modem with no SIM and no network, for the Android image's radio service. That service (from
//! Cuttlefish) speaks AT commands to a modem over vsock and exits when it finds none; Android's
//! phone process then waits for it forever and is restarted every few seconds as hung. Answering
//! as a modem with its SIM slot empty is enough for the radio service to run and the phone
//! process to start, and Android shows "no SIM".

use devices::virtio::vsock::Connection;

/// The vsock port the radio service is told the modem is on (`androidboot.modem_simulator_ports`).
pub const PORT: u32 = 9600;

/// Final answers, as the AT protocol spells them.
const OK: &str = "OK";
/// "SIM not inserted".
const NO_SIM: &str = "+CME ERROR: 10";
/// Longest command kept while waiting for its end; a longer one is dropped.
const MAX_LINE: usize = 4096;

/// What the modem says to commands whose answer is more than "OK": queries about the SIM fail
/// with no SIM, the radio reports itself off and the network unregistered.
const ANSWERS: &[(&str, &[&str])] = &[
    ("AT+CPIN?", &[NO_SIM]),
    ("AT+CIMI", &[NO_SIM]),
    ("AT+CICCID", &[NO_SIM]),
    ("AT+CRSM", &[NO_SIM]),
    ("AT+CSIM", &[NO_SIM]),
    ("AT+CGLA", &[NO_SIM]),
    ("AT+CCHO", &[NO_SIM]),
    ("AT+CFUN?", &["+CFUN: 0", OK]),
    ("AT+CREG?", &["+CREG: 2,0", OK]),
    ("AT+CGREG?", &["+CGREG: 2,0", OK]),
    ("AT+CEREG?", &["+CEREG: 2,0", OK]),
    ("AT+COPS?", &["+COPS: 0", OK]),
    ("AT+CSQ", &["+CSQ: 99,99", OK]),
    ("AT+CLCC", &[OK]),
];

/// One connection from the radio service.
#[derive(Default)]
pub struct Modem {
    line: Vec<u8>,
}

/// The modem's answer to one command line, each line framed as the protocol wants.
fn answer(command: &str) -> String {
    let lines = ANSWERS
        .iter()
        .find(|(prefix, _)| command.starts_with(prefix))
        .map_or(&[OK][..], |(_, lines)| lines);
    lines.iter().map(|line| format!("\r\n{line}\r\n")).collect()
}

impl Connection for Modem {
    fn received(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = String::new();
        for &byte in bytes {
            if byte == b'\r' || byte == b'\n' {
                let line = String::from_utf8_lossy(&self.line).trim().to_owned();
                self.line.clear();
                if !line.is_empty() {
                    out.push_str(&answer(&line));
                }
            } else if self.line.len() < MAX_LINE {
                self.line.push(byte);
            }
        }
        out.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_answered_one_line_at_a_time() {
        let mut modem = Modem::default();
        assert!(modem.received(b"AT+CMEE").is_empty());
        assert_eq!(modem.received(b"=1\r"), b"\r\nOK\r\n");
        assert_eq!(modem.received(b"AT+CPIN?\r"), b"\r\n+CME ERROR: 10\r\n");
        assert_eq!(
            modem.received(b"AT+CFUN?\r\nAT\r"),
            b"\r\n+CFUN: 0\r\n\r\nOK\r\n\r\nOK\r\n"
        );
    }
}

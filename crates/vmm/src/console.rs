//! Host side of the debug serial console.

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::SERIAL_INPUT_CHUNK;
use crate::copy;

/// Console input that outlives one boot of the machine, so that a guest reboot keeps the keyboard.
///
/// One thread reads the source for as long as the process lives. Whichever machine is running takes
/// bytes from it, and can stop taking them without losing any.
#[derive(Clone)]
pub struct ConsoleInput(Arc<Mutex<Receiver<Vec<u8>>>>);

/// The input source reached its end; no more bytes will ever arrive.
#[derive(Debug, PartialEq, Eq)]
pub struct InputEnded;

impl ConsoleInput {
    pub fn from_reader(mut source: impl Read + Send + 'static) -> io::Result<Self> {
        let (bytes, queue) = mpsc::channel();
        let pump = move || {
            let mut chunk = [0u8; SERIAL_INPUT_CHUNK];
            while let Ok(read @ 1..) = source.read(&mut chunk) {
                if bytes.send(chunk[..read].to_vec()).is_err() {
                    return;
                }
            }
        };
        thread::Builder::new()
            .name(copy::CONSOLE_INPUT_THREAD.into())
            .spawn(pump)?;
        Ok(Self(Arc::new(Mutex::new(queue))))
    }

    /// The next bytes typed, or `None` if nothing arrived within `wait`.
    pub fn next(&self, wait: Duration) -> Result<Option<Vec<u8>>, InputEnded> {
        let queue = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match queue.recv_timeout(wait) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(InputEnded),
        }
    }
}

/// Where guest serial output goes and where its input comes from.
pub struct Console {
    pub output: Box<dyn Write + Send>,
    pub input: Option<ConsoleInput>,
}

impl Console {
    /// This process's standard output, with input from `input` if there is any.
    pub fn stdout(input: Option<ConsoleInput>) -> Self {
        Self {
            output: Box::new(io::stdout()),
            input,
        }
    }
}

/// Passes console output through and records when a marker string first appears in it.
pub struct MarkerWatch<W> {
    inner: W,
    marker: Vec<u8>,
    /// The last `marker.len() - 1` bytes seen, so a marker split across writes is still found.
    tail: Vec<u8>,
    seen: Arc<OnceLock<Instant>>,
}

impl<W: Write> MarkerWatch<W> {
    /// With no marker (or an empty one) output is only passed through.
    pub fn new(inner: W, marker: Option<&str>) -> (Self, Arc<OnceLock<Instant>>) {
        let seen = Arc::new(OnceLock::new());
        let marker = marker.unwrap_or_default().as_bytes().to_vec();
        (
            Self {
                inner,
                marker,
                tail: Vec::new(),
                seen: Arc::clone(&seen),
            },
            seen,
        )
    }

    fn scan(&mut self, fresh: &[u8]) {
        if self.marker.is_empty() || self.seen.get().is_some() {
            return;
        }
        self.tail.extend_from_slice(fresh);
        if self
            .tail
            .windows(self.marker.len())
            .any(|window| window == self.marker)
        {
            let _ = self.seen.set(Instant::now());
            self.tail = Vec::new();
            return;
        }
        let keep = self.marker.len() - 1;
        let excess = self.tail.len().saturating_sub(keep);
        self.tail.drain(..excess);
    }
}

impl<W: Write> Write for MarkerWatch<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.scan(buf);
        self.inner.write_all(buf)?;
        // The guest writes a byte at a time and a shell prompt has no newline, so nothing may linger.
        self.inner.flush()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(marker: Option<&str>, chunks: &[&str]) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let (mut watch, seen) = MarkerWatch::new(&mut out, marker);
        for chunk in chunks {
            watch.write_all(chunk.as_bytes()).unwrap();
        }
        let found = seen.get().is_some();
        drop(watch);
        (out, found)
    }

    #[test]
    fn input_is_delivered_in_order_then_reports_its_end() {
        let input = ConsoleInput::from_reader(&b"hello"[..]).unwrap();
        let wait = Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            match input.next(wait) {
                Ok(Some(bytes)) => seen.extend(bytes),
                Ok(None) => panic!("input stalled"),
                Err(InputEnded) => break,
            }
        }
        assert_eq!(seen, b"hello");
        assert_eq!(input.next(Duration::ZERO), Err(InputEnded));
    }

    #[test]
    fn waiting_for_input_times_out_without_ending_it() {
        let (reader, mut writer) = io::pipe().unwrap();
        let input = ConsoleInput::from_reader(reader).unwrap();
        assert_eq!(input.next(Duration::from_millis(10)), Ok(None));
        writer.write_all(b"x").unwrap();
        assert_eq!(
            input.clone().next(Duration::from_secs(5)),
            Ok(Some(b"x".to_vec()))
        );
    }

    #[test]
    fn output_passes_through_unchanged() {
        assert_eq!(feed(Some("x"), &["hello ", "world"]).0, b"hello world");
        assert_eq!(feed(None, &["hello"]).0, b"hello");
    }

    #[test]
    fn marker_is_found_within_one_write_and_across_byte_sized_writes() {
        assert!(feed(Some("init started"), &["[0.1] init started!\n"]).1);
        let bytes: Vec<String> = "xx init started".chars().map(String::from).collect();
        let bytes: Vec<&str> = bytes.iter().map(String::as_str).collect();
        assert!(feed(Some("init started"), &bytes).1);
    }

    #[test]
    fn near_misses_and_absent_markers_are_not_reported() {
        assert!(!feed(Some("init started"), &["init start", "ing"]).1);
        assert!(!feed(Some("abc"), &["ab", "ab", "ab"]).1);
        assert!(!feed(None, &["anything"]).1);
        assert!(!feed(Some(""), &["anything"]).1);
    }

    #[test]
    fn overlapping_prefix_does_not_hide_a_later_match() {
        assert!(feed(Some("aab"), &["aa", "ab"]).1);
    }
}

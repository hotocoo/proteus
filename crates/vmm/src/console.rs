//! Host side of the debug serial console.

use std::io::{self, Read, Write};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// Where guest serial output goes and where its input comes from.
pub struct Console {
    pub output: Box<dyn Write + Send>,
    pub input: Option<Box<dyn Read + Send>>,
}

impl Console {
    pub fn stdio() -> Self {
        Self {
            output: Box::new(io::stdout()),
            input: Some(Box::new(io::stdin())),
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

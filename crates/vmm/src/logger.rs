//! Diagnostics go to stderr and nowhere else.

use log::{LevelFilter, Log, Metadata, Record};

struct Stderr;

impl Log for Stderr {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        eprintln!(
            "proteus {}: {}",
            record.level().as_str().to_lowercase(),
            record.args()
        );
    }

    fn flush(&self) {}
}

pub fn init(level: LevelFilter) {
    // A second call (tests in one process) keeps the first logger, which is the same one.
    let _ = log::set_logger(&Stderr);
    log::set_max_level(level);
}

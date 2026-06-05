//! Minimal stderr logger shared by both binaries: `[LEVEL] message`.
//! Level controlled by RUST_LOG=error|warn|info|debug|trace (default: info).

use std::io::Write;

struct StderrLogger(log::LevelFilter);

impl log::Log for StderrLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.0
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            // Ignore write failures: a closed pipe or full disk mid-GC must
            // not panic the process (eprintln! would).
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "[{:5}] {}", record.level(), record.args());
        }
    }
    fn flush(&self) {}
}

/// Install the logger. Panics if a logger is already set.
pub fn init() {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(log::LevelFilter::Info);
    log::set_boxed_logger(Box::new(StderrLogger(level))).unwrap();
    log::set_max_level(level);
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log as _;

    #[test]
    fn respects_level_filter() {
        let logger = StderrLogger(log::LevelFilter::Info);
        let info = log::Metadata::builder().level(log::Level::Info).build();
        let debug = log::Metadata::builder().level(log::Level::Debug).build();
        assert!(logger.enabled(&info));
        assert!(!logger.enabled(&debug));
    }
}

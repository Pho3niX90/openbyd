//! The log: the client's `log` records go to stderr, scrubbed.

use std::io::Write;

use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::scrub::Scrubber;

struct StderrLog {
    level: LevelFilter,
    scrubber: &'static Scrubber,
}

/// The level for `-v` flags given: warnings only, then info, then the
/// tool's own and the client's debug records.
pub fn level_for(verbose: u8) -> LevelFilter {
    match verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        _ => LevelFilter::Debug,
    }
}

impl Log for StderrLog {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        if metadata.level() > self.level {
            return false;
        }
        // At -vv only our own crates are chatty; TLS and HTTP libraries stay at info.
        self.level < LevelFilter::Debug
            || metadata.level() <= Level::Info
            || metadata.target().starts_with("openbyd")
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = self
            .scrubber
            .scrub_text(&format!("{} {}", record.level(), record.args()));
        let _ = writeln!(std::io::stderr(), "openbyd: {line}");
    }

    fn flush(&self) {}
}

/// Installs the logger. Safe to call once per process.
pub fn init(verbose: u8, scrubber: &'static Scrubber) {
    let level = level_for(verbose);
    let logger: &'static StderrLog = Box::leak(Box::new(StderrLog { level, scrubber }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_the_verbosity() {
        assert_eq!(level_for(0), LevelFilter::Warn);
        assert_eq!(level_for(1), LevelFilter::Info);
        assert_eq!(level_for(2), LevelFilter::Debug);
        assert_eq!(level_for(9), LevelFilter::Debug);
    }

    #[test]
    fn debug_is_limited_to_our_crates() {
        let scrubber: &'static Scrubber = Box::leak(Box::default());
        let logger = StderrLog {
            level: LevelFilter::Debug,
            scrubber,
        };
        let meta = |level, target| Metadata::builder().level(level).target(target).build();
        assert!(logger.enabled(&meta(Level::Debug, "openbyd_client::mqtt")));
        assert!(!logger.enabled(&meta(Level::Debug, "rustls::client")));
        assert!(logger.enabled(&meta(Level::Info, "rustls::client")));
        assert!(!logger.enabled(&meta(Level::Trace, "openbyd_client")));
        let quiet = StderrLog {
            level: LevelFilter::Warn,
            scrubber,
        };
        assert!(!quiet.enabled(&meta(Level::Info, "openbyd_client")));
        logger.log(
            &Record::builder()
                .args(format_args!("hello"))
                .level(Level::Info)
                .target("openbyd")
                .build(),
        );
        logger.flush();
    }
}

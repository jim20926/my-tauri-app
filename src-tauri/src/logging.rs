use std::io::Write;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{LevelFilter, Log, Metadata, Record};

struct TerminalLogger {
    level: LevelFilter,
}

impl Log for TerminalLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(
            stderr,
            "ts_ms={} level={} module={} message={}",
            timestamp_ms,
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {}
}

static LOGGER: OnceLock<TerminalLogger> = OnceLock::new();

fn configured_level() -> LevelFilter {
    match std::env::var("RUST_LOG").ok().as_deref().map(str::trim) {
        Some("trace") => LevelFilter::Trace,
        Some("debug") => LevelFilter::Debug,
        Some("warn") => LevelFilter::Warn,
        Some("error") => LevelFilter::Error,
        Some("off") => LevelFilter::Off,
        _ => LevelFilter::Info,
    }
}

pub fn init() {
    let level = configured_level();
    let logger = LOGGER.get_or_init(|| TerminalLogger { level });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}

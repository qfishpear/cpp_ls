//! Stderr, without fern. Fern's output enum carries an `mpsc` sender, and linking that sender imports `sched_yield`.

use chrono::Local;
use log::{LevelFilter, info};

const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(super) fn init(level: LevelFilter) {
    let _ = log::set_boxed_logger(Box::new(StderrLogger { level }));
    log::set_max_level(level);
    info!("{} v{}", CRATE_NAME, CRATE_VERSION);
}

struct StderrLogger {
    level: LevelFilter,
}

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        eprintln!(
            "[{} {} {}] {}",
            Local::now().format("%Y-%m-%d %H:%M:%S %:z"),
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {}
}

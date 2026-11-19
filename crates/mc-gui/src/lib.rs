//! The Amel desktop app: a window that copies a site and shows the
//! crawl as it happens. The crawl itself runs on its own thread with its own
//! tokio runtime; the window only listens to its events.

pub mod app;
pub mod completion;
pub mod console;
pub mod highlight;
pub mod insight;
pub mod job;
pub mod look;
pub mod mini;
pub mod palette;
pub mod prefs;
pub mod projects;
pub mod sound;

pub use app::{CopyWindow, bind_keys};

use std::sync::OnceLock;
use std::time::Instant;

static STARTED: OnceLock<Instant> = OnceLock::new();

/// Marks the moment the app started, for the startup probe.
pub fn mark_start() {
    let _ = STARTED.set(Instant::now());
}

/// Milliseconds since [`mark_start`].
pub fn startup_ms() -> u128 {
    STARTED.get().map_or(0, |start| start.elapsed().as_millis())
}

/// Resident memory of this process in KiB, where the OS reports it (Linux).
pub fn rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

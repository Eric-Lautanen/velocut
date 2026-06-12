// crates/velocut-media/src/helpers/log.rs
//
// File-based logging for the media crate.
//
// In release builds with `windows_subsystem = "windows"` (double-click launch),
// there is no console attached, so `eprintln!` output is silently discarded.
// All log calls go to a temp file instead so they're visible regardless of
// launch mode.
//
// File: %TEMP%\velocut.log  — append-only, created on first write per session.
//
// The file handle is held open for the process lifetime via a OnceLock<Mutex<File>>.
//
// Usage:
//   use crate::helpers::log::media_log;
//   media_log!("[encode] probe: AMF available");
//   media_log!("[pb] Start received (active), ts={:.3}", ts);

use std::io::Write;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

/// Write a formatted message to the VeloCut log file in the OS temp directory.
/// Never panics — failures are silently ignored.
pub fn log_impl(msg: &str) {
    let slot = LOG_FILE.get_or_init(|| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::env::temp_dir().join("velocut.log"))
            .ok()
            .map(Mutex::new)
    });

    if let Some(mutex) = slot {
        if let Ok(mut f) = mutex.lock() {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(f, "[{ts}] {msg}");
        }
    }
}

/// Convenience macro — formats like `eprintln!` but routes through `log_impl`.
#[macro_export]
macro_rules! media_log {
    ($($arg:tt)*) => {
        $crate::helpers::log::log_impl(&format!($($arg)*))
    };
}

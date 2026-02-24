// crates/velocut-ui/src/helpers/log.rs
//
// Unified logging for the UI crate.
//
// In release builds with `windows_subsystem = "windows"` (double-click launch),
// there is no console attached, so `eprintln!` output is silently discarded.
// All log calls go to a temp file instead so they're visible regardless of
// launch mode.
//
// File: %TEMP%\velocut.log  — append-only, created on first write per session.
//
// The file handle is held open for the process lifetime via a OnceLock<Mutex<File>>.
// Previously the file was opened and closed on every vlog() call. audio_module
// calls audio_log() every tick while waiting for WAV extraction (~60 calls/sec),
// which caused noticeable overhead from repeated open/close syscalls.
//
// Usage:
//   use crate::helpers::log::vlog;
//   vlog("[app] aspect ratio auto-set");
//
// Or use the macro for format string convenience:
//   velocut_log!("[export] no clips — aborting render");
//   velocut_log!("[media] {id}: {msg}");

use std::io::Write;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

/// Write `msg` to the VeloCut log file in the OS temp directory.
/// Never panics — failures are silently ignored (we're already in a fallback path).
/// The file handle is kept open for the process lifetime to avoid per-call syscall
/// overhead when called from high-frequency paths like audio_module::tick().
pub fn vlog(msg: &str) {
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

/// Convenience macro — formats like `eprintln!` but routes through `vlog`.
#[macro_export]
macro_rules! velocut_log {
    ($($arg:tt)*) => {
        $crate::helpers::log::vlog(&format!($($arg)*))
    };
}
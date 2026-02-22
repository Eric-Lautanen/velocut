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
// Usage:
//   use crate::helpers::log::vlog;
//   vlog("[app] aspect ratio auto-set");
//
// Or use the macro for format string convenience:
//   velocut_log!("[export] no clips — aborting render");
//   velocut_log!("[media] {id}: {msg}");

use std::io::Write;

/// Write `msg` to the VeloCut log file in the OS temp directory.
/// Never panics — failures are silently ignored (we're already in a fallback path).
pub fn vlog(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("velocut.log"))
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

/// Convenience macro — formats like `eprintln!` but routes through `vlog`.
#[macro_export]
macro_rules! velocut_log {
    ($($arg:tt)*) => {
        $crate::helpers::log::vlog(&format!($($arg)*))
    };
}
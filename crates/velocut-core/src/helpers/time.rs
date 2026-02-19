// crates/velocut-core/src/helpers/time.rs
//
// Shared time-formatting utilities used by both velocut-ui and any future
// crates that need human-readable timestamps.
//
// Canonical source for format_time() — previously duplicated in
// velocut-ui/src/modules/timeline.rs (as `format_time`) and
// velocut-ui/src/modules/library.rs (as `format_duration`, diverged format).

/// Format a duration in seconds as `MM:SS:FF` (minutes, seconds, frames at 30 fps).
///
/// Used on the timeline ruler where frame-level precision matters.
///
/// ```
/// use velocut_core::helpers::time::format_time;
/// assert_eq!(format_time(0.0),    "00:00:00");
/// assert_eq!(format_time(61.5),   "01:01:15");
/// assert_eq!(format_time(3599.0), "59:59:00");
/// ```
pub fn format_time(s: f64) -> String {
    let m  = (s / 60.0) as u32;
    let sc = (s % 60.0) as u32;
    let fr = ((s * 30.0) as u32) % 30;
    format!("{m:02}:{sc:02}:{fr:02}")
}

/// Format a duration in seconds as a compact human-readable string.
///
/// Used in the library panel where frame-level precision is unnecessary.
///
/// | Range         | Format       | Example   |
/// |---------------|--------------|-----------|
/// | ≥ 3600 s      | `H:MM:SS`    | `1:04:35` |
/// | ≥ 60 s        | `M:SS`       | `3:07`    |
/// | < 60 s        | `S.Xs`       | `4.2s`    |
///
/// ```
/// use velocut_core::helpers::time::format_duration;
/// assert_eq!(format_duration(4.2),    "4.2s");
/// assert_eq!(format_duration(187.0),  "3:07");
/// assert_eq!(format_duration(3875.0), "1:04:35");
/// ```
pub fn format_duration(secs: f64) -> String {
    if secs >= 3600.0 {
        format!(
            "{}:{:02}:{:02}",
            secs as u64 / 3600,
            (secs as u64 % 3600) / 60,
            secs as u64 % 60,
        )
    } else if secs >= 60.0 {
        format!("{}:{:02}", secs as u64 / 60, secs as u64 % 60)
    } else {
        format!("{secs:.1}s")
    }
}
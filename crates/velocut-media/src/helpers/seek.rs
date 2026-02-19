// crates/velocut-media/src/helpers/seek.rs
//
// Seek helper wrapping ffmpeg's avformat seek with consistent Windows EPERM
// soft-fail behaviour.
//
// Background:
//   On Windows, `avformat_seek_file` returns EPERM ("Operation not permitted")
//   in certain conditions — notably when called on a freshly-opened context
//   with max_ts=0, or on some container formats that don't support random
//   access. This is documented in the VeloCut architecture reference and has
//   been hit in both encode_clip (source_offset == 0 guard) and
//   decode_clip_frames (crossfade tail/head decode).
//
//   Rather than duplicating the guard + eprintln pattern at every call site,
//   all seeks route through here. The caller chooses how to handle failure
//   via the return value — hard error vs soft-fail is a policy decision at
//   the call site, not here.

use ffmpeg_the_third as ffmpeg;

/// Seek `ictx` to `target_secs` seconds from the start of the file.
///
/// Returns `true` if the seek succeeded (or was skipped because target is 0).
/// Returns `false` if the seek failed — the demuxer will decode from wherever
/// it currently is, and the caller's PTS-based frame filtering will skip
/// pre-roll frames correctly.
///
/// Always logs a warning on failure so seek issues are visible in the console
/// without crashing the encode.
///
/// # Why skip at 0.0
/// `avformat_seek_file(max_ts=0)` returns EPERM on Windows when called on a
/// freshly-opened context. Since the demuxer starts at position 0 by default,
/// skipping the seek entirely is both correct and avoids the error.
pub fn seek_to_secs(
    ictx: &mut ffmpeg::format::context::Input,
    target_secs: f64,
    label: &str,   // caller description for log messages e.g. "encode_clip"
) -> bool {
    if target_secs <= 0.0 {
        return true; // already at start — no seek needed
    }

    let seek_ts = (target_secs * ffmpeg::ffi::AV_TIME_BASE as f64) as i64;
    match ictx.seek(seek_ts, seek_ts..) {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "[seek] soft-fail in {label} at {target_secs:.3}s: {e} \
                 — decoding from current position, PTS filter will skip pre-roll"
            );
            false
        }
    }
}
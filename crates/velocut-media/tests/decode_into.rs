//! Playback decodes into a buffer the caller keeps.
//!
//! `LiveDecoder::next_frame_into` exists so the playback thread can pass a frame
//! buffer it already owns — normally one the UI has finished with. The property
//! that matters is not "it returns the right pixels" (the pipeline's own tests
//! cover playback) but that it does not quietly reallocate: a fresh 5 MB frame
//! costs about 1230 page faults to write, per frame, every frame.
//!
//! These run over `sequence_01.mp4`, a tracked asset, so they are real decodes
//! rather than mocks.
use std::path::PathBuf;

use velocut_media::decode::LiveDecoder;

fn source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sequence_01.mp4")
        .canonicalize()
        .expect("sample media")
}

/// Decode `count` frames from the start, returning (pointer, capacity, length,
/// checksum) per frame.
fn decode_sequence(reuse: Option<&mut Vec<u8>>, count: usize) -> Vec<(usize, usize, usize, u64)> {
    ffmpeg_the_third::init().expect("ffmpeg init");
    let path = source();
    let mut decoder =
        LiveDecoder::open(&path, 0.0, 0.0, None, Some((160, 90))).expect("open sample clip");

    let mut owned = Vec::new();
    let out: &mut Vec<u8> = match reuse {
        Some(buf) => buf,
        None => &mut owned,
    };

    let mut seen = Vec::new();
    for _ in 0..count {
        let Some((w, h, _ts)) = decoder.next_frame_into(&mut *out) else {
            break;
        };
        let checksum = out
            .iter()
            .fold(0u64, |acc, &b| acc.wrapping_mul(31).wrapping_add(b as u64));
        seen.push((
            out.as_ptr() as usize,
            (w * h * 4) as usize,
            out.len(),
            checksum,
        ));
    }
    seen
}

#[test]
fn decoding_into_a_kept_buffer_never_reallocates() {
    let mut buf: Vec<u8> = Vec::new();
    let frames = decode_sequence(Some(&mut buf), 8);
    assert!(
        frames.len() >= 5,
        "expected several frames, got {}",
        frames.len()
    );

    let (first_ptr, ..) = frames[0];
    for (i, &(ptr, want_len, len, _)) in frames.iter().enumerate() {
        assert_eq!(len, want_len, "frame {i} is not a full frame");
        // The allocation is stable from the first frame on: no realloc, so no
        // fresh pages to fault in, so no per-frame cost beyond writing pixels.
        assert_eq!(
            ptr, first_ptr,
            "frame {i} was decoded into a new allocation"
        );
    }
}

#[test]
fn decoding_into_a_buffer_is_pixel_identical_to_the_allocating_path() {
    // Same clip, same start, same size: the two entry points must agree pixel for
    // pixel, otherwise "reuse" would silently change what playback shows.
    ffmpeg_the_third::init().expect("ffmpeg init");
    let path = source();

    let mut parity =
        LiveDecoder::open(&path, 0.0, 0.0, None, Some((160, 90))).expect("open sample clip");
    let mut buf = Vec::new();
    for _ in 0..4 {
        let Some((_, _, _)) = parity.next_frame_into(&mut buf) else {
            break;
        };
        assert!(!buf.is_empty());
    }

    let mut a = LiveDecoder::open(&path, 0.0, 0.0, None, Some((160, 90))).expect("open a");
    let mut b = LiveDecoder::open(&path, 0.0, 0.0, None, Some((160, 90))).expect("open b");
    let mut kept: Vec<u8> = Vec::new();
    for i in 0..4 {
        let Some((w, h, _)) = a.next_frame_into(&mut kept) else {
            break;
        };
        let Some((data, w2, h2, _)) = b.next_frame() else {
            break;
        };
        assert_eq!((w, h), (w2, h2), "frame {i} dims");
        assert_eq!(kept.len(), data.len(), "frame {i} length");
        assert_eq!(kept, data, "frame {i} pixels");
    }
}

/// What the allocation actually costs, per decoded frame.
///
/// Ignored because it is a measurement, not an assertion: both arms decode the
/// same clip at the same size, so the only difference is where the pixels land.
/// Run it explicitly with `--ignored --nocapture`; treat the milliseconds as
/// order-of-magnitude on a shared machine, and the pointer check above as the
/// durable property.
#[test]
#[ignore = "measurement only — run explicitly with --ignored --nocapture"]
fn decode_cost_per_frame_fresh_vs_reused() {
    use std::time::Instant;

    const FRAMES: usize = 40;

    ffmpeg_the_third::init().expect("ffmpeg init");
    let path = source();

    let mut fresh_total = f64::INFINITY;
    for _ in 0..3 {
        let mut decoder =
            LiveDecoder::open(&path, 0.0, 0.0, None, Some((1504, 832))).expect("open");
        let started = Instant::now();
        let mut n = 0;
        while decoder.next_frame().is_some() && n < FRAMES {
            n += 1;
        }
        if n > 0 {
            fresh_total = fresh_total.min(started.elapsed().as_secs_f64() * 1000.0 / n as f64);
        }
    }

    let mut reused_total = f64::INFINITY;
    for _ in 0..3 {
        let mut decoder =
            LiveDecoder::open(&path, 0.0, 0.0, None, Some((1504, 832))).expect("open");
        let mut buf = Vec::new();
        let started = Instant::now();
        let mut n = 0;
        while decoder.next_frame_into(&mut buf).is_some() && n < FRAMES {
            n += 1;
        }
        if n > 0 {
            reused_total = reused_total.min(started.elapsed().as_secs_f64() * 1000.0 / n as f64);
        }
    }

    println!("\nDecode at 1504×832, best of 3 runs of up to {FRAMES} frames:");
    println!("  fresh Vec per frame  {fresh_total:.2} ms/frame");
    println!("  reused buffer        {reused_total:.2} ms/frame");
    println!(
        "  difference           {:.2} ms/frame\n",
        fresh_total - reused_total
    );
}

#[test]
fn reused_frame_slots_survive_mixed_call_styles() {
    // `next_frame_into` and the allocating `next_frame` wrapper share the same
    // AVFrame slots inside a `LiveDecoder`, as do the seek/burn loops. Interleave
    // them on one decoder against a second decoder that only ever uses one entry
    // point: if a slot kept a stale buffer, a stale reference or a wrong stride,
    // the pixels or the timestamps would drift apart.
    ffmpeg_the_third::init().expect("ffmpeg init");
    let path = source();

    let mut mixed = LiveDecoder::open(&path, 0.0, 0.0, None, Some((160, 90))).expect("open mixed");
    let mut reference =
        LiveDecoder::open(&path, 0.0, 0.0, None, Some((160, 90))).expect("open reference");

    let mut kept: Vec<u8> = Vec::new();
    let mut last_ts = f64::NEG_INFINITY;
    let mut compared = 0;

    for i in 0..6 {
        let (pixels, ts) = if i % 2 == 0 {
            let Some((_, _, ts)) = mixed.next_frame_into(&mut kept) else {
                break;
            };
            (kept.clone(), ts)
        } else {
            let Some((data, _, _, ts)) = mixed.next_frame() else {
                break;
            };
            (data, ts)
        };

        let Some((want, _, _, want_ts)) = reference.next_frame() else {
            break;
        };

        assert_eq!(pixels.len(), want.len(), "frame {i} length");
        assert_eq!(pixels, want, "frame {i} pixels");
        assert!(
            ts > last_ts,
            "frame {i} timestamp went backwards: {ts} after {last_ts}"
        );
        assert!(
            (ts - want_ts).abs() < 1e-9,
            "frame {i} timestamp {ts} vs reference {want_ts}"
        );
        last_ts = ts;
        compared += 1;
    }

    assert!(compared >= 4, "only compared {compared} frames");
}

/// What two `Video::empty()` calls per frame cost — the work this change removed.
///
/// `av_frame_alloc`/`av_frame_free` are FFmpeg's own C allocator, so they never
/// show up in a Rust allocation counter; timing the exact operations is the only
/// way to put a number on them. Ignored because it is a measurement, not an
/// assertion: run it with `--ignored --nocapture`.
#[test]
#[ignore = "measurement only — run explicitly with --ignored --nocapture"]
fn empty_frame_struct_cost() {
    use ffmpeg_the_third::util::frame::video::Video;
    use std::time::Instant;

    const ROUNDS: usize = 200_000;

    // One warm-up pass so allocator setup is not measured.
    for _ in 0..1000 {
        std::hint::black_box(Video::empty());
    }

    // Two frames per decoded frame: the receive target and the hw/CPU slot.
    let started = Instant::now();
    for _ in 0..ROUNDS {
        let a = Video::empty();
        let b = Video::empty();
        std::hint::black_box((&a, &b));
    }
    let per_frame = started.elapsed().as_secs_f64() * 1e6 / ROUNDS as f64;

    println!("\nalloc + free of the two AVFrames a decoded frame used to make:");
    println!("  {per_frame:.3} µs/frame ({ROUNDS} rounds)\n");
}

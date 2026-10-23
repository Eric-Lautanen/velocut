//! End-to-end encode checks for the centred transition model.
//!
//! These drive the real H.264/AAC pipeline over `sequence_01.mp4` (a tracked
//! asset). A transition is a zone of D seconds centred on the cut, taking D/2
//! from each neighbour, so a boundary must never change the export's total
//! length — and an over-long transition must clamp rather than run past the end
//! of a clip's material.
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::unbounded;
use velocut_core::media_types::MediaResult;
use velocut_core::transitions::{ClipTransition, TransitionKind, TransitionType};
use velocut_media::encode::{encode_timeline, ClipSpec, EncodeSpec};

fn clip(path: &Path, source_offset: f64, duration: f64) -> ClipSpec {
    ClipSpec {
        path: path.to_path_buf(),
        // Callers that lay clips out sequentially override this; the default
        // keeps the single-clip helpers terse.
        timeline_start: 0.0,
        source_offset,
        duration,
        volume: 1.0,
        skip_audio: false,
        fade_in_secs: 0.0,
        fade_in_start_secs: 0.0,
        fade_out_secs: 0.0,
        fade_out_end_secs: 0.0,
        filter: Default::default(),
    }
}

fn run_case(transition_secs: f32, tag: &str) -> (f64, PathBuf) {
    ffmpeg_the_third::init().expect("ffmpeg init");

    let root = out_source();
    let out = std::env::temp_dir().join(format!("velocut_centred_check_{tag}.mp4"));
    let _ = std::fs::remove_file(&out);

    // Two 1.0 s clips with a 0.6 s crossfade on the cut.
    let spec = EncodeSpec {
        job_id: uuid::Uuid::new_v4(),
        clips: vec![
            clip(&root, 0.0, 1.0),
            ClipSpec {
                timeline_start: 1.0,
                ..clip(&root, 1.0, 1.0)
            },
        ],
        width: export_size().0,
        height: export_size().1,
        fps: 30,
        output: out.clone(),
        transitions: vec![ClipTransition {
            after_clip_index: 0,
            kind: TransitionType::new(TransitionKind::Crossfade, transition_secs),
        }],
        audio_overlays: Vec::new(),
    };

    let (tx, rx) = unbounded();
    encode_timeline(spec, Arc::new(AtomicBool::new(false)), tx);

    let mut done = false;
    let mut last_error = None;
    while let Ok(msg) = rx.recv_timeout(Duration::from_secs(60)) {
        match msg {
            MediaResult::EncodeDone { .. } => {
                done = true;
                break;
            }
            MediaResult::EncodeError { msg, .. } => {
                last_error = Some(msg);
                break;
            }
            _ => {}
        }
    }
    assert!(last_error.is_none(), "encode failed: {last_error:?}");
    assert!(done, "encode never reported done");

    let meta = std::fs::metadata(&out).expect("output file");
    assert!(
        meta.len() > 10_000,
        "output suspiciously small: {}",
        meta.len()
    );

    // Probe the result: the centred model must preserve the 2.0 s timeline.
    let ictx = ffmpeg_the_third::format::input(&out).expect("open output");
    let secs = ictx.duration() as f64 / 1_000_000.0;
    assert!(
        (secs - 2.0).abs() < 0.2,
        "expected ~2.0s of output, got {secs:.3}s"
    );
    let has_audio = ictx
        .streams()
        .any(|s| s.parameters().medium() == ffmpeg_the_third::media::Type::Audio);
    assert!(has_audio, "export lost its audio stream");
    (secs, out)
}

#[test]
fn centred_crossfade_export_runs_and_keeps_the_timeline_length() {
    let (secs, _) = run_case(0.6, "normal");
    assert!((secs - 2.0).abs() < 0.2, "got {secs}");
}

#[test]
fn overlong_transition_is_clamped_and_still_preserves_length() {
    // 3 s of transition on two 1 s clips: clamped to 2 × 1 s, so each half is
    // the whole clip and the export is still exactly the 2 s timeline.
    let (secs, _) = run_case(3.0, "clamped");
    assert!((secs - 2.0).abs() < 0.2, "got {secs}");
}

// ── Pixel parity with the centred (preview) model ────────────────────────────

const FPS: u32 = 30;

/// Export dimensions that match the source's own aspect, so neither the
/// encoder's centre-crop nor the scrub decoder's centre-crop has to act. Then
/// the two RGBA images differ only by lossy compression, which keeps the
/// pixel comparison honest.
fn export_size() -> (u32, u32) {
    let src = out_source();
    let (_, w, h) =
        velocut_media::decode::decode_one_frame_rgba(&src, 0.0, 0.0).expect("probe source size");
    let mut oh = (320.0_f64 * h as f64 / w as f64).round() as u32;
    oh -= oh % 2; // YUV420 needs even dimensions
                  // Let the scrub decoder pick the exact output height for that aspect; the
                  // export then uses the very same size, so the comparison is pixel-aligned.
    let (_, dw, dh) = velocut_media::decode::decode_one_frame_rgba(&src, 0.0, 320.0 / oh as f32)
        .expect("probe scrub size");
    (dw, dh)
}

/// Decode one frame of an RGBA-encoded export by 0-based video frame index.
fn read_export_frame(path: &Path, want: usize, (out_w, out_h): (u32, u32)) -> Vec<u8> {
    use ffmpeg_the_third as ffmpeg;
    let mut ictx = ffmpeg::format::input(path).expect("open export");
    let stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .expect("video stream");
    let index = stream.index();
    let mut dec = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .expect("decoder ctx")
        .decoder()
        .video()
        .expect("video decoder");
    let mut scaler = ffmpeg::software::scaling::Context::get(
        dec.format(),
        dec.width(),
        dec.height(),
        ffmpeg::format::Pixel::RGBA,
        out_w,
        out_h,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )
    .expect("scaler");

    let mut seen = 0usize;
    for packet in ictx.packets() {
        let (pkt_stream, packet) = packet.expect("packet");
        if pkt_stream.index() != index {
            continue;
        }
        if dec.send_packet(&packet).is_err() {
            continue;
        }
        let mut frame = ffmpeg::util::frame::video::Video::empty();
        while dec.receive_frame(&mut frame).is_ok() {
            if seen == want {
                let mut rgba = ffmpeg::util::frame::video::Video::empty();
                scaler.run(&frame, &mut rgba).expect("scale");
                let stride = rgba.stride(0);
                let mut out = Vec::with_capacity((out_w * out_h * 4) as usize);
                for y in 0..out_h as usize {
                    let row = &rgba.data(0)[y * stride..y * stride + (out_w * 4) as usize];
                    out.extend_from_slice(row);
                }
                return out;
            }
            seen += 1;
        }
    }
    panic!("export has only {seen} frames, wanted #{want}");
}

fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: u64 = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u64).sum();
    sum as f64 / a.len() as f64
}

fn source_frame(path: &Path, ts: f64, (out_w, out_h): (u32, u32)) -> Vec<u8> {
    let aspect = out_w as f32 / out_h as f32;
    let (rgba, w, h) =
        velocut_media::decode::decode_one_frame_rgba(&path.to_path_buf(), ts, aspect)
            .expect("decode source frame");
    assert_eq!(
        (w, h),
        (out_w, out_h),
        "scrub decode size must match the export"
    );
    rgba
}

fn blend(a: &[u8], b: &[u8], alpha: f32, (out_w, out_h): (u32, u32)) -> Vec<u8> {
    let registry = velocut_core::transitions::registry();
    let crossfade = registry
        .get(&TransitionKind::Crossfade)
        .expect("crossfade registered");
    crossfade.apply_rgba(a, b, out_w, out_h, alpha)
}

#[test]
fn exported_blend_is_centred_on_the_cut_not_tail_aligned() {
    // Two 1 s clips cut at 1 s with D = 0.6 s. Centred model: body = 0.7 s
    // (21 frames), blend = 18 frames over [0.7, 1.3), clip_a's tail plays
    // source [0.7, 1.0) while clip_b holds its first frame until the cut.
    // Tail-aligned (old) model would instead start the blend at 0.4 s and pull
    // clip_a from source [0.4, 1.0) — 0.3 s earlier in the material.
    let (_, out) = run_case(0.6, "pixel");
    let size = export_size();
    let a_ts_centred = 0.7 + 1.0 / FPS as f64; // 2nd tail frame
    let a_ts_tail_aligned = 0.4 + 1.0 / FPS as f64; // 2nd frame of the old tail
    let b_ts = 1.0; // clip_b holds its first frame through the first half
    let alpha = 2.0 / 19.0; // frame_alpha(1, 18)

    let src = out_source();
    let a_centred = source_frame(&src, a_ts_centred, size);
    let a_tail = source_frame(&src, a_ts_tail_aligned, size);
    let b = source_frame(&src, b_ts, size);

    let want_centred = blend(&a_centred, &b, alpha, size);
    let want_tail = blend(&a_tail, &b, alpha, size);

    let got = read_export_frame(&out, 22, size);

    let d_centred = mean_abs_diff(&got, &want_centred);
    let d_tail = mean_abs_diff(&got, &want_tail);
    println!("pixel distance: centred {d_centred:.2} vs tail-aligned {d_tail:.2}");
    assert!(
        d_centred < d_tail,
        "exported blend matches the tail-aligned model more closely \
         (centred {d_centred:.2}, tail-aligned {d_tail:.2}) — the model regressed"
    );
    // The two models differ by a third of a second of source material, so the
    // margin should be unmistakable, not a rounding artefact.
    assert!(
        d_tail > d_centred * 1.5,
        "margin too thin to be meaningful (centred {d_centred:.2}, tail {d_tail:.2})"
    );
}

fn out_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sequence_01.mp4")
        .canonicalize()
        .expect("sample media")
}

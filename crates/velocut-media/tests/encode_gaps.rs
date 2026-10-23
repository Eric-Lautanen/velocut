//! End-to-end checks that timeline gaps survive an export.
//!
//! The preview shows black wherever no clip is under the playhead, and mixes
//! standalone audio overlays at their absolute timeline position. The encoder
//! used to concatenate `ClipSpec`s back-to-back, so any gap (including a
//! leading one) shortened the output and shifted every later clip earlier than
//! its audio. These tests drive the real H.264/AAC pipeline over
//! `sequence_01.mp4` and check the resulting duration, black gap frames and
//! clip positions.
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::unbounded;
use velocut_core::media_types::MediaResult;
use velocut_media::encode::{encode_timeline, ClipSpec, EncodeSpec};

const FPS: u32 = 30;
const OUT_W: u32 = 320;
const OUT_H: u32 = 180;

fn clip(path: &Path, timeline_start: f64, duration: f64) -> ClipSpec {
    ClipSpec {
        path: path.to_path_buf(),
        timeline_start,
        source_offset: 0.0,
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

fn encode(spec: EncodeSpec) -> PathBuf {
    ffmpeg_the_third::init().expect("ffmpeg init");
    let out = spec.output.clone();
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
    out
}

/// Decode one exported frame by 0-based index and return its mean RGB value.
fn frame_luma(path: &Path, want: usize) -> f64 {
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
        OUT_W,
        OUT_H,
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
                let mut sum = 0u64;
                let mut count = 0u64;
                for y in 0..OUT_H as usize {
                    let row = &rgba.data(0)[y * stride..y * stride + (OUT_W * 4) as usize];
                    for px in row.chunks_exact(4) {
                        sum += px[0] as u64 + px[1] as u64 + px[2] as u64;
                        count += 3;
                    }
                }
                return sum as f64 / count as f64;
            }
            seen += 1;
        }
    }
    panic!("export has only {seen} frames, wanted #{want}");
}

#[test]
fn leading_and_middle_gaps_are_black_and_keep_clips_in_place() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sequence_01.mp4")
        .canonicalize()
        .expect("sample media");
    let out = std::env::temp_dir().join("velocut_gap_check.mp4");
    let _ = std::fs::remove_file(&out);

    // Timeline: 1s gap, clip at 1s, 1s gap, clip at 3s → 4s total.
    let spec = EncodeSpec {
        job_id: uuid::Uuid::new_v4(),
        clips: vec![clip(&src, 1.0, 1.0), clip(&src, 3.0, 1.0)],
        width: OUT_W,
        height: OUT_H,
        fps: FPS,
        output: out.clone(),
        transitions: Vec::new(),
        audio_overlays: Vec::new(),
    };
    let out = encode(spec);

    let meta = std::fs::metadata(&out).expect("output file");
    assert!(
        meta.len() > 10_000,
        "output suspiciously small: {}",
        meta.len()
    );

    let ictx = ffmpeg_the_third::format::input(&out).expect("open output");
    let secs = ictx.duration() as f64 / 1_000_000.0;
    assert!(
        (secs - 4.0).abs() < 0.2,
        "gaps must count towards the duration: expected ~4.0s, got {secs:.3}s"
    );
    assert!(
        ictx.streams()
            .any(|s| s.parameters().medium() == ffmpeg_the_third::media::Type::Audio),
        "export lost its audio stream"
    );
    drop(ictx);

    // Frames [0,30) gap, [30,60) first clip, [60,90) gap, [90,120) second clip.
    let leading = frame_luma(&out, 15);
    let first = frame_luma(&out, 45);
    let middle = frame_luma(&out, 75);
    let second = frame_luma(&out, 105);
    println!(
        "mean RGB: leading {leading:.1}, first {first:.1}, middle {middle:.1}, second {second:.1}"
    );

    assert!(leading < 4.0, "leading gap is not black: {leading:.1}");
    assert!(first > 10.0, "first clip is missing/black: {first:.1}");
    assert!(middle < 4.0, "middle gap is not black: {middle:.1}");
    assert!(
        second > 10.0,
        "second clip shifted out of its slot: {second:.1}"
    );
}

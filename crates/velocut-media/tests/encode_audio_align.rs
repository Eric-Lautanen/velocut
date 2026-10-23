//! End-to-end check that the exported audio stream stays locked to the video
//! timeline when a clip contributes no audio.
//!
//! `skip_audio` stands in for a video-only source (or a clip whose audio track
//! is missing/ends early): the AAC stream's PTS come from the sample counter,
//! so without silence padding the audio simply ends early and every later clip
//! and absolute-time audio overlay is shifted earlier than the picture.
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::unbounded;
use velocut_core::media_types::MediaResult;
use velocut_media::encode::{encode_timeline, ClipSpec, EncodeSpec};

fn clip(path: &Path, timeline_start: f64, duration: f64, skip_audio: bool) -> ClipSpec {
    ClipSpec {
        path: path.to_path_buf(),
        timeline_start,
        source_offset: 0.0,
        duration,
        volume: 1.0,
        skip_audio,
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

/// End of the last audio packet in seconds (works even when the container does
/// not fill in the stream duration).
fn audio_end_secs(path: &Path) -> f64 {
    use ffmpeg_the_third as ffmpeg;
    let mut ictx = ffmpeg::format::input(path).expect("open export");
    let (idx, tb) = {
        let stream = ictx
            .streams()
            .best(ffmpeg::media::Type::Audio)
            .expect("export lost its audio stream");
        (stream.index(), f64::from(stream.time_base()))
    };

    let mut end = 0.0_f64;
    for packet in ictx.packets() {
        let (stream, packet) = packet.expect("packet");
        if stream.index() != idx {
            continue;
        }
        if let Some(pts) = packet.pts() {
            let packet_end = pts as f64 * tb + packet.duration() as f64 * tb;
            end = end.max(packet_end);
        }
    }
    end
}

fn run_case(tag: &str, first_skip_audio: bool, second_skip_audio: bool) -> f64 {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sequence_01.mp4")
        .canonicalize()
        .expect("sample media");
    let out = std::env::temp_dir().join(format!("velocut_audio_align_{tag}.mp4"));
    let _ = std::fs::remove_file(&out);

    let spec = EncodeSpec {
        job_id: uuid::Uuid::new_v4(),
        clips: vec![
            clip(&src, 0.0, 1.0, first_skip_audio),
            clip(&src, 1.0, 1.0, second_skip_audio),
        ],
        width: 320,
        height: 180,
        fps: 30,
        output: out.clone(),
        transitions: Vec::new(),
        audio_overlays: Vec::new(),
    };
    let out = encode(spec);
    let secs = audio_end_secs(&out);
    println!("{tag}: audio ends at {secs:.3}s");
    secs
}

#[test]
fn silent_first_clip_does_not_shorten_the_exported_audio() {
    let secs = run_case("first_silent", true, false);
    assert!(
        secs > 1.9,
        "audio must cover both clips (2.0s); a silent first clip left it at {secs:.3}s"
    );
}

#[test]
fn silent_second_clip_still_emits_its_silence() {
    let secs = run_case("second_silent", false, true);
    assert!(
        secs > 1.9,
        "the trailing silent clip must extend the audio to the video end; got {secs:.3}s"
    );
}

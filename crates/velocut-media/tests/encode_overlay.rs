//! End-to-end check that standalone audio overlays are mixed at their absolute
//! timeline position, including over a clip that contributes no audio.
//!
//! The export used to concatenate clip audio, so an overlay starting at 0.5 s
//! landed at the front of the audio stream whenever an earlier clip was silent.
//! Here the video clip is silent (`skip_audio`), the overlay is a 440 Hz tone
//! from 0.5 s to 1.5 s, and the decoded export must be silent → tone → silent.
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::unbounded;
use velocut_core::media_types::MediaResult;
use velocut_media::encode::{encode_timeline, AudioOverlay, ClipSpec, EncodeSpec};

const SR: u32 = 44_100;

fn write_tone_wav(path: &Path, secs: f64, freq: f64, amplitude: f64) {
    let frames = (secs * SR as f64).round() as u32;
    let data_len = frames * 4; // stereo i16
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&2u16.to_le_bytes()); // channels
    bytes.extend_from_slice(&SR.to_le_bytes());
    bytes.extend_from_slice(&(SR * 4).to_le_bytes()); // byte rate
    bytes.extend_from_slice(&4u16.to_le_bytes()); // block align
    bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..frames {
        let phase = std::f64::consts::TAU * freq * i as f64 / SR as f64;
        let sample = (amplitude * phase.sin() * i16::MAX as f64) as i16;
        bytes.extend_from_slice(&sample.to_le_bytes());
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, bytes).expect("write tone wav");
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

/// RMS of the exported audio per `window_secs` window, from timeline 0.
fn rms_windows(path: &Path, window_secs: f64) -> Vec<f64> {
    use ffmpeg_the_third as ffmpeg;
    let mut ictx = ffmpeg::format::input(path).expect("open export");
    let stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Audio)
        .expect("export lost its audio stream");
    let index = stream.index();
    let mut dec = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .expect("decoder ctx")
        .decoder()
        .audio()
        .expect("audio decoder");

    let window = (window_secs * SR as f64).round() as usize;
    let mut sums: Vec<f64> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    let mut pos = 0usize;
    let mut frame = ffmpeg::util::frame::audio::Audio::empty();

    for packet in ictx.packets() {
        let (packet_stream, packet) = packet.expect("packet");
        if packet_stream.index() != index {
            continue;
        }
        if dec.send_packet(&packet).is_err() {
            continue;
        }
        while dec.receive_frame(&mut frame).is_ok() {
            assert_eq!(
                frame.format(),
                ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Planar),
                "unexpected export audio format"
            );
            let n = frame.samples();
            // SAFETY: AAC decodes to planar f32; `n` is the plane length.
            let left =
                unsafe { std::slice::from_raw_parts(frame.data(0).as_ptr() as *const f32, n) };
            let right =
                unsafe { std::slice::from_raw_parts(frame.data(1).as_ptr() as *const f32, n) };
            for (l, r) in left.iter().zip(right.iter()) {
                let idx = pos / window;
                if sums.len() <= idx {
                    sums.push(0.0);
                    counts.push(0);
                }
                sums[idx] += f64::from(*l) * f64::from(*l) + f64::from(*r) * f64::from(*r);
                counts[idx] += 2;
                pos += 1;
            }
        }
    }

    sums.into_iter()
        .zip(counts)
        .map(|(sum, count)| (sum / count.max(1) as f64).sqrt())
        .collect()
}

#[test]
fn overlay_is_mixed_at_its_absolute_time_over_a_silent_clip() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sequence_01.mp4")
        .canonicalize()
        .expect("sample media");
    let tone = std::env::temp_dir().join("velocut_overlay_tone.wav");
    write_tone_wav(&tone, 1.0, 440.0, 0.5);
    let out = std::env::temp_dir().join("velocut_overlay_check.mp4");
    let _ = std::fs::remove_file(&out);

    let spec = EncodeSpec {
        job_id: uuid::Uuid::new_v4(),
        clips: vec![ClipSpec {
            path: src,
            timeline_start: 0.0,
            source_offset: 0.0,
            duration: 2.0,
            volume: 1.0,
            skip_audio: true,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: Default::default(),
        }],
        width: 320,
        height: 180,
        fps: 30,
        output: out.clone(),
        transitions: Vec::new(),
        audio_overlays: vec![AudioOverlay {
            path: tone,
            source_offset: 0.0,
            timeline_start: 0.5,
            duration: 1.0,
            volume: 1.0,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
        }],
    };
    let out = encode(spec);

    let rms = rms_windows(&out, 0.5);
    println!("RMS per 0.5s window: {rms:?}");
    assert!(
        rms.len() >= 4,
        "expected 2s of audio, got {} windows",
        rms.len()
    );
    assert!(
        rms[0] < 0.02,
        "0.0-0.5s must be silent (silent clip), got {:.3}",
        rms[0]
    );
    assert!(
        rms[1] > 0.15 && rms[2] > 0.15,
        "0.5-1.5s must contain the overlay tone, got {:.3}/{:.3}",
        rms[1],
        rms[2]
    );
    assert!(
        rms[3] < 0.02,
        "1.5-2.0s must be silent again, got {:.3}",
        rms[3]
    );
}

/// Preview normalizes the mix by `1/sqrt(N)` where N is the active sink count.
/// The encoder has to do the same: an overlay on top of a primary clip must be
/// 3 dB quieter than the same overlay playing where no primary is active.
#[test]
fn overlay_is_ducked_against_the_primary_like_the_preview() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sequence_01.mp4")
        .canonicalize()
        .expect("sample media");
    let tone = std::env::temp_dir().join("velocut_overlay_mix_tone.wav");
    write_tone_wav(&tone, 1.0, 440.0, 0.5);

    let build = |clip_secs: f64, overlay_start: f64, file: &str| {
        let out = std::env::temp_dir().join(file);
        let _ = std::fs::remove_file(&out);
        EncodeSpec {
            job_id: uuid::Uuid::new_v4(),
            clips: vec![ClipSpec {
                path: src.clone(),
                timeline_start: 0.0,
                source_offset: 0.0,
                duration: clip_secs,
                // Silent primary that still counts as a sink, like a clip whose
                // own audio is silent in the preview.
                volume: 0.0,
                skip_audio: false,
                fade_in_secs: 0.0,
                fade_in_start_secs: 0.0,
                fade_out_secs: 0.0,
                fade_out_end_secs: 0.0,
                filter: Default::default(),
            }],
            width: 320,
            height: 180,
            fps: 30,
            output: out,
            transitions: Vec::new(),
            audio_overlays: vec![AudioOverlay {
                path: tone.clone(),
                source_offset: 0.0,
                timeline_start: overlay_start,
                duration: 1.0,
                volume: 1.0,
                fade_in_secs: 0.0,
                fade_in_start_secs: 0.0,
                fade_out_secs: 0.0,
                fade_out_end_secs: 0.0,
            }],
        }
    };

    // Overlay over the primary: N=2, tone scaled by 1/sqrt(2).
    let over = encode(build(2.0, 0.5, "velocut_overlay_mix_over.mp4"));
    // Overlay after the primary ends (tail blank frames): N=1, full tone.
    let alone = encode(build(1.0, 1.0, "velocut_overlay_mix_alone.mp4"));

    let peak = |rms: &[f64]| rms.iter().cloned().fold(0.0f64, f64::max);
    let over_rms = peak(&rms_windows(&over, 0.25));
    let alone_rms = peak(&rms_windows(&alone, 0.25));
    println!("overlay RMS over primary={over_rms:.4}, alone={alone_rms:.4}");

    assert!(
        alone_rms > 0.2,
        "expected the overlay at full level after the primary, got {alone_rms:.4}"
    );
    let duck = over_rms / alone_rms;
    assert!(
        (0.65..=0.76).contains(&duck),
        "overlay should be 1/sqrt(2) (~0.707) of its solo level over a primary, got {duck:.3}"
    );
}

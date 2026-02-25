// crates/velocut-media/src/encode.rs
//
// Multi-clip H.264 + AAC MP4 encode pipeline.
//
// Design:
//   • `ClipSpec`   — everything needed to locate and trim one source clip.
//   • `EncodeSpec` — the complete job description handed from the UI.
//   • `encode_timeline()` — blocking function meant to run on its own thread;
//     called from MediaWorker::start_encode. Sends EncodeProgress every
//     PROGRESS_INTERVAL frames and EncodeError / EncodeDone on exit.
//
// Stream layout in the output MP4:
//   Stream 0 — H.264 video (YUV420P, CRF 18, preset medium, or HW equivalent)
//   Stream 1 — AAC audio  (FLTP stereo, 44100 Hz, 128 kbps)
//
// Hardware encoding:
//   Attempted in priority order: AMF (D3D11) → NVENC (CUDA) → VAAPI → VideoToolbox → libx264.
//   Each HW path uploads YUV420P software frames to the device via an
//   AVHWFramesContext before calling send_frame; the muxer receives standard
//   H.264 Annex-B / AVCC output regardless of which encoder ran.
//   If all HW paths fail (missing driver, wrong platform) the pipeline falls
//   through to software x264 automatically.
//
// Hardware capability probe:
//   `probe_hw_encode_capabilities()` runs a lightweight dry-run at startup
//   (no actual encode) and returns `HwEncodeCapabilities`. The UI uses this
//   to enable/disable 2K and 4K resolution options — SW-only machines are
//   capped at 1080p to prevent lockups on high-resolution encodes.
//
// PTS strategy:
//   Video: monotonically increasing frame counter (output_frame_idx) in 1/fps.
//   Audio: monotonically increasing sample counter (out_sample_idx) in 1/44100.
//   Both reset to zero at the start of the encode, eliminating discontinuities
//   introduced by source file trimming and multi-clip concatenation.
//
// Audio FIFO:
//   AAC requires exactly `encoder.frame_size()` (typically 1024) samples per
//   input frame. Decoded audio may arrive in arbitrary chunk sizes, so all
//   decoded/resampled PCM is drained into a stereo FLTP ring buffer. Full
//   frames are popped from the front and sent to the encoder; any remainder
//   carries over into the next clip. At the very end the tail is zero-padded
//   and flushed.
//
//   Audio resampler tail flush (1080p dropout fix):
//   After the decoder EOF drain the resampler may hold a partial output block
//   (SwrContext defers output until it has enough samples). A null-frame flush
//   (swr_convert with null input) extracts these buffered samples before the
//   FIFO carry-over check. Without this, 1080p clips lose the last ~20 ms of
//   audio per clip, which accumulates into audible gaps on long timelines.
//
// Overlay audio boundary contract:
//   Audio overlay tracks are encoded exactly as the user placed them — no
//   gap-filling, no truncation.  If an overlay ends before the video it simply
//   stops contributing samples (the FIFO holds silence).  If an overlay extends
//   *past* the last video clip the encoder appends black (YUV black, Y=16) video
//   frames for the duration of the tail so the audio is preserved with a blank
//   screen rather than silently dropped.
//
// Cancellation:
//   `cancel` is an Arc<AtomicBool> checked after every video frame. When set,
//   EncodeError { msg: "cancelled" } is sent — the UI treats that as an aborted
//   state distinct from a real error, keeping the cancel path identical to the
//   error path.

use std::path::PathBuf;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use crossbeam_channel::Sender;
use uuid::Uuid;

use ffmpeg_the_third as ffmpeg;
use ffmpeg::packet::Mut as PacketMut;
use ffmpeg::codec::{self, Id as CodecId};
use ffmpeg::encoder;
use ffmpeg::format::{Pixel, Sample, input as open_input, output as open_output};
use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::scaling::{Context as ScaleCtx, Flags as ScaleFlags};
use ffmpeg::software::resampling;
use ffmpeg::util::channel_layout::{ChannelLayout, ChannelLayoutMask};
use ffmpeg::util::frame::video::Video as VideoFrame;
use ffmpeg::util::frame::audio::Audio as AudioFrame;
use ffmpeg::util::rational::Rational;
use ffmpeg::Packet;

use velocut_core::media_types::MediaResult;
use velocut_core::transitions::{ClipTransition, TransitionKind, VideoTransition, registry};
use crate::helpers::yuv::{extract_yuv, write_yuv};
use crate::helpers::seek::seek_to_secs;

// ── Public types ──────────────────────────────────────────────────────────────

/// One source clip's contribution to the output timeline.
#[derive(Clone)]
pub struct ClipSpec {
    /// Absolute path to the source media file.
    pub path:          PathBuf,
    /// Seconds into the source file at which this clip begins.
    pub source_offset: f64,
    /// Duration in seconds to include from this clip.
    pub duration:      f64,
    /// Linear gain applied to decoded audio before encoding (1.0 = unity).
    pub volume:        f32,
    /// When true, no audio is decoded or pushed to the FIFO for this clip.
    pub skip_audio:    bool,
}

/// A standalone audio clip that runs in parallel with the video timeline.
#[derive(Clone)]
pub struct AudioOverlay {
    pub path:           PathBuf,
    pub source_offset:  f64,
    pub timeline_start: f64,
    pub duration:       f64,
    pub volume:         f32,
}

/// Complete description of an encode job.
pub struct EncodeSpec {
    pub job_id:  Uuid,
    pub clips:   Vec<ClipSpec>,
    pub width:   u32,
    pub height:  u32,
    pub fps:     u32,
    pub output:  PathBuf,
    pub transitions: Vec<ClipTransition>,
    pub audio_overlays: Vec<AudioOverlay>,
}

// ── Hardware capability probe ─────────────────────────────────────────────────

/// Result of the startup hardware encoder probe.
///
/// The UI reads this to decide which resolution options to annotate.
/// SW-only machines can encode at any resolution — higher resolutions
/// will be slower but the encode thread is throttled (priority + thread
/// cap) so the system stays responsive throughout.
#[derive(Debug, Clone)]
pub struct HwEncodeCapabilities {
    /// True when no HW encoder is available and libx264 will be used.
    /// The UI shows an informational note but does not restrict resolutions.
    pub sw_only:      bool,
    /// Human-readable name of the winning backend, e.g. "AMF", "NVENC",
    /// "VAAPI", "VideoToolbox", or "Software (libx264)".
    pub backend_name: &'static str,
}

/// Probe hardware encoder availability without starting an actual encode.
///
/// Tries each backend in priority order by attempting to create a D3D11/CUDA/
/// VAAPI device context and a tiny (128×128) hw_frames_ctx. No frames are
/// encoded. The probe typically completes in < 100 ms.
///
/// Call once at app startup and cache the result. Pass it to the export UI
/// so it can enable or disable 2K/4K options before the user wastes time
/// building a high-res timeline on an unsupported machine.
pub fn probe_hw_encode_capabilities() -> HwEncodeCapabilities {
    eprintln!("[encode] probing HW encode capabilities...");

    // AMF — D3D11, Windows (AMD/Intel/discrete NVIDIA via AMF runtime)
    if encoder::find_by_name("h264_amf").is_some() {
        if probe_d3d11_device(128, 128) {
            eprintln!("[encode] probe: AMF available");
            return HwEncodeCapabilities { sw_only: false, backend_name: "AMF" };
        } else {
            eprintln!("[encode] probe: h264_amf found but D3D11 device init failed — skipping AMF");
        }
    } else {
        eprintln!("[encode] probe: h264_amf not found in FFmpeg build — recompile with --enable-encoder=h264_amf for AMD/Intel GPU support");
    }

    // NVENC — CUDA, Windows/Linux
    if encoder::find_by_name("h264_nvenc").is_some() {
        if probe_cuda_device(128, 128) {
            eprintln!("[encode] probe: NVENC available");
            return HwEncodeCapabilities { sw_only: false, backend_name: "NVENC" };
        } else {
            eprintln!("[encode] probe: h264_nvenc found but CUDA device init failed — skipping NVENC");
        }
    } else {
        eprintln!("[encode] probe: h264_nvenc not found in FFmpeg build — skipping NVENC");
    }

    // VAAPI — Linux (AMD/Intel)
    if encoder::find_by_name("h264_vaapi").is_some() && probe_vaapi_device(128, 128) {
        eprintln!("[encode] probe: VAAPI available");
        return HwEncodeCapabilities { sw_only: false, backend_name: "VAAPI" };
    }

    // VideoToolbox — macOS
    if encoder::find_by_name("h264_videotoolbox").is_some() {
        eprintln!("[encode] probe: VideoToolbox available");
        return HwEncodeCapabilities { sw_only: false, backend_name: "VideoToolbox" };
    }

    eprintln!("[encode] probe: no HW encoder available — SW only, 2K/4K throttled (not disabled)");
    HwEncodeCapabilities { sw_only: true, backend_name: "Software (libx264)" }
}

/// Try to create a D3D11VA device + NV12 frames context. Returns true on success.
fn probe_d3d11_device(width: u32, height: u32) -> bool {
    let mut device_ctx: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            std::ptr::null(), std::ptr::null_mut(), 0,
        )
    };
    if ret < 0 { return false; }

    let ok = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            width, height,
        ) {
            Ok(f) => { ffmpeg::ffi::av_buffer_unref(&mut (f as *mut _)); true }
            Err(_) => false,
        }
    };
    unsafe { ffmpeg::ffi::av_buffer_unref(&mut device_ctx); }
    ok
}

/// Try to create a CUDA device + YUV420P frames context. Returns true on success.
fn probe_cuda_device(width: u32, height: u32) -> bool {
    let mut device_ctx: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            std::ptr::null(), std::ptr::null_mut(), 0,
        )
    };
    if ret < 0 { return false; }

    let ok = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
            width, height,
        ) {
            Ok(f) => { ffmpeg::ffi::av_buffer_unref(&mut (f as *mut _)); true }
            Err(_) => false,
        }
    };
    unsafe { ffmpeg::ffi::av_buffer_unref(&mut device_ctx); }
    ok
}

/// Try to create a VAAPI device + YUV420P frames context. Returns true on success.
fn probe_vaapi_device(width: u32, height: u32) -> bool {
    let mut device_ctx: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            std::ptr::null(), std::ptr::null_mut(), 0,
        )
    };
    if ret < 0 { return false; }

    let ok = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
            width, height,
        ) {
            Ok(f) => { ffmpeg::ffi::av_buffer_unref(&mut (f as *mut _)); true }
            Err(_) => false,
        }
    };
    unsafe { ffmpeg::ffi::av_buffer_unref(&mut device_ctx); }
    ok
}

// ── Constants ─────────────────────────────────────────────────────────────────

const PROGRESS_INTERVAL: u64 = 15;
const AUDIO_RATE: i32 = 44100;

// ── Hardware encoder selection ────────────────────────────────────────────────

/// Which H.264 encoder backend is active for this encode job.
#[derive(Debug, Clone, Copy, PartialEq)]
enum HwBackend {
    /// libx264 software encoder — universal fallback.
    Software,
    /// NVIDIA NVENC via CUDA device frames.
    Nvenc,
    /// Intel/AMD VAAPI via DRM device frames.
    Vaapi,
    /// Apple VideoToolbox (macOS only).
    VideoToolbox,
    /// AMD AMF via D3D11 device frames (Windows — AMD/Intel/NVIDIA via AMF runtime).
    Amf,
}

/// Open the best available H.264 encoder for the given output dimensions/rate.
///
/// Tries HW encoders in priority order; falls back to libx264 on any failure.
/// Returns (encoder, backend_tag, hw_device_ctx) where hw_device_ctx is Some
/// only for HW backends and must be kept alive for the encoder's lifetime.
///
/// The returned encoder is already open; callers must NOT call open_as_with again.
/// For HW backends the encoder's pix_fmt is the HW surface format (cuda/vaapi);
/// software frames in YUV420P are uploaded by `upload_frame_to_hw` before
/// calling send_frame.
fn try_open_hw_encoder(
    width:   u32,
    height:  u32,
    fps:     u32,
    out_tb:  Rational,
    octx:    &ffmpeg::format::context::Output,
) -> (ffmpeg::encoder::Video, HwBackend, Option<HwDeviceContext>) {
    // ── AMF (D3D11 — Windows native, AMD/Intel/NVIDIA) ───────────────────────
    eprintln!("[encode] trying AMF (D3D11)...");
    if let Some(result) = try_amf_encoder(width, height, fps, out_tb, octx) {
        eprintln!("[encode] HW encoder: AMF (D3D11)");
        return result;
    }

    // ── NVENC (CUDA — NVIDIA, Windows/Linux) ─────────────────────────────────
    // Only compiled in when nv-codec-headers are present at ffmpeg build time.
    eprintln!("[encode] trying NVENC (CUDA)...");
    if let Some(result) = try_nvenc_encoder(width, height, fps, out_tb, octx) {
        eprintln!("[encode] HW encoder: NVENC (CUDA)");
        return result;
    }

    // ── VAAPI ─────────────────────────────────────────────────────────────────
    eprintln!("[encode] trying VAAPI...");
    if let Some(result) = try_vaapi_encoder(width, height, fps, out_tb, octx) {
        eprintln!("[encode] HW encoder: VAAPI");
        return result;
    }

    // ── VideoToolbox ──────────────────────────────────────────────────────────
    eprintln!("[encode] trying VideoToolbox...");
    if let Some(result) = try_videotoolbox_encoder(width, height, fps, out_tb, octx) {
        eprintln!("[encode] HW encoder: VideoToolbox");
        return result;
    }

    // ── Software fallback ─────────────────────────────────────────────────────
    eprintln!("[encode] HW encoder: none available, using libx264 software");
    let enc = open_software_encoder(width, height, fps, out_tb, octx)
        .expect("libx264 is required — ensure it is compiled in");
    (enc, HwBackend::Software, None)
}

/// Opaque wrapper around an AVHWDeviceContext* kept alive next to the encoder.
struct HwDeviceContext {
    /// The raw AVBufferRef* for the device context.
    /// Must stay alive for as long as the encoder is open.
    ptr: *mut ffmpeg::ffi::AVBufferRef,
}

// SAFETY: HwDeviceContext owns the buffer ref and is only accessed from the
// encode thread. AVBufferRef itself is reference-counted and thread-safe for
// the ref/unref operations the encoder calls internally.
unsafe impl Send for HwDeviceContext {}
unsafe impl Sync for HwDeviceContext {}

impl Drop for HwDeviceContext {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffmpeg::ffi::av_buffer_unref(&mut self.ptr); }
        }
    }
}

/// Upload a YUV420P software frame to the HW device surface.
///
/// Allocates a new HW surface frame via `av_hwframe_get_buffer`, transfers the
/// software pixels via `av_hwframe_transfer_data`, then copies side data
/// (PTS, SAR) so the encoder sees a correctly-timestamped HW frame.
///
/// Returns the HW frame ready for send_frame, or the original sw_frame on
/// any error (the encoder will reject it, but we log and move on).
unsafe fn upload_frame_to_hw(
    sw_frame:   &VideoFrame,
    hw_frames_ctx: *mut ffmpeg::ffi::AVBufferRef,
) -> Result<VideoFrame, String> {
    // Allocate a fresh HW surface.
    let hw_raw = ffmpeg::ffi::av_frame_alloc();
    if hw_raw.is_null() {
        return Err("av_frame_alloc for HW frame failed".into());
    }

    (*hw_raw).hw_frames_ctx = ffmpeg::ffi::av_buffer_ref(hw_frames_ctx);
    let ret = ffmpeg::ffi::av_hwframe_get_buffer((*hw_raw).hw_frames_ctx, hw_raw, 0);
    if ret < 0 {
        ffmpeg::ffi::av_frame_free(&mut (hw_raw as *mut _));
        return Err(format!("av_hwframe_get_buffer failed: {ret}"));
    }

    // Copy pixel data from SW → HW surface.
    let ret = ffmpeg::ffi::av_hwframe_transfer_data(hw_raw, sw_frame.as_ptr(), 0);
    if ret < 0 {
        ffmpeg::ffi::av_frame_free(&mut (hw_raw as *mut _));
        return Err(format!("av_hwframe_transfer_data failed: {ret}"));
    }

    // Propagate PTS and SAR.
    (*hw_raw).pts = (*sw_frame.as_ptr()).pts;
    (*hw_raw).sample_aspect_ratio = (*sw_frame.as_ptr()).sample_aspect_ratio;

    // Wrap in ffmpeg-the-third's VideoFrame (which will av_frame_free on drop).
    Ok(VideoFrame::wrap(hw_raw))
}

/// Build the hw_frames_ctx for a given device context and surface formats.
///
/// The frames context tells the encoder how large each surface pool entry is
/// (width × height × hw_pix_fmt) and how many to pre-allocate.
unsafe fn build_hw_frames_ctx(
    device_ctx: *mut ffmpeg::ffi::AVBufferRef,
    hw_pix_fmt: ffmpeg::ffi::AVPixelFormat,
    sw_pix_fmt: ffmpeg::ffi::AVPixelFormat,
    width:  u32,
    height: u32,
) -> Result<*mut ffmpeg::ffi::AVBufferRef, String> {
    let frames_ref = ffmpeg::ffi::av_hwframe_ctx_alloc(device_ctx);
    if frames_ref.is_null() {
        return Err("av_hwframe_ctx_alloc failed".into());
    }

    let frames_ctx = (*frames_ref).data as *mut ffmpeg::ffi::AVHWFramesContext;
    (*frames_ctx).format    = hw_pix_fmt;
    (*frames_ctx).sw_format = sw_pix_fmt;
    (*frames_ctx).width     = width as i32;
    (*frames_ctx).height    = height as i32;
    (*frames_ctx).initial_pool_size = 20;

    let ret = ffmpeg::ffi::av_hwframe_ctx_init(frames_ref);
    if ret < 0 {
        ffmpeg::ffi::av_buffer_unref(&mut (frames_ref as *mut _));
        return Err(format!("av_hwframe_ctx_init failed: {ret}"));
    }

    Ok(frames_ref)
}

fn try_amf_encoder(
    width:  u32,
    height: u32,
    fps:    u32,
    out_tb: Rational,
    octx:   &ffmpeg::format::context::Output,
) -> Option<(ffmpeg::encoder::Video, HwBackend, Option<HwDeviceContext>)> {
    let codec = match encoder::find_by_name("h264_amf") {
        Some(c) => c,
        None => {
            eprintln!("[encode] h264_amf not found in ffmpeg build, skipping AMF");
            return None;
        }
    };

    // AMF on Windows uses a D3D11 device context.
    let mut device_ctx: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            std::ptr::null(),       // NULL = use default adapter
            std::ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        eprintln!("[encode] D3D11 device init failed ({ret}), skipping AMF");
        return None;
    }

    let frames_ctx = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12, // D3D11 requires NV12, not YUV420P
            width, height,
        ) {
            Ok(f)  => f,
            Err(e) => {
                eprintln!("[encode] AMF frames ctx: {e}");
                ffmpeg::ffi::av_buffer_unref(&mut device_ctx);
                return None;
            }
        }
    };

    let enc_ctx_obj = codec::context::Context::new_with_codec(codec);
    let mut enc = enc_ctx_obj.encoder().video().ok()?;

    enc.set_width(width);
    enc.set_height(height);
    enc.set_time_base(out_tb);
    enc.set_frame_rate(Some(Rational::new(fps as i32, 1)));
    enc.set_bit_rate(0);

    unsafe {
        (*enc.as_mut_ptr()).pix_fmt       = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
        (*enc.as_mut_ptr()).hw_frames_ctx = ffmpeg::ffi::av_buffer_ref(frames_ctx);
        ffmpeg::ffi::av_buffer_unref(&mut (frames_ctx as *mut _));
    }

    if octx.format().flags().contains(ffmpeg::format::Flags::GLOBAL_HEADER) {
        enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    // AMF quality options: quality preset + CQP equivalent to CRF 18.
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("quality",  "quality");   // slow preset — balanced quality/speed
    opts.set("rc",       "cqp");       // constant QP, analogous to CRF
    opts.set("qp_i",     "18");
    opts.set("qp_p",     "20");
    opts.set("qp_b",     "22");
    opts.set("g",        &fps.to_string());

    match enc.open_as_with(codec, opts) {
        Ok(opened) => {
            let hw_dev = HwDeviceContext { ptr: device_ctx };
            Some((opened, HwBackend::Amf, Some(hw_dev)))
        }
        Err(e) => {
            eprintln!("[encode] h264_amf open failed: {e}, skipping AMF");
            unsafe { ffmpeg::ffi::av_buffer_unref(&mut device_ctx); }
            None
        }
    }
}

fn try_nvenc_encoder(
    width:  u32,
    height: u32,
    fps:    u32,
    out_tb: Rational,
    octx:   &ffmpeg::format::context::Output,
) -> Option<(ffmpeg::encoder::Video, HwBackend, Option<HwDeviceContext>)> {
    let codec = match encoder::find_by_name("h264_nvenc") {
        Some(c) => c,
        None => {
            eprintln!("[encode] h264_nvenc not found in ffmpeg build, skipping NVENC");
            return None;
        }
    };

    // Create CUDA device context.
    let mut device_ctx: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        eprintln!("[encode] CUDA device init failed ({ret}), skipping NVENC");
        return None;
    }

    let frames_ctx = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
            width, height,
        ) {
            Ok(f)  => f,
            Err(e) => {
                eprintln!("[encode] NVENC frames ctx: {e}");
                ffmpeg::ffi::av_buffer_unref(&mut device_ctx);
                return None;
            }
        }
    };

    // Build encoder using the safe ffmpeg-the-third API, then inject hw_frames_ctx.
    let enc_ctx_obj = codec::context::Context::new_with_codec(codec);
    let mut enc = enc_ctx_obj.encoder().video().ok()?;

    enc.set_width(width);
    enc.set_height(height);
    enc.set_time_base(out_tb);
    enc.set_frame_rate(Some(Rational::new(fps as i32, 1)));
    enc.set_bit_rate(0);

    unsafe {
        // pix_fmt and hw_frames_ctx must be set via raw pointer — no safe wrappers.
        (*enc.as_mut_ptr()).pix_fmt       = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
        (*enc.as_mut_ptr()).hw_frames_ctx = ffmpeg::ffi::av_buffer_ref(frames_ctx);
        ffmpeg::ffi::av_buffer_unref(&mut (frames_ctx as *mut _));
    }

    if octx.format().flags().contains(ffmpeg::format::Flags::GLOBAL_HEADER) {
        enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    // NVENC options: rc=constqp, qp=18, preset=p4 (balanced quality/speed).
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("rc",     "constqp");
    opts.set("qp",     "18");
    opts.set("preset", "p4");
    opts.set("g",      &fps.to_string());

    match enc.open_as_with(codec, opts) {
        Ok(opened) => {
            let hw_dev = HwDeviceContext { ptr: device_ctx };
            Some((opened, HwBackend::Nvenc, Some(hw_dev)))
        }
        Err(e) => {
            eprintln!("[encode] h264_nvenc open failed: {e}, skipping");
            unsafe { ffmpeg::ffi::av_buffer_unref(&mut device_ctx); }
            None
        }
    }
}

fn try_vaapi_encoder(
    width:  u32,
    height: u32,
    fps:    u32,
    out_tb: Rational,
    octx:   &ffmpeg::format::context::Output,
) -> Option<(ffmpeg::encoder::Video, HwBackend, Option<HwDeviceContext>)> {
    let codec = match encoder::find_by_name("h264_vaapi") {
        Some(c) => c,
        None => {
            eprintln!("[encode] h264_vaapi not found in ffmpeg build, skipping VAAPI");
            return None;
        }
    };

    let mut device_ctx: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            std::ptr::null(), // NULL = auto-detect render node
            std::ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        eprintln!("[encode] VAAPI device init failed ({ret}), skipping");
        return None;
    }

    let frames_ctx = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
            width, height,
        ) {
            Ok(f)  => f,
            Err(e) => {
                eprintln!("[encode] VAAPI frames ctx: {e}");
                ffmpeg::ffi::av_buffer_unref(&mut device_ctx);
                return None;
            }
        }
    };

    let enc_ctx_obj = codec::context::Context::new_with_codec(codec);
    let mut enc = enc_ctx_obj.encoder().video().ok()?;

    enc.set_width(width);
    enc.set_height(height);
    enc.set_time_base(out_tb);
    enc.set_frame_rate(Some(Rational::new(fps as i32, 1)));
    enc.set_bit_rate(0);

    unsafe {
        (*enc.as_mut_ptr()).pix_fmt       = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*enc.as_mut_ptr()).hw_frames_ctx = ffmpeg::ffi::av_buffer_ref(frames_ctx);
        ffmpeg::ffi::av_buffer_unref(&mut (frames_ctx as *mut _));
    }

    if octx.format().flags().contains(ffmpeg::format::Flags::GLOBAL_HEADER) {
        enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    // VAAPI uses qp-based rate control; ~18 ≈ CRF 18 visually.
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("rc_mode", "CQP");
    opts.set("qp",      "18");
    opts.set("g",       &fps.to_string());

    match enc.open_as_with(codec, opts) {
        Ok(opened) => {
            let hw_dev = HwDeviceContext { ptr: device_ctx };
            Some((opened, HwBackend::Vaapi, Some(hw_dev)))
        }
        Err(e) => {
            eprintln!("[encode] h264_vaapi open failed: {e}, skipping");
            unsafe { ffmpeg::ffi::av_buffer_unref(&mut device_ctx); }
            None
        }
    }
}

fn try_videotoolbox_encoder(
    width:  u32,
    height: u32,
    fps:    u32,
    out_tb: Rational,
    octx:   &ffmpeg::format::context::Output,
) -> Option<(ffmpeg::encoder::Video, HwBackend, Option<HwDeviceContext>)> {
    // VideoToolbox accepts YUV420P input directly (no explicit HW upload needed);
    // it manages the IOSurface pool internally.
    let codec = match encoder::find_by_name("h264_videotoolbox") {
        Some(c) => c,
        None => {
            eprintln!("[encode] h264_videotoolbox not found in ffmpeg build, skipping VideoToolbox");
            return None;
        }
    };

    let enc_ctx_obj = codec::context::Context::new_with_codec(codec);
    let mut enc = enc_ctx_obj.encoder().video().ok()?;

    enc.set_width(width);
    enc.set_height(height);
    enc.set_format(Pixel::YUV420P);  // VT accepts sw frames in yuv420p
    enc.set_time_base(out_tb);
    enc.set_frame_rate(Some(Rational::new(fps as i32, 1)));
    enc.set_bit_rate(0);

    if octx.format().flags().contains(ffmpeg::format::Flags::GLOBAL_HEADER) {
        enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    let mut opts = ffmpeg::Dictionary::new();
    // VideoToolbox doesn't have CRF; use a high average bitrate for near-lossless quality.
    opts.set("b:v",            "0");           // let profile control quality
    opts.set("allow_sw",       "1");           // fall back to SW if GPU busy
    opts.set("realtime",       "0");           // prefer quality over real-time
    let gop = fps.to_string();
    opts.set("g",              &gop);

    match enc.open_as_with(codec, opts) {
        Ok(opened) => {
            // VideoToolbox: no external HwDeviceContext needed
            Some((opened, HwBackend::VideoToolbox, None))
        }
        Err(e) => {
            eprintln!("[encode] h264_videotoolbox open failed: {e}");
            None
        }
    }
}

fn open_software_encoder(
    width:  u32,
    height: u32,
    fps:    u32,
    out_tb: Rational,
    octx:   &ffmpeg::format::context::Output,
) -> Result<ffmpeg::encoder::Video, String> {
    let h264 = encoder::find(CodecId::H264)
        .ok_or_else(|| "H.264 encoder not found — is libx264 available?".to_string())?;

    let enc_ctx = codec::context::Context::new_with_codec(h264);
    let mut enc = enc_ctx.encoder().video()
        .map_err(|e| format!("create video encoder context: {e}"))?;

    enc.set_width(width);
    enc.set_height(height);
    enc.set_format(Pixel::YUV420P);
    enc.set_time_base(out_tb);
    enc.set_frame_rate(Some(Rational::new(fps as i32, 1)));
    enc.set_bit_rate(0);

    if octx.format().flags().contains(ffmpeg::format::Flags::GLOBAL_HEADER) {
        enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    // Cap libx264 to half the logical CPU count so the encoder never saturates
    // every core.  The OS schedules UI, audio, and scrub-decode threads on the
    // remaining cores; the encode runs at a reduced but consistent pace without
    // making the system unresponsive.  Minimum 1 thread; falls back to 2 if
    // available_parallelism() fails (e.g. inside a restricted sandbox).
    let thread_cap = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).max(1))
        .unwrap_or(2);

    let mut opts = ffmpeg::Dictionary::new();
    opts.set("crf",     "18");
    // "medium" is more CPU-efficient per thread than "fast" — it does more work
    // per encode pass, so the total core-seconds consumed for equivalent quality
    // is lower.  Combined with the thread cap above, this keeps peak CPU usage
    // manageable at 2K/4K on a laptop without a hardware encoder.
    opts.set("preset",  "medium");
    opts.set("threads", &thread_cap.to_string());
    opts.set("g",       &fps.to_string());

    enc.open_as_with(h264, opts)
        .map_err(|e| format!("open H.264 encoder: {e}"))
}

// ── Center-crop scaler ────────────────────────────────────────────────────────

struct CropScaler {
    ctx:    ScaleCtx,
    crop_x: u32,
    crop_y: u32,
    crop_h: u32,
}

impl CropScaler {
    fn build(
        src_fmt: Pixel,
        src_w:   u32,
        src_h:   u32,
        out_w:   u32,
        out_h:   u32,
    ) -> Self {
        let src_ar = src_w as f64 / src_h.max(1) as f64;
        let out_ar = out_w as f64 / out_h.max(1) as f64;

        let (crop_x, crop_y, crop_w, crop_h) = if (src_ar - out_ar).abs() < 1e-4 {
            (0, 0, src_w, src_h)
        } else if src_ar > out_ar {
            let cw = ((src_h as f64 * out_ar).round() as u32).min(src_w) & !1;
            let cx = ((src_w - cw) / 2) & !1;
            (cx, 0u32, cw, src_h)
        } else {
            let ch = ((src_w as f64 / out_ar).round() as u32).min(src_h) & !1;
            let cy = ((src_h - ch) / 2) & !1;
            (0u32, cy, src_w, ch)
        };

        let ctx = ScaleCtx::get(
            src_fmt, crop_w.max(2), crop_h.max(2),
            Pixel::YUV420P, out_w, out_h,
            ScaleFlags::BILINEAR,
        ).expect("CropScaler: SwsContext");

        Self { ctx, crop_x, crop_y, crop_h }
    }

    fn run(&mut self, src: &VideoFrame, dst: &mut VideoFrame) -> Result<(), String> {
        unsafe {
            let sf = src.as_ptr();
            let df = dst.as_mut_ptr();

            let (off_y, off_uv): (usize, usize) = match src.format() {
                Pixel::YUV420P | Pixel::YUVJ420P |
                Pixel::YUV422P | Pixel::YUVJ422P => {
                    (self.crop_x as usize, self.crop_x as usize / 2)
                }
                Pixel::YUV444P | Pixel::YUVJ444P => {
                    let o = self.crop_x as usize;
                    (o, o)
                }
                _ => (0, 0),
            };

            let ls = &(*sf).linesize;
            let y_row_off  = self.crop_y as usize * ls[0] as usize;
            let uv_row_off = (self.crop_y as usize / 2) * ls[1] as usize;

            let src_planes: [*const u8; 4] = [
                (*sf).data[0].add(off_y + y_row_off),
                if (*sf).data[1].is_null() { std::ptr::null() } else { (*sf).data[1].add(off_uv + uv_row_off) },
                if (*sf).data[2].is_null() { std::ptr::null() } else { (*sf).data[2].add(off_uv + uv_row_off) },
                std::ptr::null(),
            ];

            let ret = ffmpeg::ffi::sws_scale(
                self.ctx.as_mut_ptr(),
                src_planes.as_ptr() as _,
                (*sf).linesize.as_ptr(),
                0,
                self.crop_h as _,
                (*df).data.as_mut_ptr() as _,
                (*df).linesize.as_mut_ptr(),
            );

            if ret <= 0 {
                return Err(format!("CropScaler::run sws_scale returned {ret}"));
            }
        }
        Ok(())
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn encode_timeline(
    spec:   EncodeSpec,
    cancel: Arc<AtomicBool>,
    tx:     Sender<MediaResult>,
) {
    let total_frames: u64 = spec.clips.iter()
        .map(|c| (c.duration * spec.fps as f64).ceil() as u64)
        .sum::<u64>()
        .max(1);

    match run_encode(&spec, cancel, total_frames, &tx) {
        Ok(()) => {
            let _ = tx.send(MediaResult::EncodeDone {
                job_id: spec.job_id,
                path:   spec.output.clone(),
            });
        }
        Err(e) => {
            let _ = tx.send(MediaResult::EncodeError {
                job_id: spec.job_id,
                msg:    e,
            });
        }
    }
}

// ── Audio FIFO ────────────────────────────────────────────────────────────────

struct AudioFifo {
    left:  Vec<f32>,
    right: Vec<f32>,
}

impl AudioFifo {
    fn new() -> Self { Self { left: Vec::new(), right: Vec::new() } }
    fn len(&self) -> usize { self.left.len() }

    /// Like `push_scaled` but discards the first `skip` samples (pre-roll trim).
    fn push_scaled_from(&mut self, frame: &AudioFrame, volume: f32, skip: usize) {
        let n = frame.samples();
        if n <= skip { return; }
        unsafe {
            let l_bytes = frame.data(0);
            let l_f32 = std::slice::from_raw_parts(l_bytes.as_ptr() as *const f32, n);
            self.left.extend(l_f32[skip..].iter().map(|s| (s * volume).clamp(-1.0, 1.0)));

            let r_bytes = if frame.ch_layout().channels() >= 2 { frame.data(1) } else { frame.data(0) };
            let r_f32 = std::slice::from_raw_parts(r_bytes.as_ptr() as *const f32, n);
            self.right.extend(r_f32[skip..].iter().map(|s| (s * volume).clamp(-1.0, 1.0)));
        }
    }

    fn push_scaled(&mut self, frame: &AudioFrame, volume: f32) {
        let n = frame.samples();
        if n == 0 { return; }
        unsafe {
            let l_bytes = frame.data(0);
            let l_f32 = std::slice::from_raw_parts(l_bytes.as_ptr() as *const f32, n);
            self.left.extend(l_f32.iter().map(|s| (s * volume).clamp(-1.0, 1.0)));

            let r_bytes = if frame.ch_layout().channels() >= 2 { frame.data(1) } else { frame.data(0) };
            let r_f32 = std::slice::from_raw_parts(r_bytes.as_ptr() as *const f32, n);
            self.right.extend(r_f32.iter().map(|s| (s * volume).clamp(-1.0, 1.0)));
        }
    }

    fn pop_frame(&mut self, n: usize, sample_idx: i64) -> AudioFrame {
        let available = self.left.len().min(n);

        let mut frame = AudioFrame::new(
            Sample::F32(SampleType::Planar),
            n,
            ChannelLayoutMask::STEREO,
        );
        frame.set_rate(AUDIO_RATE as u32);
        frame.set_pts(Some(sample_idx));

        unsafe {
            let ldata = frame.data_mut(0);
            let ldst  = std::slice::from_raw_parts_mut(ldata.as_mut_ptr() as *mut f32, n);
            ldst[..available].copy_from_slice(&self.left[..available]);
            if available < n { ldst[available..].fill(0.0); }

            let rdata = frame.data_mut(1);
            let rdst  = std::slice::from_raw_parts_mut(rdata.as_mut_ptr() as *mut f32, n);
            rdst[..available].copy_from_slice(&self.right[..available]);
            if available < n { rdst[available..].fill(0.0); }
        }

        self.left.drain(..available);
        self.right.drain(..available);
        frame
    }
}

// ── Audio encoder state ───────────────────────────────────────────────────────

struct DecodedOverlay {
    left:         Vec<f32>,
    right:        Vec<f32>,
    start_sample: i64,
    sample_count: usize,
}

struct AudioEncState {
    encoder:        ffmpeg::encoder::Audio,
    out_sample_idx: i64,
    frame_size:     usize,
    fifo:           AudioFifo,
    audio_tb:       Rational,
    ost_audio_tb:   Rational,
    overlays:       Vec<DecodedOverlay>,
    /// Counts FIFO overrun events; used to throttle log spam at 1080p SW encode.
    fifo_overrun_count: u64,
}

impl AudioEncState {
    fn drain_fifo(
        &mut self,
        octx:  &mut ffmpeg::format::context::Output,
        flush: bool,
    ) -> Result<(), String> {
        if !flush && self.fifo.len() > 2 * self.frame_size {
            self.fifo_overrun_count += 1;
            // At 1080p with SW encoding, audio routinely outruns video by a few frames
            // (the encoder is slow; the demuxer keeps feeding audio).  Logging every
            // occurrence floods the terminal with hundreds of identical lines.
            // Print the first event so the developer sees it, then every 500th.
            if self.fifo_overrun_count == 1 || self.fifo_overrun_count % 500 == 0 {
                eprintln!(
                    "[encode] audio FIFO overrun: {} samples buffered (threshold={}); \
                     audio running ahead of video (occurrence #{})",
                    self.fifo.len(), 2 * self.frame_size, self.fifo_overrun_count,
                );
            }
        }
        while self.fifo.len() >= self.frame_size
            || (flush && self.fifo.len() > 0)
        {
            let mut frame = self.fifo.pop_frame(self.frame_size, self.out_sample_idx);

            if !self.overlays.is_empty() {
                let n = self.frame_size;
                unsafe {
                    // Derive mutable pointers from the frame's raw AVFrame directly.
                    // Using frame.data(0) (which returns &[u8]) and casting its pointer
                    // to *mut f32 is UB — Rust's aliasing rules forbid mutable access
                    // through a pointer derived from a shared reference.
                    let fptr = frame.as_mut_ptr();
                    let ldst = std::slice::from_raw_parts_mut((*fptr).data[0] as *mut f32, n);
                    let rdst = std::slice::from_raw_parts_mut((*fptr).data[1] as *mut f32, n);
                    for ov in &self.overlays {
                        for i in 0..n {
                            let ov_s = self.out_sample_idx + i as i64 - ov.start_sample;
                            if ov_s >= 0 && (ov_s as usize) < ov.sample_count {
                                let idx = ov_s as usize;
                                ldst[i] = (ldst[i] + ov.left[idx]).clamp(-1.0, 1.0);
                                rdst[i] = (rdst[i] + ov.right[idx]).clamp(-1.0, 1.0);
                            }
                        }
                    }
                }
            }

            self.out_sample_idx += self.frame_size as i64;

            self.encoder.send_frame(&frame)
                .map_err(|e| format!("send audio frame to encoder: {e}"))?;
            self.drain_packets(octx)?;
        }
        Ok(())
    }

    fn drain_packets(
        &mut self,
        octx: &mut ffmpeg::format::context::Output,
    ) -> Result<(), String> {
        let mut pkt = Packet::empty();
        while self.encoder.receive_packet(&mut pkt).is_ok() {
            pkt.set_stream(1);
            pkt.rescale_ts(self.audio_tb, self.ost_audio_tb);
            pkt.write_interleaved(octx)
                .map_err(|e| format!("write audio packet: {e}"))?;
        }
        Ok(())
    }

    fn flush_encoder(
        &mut self,
        octx: &mut ffmpeg::format::context::Output,
    ) -> Result<(), String> {
        self.encoder.send_eof()
            .map_err(|e| format!("send EOF to audio encoder: {e}"))?;
        self.drain_packets(octx)
    }
}

// ── Resampler tail flush ──────────────────────────────────────────────────────

/// Flush any samples buffered inside a SwrContext by sending a null (EOF) input.
///
/// SwrContext buffers up to one output block internally and emits it only once
/// it receives enough input. After decoder EOF the resampler may hold 512–1024
/// samples that never appear in `receive_frame` output. This function extracts
/// them by calling swr_convert with a null input pointer (the documented API for
/// flushing the internal delay line).
///
/// This is the primary fix for audio dropouts at 1080p: at high resolution the
/// encoder is slow enough that the packet loop's send_packet/receive_frame
/// interleaving leaves the resampler partially filled at clip_end, and without
/// this flush those samples are silently discarded.
fn flush_audio_resampler(
    resampler: &mut resampling::Context,
    fifo:      &mut AudioFifo,
    volume:    f32,
) {
    // A null-frame flush: allocate an output frame, call swr_convert with
    // null input, repeat until the resampler reports zero output samples.
    loop {
        // Allocate a temporary frame large enough to drain the delay line.
        // 4096 samples is safely larger than any SwrContext internal buffer.
        let mut out_frame = AudioFrame::new(
            Sample::F32(SampleType::Planar),
            4096,
            ChannelLayoutMask::STEREO,
        );
        out_frame.set_rate(AUDIO_RATE as u32);

        unsafe {
            let n_out = ffmpeg::ffi::swr_convert(
                resampler.as_mut_ptr(),
                (*out_frame.as_mut_ptr()).data.as_mut_ptr() as *mut *mut u8,
                4096,
                std::ptr::null_mut(), // null input = flush
                0,
            );
            if n_out <= 0 { break; }
            // Manually set the sample count so push_scaled reads the right slice.
            (*out_frame.as_mut_ptr()).nb_samples = n_out;
        }

        fifo.push_scaled(&out_frame, volume);
    }
}

// ── Overlay decode ────────────────────────────────────────────────────────────

fn decode_overlay(overlay: &AudioOverlay) -> Result<DecodedOverlay, String> {
    use ffmpeg::format::sample::{Sample, Type as SampleType};
    use ffmpeg::software::resampling;
    use ffmpeg::util::channel_layout::ChannelLayout;
    use ffmpeg::util::frame::audio::Audio as AudioFrame;
    use ffmpeg::media::Type as MediaType;

    let target_fmt    = Sample::F32(SampleType::Planar);
    const OUT_RATE: u32 = 44_100;

    let mut ictx = open_input(&overlay.path)
        .map_err(|e| format!("overlay open '{}': {e}", overlay.path.display()))?;

    let audio_idx = ictx
        .streams()
        .best(MediaType::Audio)
        .ok_or_else(|| format!("no audio stream in overlay '{}'", overlay.path.display()))?
        .index();

    let ast   = ictx.stream(audio_idx).unwrap();
    let in_tb = ast.time_base();
    let adec_ctx = ffmpeg::codec::context::Context::from_parameters(ast.parameters())
        .map_err(|e| format!("overlay codec ctx: {e}"))?;
    let mut adec = adec_ctx.decoder().audio()
        .map_err(|e| format!("overlay audio decoder: {e}"))?;

    let seek_ts = {
        let tb = in_tb;
        (overlay.source_offset * tb.denominator() as f64 / tb.numerator() as f64) as i64
    };
    let _ = ictx.seek(seek_ts, ..=seek_ts);

    let mut resampler: Option<resampling::Context> = None;
    let mut left:  Vec<f32> = Vec::new();
    let mut right: Vec<f32> = Vec::new();

    let clip_end = overlay.source_offset + overlay.duration;

    let push_frame = |frame: &AudioFrame,
                      left:  &mut Vec<f32>,
                      right: &mut Vec<f32>,
                      vol:   f32| {
        let n = frame.samples();
        if n == 0 { return; }
        unsafe {
            let l = std::slice::from_raw_parts(frame.data(0).as_ptr() as *const f32, n);
            let channels = frame.ch_layout().channels();
            let r_plane  = if channels >= 2 { frame.data(1) } else { frame.data(0) };
            let r = std::slice::from_raw_parts(r_plane.as_ptr() as *const f32, n);
            left.extend(l.iter().map(|s| (s * vol).clamp(-1.0, 1.0)));
            right.extend(r.iter().map(|s| (s * vol).clamp(-1.0, 1.0)));
        }
    };

    for result in ictx.packets() {
        let (stream, packet) = match result {
            Ok(p)  => p,
            Err(_) => continue,
        };
        if stream.index() != audio_idx { continue; }
        if adec.send_packet(&packet).is_err() { continue; }

        let mut raw = AudioFrame::empty();
        while adec.receive_frame(&mut raw).is_ok() {
            let pts_secs = raw.pts()
                .map(|p| p as f64 * f64::from(in_tb))
                .unwrap_or(0.0);

            if pts_secs < overlay.source_offset - 0.05 { continue; }
            if pts_secs >= clip_end { break; }

            let src_channels  = raw.ch_layout().channels();
            let needs_resample = raw.format() != target_fmt
                || raw.rate()             != OUT_RATE
                || src_channels           != 2;

            if needs_resample {
                let rs = resampler.get_or_insert_with(|| {
                    let src_layout = if src_channels >= 2 {
                        raw.ch_layout()
                    } else {
                        ChannelLayout::MONO
                    };
                    resampling::Context::get2(
                        raw.format(), src_layout,           raw.rate(),
                        target_fmt,   ChannelLayout::STEREO, OUT_RATE,
                    ).expect("overlay resampler")
                });
                let mut resampled = AudioFrame::empty();
                if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                    push_frame(&resampled, &mut left, &mut right, overlay.volume);
                }
            } else {
                push_frame(&raw, &mut left, &mut right, overlay.volume);
            }
        }
    }

    let _ = adec.send_eof();
    let mut raw = AudioFrame::empty();
    while adec.receive_frame(&mut raw).is_ok() {
        let pts_secs = raw.pts()
            .map(|p| p as f64 * f64::from(in_tb))
            .unwrap_or(0.0);
        if pts_secs >= clip_end { break; }

        let src_channels  = raw.ch_layout().channels();
        let needs_resample = raw.format() != target_fmt
            || raw.rate()             != OUT_RATE
            || src_channels           != 2;

        if needs_resample {
            if let Some(rs) = &mut resampler {
                let mut resampled = AudioFrame::empty();
                if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                    push_frame(&resampled, &mut left, &mut right, overlay.volume);
                }
            }
        } else {
            push_frame(&raw, &mut left, &mut right, overlay.volume);
        }
    }

    // Flush resampler tail — mirrors the same fix in encode_clip.
    // SwrContext buffers up to one output block internally and will not emit it
    // until it receives more input. After decoder EOF those samples are silently
    // discarded without this null-frame flush, causing the decoded sample_count
    // to be short and the overlay mixer's bounds check to cut audio off early.
    if let Some(ref mut rs) = resampler {
        loop {
            let mut tmp = AudioFrame::new(
                Sample::F32(SampleType::Planar), 4096, ChannelLayoutMask::STEREO,
            );
            tmp.set_rate(OUT_RATE);
            unsafe {
                let n_out = ffmpeg::ffi::swr_convert(
                    rs.as_mut_ptr(),
                    (*tmp.as_mut_ptr()).data.as_mut_ptr() as *mut *mut u8,
                    4096,
                    std::ptr::null_mut(), 0,
                );
                if n_out <= 0 { break; }
                (*tmp.as_mut_ptr()).nb_samples = n_out;
            }
            push_frame(&tmp, &mut left, &mut right, overlay.volume);
        }
    }

    if left.is_empty() {
        return Err(format!("overlay '{}': no audio decoded", overlay.path.display()));
    }

    let sample_count  = left.len();
    let start_sample  = (overlay.timeline_start * OUT_RATE as f64).round() as i64;

    eprintln!(
        "[encode] overlay decoded: {} samples ({:.2}s) start_sample={} ← {}",
        sample_count,
        sample_count as f64 / OUT_RATE as f64,
        start_sample,
        overlay.path.display(),
    );

    Ok(DecodedOverlay { left, right, start_sample, sample_count })
}

// ── Internal implementation ───────────────────────────────────────────────────

fn run_encode(
    spec:         &EncodeSpec,
    cancel:       Arc<AtomicBool>,
    total_frames: u64,
    tx:           &Sender<MediaResult>,
) -> Result<(), String> {
    if spec.clips.is_empty() {
        return Err("nothing to encode: timeline is empty".into());
    }

    // ── Output context ────────────────────────────────────────────────────────
    let mut octx = open_output(&spec.output)
        .map_err(|e| format!("could not open output '{}': {e}", spec.output.display()))?;

    // ── Video encoder (stream 0) ──────────────────────────────────────────────
    let out_tb   = Rational::new(1, spec.fps as i32);
    let frame_tb = Rational::new(1, spec.fps as i32);

    // Determine which codec we'll be registering for the stream.  HW encoders
    // expose themselves under their own codec ID (hevc_nvenc, h264_nvenc, etc.)
    // but we always want stream 0 to carry H.264, so use the H264 codec ID for
    // the output stream regardless of which actual encoder won.
    let h264_for_stream = encoder::find(CodecId::H264)
        .ok_or_else(|| "H.264 codec not registered".to_string())?;

    let mut ost_video = octx.add_stream(h264_for_stream)
        .map_err(|e| format!("add video stream: {e}"))?;
    ost_video.set_time_base(out_tb);

    // Open the best available encoder. This MUST happen before write_header
    // so we can copy codecpar in.  HW context (if any) is kept alive here.
    let (mut video_encoder, hw_backend, hw_device) =
        try_open_hw_encoder(spec.width, spec.height, spec.fps, out_tb, &octx);

    eprintln!("[encode] video encoder backend: {hw_backend:?}");

    // For HW backends we need the hw_frames_ctx pointer to upload frames later.
    // It lives inside the opened encoder's AVCodecContext.
    let hw_frames_ctx_ptr: *mut ffmpeg::ffi::AVBufferRef = if hw_backend != HwBackend::Software
        && hw_backend != HwBackend::VideoToolbox
    {
        unsafe { (*video_encoder.as_ptr()).hw_frames_ctx }
    } else {
        std::ptr::null_mut()
    };

    // Force square pixels — same as the original code path.
    video_encoder.set_aspect_ratio(Rational::new(1, 1));

    unsafe {
        let ret = ffmpeg::ffi::avcodec_parameters_from_context(
            (**(*octx.as_mut_ptr()).streams.add(0)).codecpar,
            video_encoder.as_ptr() as *mut ffmpeg::ffi::AVCodecContext,
        );
        if ret < 0 {
            return Err(format!("avcodec_parameters_from_context (video) failed: {ret}"));
        }
    }

    // ── Audio encoder (stream 1) ──────────────────────────────────────────────
    let audio_tb = Rational::new(1, AUDIO_RATE);

    let aac = encoder::find(CodecId::AAC)
        .ok_or_else(|| "AAC encoder not found".to_string())?;

    let mut ost_audio = octx.add_stream(aac)
        .map_err(|e| format!("add audio stream: {e}"))?;
    ost_audio.set_time_base(audio_tb);

    let audio_enc_ctx = codec::context::Context::new_with_codec(aac);
    let mut audio_enc = audio_enc_ctx.encoder().audio()
        .map_err(|e| format!("create audio encoder context: {e}"))?;

    audio_enc.set_rate(AUDIO_RATE);
    audio_enc.set_ch_layout(ChannelLayout::STEREO);
    audio_enc.set_format(Sample::F32(SampleType::Planar));
    audio_enc.set_bit_rate(128_000);

    if octx.format().flags().contains(ffmpeg::format::Flags::GLOBAL_HEADER) {
        audio_enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    let audio_encoder = audio_enc.open_as_with(aac, ffmpeg::Dictionary::new())
        .map_err(|e| format!("open AAC encoder: {e}"))?;

    let audio_frame_size = (audio_encoder.frame_size() as usize).max(1024);

    unsafe {
        let ret = ffmpeg::ffi::avcodec_parameters_from_context(
            (**(*octx.as_mut_ptr()).streams.add(1)).codecpar,
            audio_encoder.as_ptr() as *mut ffmpeg::ffi::AVCodecContext,
        );
        if ret < 0 {
            return Err(format!("avcodec_parameters_from_context (audio) failed: {ret}"));
        }
    }

    // ── Write output header ───────────────────────────────────────────────────
    ffmpeg::format::context::output::dump(&octx, 0, Some(&spec.output.to_string_lossy()));
    octx.write_header()
        .map_err(|e| format!("write output header: {e}"))?;

    let ost_audio_tb = octx.stream(1).unwrap().time_base();

    let mut audio_state = AudioEncState {
        encoder:        audio_encoder,
        out_sample_idx: 0,
        frame_size:     audio_frame_size,
        fifo:           AudioFifo::new(),
        audio_tb,
        ost_audio_tb,
        overlays: spec.audio_overlays.iter()
            .filter_map(|ov| match decode_overlay(ov) {
                Ok(d)  => Some(d),
                Err(e) => {
                    eprintln!("[encode] overlay decode failed: {e}");
                    // Surface to the UI — a silent drop here means the render
                    // completes with no audio and no visible diagnostic.
                    let _ = tx.send(MediaResult::EncodeError {
                        job_id: spec.job_id,
                        msg: format!("audio overlay decode failed: {e}"),
                    });
                    None
                }
            })
            .collect(),
        fifo_overrun_count: 0,
    };

    // ── Per-clip encode loop ──────────────────────────────────────────────────
    let mut output_frame_idx: i64 = 0;
    let mut last_video_dts: i64 = i64::MIN;
    let mut incoming_skip_secs: f64 = 0.0;
    let transition_registry = registry();

    for (clip_idx, clip) in spec.clips.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let skip = incoming_skip_secs;
        incoming_skip_secs = 0.0;

        let transition_entry = if clip_idx + 1 < spec.clips.len() {
            spec.transitions.iter()
                .find(|t| t.after_clip_index == clip_idx)
                .filter(|t| t.kind.kind != TransitionKind::Cut)
        } else {
            None
        };

        let transition_secs: f64 = transition_entry
            .map(|t| t.kind.duration_secs as f64)
            .unwrap_or(0.0);

        let effective = ClipSpec {
            path:          clip.path.clone(),
            source_offset: clip.source_offset + skip,
            duration:      (clip.duration - skip - transition_secs).max(0.0),
            volume:        clip.volume,
            skip_audio:    clip.skip_audio,
        };

        output_frame_idx = encode_clip(
            &effective,
            spec,
            &mut octx,
            &mut video_encoder,
            hw_frames_ctx_ptr,
            hw_backend,
            &mut audio_state,
            output_frame_idx,
            total_frames,
            frame_tb,
            &cancel,
            tx,
            &mut last_video_dts,
        )?;

        if let Some(entry) = transition_entry {
            let next_clip = &spec.clips[clip_idx + 1];

            let tail_spec = ClipSpec {
                path:          clip.path.clone(),
                source_offset: effective.source_offset + effective.duration,
                duration:      transition_secs,
                volume:        clip.volume,
                skip_audio:    false,
            };
            let head_spec = ClipSpec {
                path:          next_clip.path.clone(),
                source_offset: next_clip.source_offset,
                duration:      transition_secs,
                volume:        next_clip.volume,
                skip_audio:    false,
            };

            if let Some(transition_impl) = transition_registry.get(&entry.kind.kind) {
                output_frame_idx = apply_transition(
                    transition_impl.as_ref(),
                    &tail_spec,
                    &head_spec,
                    spec,
                    &mut octx,
                    &mut video_encoder,
                    hw_frames_ctx_ptr,
                    hw_backend,
                    &mut audio_state,
                    output_frame_idx,
                    total_frames,
                    frame_tb,
                    &cancel,
                    tx,
                    &mut last_video_dts,
                )?;

                incoming_skip_secs = transition_secs;
            }
        }

        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
    }

    // ── Extend video for overlay tail ─────────────────────────────────────────
    // If any audio overlay extends past the last video frame, generate black
    // (YUV limited-range black: Y=16, U=128, V=128) video frames for the
    // duration of the tail.  This preserves the user's explicit overlay
    // placement — the overlay ends exactly where they set it, not where the
    // video ends.  The FIFO is silence-padded each frame so drain_fifo mixes
    // the overlay in normally.
    {
        let video_end_sample = output_frame_idx as i64 * AUDIO_RATE as i64 / spec.fps as i64;
        let overlay_end_sample = audio_state.overlays.iter()
            .map(|ov| ov.start_sample + ov.sample_count as i64)
            .max()
            .unwrap_or(0);

        if overlay_end_sample > video_end_sample {
            let extra_samples = overlay_end_sample - video_end_sample;
            // Round up so the last partial AAC frame is always included.
            let extra_frames = ((extra_samples as f64 * spec.fps as f64 / AUDIO_RATE as f64)
                .ceil() as i64)
                .max(0);

            eprintln!(
                "[encode] overlay tail: {:.3}s past video end — appending {} blank frame(s)",
                extra_samples as f64 / AUDIO_RATE as f64,
                extra_frames,
            );

            let ost_video_tb = octx.stream(0).unwrap().time_base();

            // Pre-allocate one black YUV420P frame and reuse it for all blank frames.
            // Y=16 (black, BT.709 limited), U=V=128 (neutral chroma).
            let mut blank = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
            unsafe {
                let ptr = blank.as_mut_ptr();
                let y_stride  = (*ptr).linesize[0] as usize;
                let uv_stride = (*ptr).linesize[1] as usize;
                for row in 0..spec.height as usize {
                    let p = (*ptr).data[0].add(row * y_stride);
                    std::slice::from_raw_parts_mut(p, spec.width as usize).fill(16u8);
                }
                for row in 0..(spec.height as usize / 2) {
                    let pu = (*ptr).data[1].add(row * uv_stride);
                    let pv = (*ptr).data[2].add(row * uv_stride);
                    std::slice::from_raw_parts_mut(pu, spec.width as usize / 2).fill(128u8);
                    std::slice::from_raw_parts_mut(pv, spec.width as usize / 2).fill(128u8);
                }
                (*ptr).sample_aspect_ratio = ffmpeg::ffi::AVRational { num: 1, den: 1 };
            }

            for _ in 0..extra_frames {
                if cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }

                blank.set_pts(Some(output_frame_idx));
                send_video_frame(&blank, &mut video_encoder, hw_frames_ctx_ptr, hw_backend)?;

                let mut pkt = Packet::empty();
                while video_encoder.receive_packet(&mut pkt).is_ok() {
                    pkt.set_stream(0);
                    pkt.rescale_ts(frame_tb, ost_video_tb);
                    let raw_dts = pkt.dts().unwrap_or(0);
                    if last_video_dts != i64::MIN {
                        let prev_s = last_video_dts as f64 * f64::from(ost_video_tb);
                        let dts_s  = raw_dts          as f64 * f64::from(ost_video_tb);
                        if dts_s < prev_s {
                            let clamped = last_video_dts + 1;
                            eprintln!(
                                "[encode] blank-frame non-monotonic DTS \
                                 ({prev_s:.4}s → {dts_s:.4}s); clamping {raw_dts} → {clamped}"
                            );
                            unsafe { (*pkt.as_mut_ptr()).dts = clamped; }
                        }
                    }
                    last_video_dts = pkt.dts().unwrap_or(raw_dts);
                    pkt.write_interleaved(&mut octx)
                        .map_err(|e| format!("write blank video packet: {e}"))?;
                }

                output_frame_idx += 1;
                // Yield to the OS scheduler after each blank frame on HW paths.
                if hw_backend != HwBackend::Software {
                    std::thread::yield_now();
                }

                // Pad silence into the FIFO so drain_fifo can mix the overlay tail.
                let expected = output_frame_idx as i64 * AUDIO_RATE as i64 / spec.fps as i64;
                let have     = audio_state.out_sample_idx + audio_state.fifo.len() as i64;
                let gap      = (expected - have).max(0) as usize;
                if gap > 0 {
                    audio_state.fifo.left .extend(std::iter::repeat(0.0f32).take(gap));
                    audio_state.fifo.right.extend(std::iter::repeat(0.0f32).take(gap));
                }

                audio_state.drain_fifo(&mut octx, false)?;

                if output_frame_idx as u64 % PROGRESS_INTERVAL == 0 {
                    let _ = tx.send(MediaResult::EncodeProgress {
                        job_id:       spec.job_id,
                        frame:        output_frame_idx as u64,
                        total_frames,
                    });
                }
            }
        }
    }

    // ── Flush video encoder ───────────────────────────────────────────────────
    video_encoder.send_eof()
        .map_err(|e| format!("send EOF to video encoder: {e}"))?;

    let ost_video_tb = octx.stream(0).unwrap().time_base();
    let mut pkt = Packet::empty();
    while video_encoder.receive_packet(&mut pkt).is_ok() {
        pkt.set_stream(0);
        let frame_pts = pkt.pts().unwrap_or(pkt.dts().unwrap_or(0));
        pkt.rescale_ts(frame_tb, ost_video_tb);
        let raw_dts = pkt.dts().unwrap_or(0);
        if last_video_dts != i64::MIN {
            let prev_s = last_video_dts as f64 * f64::from(ost_video_tb);
            let dts_s  = raw_dts       as f64 * f64::from(ost_video_tb);
            if dts_s < prev_s {
                let clamped = last_video_dts + 1;
                eprintln!(
                    "[encode] encoder-flush non-monotonic DTS ({prev_s:.4}s → {dts_s:.4}s); \
                     clamping {raw_dts} → {clamped}"
                );
                unsafe { (*pkt.as_mut_ptr()).dts = clamped; }
            }
        }
        last_video_dts = pkt.dts().unwrap_or(raw_dts);
        pkt.write_interleaved(&mut octx)
            .map_err(|e| format!("write flush video packet: {e}"))?;
        output_frame_idx = output_frame_idx.max(frame_pts + 1);
    }

    // ── Trim clip audio to video boundary ─────────────────────────────────────
    // Removes clip audio (silence padding, decoded PCM) that overshot the video
    // endpoint.  Overlay audio has already been mixed into encoded AAC packets
    // by drain_fifo — the FIFO at this point contains only the clip-audio tail,
    // so trimming here never discards overlay content.
    //
    // Note: if overlays extended past video (handled above) output_frame_idx was
    // already advanced to cover the full overlay tail.  Any residual FIFO content
    // is a sub-frame rounding artifact (< frame_size samples) that is safe to
    // trim or flush.
    {
        let target_audio_samples =
            output_frame_idx as i64 * AUDIO_RATE as i64 / spec.fps as i64;
        let total_audio =
            audio_state.out_sample_idx + audio_state.fifo.len() as i64;
        let excess = (total_audio - target_audio_samples).max(0) as usize;
        if excess > 0 {
            eprintln!(
                "[encode] trimming {} trailing clip-audio samples ({:.3}s) — \
                 clip audio ran past video end ({:.3}s)",
                excess,
                excess as f64 / AUDIO_RATE as f64,
                output_frame_idx as f64 / spec.fps as f64,
            );
            let new_len = audio_state.fifo.left.len().saturating_sub(excess);
            audio_state.fifo.left.truncate(new_len);
            audio_state.fifo.right.truncate(new_len);
        } else {
            eprintln!(
                "[encode] audio/video end aligned: video={:.3}s audio={:.3}s",
                output_frame_idx as f64 / spec.fps as f64,
                total_audio as f64 / AUDIO_RATE as f64,
            );
        }
        if audio_state.fifo_overrun_count > 1 {
            eprintln!(
                "[encode] audio FIFO overrun occurred {} time(s) total \
                 (normal for SW encode at 1080p — audio decoded faster than video encoded)",
                audio_state.fifo_overrun_count,
            );
        }
    }

    // ── Flush audio FIFO then encoder ─────────────────────────────────────────
    audio_state.drain_fifo(&mut octx, true)?;
    audio_state.flush_encoder(&mut octx)?;

    octx.write_trailer()
        .map_err(|e| format!("write trailer: {e}"))?;

    // Keep hw_device alive until after write_trailer — the encoder may still
    // reference device memory during the trailer flush.
    drop(hw_device);

    Ok(())
}

/// Send one YUV420P software frame to the video encoder, uploading to the HW
/// surface if a HW backend is active.
///
/// This helper centralises the upload logic so encode_clip and apply_transition
/// both call a single function rather than duplicating the unsafe block.
fn send_video_frame(
    yuv:              &VideoFrame,
    video_encoder:    &mut ffmpeg::encoder::Video,
    hw_frames_ctx:    *mut ffmpeg::ffi::AVBufferRef,
    hw_backend:       HwBackend,
) -> Result<(), String> {
    if !hw_frames_ctx.is_null()
        && hw_backend != HwBackend::Software
        && hw_backend != HwBackend::VideoToolbox
    {
        // CUDA / VAAPI / AMF: upload SW frame to HW surface before encoding.
        let hw_frame = unsafe { upload_frame_to_hw(yuv, hw_frames_ctx) }
            .map_err(|e| format!("HW frame upload: {e}"))?;
        video_encoder.send_frame(&hw_frame)
            .map_err(|e| format!("send HW video frame to encoder: {e}"))
    } else {
        // Software or VideoToolbox: pass frame directly.
        video_encoder.send_frame(yuv)
            .map_err(|e| format!("send video frame to encoder: {e}"))
    }
}

fn encode_clip(
    clip:               &ClipSpec,
    spec:               &EncodeSpec,
    octx:               &mut ffmpeg::format::context::Output,
    video_encoder:      &mut ffmpeg::encoder::Video,
    hw_frames_ctx:      *mut ffmpeg::ffi::AVBufferRef,
    hw_backend:         HwBackend,
    audio_state:        &mut AudioEncState,
    mut out_frame_idx:  i64,
    total_frames:       u64,
    frame_tb:           Rational,
    cancel:             &Arc<AtomicBool>,
    tx:                 &Sender<MediaResult>,
    last_video_dts:     &mut i64,
) -> Result<i64, String> {
    let mut ictx = open_input(&clip.path)
        .map_err(|e| format!("open '{}': {e}", clip.path.display()))?;

    let video_stream_idx = ictx
        .streams()
        .best(MediaType::Video)
        .ok_or_else(|| format!("no video stream in '{}'", clip.path.display()))?
        .index();

    let audio_stream_idx: Option<usize> = ictx
        .streams()
        .best(MediaType::Audio)
        .map(|s| s.index());

    let in_video_tb = ictx.stream(video_stream_idx).unwrap().time_base();

    let vdec_ctx = codec::context::Context::from_parameters(
        ictx.stream(video_stream_idx).unwrap().parameters(),
    ).map_err(|e| format!("video decoder context: {e}"))?;

    let mut video_decoder = vdec_ctx.decoder().video()
        .map_err(|e| format!("open video decoder: {e}"))?;

    let mut audio_decoder: Option<ffmpeg::decoder::audio::Audio> = None;
    let mut in_audio_tb = Rational::new(1, AUDIO_RATE);

    if !clip.skip_audio {
        if let Some(asi) = audio_stream_idx {
            let ast = ictx.stream(asi).unwrap();
            in_audio_tb = ast.time_base();
            match codec::context::Context::from_parameters(ast.parameters()) {
                Ok(ctx) => match ctx.decoder().audio() {
                    Ok(dec) => { audio_decoder = Some(dec); }
                    Err(e)  => { eprintln!("[encode] audio decoder open failed for '{}': {e}", clip.path.display()); }
                },
                Err(e) => { eprintln!("[encode] audio decoder params failed for '{}': {e}", clip.path.display()); }
            }
        } else {
            eprintln!(
                "[encode] clip '{}' has no audio stream — silence will be padded \
                 (overlay tracks still mix)",
                clip.path.display(),
            );
        }
    }

    let (src_display_w, src_display_h) = {
        let stream = ictx.stream(video_stream_idx).unwrap();
        let params = stream.parameters();
        let w = params.width() as u32;
        let h = params.height() as u32;
        if w > 0 && h > 0 { (w, h) } else { (video_decoder.width(), video_decoder.height()) }
    };

    seek_to_secs(&mut ictx, clip.source_offset, "encode_clip");

    let mut video_scaler:    Option<CropScaler>          = None;
    let mut audio_resampler: Option<resampling::Context> = None;

    let clip_end   = clip.source_offset + clip.duration;
    let ost_tb     = octx.stream(0).unwrap().time_base();
    let half_frame = 0.5 / spec.fps as f64;

    let clip_start_frame_idx = out_frame_idx;
    let mut video_clip_done  = false;
    // True once the first real audio frame has been pushed to the FIFO.
    // Used to gate the silence gap padding: before audio starts we must NOT
    // pre-fill zeros, because real audio samples are arriving in the very next
    // packet. Eagerly padding silence before audio has been decoded causes the
    // AAC encoder to emit a silence→audio step that manifests as a pop/click
    // at the start of the first clip (or any clip whose first video packet is
    // demuxed before its first audio packet — the common case).
    // After audio_has_started=true the gap is typically 0 (audio is flowing),
    // but we still allow padding for the video-tail case where all audio frames
    // have been decoded but a few video frames remain (overlay mixer continuity).
    let mut audio_has_started = false;

    for result in ictx.packets() {
        let (stream, packet) = result
            .map_err(|e| format!("read packet from '{}': {e}", clip.path.display()))?;

        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let sidx = stream.index();

        // ── Video packet ──────────────────────────────────────────────────────
        if sidx == video_stream_idx {
            video_decoder.send_packet(&packet)
                .map_err(|e| format!("send video packet to decoder: {e}"))?;

            if video_clip_done {
                let mut _discard = VideoFrame::empty();
                while video_decoder.receive_frame(&mut _discard).is_ok() {}
                continue;
            }

            let mut decoded = VideoFrame::empty();
            while video_decoder.receive_frame(&mut decoded).is_ok() {
                let frame_pts_secs = decoded.pts()
                    .map(|pts| pts as f64 * f64::from(in_video_tb))
                    .unwrap_or(0.0);

                if frame_pts_secs < clip.source_offset - half_frame { continue; }

                if frame_pts_secs >= clip_end {
                    video_clip_done = true;
                    continue;
                }

                let sc = video_scaler.get_or_insert_with(|| {
                    CropScaler::build(
                        decoded.format(), src_display_w, src_display_h,
                        spec.width, spec.height,
                    )
                });

                let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
                sc.run(&decoded, &mut yuv)?;

                unsafe {
                    (*yuv.as_mut_ptr()).sample_aspect_ratio =
                        ffmpeg::ffi::AVRational { num: 1, den: 1 };
                }

                let src_rel_secs = (frame_pts_secs - clip.source_offset).max(0.0);
                let target_out_pts = clip_start_frame_idx
                    + (src_rel_secs * spec.fps as f64).round() as i64;

                if target_out_pts >= out_frame_idx {
                    loop {
                        yuv.set_pts(Some(out_frame_idx));

                        send_video_frame(&yuv, video_encoder, hw_frames_ctx, hw_backend)?;

                        let mut pkt = Packet::empty();
                        while video_encoder.receive_packet(&mut pkt).is_ok() {
                            pkt.set_stream(0);
                            pkt.rescale_ts(frame_tb, ost_tb);
                            let raw_dts = pkt.dts().unwrap_or(0);
                            if *last_video_dts != i64::MIN {
                                let prev_s = *last_video_dts as f64 * f64::from(ost_tb);
                                let dts_s  = raw_dts         as f64 * f64::from(ost_tb);
                                if dts_s < prev_s {
                                    let clamped = *last_video_dts + 1;
                                    eprintln!(
                                        "[encode] non-monotonic DTS ({prev_s:.4}s → {dts_s:.4}s); \
                                         clamping {raw_dts} → {clamped}"
                                    );
                                    unsafe { (*pkt.as_mut_ptr()).dts = clamped; }
                                }
                            }
                            *last_video_dts = pkt.dts().unwrap_or(raw_dts);
                            pkt.write_interleaved(octx)
                                .map_err(|e| format!("write video packet: {e}"))?;
                        }

                        out_frame_idx += 1;
                        // Yield to the OS scheduler after each frame on HW paths.
                        // SW encoding is already throttled structurally (thread cap
                        // at n_cores/2).  HW encoders offload to dedicated silicon
                        // but the CPU decode+scale loop feeding them runs flat-out —
                        // yield_now() lets UI, audio, and scrub threads run whenever
                        // they're waiting, at zero cost when the system is idle.
                        if hw_backend != HwBackend::Software {
                            std::thread::yield_now();
                        }

                        // Keep audio timeline in sync with video when this clip
                        // has no audio stream, or after the clip's audio has been
                        // fully consumed (video-tail frames).
                        //
                        // IMPORTANT: do NOT push silence before audio_has_started.
                        // Before the first audio packet is decoded, the FIFO is
                        // empty and the gap equals the full expected sample count.
                        // Pre-filling that with zeros causes the AAC encoder to
                        // emit a silence block immediately followed by real audio,
                        // which produces an audible pop/click at the start of the
                        // clip. Real audio arrives in the very next demux packet,
                        // so no padding is needed — it will fill the gap itself.
                        if audio_decoder.is_none() || audio_has_started {
                            let expected = out_frame_idx as i64 * AUDIO_RATE as i64 / spec.fps as i64;
                            let have     = audio_state.out_sample_idx + audio_state.fifo.len() as i64;
                            let gap      = (expected - have).max(0) as usize;
                            if gap > 0 {
                                audio_state.fifo.left.extend(std::iter::repeat(0.0f32).take(gap));
                                audio_state.fifo.right.extend(std::iter::repeat(0.0f32).take(gap));
                            }
                        }

                        if out_frame_idx as u64 % PROGRESS_INTERVAL == 0 {
                            let _ = tx.send(MediaResult::EncodeProgress {
                                job_id:       spec.job_id,
                                frame:        out_frame_idx as u64,
                                total_frames,
                            });
                        }

                        if out_frame_idx > target_out_pts { break; }
                    }
                }
            }
        }

        // ── Audio packet ──────────────────────────────────────────────────────
        else if Some(sidx) == audio_stream_idx {
            if let Some(ref mut adec) = audio_decoder {
                if adec.send_packet(&packet).is_err() { continue; }

                let mut raw = AudioFrame::empty();
                while adec.receive_frame(&mut raw).is_ok() {
                    let pts_secs = raw.pts()
                        .map(|pts| pts as f64 * f64::from(in_audio_tb))
                        .unwrap_or(0.0);

                    if pts_secs < clip.source_offset - 0.05 { continue; }
                    if pts_secs >= clip_end { continue; }

                    let pre_roll = ((clip.source_offset - pts_secs).max(0.0)
                        * AUDIO_RATE as f64).round() as usize;

                    let target_fmt = Sample::F32(SampleType::Planar);
                    let raw_channels = raw.ch_layout().channels();
                    let needs_resample = raw.format()  != target_fmt
                        || raw.rate()                  != AUDIO_RATE as u32
                        || raw_channels                != 2;

                    if needs_resample {
                        let rs = audio_resampler.get_or_insert_with(|| {
                            let src_layout = if raw.ch_layout().channels() >= 2 {
                                raw.ch_layout()
                            } else {
                                ChannelLayout::MONO
                            };
                            resampling::Context::get2(
                                raw.format(), src_layout,            raw.rate(),
                                target_fmt,   ChannelLayout::STEREO, AUDIO_RATE as u32,
                            ).expect("create audio resampler")
                        });

                        let mut resampled = AudioFrame::empty();
                        if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                            audio_state.fifo.push_scaled_from(&resampled, clip.volume as f32, pre_roll);
                            audio_has_started = true;
                        }
                    } else {
                        audio_state.fifo.push_scaled_from(&raw, clip.volume as f32, pre_roll);
                        audio_has_started = true;
                    }
                    // Do NOT drain here — draining after every audio frame causes
                    // audio to run many seconds ahead of video in the muxer interleave
                    // buffer at 1080p (slow video encoder), which makes write_interleaved
                    // drop audio packets silently. Drain once per outer packet iteration
                    // (below) so audio/video stay within one demuxer packet of each other.
                }
            }
        }

        // Drain the audio FIFO once per packet-loop iteration rather than once
        // per decoded audio frame. This throttles how far audio can run ahead of
        // video in write_interleaved's internal queue (bounded by source demux
        // rate, not by audio decoding speed). At 1080p the video encoder is slow
        // enough that eager per-frame draining let audio sprint 5–10 s ahead,
        // exceeding the MP4 muxer's interleave window and causing silent drops.
        audio_state.drain_fifo(octx, false)?;
    }

    // ── Drain video decoder at clip end ───────────────────────────────────────
    video_decoder.send_eof()
        .map_err(|e| format!("send EOF to video decoder '{}': {e}", clip.path.display()))?;
    let mut decoded = VideoFrame::empty();
    while video_decoder.receive_frame(&mut decoded).is_ok() {
        let pts_secs = decoded.pts()
            .map(|pts| pts as f64 * f64::from(in_video_tb))
            .unwrap_or(0.0);
        if pts_secs >= clip_end { break; }

        if let Some(sc) = &mut video_scaler {
            let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
            if sc.run(&decoded, &mut yuv).is_ok() {
                unsafe {
                    (*yuv.as_mut_ptr()).sample_aspect_ratio =
                        ffmpeg::ffi::AVRational { num: 1, den: 1 };
                }
                let src_rel_secs = (pts_secs - clip.source_offset).max(0.0);
                let target_out_pts = clip_start_frame_idx
                    + (src_rel_secs * spec.fps as f64).round() as i64;
                if target_out_pts >= out_frame_idx {
                    loop {
                        yuv.set_pts(Some(out_frame_idx));
                        send_video_frame(&yuv, video_encoder, hw_frames_ctx, hw_backend)?;
                        let mut pkt = Packet::empty();
                        while video_encoder.receive_packet(&mut pkt).is_ok() {
                            pkt.set_stream(0);
                            pkt.rescale_ts(frame_tb, ost_tb);
                            let raw_dts = pkt.dts().unwrap_or(0);
                            if *last_video_dts != i64::MIN {
                                let prev_s = *last_video_dts as f64 * f64::from(ost_tb);
                                let dts_s  = raw_dts         as f64 * f64::from(ost_tb);
                                if dts_s < prev_s {
                                    let clamped = *last_video_dts + 1;
                                    eprintln!(
                                        "[encode] decoder-flush non-monotonic DTS \
                                         ({prev_s:.4}s → {dts_s:.4}s); clamping {raw_dts} → {clamped}"
                                    );
                                    unsafe { (*pkt.as_mut_ptr()).dts = clamped; }
                                }
                            }
                            *last_video_dts = pkt.dts().unwrap_or(raw_dts);
                            pkt.write_interleaved(octx)
                                .map_err(|e| format!("decoder-flush write video packet: {e}"))?;
                        }
                        out_frame_idx += 1;
                        // Yield to the OS scheduler after each frame on HW paths.
                        // SW encoding is already throttled structurally (thread cap
                        // at n_cores/2).  HW encoders offload to dedicated silicon
                        // but the CPU decode+scale loop feeding them runs flat-out —
                        // yield_now() lets UI, audio, and scrub threads run whenever
                        // they're waiting, at zero cost when the system is idle.
                        if hw_backend != HwBackend::Software {
                            std::thread::yield_now();
                        }
                        // Same silence-padding logic as the main packet loop.
                        if audio_decoder.is_none() || audio_has_started {
                            let expected = out_frame_idx as i64 * AUDIO_RATE as i64 / spec.fps as i64;
                            let have     = audio_state.out_sample_idx + audio_state.fifo.len() as i64;
                            let gap      = (expected - have).max(0) as usize;
                            if gap > 0 {
                                audio_state.fifo.left.extend(std::iter::repeat(0.0f32).take(gap));
                                audio_state.fifo.right.extend(std::iter::repeat(0.0f32).take(gap));
                            }
                        }
                        if out_frame_idx > target_out_pts { break; }
                    }
                }
            }
        }
    }

    // ── Drain audio decoder at clip end ───────────────────────────────────────
    if let Some(ref mut adec) = audio_decoder {
        let _ = adec.send_eof();
        let mut raw = AudioFrame::empty();
        while adec.receive_frame(&mut raw).is_ok() {
            let pts_secs = raw.pts()
                .map(|pts| pts as f64 * f64::from(in_audio_tb))
                .unwrap_or(0.0);
            if pts_secs >= clip_end { break; }

            let pre_roll = ((clip.source_offset - pts_secs).max(0.0)
                * AUDIO_RATE as f64).round() as usize;

            let target_fmt = Sample::F32(SampleType::Planar);
            let raw_channels = raw.ch_layout().channels();
            let needs_resample = raw.format()  != target_fmt
                || raw.rate()                  != AUDIO_RATE as u32
                || raw_channels                != 2;

            if needs_resample {
                if let Some(rs) = &mut audio_resampler {
                    let mut resampled = AudioFrame::empty();
                    if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                        audio_state.fifo.push_scaled_from(&resampled, clip.volume as f32, pre_roll);
                    }
                }
            } else {
                audio_state.fifo.push_scaled_from(&raw, clip.volume as f32, pre_roll);
            }
        }

        // ── Resampler tail flush (1080p audio dropout fix) ────────────────────
        // After decoder EOF the SwrContext may hold a partial output block that
        // was never emitted because it was waiting for more input. Flush it now
        // by sending a null input frame. Without this, the tail ~20 ms per clip
        // is silently discarded, causing audible audio gaps at higher resolutions
        // where the encode thread is slower relative to the audio decode rate.
        if let Some(ref mut rs) = audio_resampler {
            flush_audio_resampler(rs, &mut audio_state.fifo, clip.volume);
        }

        audio_state.drain_fifo(octx, false)?;
    }

    // ── Final silence pad / excess trim for the whole clip ────────────────────
    // Ensures the FIFO is exactly aligned to the clip video endpoint:
    //   • Gap   → pad silence so overlays continue to be mixed.
    //   • Excess → trim FIFO so the last AAC frame's tail (which may extend a
    //              few ms past clip_end due to frame alignment) is removed before
    //              apply_transition pushes the transition audio starting at the
    //              same clip_end position.  Without the trim the FIFO contains a
    //              ~23ms duplicate of the transition-start audio → crackle.
    {
        let expected = out_frame_idx as i64 * AUDIO_RATE as i64 / spec.fps as i64;
        let have     = audio_state.out_sample_idx + audio_state.fifo.len() as i64;
        if have < expected {
            let gap = (expected - have) as usize;
            audio_state.fifo.left.extend(std::iter::repeat(0.0f32).take(gap));
            audio_state.fifo.right.extend(std::iter::repeat(0.0f32).take(gap));
        } else if have > expected {
            let excess = (have - expected) as usize;
            let new_len = audio_state.fifo.left.len().saturating_sub(excess);
            audio_state.fifo.left.truncate(new_len);
            audio_state.fifo.right.truncate(new_len);
        }
        // Drain any full frames the silence padding completed.
        audio_state.drain_fifo(octx, false)?;
    }

    Ok(out_frame_idx)
}

// ── Crossfade helpers ─────────────────────────────────────────────────────────

fn decode_clip_frames(
    clip: &ClipSpec,
    spec: &EncodeSpec,
) -> Result<Vec<Vec<u8>>, String> {
    let mut ictx = open_input(&clip.path)
        .map_err(|e| format!("crossfade open '{}': {e}", clip.path.display()))?;

    let video_stream_idx = ictx.streams().best(MediaType::Video)
        .ok_or_else(|| format!("no video stream in '{}' for crossfade", clip.path.display()))?
        .index();

    let in_video_tb = ictx.stream(video_stream_idx).unwrap().time_base();

    let (src_display_w, src_display_h) = {
        let stream = ictx.stream(video_stream_idx).unwrap();
        let params = stream.parameters();
        let w = params.width() as u32;
        let h = params.height() as u32;
        (w, h)
    };

    let vdec_ctx = codec::context::Context::from_parameters(
        ictx.stream(video_stream_idx).unwrap().parameters(),
    ).map_err(|e| format!("crossfade video decoder context: {e}"))?;

    let mut video_decoder = vdec_ctx.decoder().video()
        .map_err(|e| format!("crossfade open video decoder: {e}"))?;

    seek_to_secs(&mut ictx, clip.source_offset, "decode_clip_frames");

    let mut video_scaler: Option<CropScaler> = None;
    let clip_end   = clip.source_offset + clip.duration;
    let half_frame = 0.5 / spec.fps as f64;
    let w = spec.width  as usize;
    let h = spec.height as usize;
    let uv_w = w / 2;
    let uv_h = h / 2;

    let mut frames: Vec<Vec<u8>> = Vec::new();

    'packet_loop: for result in ictx.packets() {
        let (stream, packet) = result
            .map_err(|e| format!("crossfade read packet: {e}"))?;

        if stream.index() != video_stream_idx { continue; }

        video_decoder.send_packet(&packet)
            .map_err(|e| format!("crossfade send packet: {e}"))?;

        let mut decoded = VideoFrame::empty();
        while video_decoder.receive_frame(&mut decoded).is_ok() {
            let pts_secs = decoded.pts()
                .map(|pts| pts as f64 * f64::from(in_video_tb))
                .unwrap_or(0.0);

            if pts_secs < clip.source_offset - half_frame { continue; }
            if pts_secs >= clip_end { break 'packet_loop; }

            let sc = video_scaler.get_or_insert_with(|| {
                CropScaler::build(
                    decoded.format(), src_display_w, src_display_h,
                    spec.width, spec.height,
                )
            });

            let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
            sc.run(&decoded, &mut yuv)
                .map_err(|e| format!("crossfade scale: {e}"))?;

            frames.push(extract_yuv(&yuv, w, h, uv_w, uv_h));
        }
    }

    let _ = video_decoder.send_eof();
    let mut decoded = VideoFrame::empty();
    while video_decoder.receive_frame(&mut decoded).is_ok() {
        let pts_secs = decoded.pts()
            .map(|pts| pts as f64 * f64::from(in_video_tb))
            .unwrap_or(0.0);
        if pts_secs >= clip_end { break; }

        if let Some(sc) = &mut video_scaler {
            let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
            if sc.run(&decoded, &mut yuv).is_ok() {
                frames.push(extract_yuv(&yuv, w, h, uv_w, uv_h));
            }
        }
    }

    Ok(frames)
}

fn decode_clip_audio(
    clip: &ClipSpec,
) -> Result<(Vec<f32>, Vec<f32>), String> {
    let mut ictx = open_input(&clip.path)
        .map_err(|e| format!("transition audio open '{}': {e}", clip.path.display()))?;

    let audio_stream_idx = match ictx.streams().best(MediaType::Audio) {
        Some(s) => s.index(),
        None    => return Ok((Vec::new(), Vec::new())),
    };

    let ast         = ictx.stream(audio_stream_idx).unwrap();
    let in_audio_tb = ast.time_base();

    let adec_ctx = codec::context::Context::from_parameters(ast.parameters())
        .map_err(|e| format!("transition audio decoder ctx '{}': {e}", clip.path.display()))?;
    let mut adec = adec_ctx.decoder().audio()
        .map_err(|e| format!("transition audio decoder open '{}': {e}", clip.path.display()))?;

    seek_to_secs(&mut ictx, clip.source_offset, "decode_clip_audio");

    let clip_end    = clip.source_offset + clip.duration;
    let target_fmt  = Sample::F32(SampleType::Planar);
    let mut audio_resampler: Option<resampling::Context> = None;
    let mut left  = Vec::<f32>::new();
    let mut right = Vec::<f32>::new();

    fn push_frame(frame: &AudioFrame, vol: f32, left: &mut Vec<f32>, right: &mut Vec<f32>, skip: usize) {
        let n = frame.samples();
        if n <= skip { return; }
        unsafe {
            let l_bytes = frame.data(0);
            let l_f32   = std::slice::from_raw_parts(l_bytes.as_ptr() as *const f32, n);
            left.extend(l_f32[skip..].iter().map(|s| (s * vol).clamp(-1.0, 1.0)));

            let r_bytes = if frame.ch_layout().channels() >= 2 { frame.data(1) } else { frame.data(0) };
            let r_f32   = std::slice::from_raw_parts(r_bytes.as_ptr() as *const f32, n);
            right.extend(r_f32[skip..].iter().map(|s| (s * vol).clamp(-1.0, 1.0)));
        }
    }

    'pkt: for result in ictx.packets() {
        let (stream, packet) = result
            .map_err(|e| format!("transition audio read packet: {e}"))?;
        if stream.index() != audio_stream_idx { continue; }

        if adec.send_packet(&packet).is_err() { continue; }

        let mut raw = AudioFrame::empty();
        while adec.receive_frame(&mut raw).is_ok() {
            let pts_secs = raw.pts()
                .map(|pts| pts as f64 * f64::from(in_audio_tb))
                .unwrap_or(0.0);
            if pts_secs < clip.source_offset - 0.05 { continue; }
            if pts_secs >= clip_end { break 'pkt; }

            let pre_roll = ((clip.source_offset - pts_secs).max(0.0)
                * AUDIO_RATE as f64).round() as usize;

            let raw_channels   = raw.ch_layout().channels();
            let needs_resample = raw.format() != target_fmt
                || raw.rate()               != AUDIO_RATE as u32
                || raw_channels             != 2;

            if needs_resample {
                let rs = audio_resampler.get_or_insert_with(|| {
                    let src_layout = if raw.ch_layout().channels() >= 2 {
                        raw.ch_layout()
                    } else {
                        ChannelLayout::MONO
                    };
                    resampling::Context::get2(
                        raw.format(), src_layout,            raw.rate(),
                        target_fmt,   ChannelLayout::STEREO, AUDIO_RATE as u32,
                    ).expect("create audio resampler (transition)")
                });
                let mut resampled = AudioFrame::empty();
                if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                    push_frame(&resampled, clip.volume, &mut left, &mut right, pre_roll);
                }
            } else {
                push_frame(&raw, clip.volume, &mut left, &mut right, pre_roll);
            }
        }
    }

    let _ = adec.send_eof();
    let mut raw = AudioFrame::empty();
    while adec.receive_frame(&mut raw).is_ok() {
        let pts_secs = raw.pts()
            .map(|pts| pts as f64 * f64::from(in_audio_tb))
            .unwrap_or(0.0);
        if pts_secs >= clip_end { break; }

        let raw_channels   = raw.ch_layout().channels();
        let needs_resample = raw.format() != target_fmt
            || raw.rate()               != AUDIO_RATE as u32
            || raw_channels             != 2;

        if needs_resample {
            if let Some(rs) = &mut audio_resampler {
                let mut resampled = AudioFrame::empty();
                if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                    push_frame(&resampled, clip.volume, &mut left, &mut right, 0);
                }
            }
        } else {
            push_frame(&raw, clip.volume, &mut left, &mut right, 0);
        }
    }

    // Flush resampler tail (same fix as encode_clip).
    if let Some(ref mut rs) = audio_resampler {
        let n_buffered = unsafe { ffmpeg::ffi::swr_get_delay(rs.as_mut_ptr(), AUDIO_RATE as i64) };
        if n_buffered > 0 {
            let mut tmp_frame = AudioFrame::new(
                Sample::F32(SampleType::Planar), 4096, ChannelLayoutMask::STEREO);
            tmp_frame.set_rate(AUDIO_RATE as u32);
            unsafe {
                let n_out = ffmpeg::ffi::swr_convert(
                    rs.as_mut_ptr(),
                    (*tmp_frame.as_mut_ptr()).data.as_mut_ptr() as *mut *mut u8,
                    4096,
                    std::ptr::null_mut(), 0,
                );
                if n_out > 0 {
                    (*tmp_frame.as_mut_ptr()).nb_samples = n_out;
                    push_frame(&tmp_frame, clip.volume, &mut left, &mut right, 0);
                }
            }
        }
    }

    Ok((left, right))
}

fn apply_transition(
    transition:    &dyn VideoTransition,
    tail_spec:     &ClipSpec,
    head_spec:     &ClipSpec,
    spec:          &EncodeSpec,
    octx:          &mut ffmpeg::format::context::Output,
    video_encoder: &mut ffmpeg::encoder::Video,
    hw_frames_ctx: *mut ffmpeg::ffi::AVBufferRef,
    hw_backend:    HwBackend,
    audio_state:   &mut AudioEncState,
    mut out_frame_idx: i64,
    total_frames:  u64,
    frame_tb:      Rational,
    cancel:        &Arc<AtomicBool>,
    tx:            &Sender<MediaResult>,
    last_video_dts: &mut i64,
) -> Result<i64, String> {
    let tail_frames = decode_clip_frames(tail_spec, spec)?;
    let head_frames = decode_clip_frames(head_spec, spec)?;

    let (tail_audio_l, tail_audio_r) = decode_clip_audio(tail_spec)?;
    let (head_audio_l, head_audio_r) = decode_clip_audio(head_spec)?;

    let samples_per_frame_f = AUDIO_RATE as f64 / spec.fps as f64;

    let n = tail_frames.len().min(head_frames.len());
    if n == 0 {
        return Ok(out_frame_idx);
    }

    let w    = spec.width  as usize;
    let h    = spec.height as usize;
    let uv_w = w / 2;
    let uv_h = h / 2;
    let ost_tb = octx.stream(0).unwrap().time_base();

    for i in 0..n {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let alpha   = velocut_core::transitions::helpers::frame_alpha(i, n);
        let blended = transition.apply(
            &tail_frames[i],
            &head_frames[i],
            spec.width,
            spec.height,
            alpha,
        );

        let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
        yuv.set_pts(Some(out_frame_idx));
        unsafe {
            (*yuv.as_mut_ptr()).sample_aspect_ratio =
                ffmpeg::ffi::AVRational { num: 1, den: 1 };
        }

        write_yuv(&blended, &mut yuv, w, h, uv_w, uv_h);

        send_video_frame(&yuv, video_encoder, hw_frames_ctx, hw_backend)?;

        let mut pkt = Packet::empty();
        while video_encoder.receive_packet(&mut pkt).is_ok() {
            pkt.set_stream(0);
            pkt.rescale_ts(frame_tb, ost_tb);
            let raw_dts = pkt.dts().unwrap_or(0);
            if *last_video_dts != i64::MIN {
                let prev_s = *last_video_dts as f64 * f64::from(ost_tb);
                let dts_s  = raw_dts         as f64 * f64::from(ost_tb);
                if dts_s < prev_s {
                    let clamped = *last_video_dts + 1;
                    eprintln!(
                        "[transition] non-monotonic DTS ({prev_s:.4}s → {dts_s:.4}s); \
                         clamping {raw_dts} → {clamped}"
                    );
                    unsafe { (*pkt.as_mut_ptr()).dts = clamped; }
                }
            }
            *last_video_dts = pkt.dts().unwrap_or(raw_dts);
            pkt.write_interleaved(octx)
                .map_err(|e| format!("transition write packet: {e}"))?;
        }

        let sample_start = (i       as f64 * samples_per_frame_f).round() as usize;
        let sample_end   = ((i + 1) as f64 * samples_per_frame_f).round() as usize;
        let af = alpha as f32;

        // Clamp tail/head vec access: if AAC frame alignment leaves the vec a few
        // samples short of sample_end, hold the last valid sample rather than
        // snapping to 0.0.  A hard snap to silence mid-crossfade is audible as
        // crackling; holding the last sample produces a near-identical waveform
        // (alpha≈1 at tail-end so tail contribution is ≈0 anyway).
        let tail_last_l = tail_audio_l.last().copied().unwrap_or(0.0);
        let tail_last_r = tail_audio_r.last().copied().unwrap_or(0.0);
        let head_last_l = head_audio_l.last().copied().unwrap_or(0.0);
        let head_last_r = head_audio_r.last().copied().unwrap_or(0.0);

        for s in sample_start..sample_end {
            let t_l = tail_audio_l.get(s).copied().unwrap_or(tail_last_l);
            let t_r = tail_audio_r.get(s).copied().unwrap_or(tail_last_r);
            let h_l = head_audio_l.get(s).copied().unwrap_or(head_last_l);
            let h_r = head_audio_r.get(s).copied().unwrap_or(head_last_r);
            audio_state.fifo.left .push((t_l * (1.0 - af) + h_l * af).clamp(-1.0, 1.0));
            audio_state.fifo.right.push((t_r * (1.0 - af) + h_r * af).clamp(-1.0, 1.0));
        }

        audio_state.drain_fifo(octx, false)?;

        out_frame_idx += 1;
        // Yield to the OS scheduler after each transition frame on HW paths.
        if hw_backend != HwBackend::Software {
            std::thread::yield_now();
        }

        if out_frame_idx as u64 % PROGRESS_INTERVAL == 0 {
            let _ = tx.send(MediaResult::EncodeProgress {
                job_id:       spec.job_id,
                frame:        out_frame_idx as u64,
                total_frames,
            });
        }
    }

    Ok(out_frame_idx)
}
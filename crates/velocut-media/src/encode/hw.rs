// crates/velocut-media/src/encode/hw.rs
//
// Hardware encoder selection, device probing, and frame upload.
// Extracted from encode/mod.rs.

use ffmpeg::codec::{self, Id as CodecId};
use ffmpeg::encoder;
use ffmpeg::format::Pixel;
use ffmpeg::util::frame::video::Video as VideoFrame;
use ffmpeg::util::rational::Rational;
use ffmpeg_the_third as ffmpeg;

use super::HwEncodeCapabilities;

// ── Hardware capability probe ─────────────────────────────────────────────────

/// Probe hardware encoder availability without starting an actual encode.
///
/// Tries each backend in priority order by attempting to create a D3D11/CUDA/
/// VAAPI device context and a tiny (128×128) hw_frames_ctx. No frames are
/// encoded. The probe typically completes in < 100 ms.
///
/// Call once at app startup and cache the result. Pass it to the export UI
/// so it can annotate resolution options before the user starts a render.
pub fn probe_hw_encode_capabilities() -> HwEncodeCapabilities {
    crate::media_log!("[encode] probing HW encode capabilities...");

    // AMF — D3D11, Windows (AMD/Intel/discrete NVIDIA via AMF runtime)
    if encoder::find_by_name("h264_amf").is_some() {
        if probe_d3d11_device(1920, 1080) {
            crate::media_log!("[encode] probe: AMF available");
            return HwEncodeCapabilities {
                sw_only: false,
                backend_name: "AMF",
            };
        } else {
            crate::media_log!("[encode] probe: h264_amf found but D3D11 device init failed — skipping AMF");
        }
    } else {
        crate::media_log!("[encode] probe: h264_amf not found in FFmpeg build — recompile with --enable-encoder=h264_amf for AMD/Intel GPU support");
    }

    // NVENC — CUDA, Windows/Linux
    if encoder::find_by_name("h264_nvenc").is_some() {
        if probe_cuda_device(1920, 1080) {
            crate::media_log!("[encode] probe: NVENC available");
            return HwEncodeCapabilities {
                sw_only: false,
                backend_name: "NVENC",
            };
        } else {
            crate::media_log!(
                "[encode] probe: h264_nvenc found but CUDA device init failed — skipping NVENC"
            );
        }
    } else {
        crate::media_log!("[encode] probe: h264_nvenc not found in FFmpeg build — skipping NVENC");
    }

    // VAAPI — Linux (AMD/Intel)
    if encoder::find_by_name("h264_vaapi").is_some() && probe_vaapi_device(1920, 1080) {
        crate::media_log!("[encode] probe: VAAPI available");
        return HwEncodeCapabilities {
            sw_only: false,
            backend_name: "VAAPI",
        };
    }

    // VideoToolbox — macOS
    if encoder::find_by_name("h264_videotoolbox").is_some() {
        crate::media_log!("[encode] probe: VideoToolbox available");
        return HwEncodeCapabilities {
            sw_only: false,
            backend_name: "VideoToolbox",
        };
    }

    crate::media_log!("[encode] probe: no HW encoder available — SW only, throttled at all resolutions");
    HwEncodeCapabilities {
        sw_only: true,
        backend_name: "Software (libx264)",
    }
}

/// Try to create a D3D11VA device + NV12 frames context. Returns true on success.
fn probe_d3d11_device(width: u32, height: u32) -> bool {
    let mut device_ctx: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        return false;
    }

    let ok = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            width,
            height,
        ) {
            Ok(f) => {
                ffmpeg::ffi::av_buffer_unref(&mut (f as *mut _));
                true
            }
            Err(_) => false,
        }
    };
    unsafe {
        ffmpeg::ffi::av_buffer_unref(&mut device_ctx);
    }
    ok
}

/// Try to create a CUDA device + YUV420P frames context. Returns true on success.
fn probe_cuda_device(width: u32, height: u32) -> bool {
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
        return false;
    }

    let ok = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
            width,
            height,
        ) {
            Ok(f) => {
                ffmpeg::ffi::av_buffer_unref(&mut (f as *mut _));
                true
            }
            Err(_) => false,
        }
    };
    unsafe {
        ffmpeg::ffi::av_buffer_unref(&mut device_ctx);
    }
    ok
}

/// Try to create a VAAPI device + YUV420P frames context. Returns true on success.
fn probe_vaapi_device(width: u32, height: u32) -> bool {
    let mut device_ctx: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        return false;
    }

    let ok = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
            width,
            height,
        ) {
            Ok(f) => {
                ffmpeg::ffi::av_buffer_unref(&mut (f as *mut _));
                true
            }
            Err(_) => false,
        }
    };
    unsafe {
        ffmpeg::ffi::av_buffer_unref(&mut device_ctx);
    }
    ok
}

// ── Hardware encoder selection ────────────────────────────────────────────────

/// Which H.264 encoder backend is active for this encode job.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum HwBackend {
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
pub(super) fn try_open_hw_encoder(
    width: u32,
    height: u32,
    fps: u32,
    out_tb: Rational,
    octx: &ffmpeg::format::context::Output,
) -> (ffmpeg::encoder::Video, HwBackend, Option<HwDeviceContext>) {
    // ── AMF (D3D11 — Windows native, AMD/Intel/NVIDIA) ───────────────────────
    crate::media_log!("[encode] trying AMF (D3D11)...");
    if let Some(result) = try_amf_encoder(width, height, fps, out_tb, octx) {
        crate::media_log!("[encode] HW encoder: AMF (D3D11)");
        return result;
    }

    // ── NVENC (CUDA — NVIDIA, Windows/Linux) ─────────────────────────────────
    // Only compiled in when nv-codec-headers are present at ffmpeg build time.
    crate::media_log!("[encode] trying NVENC (CUDA)...");
    if let Some(result) = try_nvenc_encoder(width, height, fps, out_tb, octx) {
        crate::media_log!("[encode] HW encoder: NVENC (CUDA)");
        return result;
    }

    // ── VAAPI ─────────────────────────────────────────────────────────────────
    crate::media_log!("[encode] trying VAAPI...");
    if let Some(result) = try_vaapi_encoder(width, height, fps, out_tb, octx) {
        crate::media_log!("[encode] HW encoder: VAAPI");
        return result;
    }

    // ── VideoToolbox ──────────────────────────────────────────────────────────
    crate::media_log!("[encode] trying VideoToolbox...");
    if let Some(result) = try_videotoolbox_encoder(width, height, fps, out_tb, octx) {
        crate::media_log!("[encode] HW encoder: VideoToolbox");
        return result;
    }

    // ── Software fallback ─────────────────────────────────────────────────────
    crate::media_log!("[encode] HW encoder: none available, using libx264 software");
    let enc = open_software_encoder(width, height, fps, out_tb, octx)
        .expect("libx264 is required — ensure it is compiled in");
    (enc, HwBackend::Software, None)
}

/// Opaque wrapper around an AVHWDeviceContext* kept alive next to the encoder.
pub(super) struct HwDeviceContext {
    /// The raw AVBufferRef* for the device context.
    /// Must stay alive for as long as the encoder is open.
    ptr: *mut ffmpeg::ffi::AVBufferRef,
}

// SAFETY: HwDeviceContext owns an AVBufferRef which is reference-counted
// by FFmpeg. The internal AVHWDeviceContext is thread-safe for concurrent
// access (FFmpeg serializes all HW device operations internally).
// Sync is required because the HwDeviceContext is stored alongside the encoder
// in structures that cross channel boundaries (crossbeam requires Sync).
// Only the encode thread dereferences the pointer; the impl exists solely
// for channel compatibility.
unsafe impl Send for HwDeviceContext {}
unsafe impl Sync for HwDeviceContext {}

impl Drop for HwDeviceContext {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                ffmpeg::ffi::av_buffer_unref(&mut self.ptr);
            }
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
pub(super) unsafe fn upload_frame_to_hw(
    sw_frame: &VideoFrame,
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
    width: u32,
    height: u32,
) -> Result<*mut ffmpeg::ffi::AVBufferRef, String> {
    let frames_ref = ffmpeg::ffi::av_hwframe_ctx_alloc(device_ctx);
    if frames_ref.is_null() {
        return Err("av_hwframe_ctx_alloc failed".into());
    }

    let frames_ctx = (*frames_ref).data as *mut ffmpeg::ffi::AVHWFramesContext;
    (*frames_ctx).format = hw_pix_fmt;
    (*frames_ctx).sw_format = sw_pix_fmt;
    (*frames_ctx).width = width as i32;
    (*frames_ctx).height = height as i32;
    (*frames_ctx).initial_pool_size = 20;

    let ret = ffmpeg::ffi::av_hwframe_ctx_init(frames_ref);
    if ret < 0 {
        ffmpeg::ffi::av_buffer_unref(&mut (frames_ref as *mut _));
        return Err(format!("av_hwframe_ctx_init failed: {ret}"));
    }

    Ok(frames_ref)
}

fn try_amf_encoder(
    width: u32,
    height: u32,
    fps: u32,
    out_tb: Rational,
    octx: &ffmpeg::format::context::Output,
) -> Option<(ffmpeg::encoder::Video, HwBackend, Option<HwDeviceContext>)> {
    let codec = match encoder::find_by_name("h264_amf") {
        Some(c) => c,
        None => {
            crate::media_log!("[encode] h264_amf not found in ffmpeg build, skipping AMF");
            return None;
        }
    };

    // AMF on Windows uses a D3D11 device context.
    let mut device_ctx: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device_ctx,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            std::ptr::null(), // NULL = use default adapter
            std::ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        crate::media_log!("[encode] D3D11 device init failed ({ret}), skipping AMF");
        return None;
    }

    let frames_ctx = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12, // D3D11 requires NV12, not YUV420P
            width,
            height,
        ) {
            Ok(f) => f,
            Err(e) => {
                crate::media_log!("[encode] AMF frames ctx: {e}");
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
        (*enc.as_mut_ptr()).pix_fmt = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
        (*enc.as_mut_ptr()).hw_frames_ctx = ffmpeg::ffi::av_buffer_ref(frames_ctx);
        ffmpeg::ffi::av_buffer_unref(&mut (frames_ctx as *mut _));
    }

    if octx
        .format()
        .flags()
        .contains(ffmpeg::format::Flags::GLOBAL_HEADER)
    {
        enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    // AMF quality options: quality preset + CQP equivalent to CRF 18.
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("quality", "quality"); // slow preset — balanced quality/speed
    opts.set("rc", "cqp"); // constant QP, analogous to CRF
    opts.set("qp_i", "18");
    opts.set("qp_p", "20");
    opts.set("qp_b", "22");
    opts.set("g", &fps.to_string());

    match enc.open_as_with(codec, opts) {
        Ok(opened) => {
            let hw_dev = HwDeviceContext { ptr: device_ctx };
            Some((opened, HwBackend::Amf, Some(hw_dev)))
        }
        Err(e) => {
            crate::media_log!("[encode] h264_amf open failed: {e}, skipping AMF");
            unsafe {
                ffmpeg::ffi::av_buffer_unref(&mut device_ctx);
            }
            None
        }
    }
}

fn try_nvenc_encoder(
    width: u32,
    height: u32,
    fps: u32,
    out_tb: Rational,
    octx: &ffmpeg::format::context::Output,
) -> Option<(ffmpeg::encoder::Video, HwBackend, Option<HwDeviceContext>)> {
    let codec = match encoder::find_by_name("h264_nvenc") {
        Some(c) => c,
        None => {
            crate::media_log!("[encode] h264_nvenc not found in ffmpeg build, skipping NVENC");
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
        crate::media_log!("[encode] CUDA device init failed ({ret}), skipping NVENC");
        return None;
    }

    let frames_ctx = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
            width,
            height,
        ) {
            Ok(f) => f,
            Err(e) => {
                crate::media_log!("[encode] NVENC frames ctx: {e}");
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
        (*enc.as_mut_ptr()).pix_fmt = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
        (*enc.as_mut_ptr()).hw_frames_ctx = ffmpeg::ffi::av_buffer_ref(frames_ctx);
        ffmpeg::ffi::av_buffer_unref(&mut (frames_ctx as *mut _));
    }

    if octx
        .format()
        .flags()
        .contains(ffmpeg::format::Flags::GLOBAL_HEADER)
    {
        enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    // NVENC options: rc=constqp, qp=18, preset=p4 (balanced quality/speed).
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("rc", "constqp");
    opts.set("qp", "18");
    opts.set("preset", "p4");
    opts.set("g", &fps.to_string());

    match enc.open_as_with(codec, opts) {
        Ok(opened) => {
            let hw_dev = HwDeviceContext { ptr: device_ctx };
            Some((opened, HwBackend::Nvenc, Some(hw_dev)))
        }
        Err(e) => {
            crate::media_log!("[encode] h264_nvenc open failed: {e}, skipping");
            unsafe {
                ffmpeg::ffi::av_buffer_unref(&mut device_ctx);
            }
            None
        }
    }
}

fn try_vaapi_encoder(
    width: u32,
    height: u32,
    fps: u32,
    out_tb: Rational,
    octx: &ffmpeg::format::context::Output,
) -> Option<(ffmpeg::encoder::Video, HwBackend, Option<HwDeviceContext>)> {
    let codec = match encoder::find_by_name("h264_vaapi") {
        Some(c) => c,
        None => {
            crate::media_log!("[encode] h264_vaapi not found in ffmpeg build, skipping VAAPI");
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
        crate::media_log!("[encode] VAAPI device init failed ({ret}), skipping");
        return None;
    }

    let frames_ctx = unsafe {
        match build_hw_frames_ctx(
            device_ctx,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
            width,
            height,
        ) {
            Ok(f) => f,
            Err(e) => {
                crate::media_log!("[encode] VAAPI frames ctx: {e}");
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
        (*enc.as_mut_ptr()).pix_fmt = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*enc.as_mut_ptr()).hw_frames_ctx = ffmpeg::ffi::av_buffer_ref(frames_ctx);
        ffmpeg::ffi::av_buffer_unref(&mut (frames_ctx as *mut _));
    }

    if octx
        .format()
        .flags()
        .contains(ffmpeg::format::Flags::GLOBAL_HEADER)
    {
        enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    // VAAPI uses qp-based rate control; ~18 ≈ CRF 18 visually.
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("rc_mode", "CQP");
    opts.set("qp", "18");
    opts.set("g", &fps.to_string());

    match enc.open_as_with(codec, opts) {
        Ok(opened) => {
            let hw_dev = HwDeviceContext { ptr: device_ctx };
            Some((opened, HwBackend::Vaapi, Some(hw_dev)))
        }
        Err(e) => {
            crate::media_log!("[encode] h264_vaapi open failed: {e}, skipping");
            unsafe {
                ffmpeg::ffi::av_buffer_unref(&mut device_ctx);
            }
            None
        }
    }
}

fn try_videotoolbox_encoder(
    width: u32,
    height: u32,
    fps: u32,
    out_tb: Rational,
    octx: &ffmpeg::format::context::Output,
) -> Option<(ffmpeg::encoder::Video, HwBackend, Option<HwDeviceContext>)> {
    // VideoToolbox accepts YUV420P input directly (no explicit HW upload needed);
    // it manages the IOSurface pool internally.
    let codec = match encoder::find_by_name("h264_videotoolbox") {
        Some(c) => c,
        None => {
            crate::media_log!(
                "[encode] h264_videotoolbox not found in ffmpeg build, skipping VideoToolbox"
            );
            return None;
        }
    };

    let enc_ctx_obj = codec::context::Context::new_with_codec(codec);
    let mut enc = enc_ctx_obj.encoder().video().ok()?;

    enc.set_width(width);
    enc.set_height(height);
    enc.set_format(Pixel::YUV420P); // VT accepts sw frames in yuv420p
    enc.set_time_base(out_tb);
    enc.set_frame_rate(Some(Rational::new(fps as i32, 1)));
    enc.set_bit_rate(0);

    if octx
        .format()
        .flags()
        .contains(ffmpeg::format::Flags::GLOBAL_HEADER)
    {
        enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    let mut opts = ffmpeg::Dictionary::new();
    // VideoToolbox doesn't have CRF; use a high average bitrate for near-lossless quality.
    opts.set("b:v", "0"); // let profile control quality
    opts.set("allow_sw", "1"); // fall back to SW if GPU busy
    opts.set("realtime", "0"); // prefer quality over real-time
    let gop = fps.to_string();
    opts.set("g", &gop);

    match enc.open_as_with(codec, opts) {
        Ok(opened) => {
            // VideoToolbox: no external HwDeviceContext needed
            Some((opened, HwBackend::VideoToolbox, None))
        }
        Err(e) => {
            crate::media_log!("[encode] h264_videotoolbox open failed: {e}");
            None
        }
    }
}

pub(super) fn open_software_encoder(
    width: u32,
    height: u32,
    fps: u32,
    out_tb: Rational,
    octx: &ffmpeg::format::context::Output,
) -> Result<ffmpeg::encoder::Video, String> {
    let h264 = encoder::find(CodecId::H264)
        .ok_or_else(|| "H.264 encoder not found — is libx264 available?".to_string())?;

    let enc_ctx = codec::context::Context::new_with_codec(h264);
    let mut enc = enc_ctx
        .encoder()
        .video()
        .map_err(|e| format!("create video encoder context: {e}"))?;

    enc.set_width(width);
    enc.set_height(height);
    enc.set_format(Pixel::YUV420P);
    enc.set_time_base(out_tb);
    enc.set_frame_rate(Some(Rational::new(fps as i32, 1)));
    enc.set_bit_rate(0);

    if octx
        .format()
        .flags()
        .contains(ffmpeg::format::Flags::GLOBAL_HEADER)
    {
        enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    // Cap libx264 to half the logical CPU count so the encoder never saturates
    // every core at any resolution (480p through 4K). The OS schedules UI,
    // audio, and scrub-decode threads on the remaining cores; the encode runs
    // at a reduced but consistent pace without making the system unresponsive.
    // Minimum 1 thread; falls back to 2 if available_parallelism() fails
    // (e.g. inside a restricted sandbox).
    let thread_cap = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).max(1))
        .unwrap_or(2);

    let mut opts = ffmpeg::Dictionary::new();
    opts.set("crf", "18");
    // "medium" is more CPU-efficient per thread than "fast" — it does more work
    // per encode pass, so the total core-seconds consumed for equivalent quality
    // is lower.  Combined with the thread cap and per-frame yield_now() below,
    // this keeps peak CPU usage manageable at any resolution on any hardware.
    opts.set("preset", "medium");
    opts.set("threads", &thread_cap.to_string());
    opts.set("g", &fps.to_string());

    enc.open_as_with(h264, opts)
        .map_err(|e| format!("open H.264 encoder: {e}"))
}

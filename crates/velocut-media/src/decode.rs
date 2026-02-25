// crates/velocut-media/src/decode.rs
//
// LiveDecoder: stateful per-clip decoder that avoids re-open/seek every frame.
// decode_frame: one-shot frame decode for preview and PNG save.

use std::path::PathBuf;
use anyhow::Result;
use crossbeam_channel::Sender;
use uuid::Uuid;

use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::{input, Pixel};
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as SwsContext, flag::Flags};
use ffmpeg::ffi;

use velocut_core::media_types::MediaResult;

// ── Stateful per-clip decoder ─────────────────────────────────────────────────

pub struct LiveDecoder {
    pub path:           PathBuf,
    pub ictx:           ffmpeg::format::context::Input,
    pub decoder:        ffmpeg::decoder::video::Video,
    pub video_idx:      usize,
    pub last_pts:       i64,
    pub tb_num:         i32,
    pub tb_den:         i32,
    pub out_w:          u32,
    pub out_h:          u32,
    pub scaler:         SwsContext,
    /// Source decoder format + dimensions — used as the cache key when the scrub
    /// thread tries to reuse a SwsContext across a LiveDecoder reset.  SwsContext
    /// only needs to be re-created when these change (different camera/format).
    pub decoder_fmt:    Pixel,
    pub decoder_w:      u32,
    pub decoder_h:      u32,
    /// If non-zero, next_frame() skips (decode-only, no scale/alloc) all frames
    /// whose PTS is below this threshold, then clears the field.
    /// Used to burn through the GOP after a keyframe-aligned seek without blocking
    /// the thread on advance_to() — which scales every skipped frame needlessly.
    pub skip_until_pts: i64,

    /// [Opt 1] Reusable RGBA output buffer — avoids a heap allocation per frame.
    /// Capacity is pre-sized to out_w * out_h * 4 at construction and maintained
    /// across calls. Each call: clear() + extend_from_slice per row (bulk memcpy),
    /// then clone() once to hand off to the caller.  Saves the flat_map().collect()
    /// iterator overhead and guarantees no mid-frame realloc.
    frame_buf: Vec<u8>,

    /// D3D11VA hardware device context.  Some when hwaccel init succeeded;
    /// None when unavailable or init failed (automatic CPU fallback).
    /// Kept alive for the lifetime of the decoder — FFmpeg ref-counts it internally.
    #[allow(dead_code)]
    hw_device_ctx: Option<HwDeviceCtx>,
}

// ── D3D11VA hardware device context ──────────────────────────────────────────

/// RAII wrapper around an `AVBufferRef*` holding an `AVHWDeviceContext`.
/// Calls `av_buffer_unref` on drop so the ref-count is always balanced.
struct HwDeviceCtx(*mut ffi::AVBufferRef);

impl Drop for HwDeviceCtx {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::av_buffer_unref(&mut self.0); }
        }
    }
}

// Safety: the AVBufferRef is ref-counted internally by FFmpeg and its
// internal data (AVHWDeviceContext + D3D11Device) is thread-safe for
// concurrent decoding.  We never alias the raw pointer across threads.
unsafe impl Send for HwDeviceCtx {}

/// Try to create a D3D11VA hardware device context.
///
/// Returns `None` silently on any failure — callers fall back to CPU decode.
/// `adapter_index` 0 = system default GPU (correct for single-GPU machines;
/// multi-GPU machines may want the user to configure this later).
fn try_create_d3d11va_device() -> Option<HwDeviceCtx> {
    unsafe {
        let mut hw_ctx: *mut ffi::AVBufferRef = std::ptr::null_mut();
        // AV_HWDEVICE_TYPE_D3D11VA = 5 in FFmpeg's enum.
        // Passing NULL opts/device string = use default adapter.
        let ret = ffi::av_hwdevice_ctx_create(
            &mut hw_ctx,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        );
        if ret < 0 || hw_ctx.is_null() {
            eprintln!("[hwaccel] D3D11VA device init failed ({}), using CPU decode", ret);
            return None;
        }
        eprintln!("[hwaccel] D3D11VA device created");
        Some(HwDeviceCtx(hw_ctx))
    }
}

/// AVCodecContext.get_format callback — selects the best available D3D11VA
/// pixel format from the list FFmpeg offers at codec-open time.
///
/// Two D3D11VA pixel format variants exist:
///
/// • `AV_PIX_FMT_D3D11` (172) — newer d3d11va2 API.  FFmpeg allocates
///   `hw_frames_ctx` automatically; no manual setup needed.  Preferred.
///
/// • `AV_PIX_FMT_D3D11VA_VLD` (113) — older d3d11va API.  The application
///   must allocate and initialise `hw_frames_ctx` on `AVCodecContext` before
///   returning this format, otherwise FFmpeg prints "Invalid pixfmt for
///   hwaccel!" and falls back to software.  Handled by
///   `allocate_d3d11va_vld_frames_ctx`.
///
/// If neither hardware format is offered, the callback scans for
/// `AV_PIX_FMT_YUV420P` (0) or `AV_PIX_FMT_NV12` (23) so we return a
/// known-good CPU format rather than blindly returning `*fmt` (which could
/// be another failing hwaccel format).
///
/// All comparisons use `as i32` so the code compiles whether `AVPixelFormat`
/// is a Rust enum or a C-compatible type alias.
unsafe extern "C" fn get_format_d3d11va(
    ctx: *mut ffi::AVCodecContext,
    fmt: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    // Use enum constants from the actual FFmpeg build rather than hardcoded
    // integers — the numeric values can differ between FFmpeg versions/forks.
    let d3d11     = ffi::AVPixelFormat::AV_PIX_FMT_D3D11      as i32;
    let d3d11_vld = ffi::AVPixelFormat::AV_PIX_FMT_D3D11VA_VLD as i32;
    let yuv420p   = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P     as i32;
    let nv12      = ffi::AVPixelFormat::AV_PIX_FMT_NV12         as i32;

    // Diagnostic: dump all formats FFmpeg is offering at codec-open time.
    {
        let mut p = fmt;
        let mut offered = Vec::new();
        while (*p) as i32 != -1 {
            offered.push((*p) as i32);
            p = p.add(1);
        }
        eprintln!("[hwaccel] get_format called — offered: {:?} (d3d11={} vld={} yuv420p={} nv12={})",
            offered, d3d11, d3d11_vld, yuv420p, nv12);
    }

    // First pass: prefer AV_PIX_FMT_D3D11 (d3d11va2) — auto hw_frames_ctx.
    let mut p = fmt;
    while (*p) as i32 != -1 {
        if (*p) as i32 == d3d11 {
            eprintln!("[hwaccel] get_format: selected AV_PIX_FMT_D3D11 ({})", d3d11);
            return *p;
        }
        p = p.add(1);
    }

    // Second pass: AV_PIX_FMT_D3D11VA_VLD (older API) — must alloc hw_frames_ctx.
    p = fmt;
    while (*p) as i32 != -1 {
        if (*p) as i32 == d3d11_vld {
            eprintln!("[hwaccel] get_format: AV_PIX_FMT_D3D11VA_VLD ({}) — allocating hw_frames_ctx", d3d11_vld);
            if allocate_d3d11va_vld_frames_ctx(ctx) {
                return *p;
            }
            eprintln!("[hwaccel] get_format: hw_frames_ctx alloc failed, falling through to CPU");
            break;
        }
        p = p.add(1);
    }

    // CPU fallback: prefer YUV420P then NV12, else first offered.
    p = fmt;
    while (*p) as i32 != -1 {
        if (*p) as i32 == yuv420p { return *p; }
        p = p.add(1);
    }
    p = fmt;
    while (*p) as i32 != -1 {
        if (*p) as i32 == nv12 { return *p; }
        p = p.add(1);
    }
    eprintln!("[hwaccel] get_format: no preferred CPU format found, returning first offered");
    *fmt
}

/// Allocate and initialise an `AVHWFramesContext` for `AV_PIX_FMT_D3D11VA_VLD`.
///
/// Required when the decoder only offers format 113 (older d3d11va API).
/// The newer d3d11va2 (format 172) handles this automatically.
/// Returns `true` on success, `false` on any failure (CPU fallback).
unsafe fn allocate_d3d11va_vld_frames_ctx(ctx: *mut ffi::AVCodecContext) -> bool {
    let hw_device_ctx = (*ctx).hw_device_ctx;
    if hw_device_ctx.is_null() {
        eprintln!("[hwaccel] allocate_d3d11va_vld_frames_ctx: hw_device_ctx is NULL");
        return false;
    }

    let frames_ref = ffi::av_hwframe_ctx_alloc(hw_device_ctx);
    if frames_ref.is_null() {
        eprintln!("[hwaccel] av_hwframe_ctx_alloc failed");
        return false;
    }

    {
        let frames_ctx = (*frames_ref).data as *mut ffi::AVHWFramesContext;
        (*frames_ctx).format            = ffi::AVPixelFormat::AV_PIX_FMT_D3D11VA_VLD; // outer hw format
        (*frames_ctx).sw_format         = ffi::AVPixelFormat::AV_PIX_FMT_NV12;        // inner sw format
        (*frames_ctx).width             = (*ctx).coded_width;
        (*frames_ctx).height            = (*ctx).coded_height;
        // [Fix 3] Pool of 4 is sufficient for the decode pipeline depth (1 being
        // decoded + 1 in transfer + 2 in flight to renderer).  The original 10
        // allocated 10 × NV12 GPU surfaces per playback decoder; with 5 clips open
        // simultaneously that's 50 surfaces, each backed by a D3D11 staging buffer
        // for av_hwframe_transfer_data readback.  4 per decoder cuts that to 20.
        (*frames_ctx).initial_pool_size = 4;
    }

    let ret = ffi::av_hwframe_ctx_init(frames_ref);
    if ret < 0 {
        eprintln!("[hwaccel] av_hwframe_ctx_init failed ({})", ret);
        let mut p = frames_ref;
        ffi::av_buffer_unref(&mut p);
        return false;
    }

    // Codec context takes ownership of its ref; release our local ref below.
    (*ctx).hw_frames_ctx = ffi::av_buffer_ref(frames_ref);
    let mut p = frames_ref;
    ffi::av_buffer_unref(&mut p);

    if (*ctx).hw_frames_ctx.is_null() {
        eprintln!("[hwaccel] av_buffer_ref for hw_frames_ctx failed");
        return false;
    }

    eprintln!("[hwaccel] D3D11VA VLD hw_frames_ctx allocated ({}x{})",
        (*ctx).coded_width, (*ctx).coded_height);
    true
}

/// Transfer a hardware frame (D3D11 surface) to a CPU-accessible frame.
///
/// GPU frames are detected via `hw_frames_ctx != NULL` — more robust than
/// pixel format integer constants, which can vary between FFmpeg builds.
/// Returns the transferred frame on success, or the original frame unchanged
/// if it is already a CPU frame.
fn ensure_cpu_frame(
    frame: ffmpeg::util::frame::video::Video,
) -> ffmpeg::util::frame::video::Video {
    unsafe {
        // hw_frames_ctx is non-NULL on every hardware-accelerated frame.
        // CPU frames always have a NULL hw_frames_ctx.
        if (*frame.as_ptr()).hw_frames_ctx.is_null() {
            return frame;
        }
        let mut cpu_frame = ffmpeg::util::frame::video::Video::empty();
        let ret = ffi::av_hwframe_transfer_data(
            cpu_frame.as_mut_ptr(),
            frame.as_ptr(),
            0,
        );
        if ret < 0 {
            eprintln!("[hwaccel] av_hwframe_transfer_data failed ({}), dropping frame", ret);
            return frame;
        }
        // Copy presentation metadata — FFmpeg does not propagate this during transfer.
        (*cpu_frame.as_mut_ptr()).pts                   = (*frame.as_ptr()).pts;
        (*cpu_frame.as_mut_ptr()).pkt_dts               = (*frame.as_ptr()).pkt_dts;
        (*cpu_frame.as_mut_ptr()).best_effort_timestamp = (*frame.as_ptr()).best_effort_timestamp;
        cpu_frame
    }
}

// ── Center-crop + scale helper ────────────────────────────────────────────────

/// Decode `frame` (already on CPU) to `(dst_w × dst_h)` RGBA with a
/// **center-crop** so the output matches the canvas AR exactly.
///
/// How it works: the crop region in source coordinates that matches the
/// `(dst_w / dst_h)` aspect ratio is identified, then `sws_scale` is called
/// with pointer arithmetic that points directly into the cropped window — no
/// intermediate buffer needed.  The SwsContext is built for the crop region,
/// so swscale scales `(crop_w × crop_h)` → `(dst_w × dst_h)` in one pass.
///
/// Handles **NV12** (D3D11VA CPU readback) and **YUV420P / YUVJ420P**
/// (software decoders).  Returns `None` for any other pixel format or on any
/// FFmpeg failure; callers fall back to a stretch-scale in that case so
/// playback degrades gracefully instead of freezing.
///
/// # Safety
/// Uses raw FFmpeg API: `sws_getContext`, `sws_scale`, `sws_freeContext`.
unsafe fn center_crop_and_scale(
    frame: &ffmpeg::util::frame::video::Video,
    dst_w: u32,
    dst_h: u32,
) -> Option<Vec<u8>> {
    if dst_w == 0 || dst_h == 0 { return None; }
    let src_w = frame.width()  as u32;
    let src_h = frame.height() as u32;
    if src_w == 0 || src_h == 0 { return None; }

    let src_ar = src_w as f32 / src_h as f32;
    let dst_ar = dst_w as f32 / dst_h as f32;

    // Center-crop region in source pixels that matches the target AR.
    let (crop_x, crop_y, crop_w, crop_h) = if (src_ar - dst_ar).abs() < 0.02 {
        // ARs already match — use the full source frame.
        (0u32, 0u32, src_w, src_h)
    } else if src_ar > dst_ar {
        // Source is wider → crop left and right.
        let w = ((src_h as f32 * dst_ar) as u32 & !1).max(2).min(src_w);
        ((src_w - w) / 2, 0, w, src_h)
    } else {
        // Source is taller → crop top and bottom.
        let h = ((src_w as f32 / dst_ar) as u32 & !1).max(2).min(src_h);
        (0, (src_h - h) / 2, src_w, h)
    };

    let raw      = frame.as_ptr();
    let fmt_i    = (*raw).format;
    let nv12     = ffi::AVPixelFormat::AV_PIX_FMT_NV12     as i32;
    let yuv420p  = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P  as i32;
    let yuvj420p = ffi::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32;
    // P010LE: D3D11VA output for 10-bit content (H.264 Hi10P, HEVC Main10, etc.).
    // Same semi-planar layout as NV12 but 2 bytes per luma and chroma sample.
    let p010le   = ffi::AVPixelFormat::AV_PIX_FMT_P010LE   as i32;

    if fmt_i != nv12 && fmt_i != yuv420p && fmt_i != yuvj420p && fmt_i != p010le {
        return None; // unsupported — caller uses stretch fallback
    }

    // Per-plane strides and base pointers.
    let s0 = (*raw).linesize[0] as usize;
    let s1 = (*raw).linesize[1] as usize;
    let s2 = (*raw).linesize[2] as usize;
    let d0 = (*raw).data[0];
    let d1 = (*raw).data[1];
    let d2 = (*raw).data[2];

    // Offset each plane pointer to the top-left corner of the crop window.
    //
    // NV12:    plane-0 = Y  (1 B/luma-px), plane-1 = UV interleaved (2 B per 2×2 block)
    //          → chroma stride is half-height; horiz offset equals luma horiz offset.
    //
    // P010LE:  identical semi-planar layout to NV12 but 2 B/sample throughout,
    //          so horizontal byte offset = crop_x * 2 for both Y and UV planes.
    //
    // YUV420P: plane-0 = Y  (1 B/luma-px), plane-1 = U, plane-2 = V (quarter-size each)
    //          → chroma stride and offset are both halved in each axis.
    let (src_ptrs, src_strides): ([*const u8; 4], [i32; 4]) = if fmt_i == nv12 {
        let p0 = d0.add(crop_y as usize * s0 + crop_x as usize);
        let p1 = d1.add((crop_y as usize / 2) * s1 + crop_x as usize);
        ([p0, p1, std::ptr::null(), std::ptr::null()],
         [s0 as i32, s1 as i32, 0, 0])
    } else if fmt_i == p010le {
        let p0 = d0.add(crop_y as usize * s0 + crop_x as usize * 2);
        let p1 = d1.add((crop_y as usize / 2) * s1 + crop_x as usize * 2);
        ([p0, p1, std::ptr::null(), std::ptr::null()],
         [s0 as i32, s1 as i32, 0, 0])
    } else { // YUV420P / YUVJ420P
        let p0 = d0.add(crop_y as usize * s0 + crop_x as usize);
        let p1 = d1.add((crop_y as usize / 2) * s1 + crop_x as usize / 2);
        let p2 = d2.add((crop_y as usize / 2) * s2 + crop_x as usize / 2);
        ([p0, p1, p2, std::ptr::null()],
         [s0 as i32, s1 as i32, s2 as i32, 0])
    };

    // Build a one-shot SwsContext from (crop_w × crop_h) → (dst_w × dst_h) RGBA.
    // SWS_BILINEAR = 2.
    let pixel_fmt = if fmt_i == nv12 { ffi::AVPixelFormat::AV_PIX_FMT_NV12 }
                    else if fmt_i == p010le { ffi::AVPixelFormat::AV_PIX_FMT_P010LE }
                    else if fmt_i == yuv420p { ffi::AVPixelFormat::AV_PIX_FMT_YUV420P }
                    else { ffi::AVPixelFormat::AV_PIX_FMT_YUVJ420P };

    let sws = ffi::sws_getContext(
        crop_w as i32, crop_h as i32, pixel_fmt,
        dst_w  as i32, dst_h  as i32, ffi::AVPixelFormat::AV_PIX_FMT_RGBA,
        2, // SWS_BILINEAR
        std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(),
    );
    if sws.is_null() { return None; }

    // Allocate packed RGBA output — no stride padding needed.
    let row_bytes   = dst_w as usize * 4;
    let mut buf     = vec![0u8; row_bytes * dst_h as usize];
    let dst_stride  = row_bytes as i32;
    let mut dst_ptrs: [*mut u8; 4] = [
        buf.as_mut_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(),
    ];
    let mut dst_strides = [dst_stride, 0i32, 0i32, 0i32];

    // srcSliceY = 0: the source pointers already start at the crop row.
    // srcSliceH = crop_h: process exactly the crop window height.
    let rows = ffi::sws_scale(
        sws,
        src_ptrs.as_ptr(),
        src_strides.as_ptr(),
        0, crop_h as i32,
        dst_ptrs.as_mut_ptr(),
        dst_strides.as_mut_ptr(),
    );
    ffi::sws_freeContext(sws);

    if rows > 0 { Some(buf) } else { None }
}

// ── Stateful per-clip decoder ─────────────────────────────────────────────────
impl LiveDecoder {
    /// Open a decoder at `timestamp` seconds.
    ///
    /// `cached_scaler` — if the caller holds a `SwsContext` from a previous
    /// `LiveDecoder` (same scrub thread, different clip), pass it here with
    /// its source key `(fmt, w, h)`.  If the key matches the new clip's codec
    /// parameters the context is reused and `SwsContext::get` is skipped,
    /// saving the internal lookup-table initialisation that dominates
    /// construction cost.
    pub fn open(
        path:          &PathBuf,
        timestamp:     f64,
        aspect:        f32,   // >0 = scrub mode (320px wide); <=0 = playback/HQ mode (native res)
        cached_scaler: Option<(SwsContext, Pixel, u32, u32)>,
        forced_size:   Option<(u32, u32)>,  // when Some, override aspect/native logic entirely
    ) -> Result<Self> {
        let mut ictx = input(path)?;
        let video_idx = ictx.streams().best(Type::Video)
            .ok_or_else(|| anyhow::anyhow!("no video stream"))?.index();

        // Build the codec context inside the stream-borrow block.
        // Context::from_parameters copies AVCodecParameters into a new
        // AVCodecContext, so dec_ctx is fully owned — the stream borrow is
        // released when the block ends and ictx is free for seeking.
        // This eliminates the previous second input() open that existed solely
        // as a borrow-checker workaround.
        let (tb_num, tb_den, seek_ts, dec_ctx) = {
            let stream = ictx.stream(video_idx).unwrap();
            let tb = stream.time_base();
            let seek_ts = (timestamp * tb.denominator() as f64 / tb.numerator() as f64) as i64;
            let dec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
            (tb.numerator(), tb.denominator(), seek_ts, dec_ctx)
        };

        // [Fix 1] Discard all non-video streams at the demuxer level.
        // Without this, FFmpeg allocates and buffers audio/subtitle packets even
        // though we immediately drop them in the packet loop — with 5 LiveDecoders
        // open simultaneously that's 5 demuxers each buffering a full audio track.
        // StreamMut doesn't expose a set_discard() setter in this fork, so we
        // write the AVStream.discard field directly via the raw pointer.
        for mut stream in ictx.streams_mut() {
            if stream.index() != video_idx {
                unsafe {
                    (*stream.as_mut_ptr()).discard = ffi::AVDiscard::AVDISCARD_ALL;
                }
            }
        }

        let _ = ictx.seek(seek_ts, ..=seek_ts);

        // Make dec_ctx mutable so we can write hw_device_ctx onto the raw
        // AVCodecContext* BEFORE avcodec_open2 is called internally by
        // dec_ctx.decoder().video()?.  FFmpeg requires the device context to
        // be present at open time — setting it afterwards is silently ignored.
        let mut dec_ctx = dec_ctx;

        // ── D3D11VA hardware acceleration (pre-open) ──────────────────────────
        // Attach hw_device_ctx + get_format callback to the raw AVCodecContext*
        // BEFORE .decoder().video()? triggers avcodec_open2 internally.
        // Scrub-mode decoders (aspect > 0) skip hwaccel — short-lived, low-res.
        // Playback / HQ decoders (aspect <= 0) use hwaccel when available.
        let hw_device_ctx = if aspect <= 0.0 {
            let maybe_hw = try_create_d3d11va_device();
            if let Some(ref hw) = maybe_hw {
                unsafe {
                    let hw_ref = ffi::av_buffer_ref(hw.0);
                    if !hw_ref.is_null() {
                        (*dec_ctx.as_mut_ptr()).hw_device_ctx = hw_ref;
                        (*dec_ctx.as_mut_ptr()).get_format    = Some(get_format_d3d11va);
                        eprintln!("[hwaccel] D3D11VA attached to decoder context (pre-open)");
                    }
                }
            }
            maybe_hw
        } else {
            None
        };

        // Build decoder — width/height come from here, replacing the previous
        // unsafe raw-pointer read from stream.parameters().as_ptr().
        let decoder = dec_ctx.decoder().video()?;
        let raw_w = decoder.width().max(2);
        let raw_h = decoder.height().max(2);

        // Resolution strategy (in priority order):
        //   forced_size     → exact (w, h) override; used by decoder_b so it always
        //                     matches the primary decoder's output — prevents blend
        //                     size mismatches when two clips have different native res.
        //   aspect > 0      → scrub mode: fixed 320 px wide, native source AR.
        //                     Low-res on purpose — shown only during active scrub (L1/L2).
        //   aspect <= 0     → playback / HQ mode: native source dimensions, no downscale.
        let (out_w, out_h) = if let Some((fw, fh)) = forced_size {
            (fw, fh)
        } else if aspect > 0.0 {
            let w: u32 = 320;
            let h: u32 = ((w as f32 * raw_h as f32 / raw_w.max(1) as f32) as u32).max(2) & !1;
            (w, h)
        } else {
            (raw_w, raw_h)
        };

        let dec_fmt = decoder.format();
        let dec_w   = decoder.width();
        let dec_h   = decoder.height();

        // [Fix] Guard against AV_PIX_FMT_NONE (Annex-B H.264 in MP4 containers
        // never report a pixel format until the first packet is decoded) and zero
        // coded dimensions.  Without this guard, SwsContext::get returns Err, the
        // `?` propagates, open() fails, the pb thread sets decoder=None and goes
        // idle — but tick() never restarts it because playback_media_id was already
        // updated before the start_playback call.  Result: permanent playback freeze
        // when a clip with a different resolution/container follows the initial clips
        // (e.g. a 1080p 16:9 clip after 480p 2:3 clips on the same timeline).
        //
        // The placeholder format (YUV420P) is intentionally different from the
        // stored decoder_fmt (Pixel::None), so the first decoded frame always
        // triggers the `actual_fmt != self.decoder_fmt` mismatch check in
        // next_frame()/advance_to(), which rebuilds the SwsContext with the real
        // format and display dimensions before running the scaler.
        //
        // [Opt #1] Reuse cached SwsContext when source format/dimensions haven't
        // changed — avoids re-running lookup-table initialisation on every
        // backward scrub or cross-clip reset.  Out dimensions are fixed for the
        // lifetime of a session (same `aspect` is always passed), so a matching
        // source key is sufficient to guarantee safe reuse.
        let scaler = match cached_scaler {
            Some((sws, cf, cw, ch)) if cf == dec_fmt && cw == dec_w && ch == dec_h => sws,
            _ => {
                let sws_src_fmt = if dec_fmt == Pixel::None { Pixel::YUV420P } else { dec_fmt };
                let sws_src_w   = dec_w.max(2);
                let sws_src_h   = dec_h.max(2);
                SwsContext::get(sws_src_fmt, sws_src_w, sws_src_h, Pixel::RGBA, out_w, out_h, Flags::BILINEAR)?
            }
        };

        Ok(Self {
            path: path.clone(), ictx, decoder, video_idx,
            last_pts: seek_ts.saturating_sub(1), tb_num, tb_den, out_w, out_h, scaler,
            decoder_fmt: dec_fmt, decoder_w: dec_w, decoder_h: dec_h,
            skip_until_pts: 0,
            frame_buf: Vec::with_capacity(out_w as usize * out_h as usize * 4),
            hw_device_ctx,
        })
    }

    pub fn ts_to_pts(&self, t: f64) -> i64 {
        (t * self.tb_den as f64 / self.tb_num as f64) as i64
    }

    pub fn pts_to_secs(&self, pts: i64) -> f64 {
        pts as f64 * self.tb_num as f64 / self.tb_den as f64
    }

    /// Decode the next frame sequentially (no seek). Returns `(pixels, w, h, ts_secs)` or None at EOF.
    ///
    /// If `skip_until_pts` is set (non-zero), frames before that PTS are decoded
    /// but not scaled — decode-only is ~4x faster than decode+scale+alloc, so this
    /// burns through a GOP in ~50 ms instead of ~200 ms.  Once the threshold is
    /// reached the field is cleared and normal decode+scale resumes.
    ///
    /// [Opt 1] RGBA output is assembled via extend_from_slice (bulk memcpy per row)
    /// into a reused self.frame_buf, then cloned once for the return value.  This
    /// avoids the flat_map().collect() iterator overhead and the implicit Vec growth
    /// that could occur when the size_hint is imprecise.
    pub fn next_frame(&mut self) -> Option<(Vec<u8>, u32, u32, f64)> {
        // Max video packets to process per call while in skip mode.
        // Keeps each call bounded at ~30 ms so the pb thread stays responsive.
        // Caller detects "still burning" vs EOF by checking skip_until_pts > 0
        // after a None return.
        const MAX_SKIP_PACKETS: usize = 60;
        let mut skip_packets = 0usize;
        let tb_num = self.tb_num;
        let tb_den = self.tb_den;

        for (stream, packet) in self.ictx.packets().flatten() {
            if stream.index() != self.video_idx { continue; }
            if self.decoder.send_packet(&packet).is_err() { continue; }
            let mut decoded = ffmpeg::util::frame::video::Video::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(self.last_pts + 1);
                self.last_pts = pts;
                // Burn-through: skip scaler for pre-target frames after a seek.
                // Avoids decode+scale+alloc for every GOP frame we don't need.
                if self.skip_until_pts > 0 && pts < self.skip_until_pts {
                    continue;
                }
                self.skip_until_pts = 0; // reached target — disable skip
                let ts_secs = pts as f64 * tb_num as f64 / tb_den as f64;

                // Transfer GPU surface to CPU.  ensure_cpu_frame takes ownership of
                // `decoded` by value, so reinitialise the outer binding immediately
                // so receive_frame() gets a live &mut on the next loop iteration.
                let cpu = ensure_cpu_frame(decoded);
                decoded = ffmpeg::util::frame::video::Video::empty();

                // Primary path: center-crop source to canvas AR, scale to (out_w × out_h).
                // Handles NV12 and YUV420P/YUVJ420P.  Falls back to the cached stretch-
                // scaler for other formats.  If both fail, skip this frame — the while loop
                // naturally retries with the re-initialised `decoded` above; no freeze.
                let data_opt = unsafe { center_crop_and_scale(&cpu, self.out_w, self.out_h) }
                    .or_else(|| {
                        // Fallback: rebuild SwsContext if format/dims changed, then stretch.
                        let af = cpu.format();
                        let aw = cpu.width();
                        let ah = cpu.height();
                        if af != self.decoder_fmt
                            || aw != self.decoder_w
                            || ah != self.decoder_h
                        {
                            if let Ok(sws) = SwsContext::get(
                                af, aw, ah, Pixel::RGBA, self.out_w, self.out_h, Flags::BILINEAR,
                            ) {
                                self.scaler      = sws;
                                self.decoder_fmt = af;
                                self.decoder_w   = aw;
                                self.decoder_h   = ah;
                            }
                        }
                        let mut out = ffmpeg::util::frame::video::Video::empty();
                        self.scaler.run(&cpu, &mut out).ok()?;
                        Some(copy_frame_rgba(&mut self.frame_buf, &out, self.out_w, self.out_h))
                    });
                if let Some(data) = data_opt {
                    return Some((data, self.out_w, self.out_h, ts_secs));
                }
                // Both paths failed (e.g. GPU→CPU transfer error, scaler mismatch):
                // skip this frame; the while loop retries automatically.
            }
            // After each video packet in skip mode, check the chunk limit.
            // Return None with skip_until_pts still set so the caller knows
            // the burn is ongoing (not EOF) and can send a raw primary frame
            // this tick and call us again next iteration.
            if self.skip_until_pts > 0 {
                skip_packets += 1;
                if skip_packets >= MAX_SKIP_PACKETS {
                    return None;
                }
            }
        }
        None
    }

    /// [Opt 2] Read forward until we find a frame at or past `target_pts`.
    ///
    /// Pre-target frames are now decoded-only (no swscale, no alloc) — identical to
    /// the burn_to_pts fast-path.  The scaler runs exactly once on the frame that
    /// meets or exceeds target_pts.  For a 5 s GOP at 60 fps that's ~300 frames
    /// decoded without scaling instead of the original 300 × (decode + swscale + alloc).
    ///
    /// Behaviour change vs original: if the stream reaches EOF before target_pts is
    /// found, None is returned (the caller keeps the last displayed frame).  The
    /// original returned the last *scaled* frame on EOF, but that path triggered a
    /// scaler alloc for every pre-target frame.  The EOF case (scrub past clip end)
    /// is benign — the scrub thread's needs_reset logic handles re-open on the next
    /// request.
    pub fn advance_to(&mut self, target_pts: i64) -> Option<(Vec<u8>, u32, u32)> {
        for (stream, packet) in self.ictx.packets().flatten() {
            if stream.index() != self.video_idx { continue; }
            if self.decoder.send_packet(&packet).is_err() { continue; }
            let mut decoded = ffmpeg::util::frame::video::Video::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(self.last_pts + 1);
                self.last_pts = pts;
                // [Opt 2] Decode-only for all frames before the target PTS.
                // ~4x faster than decode+scale+alloc for the same set of frames.
                if pts < target_pts { continue; }
                // Target reached — center-crop source to canvas AR and scale.
                // Same pattern as next_frame: reinitialise `decoded` after the move
                // so the while loop can retry on failure without a borrow-after-move.
                let cpu = ensure_cpu_frame(decoded);
                decoded = ffmpeg::util::frame::video::Video::empty();

                let data_opt = unsafe { center_crop_and_scale(&cpu, self.out_w, self.out_h) }
                    .or_else(|| {
                        let af = cpu.format();
                        let aw = cpu.width();
                        let ah = cpu.height();
                        if af != self.decoder_fmt
                            || aw != self.decoder_w
                            || ah != self.decoder_h
                        {
                            if let Ok(sws) = SwsContext::get(
                                af, aw, ah, Pixel::RGBA, self.out_w, self.out_h, Flags::BILINEAR,
                            ) {
                                self.scaler      = sws;
                                self.decoder_fmt = af;
                                self.decoder_w   = aw;
                                self.decoder_h   = ah;
                            }
                        }
                        let mut out = ffmpeg::util::frame::video::Video::empty();
                        self.scaler.run(&cpu, &mut out).ok()?;
                        Some(copy_frame_rgba(&mut self.frame_buf, &out, self.out_w, self.out_h))
                    });
                if let Some(data) = data_opt {
                    return Some((data, self.out_w, self.out_h));
                }
                // Both paths failed: skip this frame, try the next.
            }
        }
        // EOF before target_pts: return None. Caller retains its current frame.
        None
    }

    /// Decode and discard frames without scaling until `last_pts >= target_pts`.
    ///
    /// This is the fast seek used by the playback thread after a keyframe-aligned
    /// open().  Skipping the scaler+alloc makes it ~4-8x faster than advance_to().
    ///
    /// Runs synchronously — call this in the Start handler BEFORE entering the
    /// frame-send loop so the first frame the thread sends is at the correct
    /// position.  The caller is blocked for the duration but the channel is empty
    /// at this point, so the UI simply shows held_frame until playback begins.
    pub fn burn_to_pts(&mut self, target_pts: i64) {
        if target_pts <= 0 || target_pts <= self.last_pts { return; }
        'outer: for (stream, packet) in self.ictx.packets().flatten() {
            if stream.index() != self.video_idx { continue; }
            if self.decoder.send_packet(&packet).is_err() { continue; }
            let mut decoded = ffmpeg::util::frame::video::Video::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(self.last_pts + 1);
                self.last_pts = pts;
                if pts >= target_pts { break 'outer; }
            }
        }
    }
}

// ── Frame copy helper ─────────────────────────────────────────────────────────

/// [Opt 1] Copy an RGBA-format ffmpeg VideoFrame into `buf`, stripping stride
/// padding, and return a clone of the filled buffer.
///
/// `buf` is reused across calls — it is only reallocated when `out_w * out_h * 4`
/// exceeds its current capacity (i.e. never in steady state for a fixed-resolution
/// source).  The returned Vec is a single allocation of the exact frame size.
#[inline]
fn copy_frame_rgba(
    buf:   &mut Vec<u8>,
    frame: &ffmpeg::util::frame::video::Video,
    out_w: u32,
    out_h: u32,
) -> Vec<u8> {
    let stride    = frame.stride(0);
    let raw       = frame.data(0);
    let row_bytes = out_w as usize * 4;
    let needed    = row_bytes * out_h as usize;

    buf.clear();
    // Reserve only triggers a realloc when dimensions change (e.g. first frame,
    // or aspect ratio change mid-session).
    if buf.capacity() < needed {
        buf.reserve(needed);
    }
    for row in 0..out_h as usize {
        let s = row * stride;
        buf.extend_from_slice(&raw[s..s + row_bytes]);
    }
    buf.clone()
}

// ── One-shot frame decode (preview + PNG save) ────────────────────────────────

pub fn decode_frame(
    path:         &PathBuf,
    id:           Uuid,
    timestamp:    f64,
    aspect:       f32,               // 0.0 = use native resolution (or preview_size if set)
    save_png:     bool,              // true = write PNG to dest, false = send VideoFrame
    dest:         Option<PathBuf>,
    tx:           &Sender<MediaResult>,
    preview_size: Option<(u32, u32)>, // UI panel dims — decode at this size when aspect<=0; ignored for PNG
) -> Result<()> {
    let mut ictx = input(path)?;

    // Build codec context in the same block as the stream borrow.
    // Context::from_parameters copies the parameters, so dec_ctx is fully
    // owned after the block — ictx is free for seeking without a second open.
    let (video_stream_idx, seek_ts, tb_num, tb_den, dec_ctx) = {
        let stream = ictx.streams().best(Type::Video)
            .ok_or_else(|| anyhow::anyhow!("no video stream"))?;
        let idx = stream.index();
        let tb  = stream.time_base();
        let ts  = (timestamp * tb.denominator() as f64 / tb.numerator() as f64) as i64;
        let dec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
        (idx, ts, tb.numerator() as f64, tb.denominator() as f64, dec_ctx)
    };

    // Discard non-video streams — one-shot decode, no need to demux audio.
    // StreamMut has no set_discard() setter in this fork; write AVStream.discard directly.
    for mut stream in ictx.streams_mut() {
        if stream.index() != video_stream_idx {
            unsafe { (*stream.as_mut_ptr()).discard = ffi::AVDiscard::AVDISCARD_ALL; }
        }
    }

    // [Fix] Guard seek at ts=0: seeking to position 0 returns EPERM on Windows
    // (the OS rejects a SetFilePointer call FFmpeg makes on the demux path).
    // decode_one_frame_rgba applies the same guard; decode_frame must too.
    if seek_ts > 0 {
        let _ = ictx.seek(seek_ts, ..=seek_ts);
    }
    let mut decoder = dec_ctx.decoder().video()?;

    let out_fmt = if save_png { Pixel::RGB24 } else { Pixel::RGBA };

    // [Fix] Build the SwsContext lazily on the first decoded frame rather than
    // eagerly from decoder metadata.  Two reasons:
    //   1. Annex-B H.264: decoder.format() == AV_PIX_FMT_NONE before the first
    //      packet, so SwsContext::get fails immediately and the function returns
    //      Err — no frame is ever decoded.
    //   2. AVCC H.264: decoder.width()/height() are coded dimensions (e.g. 1088)
    //      rather than display dimensions (e.g. 1080), producing a subtly wrong
    //      source rect in the scaler and distorted output.
    // Both issues are fixed identically to decode_one_frame_rgba and probe.rs.
    //
    // out_w / out_h are also deferred to the first frame for the same reason:
    // decoder.width()/height() before packets can be coded dims.
    let mut scaler: Option<SwsContext> = None;
    let mut out_w: u32 = 0;
    let mut out_h: u32 = 0;

    // last_good holds the most-recently scaled frame in case we hit EOF before
    // reaching seek_ts (e.g. requesting the final frame of a clip).
    let mut last_good: Option<ffmpeg::util::frame::video::Video> = None;

    for (stream, packet) in ictx.packets().flatten() {
        if stream.index() != video_stream_idx { continue; }
        decoder.send_packet(&packet)?;
        let mut decoded = ffmpeg::util::frame::video::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            // Initialise scaler and output dimensions from the first decoded frame.
            // Doing this here (lazily) rather than from decoder metadata avoids:
            //   • AV_PIX_FMT_NONE for Annex-B streams (format unknown pre-packet)
            //   • Coded-dimension mismatch for AVCC streams (1088 vs 1080)
            if scaler.is_none() {
                let frame_w = decoded.width();
                let frame_h = decoded.height();
                let (w, h) = if save_png {
                    // PNG export: always native resolution regardless of preview_size.
                    (frame_w, frame_h)
                } else if aspect > 0.0 {
                    // Scrub mode: fixed 640px wide, source AR height.
                    let w: u32 = 640;
                    let h: u32 = ((w as f32 / aspect.max(0.01)) as u32).max(2) & !1;
                    (w, h)
                } else if let Some((pw, ph)) = preview_size {
                    // HQ scrub (L3 idle): decode at player panel dimensions.
                    // Avoids decoding 4K only to display at e.g. 960×540.
                    // Same resolution the playback path uses (preview_size in LiveDecoder::open).
                    (pw, ph)
                } else {
                    // No panel size known yet — fall back to native source resolution.
                    (frame_w, frame_h)
                };
                out_w = w;
                out_h = h;
                match SwsContext::get(
                    decoded.format(), frame_w, frame_h,
                    out_fmt, out_w, out_h, Flags::BILINEAR,
                ) {
                    Ok(s)  => { scaler = Some(s); }
                    Err(e) => { return Err(anyhow::anyhow!("SwsContext::get failed: {e}")); }
                }
            }
            let mut out_frame = ffmpeg::util::frame::video::Video::empty();
            scaler.as_mut().unwrap().run(&decoded, &mut out_frame)?;
            last_good = Some(out_frame.clone());
            // Skip frames that landed before our target due to keyframe-aligned seek.
            // Compare in seconds — pts+2 in raw units is timebase-dependent (22µs at
            // 1/90000 but 80ms at 1/25), which would incorrectly skip real frames.
            if let Some(pts) = decoded.pts() {
                let pts_secs = pts as f64 * tb_num / tb_den;
                if pts_secs < timestamp - (1.0 / 60.0) { continue; }
            }
            emit_frame(&out_frame, id, out_w, out_h, save_png, &dest, tx)?;
            return Ok(());
        }
    }

    // EOF reached without hitting seek_ts — emit the last frame we saw.
    if let Some(out_frame) = last_good {
        if out_w == 0 { return Err(anyhow::anyhow!("no frame found at t={timestamp:.3}")); }
        emit_frame(&out_frame, id, out_w, out_h, save_png, &dest, tx)?;
        return Ok(());
    }

    Err(anyhow::anyhow!("no frame found at t={timestamp:.3}"))
}

/// Emit a decoded frame: either write a PNG to disk or send a VideoFrame result.
fn emit_frame(
    out_frame: &ffmpeg::util::frame::video::Video,
    id:        Uuid,
    out_w:     u32,
    out_h:     u32,
    save_png:  bool,
    dest:      &Option<PathBuf>,
    tx:        &Sender<MediaResult>,
) -> Result<()> {
    let stride = out_frame.stride(0);
    let raw    = out_frame.data(0);

    if save_png {
        use std::io::BufWriter;
        let dest_path = dest.clone()
            .ok_or_else(|| anyhow::anyhow!("no dest path for PNG save"))?;
        let file = std::fs::File::create(&dest_path)?;
        let w    = &mut BufWriter::new(file);
        let mut encoder = png::Encoder::new(w, out_w, out_h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        let row_bytes = out_w as usize * 3;
        let rows: Vec<&[u8]> = (0..out_h as usize)
            .map(|row| &raw[row * stride..row * stride + row_bytes])
            .collect();
        writer.write_image_data(&rows.concat())?;
        eprintln!("[media] PNG saved → {}", dest_path.display());
        let _ = tx.send(MediaResult::FrameSaved { path: dest_path });
    } else {
        let data: Vec<u8> = (0..out_h as usize)
            .flat_map(|row| {
                let start = row * stride;
                &raw[start..start + out_w as usize * 4]
            })
            .copied()
            .collect();
        let _ = tx.send(MediaResult::VideoFrame { id, width: out_w, height: out_h, data });
    }
    Ok(())
}

// ── One-shot RGBA frame decode for transition blending ────────────────────────

/// Decode a single RGBA frame at `ts` seconds from `path` and return the raw
/// pixel data together with the output dimensions.
///
/// `aspect` follows the same convention as `LiveDecoder::open`:
/// - `aspect > 0`  → scrub mode: output width fixed at 320 px, height from source AR.
/// - `aspect <= 0` → HQ / L3-idle mode: native source resolution, no downscale.
///
/// Other invariants:
/// - SwsContext is built lazily from the first decoded frame (same invariant
///   as probe.rs — avoids AVCC coded-dimension / Annex-B format issues).
/// - Seeks are skipped when `ts <= 0.0` (Windows EPERM guard, matching
///   `helpers::seek::seek_to_secs`).
/// - Falls back to the last decoded frame on EOF (same as `decode_frame`).
///
/// Used by `MediaWorker::request_transition_frame` (320 px scrub) and
/// `MediaWorker::request_transition_frame_hq` (native res, L3 idle).
pub fn decode_one_frame_rgba(path: &PathBuf, ts: f64, aspect: f32) -> Result<(Vec<u8>, u32, u32)> {
    let mut ictx = input(path)?;

    let (video_idx, seek_ts, tb_num, tb_den, dec_ctx) = {
        let stream = ictx.streams().best(Type::Video)
            .ok_or_else(|| anyhow::anyhow!("no video stream"))?;
        let idx = stream.index();
        let tb  = stream.time_base();
        let ts_raw = (ts * tb.denominator() as f64 / tb.numerator() as f64) as i64;
        let dec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
        (idx, ts_raw, tb.numerator() as f64, tb.denominator() as f64, dec_ctx)
    };

    // Discard non-video streams — one-shot decode, no audio needed.
    // StreamMut has no set_discard() setter in this fork; write AVStream.discard directly.
    for mut stream in ictx.streams_mut() {
        if stream.index() != video_idx {
            unsafe { (*stream.as_mut_ptr()).discard = ffi::AVDiscard::AVDISCARD_ALL; }
        }
    }

    // Honour the seek_to_secs invariant: skip seek at ts=0 (Windows EPERM).
    if seek_ts > 0 {
        let _ = ictx.seek(seek_ts, ..=seek_ts);
    }

    let mut decoder = dec_ctx.decoder().video()?;
    let raw_w = decoder.width().max(2);
    let raw_h = decoder.height().max(2);
    // aspect > 0: 320-px scrub (fast, low-res).  aspect <= 0: native source
    // dimensions (HQ, used by L3-idle transition blend).
    let (out_w, out_h): (u32, u32) = if aspect > 0.0 {
        let w: u32 = 320;
        let h: u32 = ((w as f32 * raw_h as f32 / raw_w.max(1) as f32) as u32).max(2) & !1;
        (w, h)
    } else {
        (raw_w, raw_h)
    };

    // Lazy SwsContext — built from first decoded frame, not from decoder metadata.
    // Matches probe.rs: AVCC codecs report coded dims (e.g. 1088 ≠ 1080) before
    // the first packet; Annex-B has AV_PIX_FMT_NONE until then.
    let mut scaler: Option<SwsContext> = None;
    // [Fix 2] last_good uses move semantics — only one Vec<u8> lives at a time.
    // Previously buf.clone() was stored in last_good AND returned, meaning two
    // full-frame allocations existed simultaneously for every pre-target frame.
    let mut last_good: Option<Vec<u8>> = None;

    for (stream, packet) in ictx.packets().flatten() {
        if stream.index() != video_idx { continue; }
        if decoder.send_packet(&packet).is_err() { continue; }
        let mut decoded = ffmpeg::util::frame::video::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if scaler.is_none() {
                scaler = Some(SwsContext::get(
                    decoded.format(), decoded.width(), decoded.height(),
                    Pixel::RGBA, out_w, out_h, Flags::BILINEAR,
                )?);
            }
            let mut out_frame = ffmpeg::util::frame::video::Video::empty();
            scaler.as_mut().unwrap().run(&decoded, &mut out_frame)?;

            // Strip stride padding into a dense buffer.
            let stride    = out_frame.stride(0);
            let raw       = out_frame.data(0);
            let row_bytes = out_w as usize * 4;
            let mut buf   = Vec::with_capacity(row_bytes * out_h as usize);
            for row in 0..out_h as usize {
                buf.extend_from_slice(&raw[row * stride..row * stride + row_bytes]);
            }

            // Check PTS before deciding whether to return or continue scanning.
            // Store buf into last_good via move (no clone), then return a clone
            // only for the hit case — one allocation total on the happy path.
            let pts_ok = decoded.pts().map_or(true, |pts| {
                pts as f64 * tb_num / tb_den >= ts - (1.0 / 60.0)
            });

            last_good = Some(buf);

            if pts_ok {
                // Move out of last_good — no extra clone needed.
                return Ok((last_good.take().unwrap(), out_w, out_h));
            }
        }
    }

    // EOF before target — return the last frame we scaled (matches decode_frame behaviour).
    if let Some(buf) = last_good {
        return Ok((buf, out_w, out_h));
    }
    Err(anyhow::anyhow!("no frame found at t={ts:.3}"))
}
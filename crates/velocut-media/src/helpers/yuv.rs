// crates/velocut-media/src/helpers/yuv.rs
//
// YUV420P frame utilities shared across the encode pipeline.
//
// These were previously inlined as nested functions inside decode_clip_frames
// in encode.rs. Extracted here so they can be:
//   - Reused by future effects/transition helpers without touching encode.rs
//   - Unit tested independently
//   - Used by the concurrent crossfade decode path (Section 7)
//
// Layout convention for packed YUV420P byte vecs:
//   [0 .. w*h]              — Y plane, packed (no stride)
//   [w*h .. w*h + uv_w*uv_h]        — U plane, packed
//   [w*h + uv_w*uv_h .. end]         — V plane, packed
//
// "Packed" means strides are removed — each row is exactly w (or uv_w) bytes.
// ffmpeg VideoFrame rows may have padding; extract_yuv strips it.

use ffmpeg_the_third::util::frame::video::Video as VideoFrame;

/// Extract packed (stride-free) YUV420P bytes from a scaled VideoFrame.
///
/// The frame must already be in `Pixel::YUV420P` format — call swscale first.
/// Returns a single Vec laid out as Y ++ U ++ V (see module doc for offsets).
pub fn extract_yuv(yuv: &VideoFrame, w: usize, h: usize, uv_w: usize, uv_h: usize) -> Vec<u8> {
    let mut raw = vec![0u8; w * h + uv_w * uv_h * 2];

    // Y plane
    let y_stride = yuv.stride(0);
    let y_src    = yuv.data(0);
    for row in 0..h {
        raw[row * w .. row * w + w]
            .copy_from_slice(&y_src[row * y_stride .. row * y_stride + w]);
    }

    // U plane
    let u_offset = w * h;
    let u_stride = yuv.stride(1);
    let u_src    = yuv.data(1);
    for row in 0..uv_h {
        let dst = u_offset + row * uv_w;
        raw[dst .. dst + uv_w]
            .copy_from_slice(&u_src[row * u_stride .. row * u_stride + uv_w]);
    }

    // V plane
    let v_offset = u_offset + uv_w * uv_h;
    let v_stride = yuv.stride(2);
    let v_src    = yuv.data(2);
    for row in 0..uv_h {
        let dst = v_offset + row * uv_w;
        raw[dst .. dst + uv_w]
            .copy_from_slice(&v_src[row * v_stride .. row * v_stride + uv_w]);
    }

    raw
}

/// Linear blend of two packed YUV420P frames.
///
/// `alpha` = 0.0 → 100% frame_a (outgoing clip)
/// `alpha` = 1.0 → 100% frame_b (incoming clip)
///
/// Blend is performed in gamma-encoded byte space — a linear approximation
/// that is visually correct for typical dissolves on SDR content.
/// For HDR or wide-gamut content a linear-light blend would be more accurate,
/// but that requires float intermediate storage and is out of scope here.
///
/// Both slices must have the same length (same frame dimensions).
/// Panics in debug builds if lengths differ; silently clips in release.
pub fn blend_yuv_frame(frame_a: &[u8], frame_b: &[u8], alpha: f32) -> Vec<u8> {
    debug_assert_eq!(frame_a.len(), frame_b.len(),
        "blend_yuv_frame: frame size mismatch — both clips must be scaled to the same output dimensions");

    let inv = 1.0 - alpha;
    frame_a.iter()
        .zip(frame_b.iter())
        .map(|(&a, &b)| (inv * a as f32 + alpha * b as f32).round() as u8)
        .collect()
}

/// Write a packed YUV420P buffer back into a VideoFrame's planes, respecting stride.
///
/// The inverse of `extract_yuv` — used when the blended frame needs to be sent
/// to the encoder (which expects a strided VideoFrame, not a packed buffer).
pub fn write_yuv(packed: &[u8], yuv: &mut VideoFrame, w: usize, h: usize, uv_w: usize, uv_h: usize) {
    // Y plane
    let y_stride = yuv.stride(0);
    let y_dst    = yuv.data_mut(0);
    for row in 0..h {
        y_dst[row * y_stride .. row * y_stride + w]
            .copy_from_slice(&packed[row * w .. row * w + w]);
    }

    // U plane
    let u_offset = w * h;
    let u_stride = yuv.stride(1);
    let u_dst    = yuv.data_mut(1);
    for row in 0..uv_h {
        let src = u_offset + row * uv_w;
        u_dst[row * u_stride .. row * u_stride + uv_w]
            .copy_from_slice(&packed[src .. src + uv_w]);
    }

    // V plane
    let v_offset = u_offset + uv_w * uv_h;
    let v_stride = yuv.stride(2);
    let v_dst    = yuv.data_mut(2);
    for row in 0..uv_h {
        let src = v_offset + row * uv_w;
        v_dst[row * v_stride .. row * v_stride + uv_w]
            .copy_from_slice(&packed[src .. src + uv_w]);
    }
}
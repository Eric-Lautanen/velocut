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
//   [0 .. w*h]                    — Y plane, packed (no stride)
//   [w*h .. w*h + (w/2)*(h/2)]   — U plane, packed
//   [w*h + (w/2)*(h/2) .. end]   — V plane, packed
//
// "Packed" means strides are removed — each row is exactly w (or w/2) bytes.
// ffmpeg VideoFrame rows may have padding; extract_yuv strips it.

use ffmpeg_the_third::util::frame::video::Video as VideoFrame;

/// Extract packed (stride-free) YUV420P bytes from a scaled VideoFrame.
///
/// The frame must already be in `Pixel::YUV420P` format — call swscale first.
/// Returns a single Vec laid out as Y ++ U ++ V (see module doc for offsets).
///
/// Chroma dimensions are computed as `w/2` and `h/2` (YUV420P spec).
pub fn extract_yuv(yuv: &VideoFrame, w: usize, h: usize) -> Vec<u8> {
    let uv_w = w / 2;
    let uv_h = h / 2;
    let mut raw = vec![0u8; w * h + uv_w * uv_h * 2];

    // Y plane
    let y_stride = yuv.stride(0);
    let y_src = yuv.data(0);
    for row in 0..h {
        raw[row * w..row * w + w].copy_from_slice(&y_src[row * y_stride..row * y_stride + w]);
    }

    // U plane
    let u_offset = w * h;
    let u_stride = yuv.stride(1);
    let u_src = yuv.data(1);
    for row in 0..uv_h {
        let dst = u_offset + row * uv_w;
        raw[dst..dst + uv_w].copy_from_slice(&u_src[row * u_stride..row * u_stride + uv_w]);
    }

    // V plane
    let v_offset = u_offset + uv_w * uv_h;
    let v_stride = yuv.stride(2);
    let v_src = yuv.data(2);
    for row in 0..uv_h {
        let dst = v_offset + row * uv_w;
        raw[dst..dst + uv_w].copy_from_slice(&v_src[row * v_stride..row * v_stride + uv_w]);
    }

    raw
}

/// Write a packed YUV420P buffer back into a VideoFrame's planes, respecting stride.
///
/// The inverse of `extract_yuv` — used when the blended frame needs to be sent
/// to the encoder (which expects a strided VideoFrame, not a packed buffer).
///
/// Chroma dimensions are computed as `w/2` and `h/2` (YUV420P spec).
pub fn write_yuv(packed: &[u8], yuv: &mut VideoFrame, w: usize, h: usize) {
    let uv_w = w / 2;
    let uv_h = h / 2;

    // Y plane
    let y_stride = yuv.stride(0);
    let y_dst = yuv.data_mut(0);
    for row in 0..h {
        y_dst[row * y_stride..row * y_stride + w].copy_from_slice(&packed[row * w..row * w + w]);
    }

    // U plane
    let u_offset = w * h;
    let u_stride = yuv.stride(1);
    let u_dst = yuv.data_mut(1);
    for row in 0..uv_h {
        let src = u_offset + row * uv_w;
        u_dst[row * u_stride..row * u_stride + uv_w].copy_from_slice(&packed[src..src + uv_w]);
    }

    // V plane
    let v_offset = u_offset + uv_w * uv_h;
    let v_stride = yuv.stride(2);
    let v_dst = yuv.data_mut(2);
    for row in 0..uv_h {
        let src = v_offset + row * uv_w;
        v_dst[row * v_stride..row * v_stride + uv_w].copy_from_slice(&packed[src..src + uv_w]);
    }
}

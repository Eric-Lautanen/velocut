// crates/velocut-media/src/worker/blend.rs
//
// RGBA transition blending and crop utilities for the worker.
// Extracted from worker.rs to keep the file size manageable.
//
// These functions operate on raw RGBA byte slices — no FFmpeg types cross
// this boundary (all FFmpeg work happens in decode.rs and encode.rs).

use std::path::PathBuf;

use velocut_core::transitions::{registry, TransitionKind};

use crate::decode::LiveDecoder;

// ── ActiveBlend ───────────────────────────────────────────────────────────────

pub(super) struct ActiveBlend {
    pub spec: velocut_core::media_types::PlaybackTransitionSpec,
    pub aspect: f32,
    pub decoder_b: Option<LiveDecoder>,
}

// ── Transition scrub frame decode ─────────────────────────────────────────────

/// Decode a single frame for the transition scrub thread, reusing an existing
/// LiveDecoder when the same file is being scrubbed forward within 2 seconds.
///
/// The `live` slot holds `(path, decoder)`.  On a hit (same path, small forward
/// delta), the decoder advances in-place — no re-open overhead.  On a miss the
/// decoder is re-opened with a keyframe-aligned seek, and the old SwsContext is
/// recycled.
pub(super) fn decode_transition_scrub_frame(
    live: &mut Option<(PathBuf, LiveDecoder)>,
    path: &PathBuf,
    ts: f64,
) -> Option<(Vec<u8>, u32, u32)> {
    let needs_open = live.as_ref().map(|(p, _)| p != path).unwrap_or(true);

    if needs_open {
        let cached_sws = live
            .take()
            .map(|(_, d)| (d.scaler, d.decoder_fmt, d.decoder_w, d.decoder_h));
        match LiveDecoder::open(path, ts, 1.0, cached_sws, None) {
            Ok(mut d) => {
                let target_pts = d.ts_to_pts(ts);
                let result = d.advance_to(target_pts);
                *live = Some((path.clone(), d));
                result
            }
            Err(e) => {
                crate::media_log!("[transition_scrub] LiveDecoder::open: {e}");
                None
            }
        }
    } else if let Some((_, d)) = live.as_mut() {
        let tpts = d.ts_to_pts(ts);
        let needs_seek = tpts < d.last_pts || tpts > d.last_pts + d.ts_to_pts(2.0);
        if needs_seek {
            if let Err(e) = d.seek_to(ts) {
                crate::media_log!("[transition_scrub] seek_to failed: {e}");
                return None;
            }
        }
        d.advance_to(tpts)
    } else {
        None
    }
}

// ── RGBA crop ─────────────────────────────────────────────────────────────────

/// Center-crop and optionally scale an RGBA frame from `src_w×src_h` to `dst_w×dst_h`.
///
/// Uses bilinear interpolation in the general (scaling) path for smooth results
/// when transitioning between mixed-resolution clips. A fast-path avoids scaling
/// when the cropped region already matches the destination dimensions.
pub(super) fn crop_rgba(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let src_ar = src_w as f32 / src_h.max(1) as f32;
    let dst_ar = dst_w as f32 / dst_h.max(1) as f32;

    let (off_x, off_y, used_w, used_h) = if src_ar > dst_ar {
        let used_w = (src_h as f32 * dst_ar) as u32;
        let off_x = (src_w - used_w) / 2;
        (off_x, 0u32, used_w, src_h)
    } else {
        let used_h = (src_w as f32 / dst_ar) as u32;
        let off_y = (src_h - used_h) / 2;
        (0u32, off_y, src_w, used_h)
    };

    let dst_pitch = (dst_w * 4) as usize;
    let src_pitch = (src_w * 4) as usize;
    let mut out = vec![0u8; (dst_w * dst_h * 4) as usize];

    // Fast path: no scaling, just offset crop (most common mixed-res case).
    if used_w == dst_w && used_h == dst_h {
        for dy in 0..dst_h {
            let src_y = off_y + dy;
            let src_start = (src_y as usize * src_w as usize + off_x as usize) * 4;
            let dst_start = dy as usize * dst_pitch;
            out[dst_start..dst_start + dst_pitch]
                .copy_from_slice(&src[src_start..src_start + dst_pitch]);
        }
        return out;
    }

    let sx = used_w as f32 / dst_w.max(1) as f32;
    let sy = used_h as f32 / dst_h.max(1) as f32;

    // General path: row-by-row bilinear interpolation for smooth downscaling.
    for dy in 0..dst_h {
        let src_y_f = off_y as f32 + dy as f32 * sy;
        let src_y0 = (src_y_f as u32).clamp(0, src_h.saturating_sub(1));
        let src_y1 = (src_y0 + 1).min(src_h.saturating_sub(1));
        let y_frac = src_y_f - src_y_f.floor();

        let row0_offset = src_y0 as usize * src_pitch;
        let row1_offset = src_y1 as usize * src_pitch;
        let row0 = &src[row0_offset..row0_offset + src_pitch];
        let row1 = &src[row1_offset..row1_offset + src_pitch];
        let dst_row = &mut out[dy as usize * dst_pitch..][..dst_pitch];

        for dx in 0..dst_w {
            let src_x_f = off_x as f32 + dx as f32 * sx;
            let src_x0 = (src_x_f as u32).clamp(0, src_w.saturating_sub(1));
            let src_x1 = (src_x0 + 1).min(src_w.saturating_sub(1));
            let x_frac = src_x_f - src_x_f.floor();

            let di = dx as usize * 4;
            for c in 0..4 {
                let p00 = row0[src_x0 as usize * 4 + c] as f32;
                let p01 = row1[src_x0 as usize * 4 + c] as f32;
                let p10 = row0[src_x1 as usize * 4 + c] as f32;
                let p11 = row1[src_x1 as usize * 4 + c] as f32;
                let v = p00 * (1.0 - x_frac) * (1.0 - y_frac)
                    + p10 * x_frac * (1.0 - y_frac)
                    + p01 * (1.0 - x_frac) * y_frac
                    + p11 * x_frac * y_frac;
                dst_row[di + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

// ── RGBA transition blending ──────────────────────────────────────────────────

/// Blend two RGBA frames according to the registered transition algorithm.
///
/// `alpha = 0.0` → 100% `a`, `alpha = 1.0` → 100% `b`.
/// For `TransitionKind::Cut`, returns `a` unchanged (no blend).
pub(super) fn blend_rgba_transition(
    a: &[u8],
    b: &[u8],
    w: u32,
    h: u32,
    alpha: f32,
    kind: TransitionKind,
) -> Vec<u8> {
    if kind == TransitionKind::Cut {
        return a.to_vec();
    }

    let reg = registry();

    reg.get(&kind)
        .expect(
            "blend_rgba_transition: unregistered TransitionKind — add it to declare_transitions!",
        )
        .apply_rgba(a, b, w, h, alpha)
}

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
        // Seek within the existing decoder instead of reopening when:
        //   a) backward movement - advance_to can only go forward
        //   b) large forward jump > 2 s - avoid decoding hundreds of frames
        let needs_seek = tpts < d.last_pts || tpts > d.last_pts + d.ts_to_pts(2.0);
        if needs_seek {
            if let Err(e) = d.seek_to(ts) {
                crate::media_log!("[transition_scrub] seek_to failed: {e}");
                return None;
            }
            // After a backward seek, use next_frame to get the frame at the target.
            // advance_to may fail to find the target if the decoder needs to sync.
            let result = d.next_frame().map(|(data, w, h, _ts)| (data, w, h));
            return result;
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
    let mut out = Vec::new();
    blend_rgba_transition_into(a, b, w, h, alpha, kind, &mut out);
    out
}

/// Same blend, into a caller-owned buffer.
///
/// `out` is resized to the frame size and fully overwritten. During playback
/// this is the difference between faulting ~1200 fresh pages per displayed
/// frame and writing into memory that is already mapped: a 5 MB frame buffer
/// that is allocated and freed every frame is handed back by the allocator to
/// the OS, so every write pays a page fault again.
pub(super) fn blend_rgba_transition_into(
    a: &[u8],
    b: &[u8],
    w: u32,
    h: u32,
    alpha: f32,
    kind: TransitionKind,
    out: &mut Vec<u8>,
) {
    if kind == TransitionKind::Cut {
        out.clear();
        out.extend_from_slice(a);
        return;
    }

    let reg = registry();

    reg.get(&kind)
        .expect(
            "blend_rgba_transition: unregistered TransitionKind — add it to declare_transitions!",
        )
        .apply_rgba_into(a, b, w, h, alpha, out)
}

/// Rotate a freshly blended frame into `held` and return the copy that goes out
/// on the channel.
///
/// The playback thread keeps two buffers alive: `scratch` (where the next blend
/// is written) and `held` (the last blended frame, re-sent when decoder_b is
/// still burning). Swapping them costs two pointer moves, so the blend always
/// writes into already-faulted pages and the only allocation per frame is the
/// copy the channel takes ownership of.
///
/// `send_into` is the buffer the outgoing copy goes into — normally one the UI
/// has finished with and handed back, which is what keeps the copy from faulting
/// fresh pages. An empty `Vec` is fine and simply allocates.
///
/// Returns the frame to send. On the first blend, or after a resolution change,
/// `scratch` is left empty and the next blend re-allocates it once.
pub(super) fn rotate_blended_frame(
    scratch: &mut Vec<u8>,
    held: &mut Option<Vec<u8>>,
    send_into: Vec<u8>,
) -> Vec<u8> {
    let mut previous = Some(std::mem::take(scratch));
    std::mem::swap(held, &mut previous);
    if let Some(recycled) = previous {
        *scratch = recycled;
    }

    let frame = held
        .as_ref()
        .expect("rotate_blended_frame called with nothing blended");
    let mut out = send_into;
    out.clear();
    out.extend_from_slice(frame);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for the blend: leave `scratch` holding a frame of `tag` bytes.
    fn fill(scratch: &mut Vec<u8>, tag: u8, len: usize) {
        scratch.clear();
        scratch.resize(len, tag);
    }

    #[test]
    fn rotation_keeps_the_sent_frame_and_recycles_the_previous_one() {
        const LEN: usize = 64;
        let len = LEN;
        let mut scratch: Vec<u8> = Vec::new();
        let mut held: Option<Vec<u8>> = None;

        // Frame 1: nothing held yet, so there is no buffer to recycle.
        fill(&mut scratch, 1, len);
        let sent = rotate_blended_frame(&mut scratch, &mut held, Vec::new());
        assert_eq!(sent, vec![1u8; len]);
        assert_eq!(held.as_deref(), Some(&[1u8; LEN][..]));
        assert!(
            scratch.is_empty(),
            "scratch should be empty after the first frame"
        );

        // Frame 2: the sent copy is the only positive; scratch still cold.
        fill(&mut scratch, 2, len);
        let sent = rotate_blended_frame(&mut scratch, &mut held, Vec::new());
        assert_eq!(sent, vec![2u8; len]);
        assert_eq!(held.as_deref(), Some(&[2u8; LEN][..]));
        assert_eq!(
            scratch.len(),
            len,
            "old held frame becomes the next scratch"
        );
        assert_eq!(scratch, vec![1u8; len]);

        // Frame 3: steady state — the scratch is the previous held buffer, so the
        // blend writes into memory that is already faulted in.
        let capacity = scratch.capacity();
        scratch.clear();
        scratch.resize(len, 3);
        assert_eq!(scratch.capacity(), capacity, "scratch must not reallocate");
        let sent = rotate_blended_frame(&mut scratch, &mut held, Vec::new());
        assert_eq!(sent, vec![3u8; len]);
        assert_eq!(held.as_deref(), Some(&[3u8; LEN][..]));
        assert_eq!(scratch, vec![2u8; len]);
    }

    #[test]
    fn resolution_change_does_not_mix_frame_sizes() {
        let mut scratch: Vec<u8> = Vec::new();
        let mut held: Option<Vec<u8>> = None;

        fill(&mut scratch, 1, 64);
        let _ = rotate_blended_frame(&mut scratch, &mut held, Vec::new());

        // New resolution: the held frame is a different size, so whoever reads it
        // (the held/burn path) must not treat it as a frame at the new size, and
        // the recycled scratch is resized by the next blend rather than trusted.
        fill(&mut scratch, 2, 256);
        let sent = rotate_blended_frame(&mut scratch, &mut held, Vec::new());
        assert_eq!(sent.len(), 256);
        assert_eq!(held.as_ref().map(|h| h.len()), Some(256));
        assert_eq!(scratch.len(), 64, "recycled buffer keeps its old size");
    }

    #[test]
    fn the_sent_copy_reuses_a_returned_buffer_instead_of_allocating() {
        let len = 1024;
        let mut scratch: Vec<u8> = Vec::new();
        let mut held: Option<Vec<u8>> = None;

        // Prime held + scratch.
        fill(&mut scratch, 1, len);
        let _ = rotate_blended_frame(&mut scratch, &mut held, Vec::new());

        // A buffer the UI returned, holding its previous (now irrelevant) pixels.
        let returned = vec![0x5Au8; len];
        let capacity = returned.capacity();
        let address = returned.as_ptr();

        fill(&mut scratch, 2, len);
        let sent = rotate_blended_frame(&mut scratch, &mut held, returned);
        assert_eq!(
            sent,
            vec![2u8; len],
            "returned buffer must be fully overwritten"
        );
        assert_eq!(
            sent.capacity(),
            capacity,
            "the returned allocation was reused"
        );
        assert_eq!(
            sent.as_ptr(),
            address,
            "the copy landed in a new allocation"
        );
    }

    #[test]
    fn a_short_returned_buffer_still_produces_a_full_frame() {
        let mut scratch: Vec<u8> = Vec::new();
        let mut held: Option<Vec<u8>> = None;
        fill(&mut scratch, 1, 256);
        let _ = rotate_blended_frame(&mut scratch, &mut held, Vec::new());

        // e.g. a buffer from the previous resolution.
        fill(&mut scratch, 2, 256);
        let sent = rotate_blended_frame(&mut scratch, &mut held, Vec::with_capacity(8));
        assert_eq!(sent.len(), 256);
        assert_eq!(sent, vec![2u8; 256]);
    }

    #[test]
    fn blending_into_a_buffer_matches_the_allocating_path() {
        let (w, h) = (16u32, 9u32);
        let len = (w * h * 4) as usize;
        let a: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let b: Vec<u8> = (0..len).map(|i| ((i * 7) % 253) as u8).collect();
        for kind in [
            TransitionKind::Crossfade,
            TransitionKind::Wipe,
            TransitionKind::Iris,
            TransitionKind::ClockWipe,
        ] {
            let mut out = Vec::new();
            for step in 0..=4 {
                let alpha = step as f32 / 4.0;
                blend_rgba_transition_into(&a, &b, w, h, alpha, kind, &mut out);
                let want = blend_rgba_transition(&a, &b, w, h, alpha, kind);
                assert_eq!(out, want, "{kind:?} at alpha={alpha}");
            }
        }
    }

    #[test]
    fn cut_copies_frame_a_into_the_reused_buffer() {
        let (w, h) = (4u32, 2u32);
        let len = (w * h * 4) as usize;
        let a = vec![7u8; len];
        let b = vec![9u8; len];
        let mut out = vec![0xFFu8; len + 32]; // deliberately dirty and too long
        blend_rgba_transition_into(&a, &b, w, h, 0.5, TransitionKind::Cut, &mut out);
        assert_eq!(out, a);
    }
}

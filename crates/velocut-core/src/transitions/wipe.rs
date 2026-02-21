// crates/velocut-core/src/transitions/wipe.rs
//
// Left-to-right horizontal wipe transition.
//
// A vertical bar sweeps from left to right across the frame, uncovering
// frame_b from the left as frame_a exits to the right.  A small feather
// (FEATHER = 2 % of frame width) softens the leading edge against compressed
// content without making the boundary feel mushy.
//
// `wipe_alpha(nx, edge, FEATHER)` returns 1.0 for pixels *right* of the edge
// and 0.0 for pixels *left* of it.  By passing arguments to blend_byte as
// (frame_b, frame_a, wa) we get:
//
//   wa = 0.0 (left of edge)  → frame_b  (already uncovered)
//   wa = 1.0 (right of edge) → frame_a  (not yet revealed)
//
// Alpha mapping:
//   edge   = ease_in_out(alpha)           — bar position 0→1 as alpha goes 0→1
//   per-px = blend_byte(b, a, wipe_alpha) — frame_b left of bar, frame_a right

use crate::transitions::{TransitionKind, TransitionType, VideoTransition};
use crate::transitions::helpers::{
    ease_in_out, norm_x, split_planes, wipe_alpha, blend_byte,
};

/// Feather width as a fraction of frame width.
///
/// 0.02 = 2 %, roughly 38 px at 1920-wide output.
/// Set to 0.0 for a perfectly binary cut-edge wipe.
const FEATHER: f32 = 0.02;

/// Left-to-right wipe: frame_b is uncovered by a sweeping vertical bar.
pub struct Wipe;

impl VideoTransition for Wipe {
    fn kind(&self) -> TransitionKind {
        TransitionKind::Wipe
    }

    fn label(&self) -> &'static str {
        "Wipe"
    }

    fn icon(&self) -> &'static str {
        "▶️"
    }

    fn default_duration_secs(&self) -> f32 {
        0.5
    }

    fn build(&self, duration_secs: f32) -> TransitionType {
        TransitionType::new(TransitionKind::Wipe, duration_secs)
    }

    /// Blend frame_a and frame_b using an eased left-to-right wipe.
    ///
    /// Each YUV plane is processed independently at its native resolution.
    /// The bar position is mapped through `ease_in_out` so it accelerates
    /// out of the first clip and decelerates into the second.
    ///
    /// Blend convention: `blend_byte(b, a, wa)` so wa=0 → frame_b (left of bar).
    fn apply(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        width:   u32,
        height:  u32,
        alpha:   f32,
    ) -> Vec<u8> {
        debug_assert_eq!(
            frame_a.len(), frame_b.len(),
            "Wipe::apply — frame size mismatch: {} vs {}",
            frame_a.len(), frame_b.len(),
        );

        // Eased bar position in [0, 1]: the normalised x-coordinate of the leading edge.
        let edge = ease_in_out(alpha);

        let (ay, au, av) = split_planes(frame_a, width, height);
        let (by, bu, bv) = split_planes(frame_b, width, height);

        let mut out = Vec::with_capacity(frame_a.len());

        // ── Y plane (full resolution) ────────────────────────────────────────
        for py in 0..height {
            for px in 0..width {
                let nx  = norm_x(px, width);
                let wa  = wipe_alpha(nx, edge, FEATHER);
                let idx = (py * width + px) as usize;
                out.push(blend_byte(by[idx], ay[idx], wa));
            }
        }

        // ── Chroma planes (half resolution each dimension) ───────────────────
        let uw = width  / 2;
        let uh = height / 2;

        for py in 0..uh {
            for px in 0..uw {
                let nx  = norm_x(px, uw);
                let wa  = wipe_alpha(nx, edge, FEATHER);
                let idx = (py * uw + px) as usize;
                out.push(blend_byte(bu[idx], au[idx], wa));
            }
        }

        for py in 0..uh {
            for px in 0..uw {
                let nx  = norm_x(px, uw);
                let wa  = wipe_alpha(nx, edge, FEATHER);
                let idx = (py * uw + px) as usize;
                out.push(blend_byte(bv[idx], av[idx], wa));
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transitions::helpers::{y_len, uv_len};

    fn yuv_frame(y_val: u8, uv_val: u8, w: u32, h: u32) -> Vec<u8> {
        let mut buf = vec![y_val; y_len(w, h)];
        buf.extend(vec![uv_val; uv_len(w, h) * 2]);
        buf
    }

    #[test]
    fn wipe_alpha_zero_is_all_frame_a() {
        let t = Wipe;
        let (w, h) = (8_u32, 4_u32);
        let a = yuv_frame(200, 100, w, h);
        let b = yuv_frame(50,  128, w, h);
        let out = t.apply(&a, &b, w, h, 0.0);
        // edge = ease_in_out(0) = 0.0; all nx > 0+feather/2 → wa = 1.0 → frame_a
        assert!(
            out[..y_len(w, h)].iter().all(|&v| v == 200),
            "all Y should be frame_a at alpha=0"
        );
    }

    #[test]
    fn wipe_alpha_one_is_all_frame_b() {
        let t = Wipe;
        let (w, h) = (8_u32, 4_u32);
        let a = yuv_frame(200, 100, w, h);
        let b = yuv_frame(50,  128, w, h);
        let out = t.apply(&a, &b, w, h, 1.0);
        // edge = ease_in_out(1) = 1.0; all nx < 1.0-feather/2 → wa = 0.0 → frame_b
        assert!(
            out[..y_len(w, h)].iter().all(|&v| v == 50),
            "all Y should be frame_b at alpha=1"
        );
    }

    #[test]
    fn wipe_half_alpha_splits_left_right() {
        let t = Wipe;
        let (w, h) = (8_u32, 4_u32);
        let a = yuv_frame(200, 100, w, h);
        let b = yuv_frame(50,  128, w, h);
        let out = t.apply(&a, &b, w, h, 0.5);
        // edge = 0.5; col 0 (nx=0.0625) left of bar → frame_b (50)
        assert_eq!(out[0], 50,  "leftmost pixel should be frame_b at alpha=0.5");
        // col 7 (nx=0.9375) right of bar → frame_a (200)
        assert_eq!(out[7], 200, "rightmost pixel should be frame_a at alpha=0.5");
    }

    #[test]
    fn wipe_output_length_matches_input() {
        let t = Wipe;
        let (w, h) = (8_u32, 4_u32);
        let a = yuv_frame(100, 128, w, h);
        let b = yuv_frame(200, 128, w, h);
        for alpha in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let out = t.apply(&a, &b, w, h, alpha);
            assert_eq!(out.len(), a.len(), "length mismatch at alpha={alpha}");
        }
    }
}
// crates/velocut-core/src/transitions/barn_doors.rs
//
// Barn doors: the left half of frame_a slides left and the right half slides
// right, opening like a pair of barn doors to reveal frame_b behind them.
//
// No blending — hard pixel copy.  The gap between the two doors grows
// symmetrically from the center until both doors are fully off-screen.
//
// Per-pixel logic (Y plane, repeated for U/V at half resolution):
//   slide = ease_in_out_cubic(alpha) × width/2   (0 → fully closed, width/2 → fully open)
//
//   px < center - slide  → left door still visible; sample frame_a at px + slide
//   px ≥ center + slide  → right door still visible; sample frame_a at px - slide
//   otherwise            → gap; sample frame_b at px

use crate::transitions::{TransitionKind, TransitionType, VideoTransition};
use crate::transitions::helpers::{
    alloc_frame, ease_in_out_cubic, sample_plane_clamped, split_planes, uv_len, y_len,
};

pub struct BarnDoors;

impl VideoTransition for BarnDoors {
    fn kind(&self) -> TransitionKind {
        TransitionKind::BarnDoors
    }

    fn label(&self) -> &'static str {
        "Barn Doors"
    }

    fn icon(&self) -> &'static str {
        "⊟"
    }

    fn default_duration_secs(&self) -> f32 {
        1.0
    }

    fn build(&self, duration_secs: f32) -> TransitionType {
        TransitionType::new(TransitionKind::BarnDoors, duration_secs)
    }

    fn apply(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        width:   u32,
        height:  u32,
        alpha:   f32,
    ) -> Vec<u8> {
        debug_assert_eq!(frame_a.len(), frame_b.len(),
            "BarnDoors::apply — frame size mismatch");

        let t     = ease_in_out_cubic(alpha);
        let slide = (t * width as f32 * 0.5).round() as i32;
        let half  = (width / 2) as i32;

        let yl  = y_len(width, height);
        let uvl = uv_len(width, height);
        let mut out = alloc_frame(width, height);

        let (ya, ua, va) = split_planes(frame_a, width, height);
        let (yb, ub, vb) = split_planes(frame_b, width, height);

        // ── Y plane (full res) ───────────────────────────────────────────────
        for py in 0..height {
            for px in 0..width as i32 {
                let i = (py * width + px as u32) as usize;
                if px < half - slide {
                    // Left door — sample frame_a shifted right by slide
                    let sx = px + slide;
                    out[i] = sample_plane_clamped(ya, sx, py as i32, width, height);
                } else if px >= half + slide {
                    // Right door — sample frame_a shifted left by slide
                    let sx = px - slide;
                    out[i] = sample_plane_clamped(ya, sx, py as i32, width, height);
                } else {
                    // Gap between doors — reveal frame_b
                    out[i] = sample_plane_clamped(yb, px, py as i32, width, height);
                }
            }
        }

        // ── Chroma planes (half res) ─────────────────────────────────────────
        let uw      = width  / 2;
        let uh      = height / 2;
        let c_slide = slide / 2;          // chroma slide is half the luma slide
        let c_half  = (uw / 2) as i32;   // chroma center column = luma center in chroma space

        let mut write_chroma = |src_a: &[u8], src_b: &[u8], off: usize| {
            for py in 0..uh {
                for px in 0..uw as i32 {
                    let i = off + (py * uw + px as u32) as usize;
                    if px < c_half - c_slide {
                        out[i] = sample_plane_clamped(src_a, px + c_slide, py as i32, uw, uh);
                    } else if px >= c_half + c_slide {
                        out[i] = sample_plane_clamped(src_a, px - c_slide, py as i32, uw, uh);
                    } else {
                        out[i] = sample_plane_clamped(src_b, px, py as i32, uw, uh);
                    }
                }
            }
        };

        write_chroma(ua, ub, yl);
        write_chroma(va, vb, yl + uvl);

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(y: u8, uv: u8, w: u32, h: u32) -> Vec<u8> {
        let yl = y_len(w, h); let uvl = uv_len(w, h);
        let mut b = vec![0u8; yl + uvl * 2];
        b[..yl].fill(y); b[yl..yl+uvl].fill(uv); b[yl+uvl..].fill(uv); b
    }

    #[test]
    fn output_length_matches_input() {
        let (w, h) = (8, 4);
        let a = packed(100, 128, w, h);
        let b = packed(200, 64, w, h);
        assert_eq!(BarnDoors.apply(&a, &b, w, h, 0.5).len(), a.len());
    }

    #[test]
    fn alpha_zero_returns_frame_a() {
        let (w, h) = (8, 4);
        let a = packed(100, 128, w, h);
        let b = packed(200, 64, w, h);
        let result = BarnDoors.apply(&a, &b, w, h, 0.0);
        // slide=0 → no gap, entire output is frame_a
        assert!(result[..y_len(w,h)].iter().all(|&v| v == 100));
    }

    #[test]
    fn alpha_one_returns_frame_b() {
        let (w, h) = (8, 4);
        let a = packed(100, 128, w, h);
        let b = packed(200, 64, w, h);
        let result = BarnDoors.apply(&a, &b, w, h, 1.0);
        // slide=w/2 → doors fully off screen, entire output is frame_b
        assert!(result[..y_len(w,h)].iter().all(|&v| v == 200));
    }

    #[test]
    fn center_column_shows_frame_b_at_midpoint() {
        let (w, h) = (16, 4);
        let a = packed(50, 128, w, h);
        let b = packed(150, 128, w, h);
        let result = BarnDoors.apply(&a, &b, w, h, 0.5);
        // Center column should be inside the gap → frame_b
        let center_px = (h / 2 * w + w / 2) as usize;
        assert!(result[center_px] > 100,
            "center pixel should be frame_b at alpha=0.5 (got {})", result[center_px]);
    }
}
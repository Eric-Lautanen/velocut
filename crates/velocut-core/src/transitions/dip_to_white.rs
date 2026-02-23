// crates/velocut-core/src/transitions/dip_to_white.rs
//
// Dip to white: first half of the transition fades frame_a to white;
// second half fades from white into frame_b.
//
// White in YUV420P: Y = 255, U = 128, V = 128.
//
// Per-pixel logic:
//   alpha < 0.5 → blend Y toward 255, UV toward 128 (a → white)
//   alpha ≥ 0.5 → blend from 255/128 toward frame_b (white → b)
//
// Both halves use ease_in_out so the flash accelerates in and out
// rather than burning at a constant rate.

use crate::transitions::{TransitionKind, TransitionType, VideoTransition};
use crate::transitions::helpers::{
    alloc_frame, blend_byte, ease_in_out, split_planes, uv_len, y_len,
};

pub struct DipToWhite;

impl VideoTransition for DipToWhite {
    fn kind(&self) -> TransitionKind {
        TransitionKind::DipToWhite
    }

    fn label(&self) -> &'static str {
        "Dip to White"
    }

    fn icon(&self) -> &'static str {
        "*"
    }

    fn default_duration_secs(&self) -> f32 {
        1.0
    }

    fn build(&self, duration_secs: f32) -> TransitionType {
        TransitionType::new(TransitionKind::DipToWhite, duration_secs)
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
            "DipToWhite::apply — frame size mismatch");

        let yl  = y_len(width, height);
        let uvl = uv_len(width, height);
        let mut out = alloc_frame(width, height);

        let (ya, ua, va) = split_planes(frame_a, width, height);
        let (yb, ub, vb) = split_planes(frame_b, width, height);

        if alpha < 0.5 {
            // ── frame_a → white ───────────────────────────────────────────────
            let t = ease_in_out(alpha * 2.0);
            for i in 0..yl  { out[i]          = blend_byte(ya[i], 255, t); }
            for i in 0..uvl { out[yl + i]     = blend_byte(ua[i], 128, t); }
            for i in 0..uvl { out[yl+uvl + i] = blend_byte(va[i], 128, t); }
        } else {
            // ── white → frame_b ───────────────────────────────────────────────
            let t = ease_in_out((alpha - 0.5) * 2.0);
            for i in 0..yl  { out[i]          = blend_byte(255, yb[i], t); }
            for i in 0..uvl { out[yl + i]     = blend_byte(128, ub[i], t); }
            for i in 0..uvl { out[yl+uvl + i] = blend_byte(128, vb[i], t); }
        }

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
        assert_eq!(DipToWhite.apply(&a, &b, w, h, 0.5).len(), a.len());
    }

    #[test]
    fn alpha_zero_returns_frame_a() {
        let (w, h) = (8, 4);
        let a = packed(100, 128, w, h);
        let b = packed(200, 64, w, h);
        let result = DipToWhite.apply(&a, &b, w, h, 0.0);
        assert!(result[..y_len(w,h)].iter().all(|&v| v == 100));
    }

    #[test]
    fn alpha_one_returns_frame_b() {
        let (w, h) = (8, 4);
        let a = packed(100, 128, w, h);
        let b = packed(200, 64, w, h);
        let result = DipToWhite.apply(&a, &b, w, h, 1.0);
        assert!(result[..y_len(w,h)].iter().all(|&v| v == 200));
    }

    #[test]
    fn midpoint_is_white() {
        let (w, h) = (8, 4);
        let a = packed(0, 128, w, h);
        let b = packed(0, 128, w, h);
        let result = DipToWhite.apply(&a, &b, w, h, 0.5);
        // At exact midpoint ease_in_out(1.0) = 1.0 → full white
        assert!(result[..y_len(w,h)].iter().all(|&v| v == 255));
    }
}
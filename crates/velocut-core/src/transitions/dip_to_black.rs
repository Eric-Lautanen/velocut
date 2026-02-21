// crates/velocut-core/src/transitions/dip_to_black.rs
//
// Dip-to-black transition: outgoing clip fades to black over the first half
// of the overlap; incoming clip rises from black over the second half.
//
// This is distinctly different from a crossfade — the two clips never mix
// directly. Each half of the transition is a clean fade-to/from-black,
// giving the classic film/broadcast "cut on black" feel.
//
// Alpha mapping (smooth-step eased within each half):
//   alpha 0.0 → 0.5 : frame_a × (1 − ease_in_out(alpha × 2))
//   alpha 0.5 → 1.0 : frame_b × ease_in_out((alpha − 0.5) × 2)
//
// The UV channels (chroma) are blended identically to luma — fading to
// neutral grey (128) would be more correct for YUV, but byte-blending
// toward 0 is an acceptable SDR approximation and keeps the algorithm
// consistent with crossfade.rs's `blend_byte` approach.

use crate::transitions::{TransitionKind, TransitionType, VideoTransition};
use crate::transitions::helpers::{blend_byte, ease_in_out};

/// Fade-to-black between two clips.
///
/// Each clip fades independently through black; they never blend directly
/// with each other. The crossover point is the exact midpoint of the overlap.
pub struct DipToBlack;

impl VideoTransition for DipToBlack {
    fn kind(&self) -> TransitionKind {
        TransitionKind::DipToBlack
    }

    fn label(&self) -> &'static str {
        "Dip to Black"
    }

    fn icon(&self) -> &'static str {
        "⬛️"  // U+2B1B U+FE0F — variation selector required for emoji rendering
    }

    fn default_duration_secs(&self) -> f32 {
        0.8  // slightly longer than crossfade — the double-fade needs room to breathe
    }

    fn build(&self, duration_secs: f32) -> TransitionType {
        TransitionType::new(TransitionKind::DipToBlack, duration_secs)
    }

    /// Blend frame_a or frame_b toward black depending on which half of the
    /// overlap we are in.
    ///
    /// `alpha = 0.0` → 100 % frame_a (transition hasn't started yet)
    /// `alpha = 0.5` → both clips are fully black (deepest point of the dip)
    /// `alpha = 1.0` → 100 % frame_b (transition is complete)
    ///
    /// Smooth-step easing is applied inside each half-ramp so the fade
    /// accelerates into black and decelerates out of it, matching how the
    /// eye perceives a natural film dip.
    fn apply(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        _width:  u32,
        _height: u32,
        alpha:   f32,
    ) -> Vec<u8> {
        debug_assert_eq!(
            frame_a.len(),
            frame_b.len(),
            "DipToBlack::apply — frame size mismatch: {} vs {}",
            frame_a.len(),
            frame_b.len(),
        );

        if alpha <= 0.5 {
            // First half: fade frame_a out to black.
            // ramp goes 0→1 as alpha goes 0→0.5
            let ramp = ease_in_out(alpha * 2.0);
            frame_a.iter()
                .map(|&a| blend_byte(a, 0, ramp))
                .collect()
        } else {
            // Second half: fade frame_b in from black.
            // ramp goes 0→1 as alpha goes 0.5→1.0
            let ramp = ease_in_out((alpha - 0.5) * 2.0);
            frame_b.iter()
                .map(|&b| blend_byte(0, b, ramp))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(value: u8, len: usize) -> Vec<u8> {
        vec![value; len]
    }

    #[test]
    fn dip_alpha_zero_returns_frame_a() {
        let t = DipToBlack;
        let a = make_frame(200, 12);
        let b = make_frame(150, 12);
        let result = t.apply(&a, &b, 4, 3, 0.0);
        // ease_in_out(0.0) = 0.0 → blend(200, 0, 0.0) = 200
        assert!(result.iter().all(|&v| v == 200));
    }

    #[test]
    fn dip_alpha_one_returns_frame_b() {
        let t = DipToBlack;
        let a = make_frame(200, 12);
        let b = make_frame(150, 12);
        let result = t.apply(&a, &b, 4, 3, 1.0);
        // ease_in_out(1.0) = 1.0 → blend(0, 150, 1.0) = 150
        assert!(result.iter().all(|&v| v == 150));
    }

    #[test]
    fn dip_midpoint_is_black() {
        let t = DipToBlack;
        let a = make_frame(255, 12);
        let b = make_frame(255, 12);
        let result = t.apply(&a, &b, 4, 3, 0.5);
        // First half at alpha=0.5: ease_in_out(1.0) = 1.0 → blend(255, 0, 1.0) = 0
        assert!(result.iter().all(|&v| v == 0), "midpoint should be black, got {:?}", result);
    }

    #[test]
    fn dip_first_half_approaches_black() {
        let t = DipToBlack;
        let a = make_frame(200, 12);
        let b = make_frame(200, 12);
        // At alpha=0.25, ramp = ease_in_out(0.5) = 0.5 → blend(200, 0, 0.5) = 100
        let result = t.apply(&a, &b, 4, 3, 0.25);
        assert!(result.iter().all(|&v| v == 100));
    }

    #[test]
    fn dip_second_half_rises_from_black() {
        let t = DipToBlack;
        let a = make_frame(200, 12);
        let b = make_frame(200, 12);
        // At alpha=0.75, ramp = ease_in_out(0.5) = 0.5 → blend(0, 200, 0.5) = 100
        let result = t.apply(&a, &b, 4, 3, 0.75);
        assert!(result.iter().all(|&v| v == 100));
    }

    #[test]
    fn dip_output_length_matches_input() {
        let t = DipToBlack;
        let len = 100;
        let a = make_frame(80, len);
        let b = make_frame(160, len);
        let result = t.apply(&a, &b, 10, 10, 0.3);
        assert_eq!(result.len(), len);
    }
}
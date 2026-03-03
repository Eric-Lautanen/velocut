// crates/velocut-core/src/filters/helpers.rs
//
// Pure pixel-math for the filter system. No FFmpeg dependency.
//
// Two entry points:
//   apply_filter_rgba  -- RGBA byte slice, scrub/playback path.
//   apply_filter_yuv   -- in-place YUV420P planes, encode path.
//
// Both use rayon par_chunks_mut (same pattern as transition apply_rgba).
// Both skip all work when params.is_identity().
//
// Op order: brightness -> contrast -> gamma -> saturation -> hue -> temperature.
// All values clamped to [0,1] before writing back.

use rayon::prelude::*;
use super::FilterParams;

// ── RGBA path (scrub / playback) ──────────────────────────────────────────────

/// Apply `params` in-place to an RGBA byte buffer (len == w * h * 4).
/// No-ops immediately if params.is_identity().
pub fn apply_filter_rgba(pixels: &mut [u8], params: &FilterParams) {
    if params.is_identity() { return; }
    let p = params.apply_strength();

    pixels.par_chunks_mut(4).for_each(|px| {
        let mut r = px[0] as f32 / 255.0;
        let mut g = px[1] as f32 / 255.0;
        let mut b = px[2] as f32 / 255.0;
        // alpha (px[3]) is never touched

        apply_luma_ops(&mut r, &mut g, &mut b, &p);
        apply_chroma_ops(&mut r, &mut g, &mut b, &p);

        px[0] = (r.clamp(0.0, 1.0) * 255.0).round() as u8;
        px[1] = (g.clamp(0.0, 1.0) * 255.0).round() as u8;
        px[2] = (b.clamp(0.0, 1.0) * 255.0).round() as u8;
    });
}

// ── YUV420P path (encode) ─────────────────────────────────────────────────────

/// Apply `params` in-place to packed YUV420P planes.
///
/// Expected layout (same as encode.rs after CropScaler):
///   y_plane  w * h bytes          (one byte per pixel)
///   u_plane  (w/2) * (h/2) bytes
///   v_plane  (w/2) * (h/2) bytes
///
/// No-ops immediately if params.is_identity().
pub fn apply_filter_yuv(
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
    params:  &FilterParams,
) {
    if params.is_identity() { return; }
    let p = params.apply_strength();

    // Y plane: brightness, contrast, gamma, temperature luma nudge
    y_plane.par_iter_mut().for_each(|y| {
        let mut luma = *y as f32 / 255.0;
        luma += p.brightness;
        luma  = (luma - 0.5) * p.contrast + 0.5;
        if p.gamma != 1.0 && luma > 0.0 {
            luma = luma.powf(1.0 / p.gamma);
        }
        // Temperature: warm tone very slightly lifts luma (amber carry)
        luma += p.temperature * 0.04;
        *y = (luma.clamp(0.0, 1.0) * 255.0).round() as u8;
    });

    // Saturation: scale UV toward/away from chroma centre (128 = neutral)
    if p.saturation != 1.0 {
        let s = p.saturation;
        u_plane.par_iter_mut().for_each(|u| {
            let su = (*u as f32 - 128.0) / 127.0 * s;
            *u = ((su * 127.0 + 128.0).clamp(0.0, 255.0)) as u8;
        });
        v_plane.par_iter_mut().for_each(|v| {
            let sv = (*v as f32 - 128.0) / 127.0 * s;
            *v = ((sv * 127.0 + 128.0).clamp(0.0, 255.0)) as u8;
        });
    }

    // Hue: rotate UV chroma vector by hue degrees.
    // U' = U*cos(t) - V*sin(t),  V' = U*sin(t) + V*cos(t)
    if p.hue != 0.0 {
        let theta = p.hue.to_radians();
        let cos_t = theta.cos();
        let sin_t = theta.sin();
        u_plane.par_iter_mut()
            .zip(v_plane.par_iter_mut())
            .for_each(|(u, v)| {
                let fu = (*u as f32 - 128.0) / 127.0;
                let fv = (*v as f32 - 128.0) / 127.0;
                let nu = fu * cos_t - fv * sin_t;
                let nv = fu * sin_t + fv * cos_t;
                *u = ((nu * 127.0 + 128.0).clamp(0.0, 255.0)) as u8;
                *v = ((nv * 127.0 + 128.0).clamp(0.0, 255.0)) as u8;
            });
    }

    // Temperature on UV: warm -> +V, -U (amber/orange); cool -> -V, +U (blue)
    if p.temperature != 0.0 {
        let t = p.temperature * 0.18;
        u_plane.par_iter_mut().for_each(|u| {
            let fu = (*u as f32 - 128.0) / 127.0 - t;
            *u = ((fu * 127.0 + 128.0).clamp(0.0, 255.0)) as u8;
        });
        v_plane.par_iter_mut().for_each(|v| {
            let fv = (*v as f32 - 128.0) / 127.0 + t;
            *v = ((fv * 127.0 + 128.0).clamp(0.0, 255.0)) as u8;
        });
    }
}

// ── Shared RGB helpers ────────────────────────────────────────────────────────

#[inline]
fn apply_luma_ops(r: &mut f32, g: &mut f32, b: &mut f32, p: &FilterParams) {
    // Brightness — additive luma shift
    *r += p.brightness;
    *g += p.brightness;
    *b += p.brightness;
    // Contrast — multiplicative around 0.5 mid-gray
    if p.contrast != 1.0 {
        *r = (*r - 0.5) * p.contrast + 0.5;
        *g = (*g - 0.5) * p.contrast + 0.5;
        *b = (*b - 0.5) * p.contrast + 0.5;
    }
    // Gamma — power curve per channel (preserves colour balance)
    if p.gamma != 1.0 {
        let inv = 1.0 / p.gamma;
        if *r > 0.0 { *r = r.powf(inv); }
        if *g > 0.0 { *g = g.powf(inv); }
        if *b > 0.0 { *b = b.powf(inv); }
    }
}

#[inline]
fn apply_chroma_ops(r: &mut f32, g: &mut f32, b: &mut f32, p: &FilterParams) {
    // Saturation — luma-weighted desaturate blend
    if p.saturation != 1.0 {
        let luma = 0.299 * *r + 0.587 * *g + 0.114 * *b;
        *r = luma + (*r - luma) * p.saturation;
        *g = luma + (*g - luma) * p.saturation;
        *b = luma + (*b - luma) * p.saturation;
    }
    // Hue rotation — RGB -> HSV, rotate H, HSV -> RGB
    if p.hue != 0.0 {
        let (h, s, v) = rgb_to_hsv(*r, *g, *b);
        let h2 = (h + p.hue).rem_euclid(360.0);
        let (nr, ng, nb) = hsv_to_rgb(h2, s, v);
        *r = nr; *g = ng; *b = nb;
    }
    // Temperature — additive R/B nudge
    if p.temperature != 0.0 {
        let t = p.temperature * 0.15;
        *r = (*r + t).clamp(0.0, 1.0);
        *b = (*b - t).clamp(0.0, 1.0);
    }
}

// ── RGB <-> HSV ───────────────────────────────────────────────────────────────

fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let cmax  = r.max(g).max(b);
    let cmin  = r.min(g).min(b);
    let delta = cmax - cmin;
    let h = if delta < 1e-6 {
        0.0
    } else if cmax == r {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if cmax == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let s = if cmax < 1e-6 { 0.0 } else { delta / cmax };
    (h, s, cmax)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    if s < 1e-6 { return (v, v, v); }
    let h6 = h / 60.0;
    let i  = h6.floor() as i32 % 6;
    let f  = h6 - h6.floor();
    let p  = v * (1.0 - s);
    let q  = v * (1.0 - s * f);
    let t  = v * (1.0 - s * (1.0 - f));
    match i {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::{FilterKind, FilterParams};

    #[test]
    fn identity_rgba_no_op() {
        let orig = vec![128u8, 64, 192, 255,  200, 100, 50, 255];
        let mut px = orig.clone();
        apply_filter_rgba(&mut px, &FilterParams::none());
        assert_eq!(px, orig);
    }

    #[test]
    fn zero_strength_no_op() {
        let orig = vec![128u8, 64, 192, 255];
        let mut px = orig.clone();
        let mut p = FilterParams::from_preset(FilterKind::Vivid);
        p.strength = 0.0;
        apply_filter_rgba(&mut px, &p);
        assert_eq!(px, orig);
    }

    #[test]
    fn bw_produces_equal_channels() {
        let mut px = vec![200u8, 100, 50, 255];
        apply_filter_rgba(&mut px, &FilterParams::from_preset(FilterKind::BlackAndWhite));
        // After full desaturate all RGB channels must be equal (greyscale luma)
        assert_eq!(px[0], px[1]);
        assert_eq!(px[1], px[2]);
    }

    #[test]
    fn identity_yuv_no_op() {
        let y = vec![128u8; 16];
        let u = vec![128u8; 4];
        let v = vec![128u8; 4];
        let mut y2 = y.clone(); let mut u2 = u.clone(); let mut v2 = v.clone();
        apply_filter_yuv(&mut y2, &mut u2, &mut v2, &FilterParams::none());
        assert_eq!(y2, y); assert_eq!(u2, u); assert_eq!(v2, v);
    }

    #[test]
    fn hsv_roundtrip() {
        let (r, g, b) = (0.8f32, 0.3, 0.5);
        let (h, s, v) = rgb_to_hsv(r, g, b);
        let (r2, g2, b2) = hsv_to_rgb(h, s, v);
        assert!((r - r2).abs() < 1e-5);
        assert!((g - g2).abs() < 1e-5);
        assert!((b - b2).abs() < 1e-5);
    }
}
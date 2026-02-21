// crates/velocut-core/src/transitions/mod.rs
//
// Transition system for VeloCut.
//
// Three layers live here:
//
//   1. Serialized project types — `TransitionType` and `TimelineTransition`
//      are stored in ProjectState and written to disk. Their shape must stay
//      backward-compatible. `ClipTransition` is encode-only (not serialized).
//
//   2. `VideoTransition` trait — the algorithm contract. Each transition is a
//      zero-size (or config) struct that implements this trait. The `apply()`
//      method receives packed YUV420P buffers and an alpha in [0.0, 1.0] and
//      returns a blended packed buffer. No FFmpeg types cross this boundary —
//      the media crate handles `extract_yuv` / `write_yuv` on both sides.
//
//   3. Registry — a `HashMap<TransitionKind, Box<dyn VideoTransition>>` built
//      once via `registry()`. Both `encode.rs` and `preview_module.rs` call
//      into this rather than matching on `TransitionType` directly.
//
// Adding a new transition:
//   1. Add a variant to `TransitionKind` (discriminant, no data).
//   2. Add a matching variant to `TransitionType` (carries runtime params like
//      duration — these are serialized into the project file).
//   3. Create `my_transition.rs` in this folder, impl `VideoTransition`.
//   4. Add `mod my_transition;` below and one line to the `registry()` vec.
//   Done — encode and preview pick it up automatically.

mod crossfade;
// mod wipe;  ← future: add mod declaration here

use std::collections::HashMap;
use uuid::Uuid;
use serde::{Deserialize, Serialize};

// ── Serialized project types ──────────────────────────────────────────────────

/// Discriminant-only enum used as the registry key.
///
/// Unlike `TransitionType`, this carries no runtime parameters — it identifies
/// *which algorithm* to look up, not how that algorithm is configured for a
/// particular clip boundary.
///
/// Kept separate from `TransitionType` so the registry can be keyed on a
/// `Copy` type without needing to pattern-match on data-carrying variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransitionKind {
    Cut,
    Crossfade,
    // Wipe,  ← add here when adding a new transition
}

/// Serialized transition variant stored in `ProjectState`.
///
/// Carries runtime parameters (e.g. `duration_secs`) that configure the
/// transition for a specific clip boundary. Serialized into the project file —
/// never change existing variant shapes without a migration path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransitionType {
    /// Hard cut — no blending, zero encode overhead.
    Cut,
    /// Linear dissolve. Both clips are shortened by `duration_secs` so total
    /// output duration is preserved.
    Crossfade { duration_secs: f32 },
    // Wipe { duration_secs: f32, direction: WipeDirection },  ← future
}

impl Default for TransitionType {
    fn default() -> Self { TransitionType::Cut }
}

impl TransitionType {
    /// Return the discriminant for registry lookup, stripping runtime params.
    pub fn kind(&self) -> TransitionKind {
        match self {
            TransitionType::Cut                 => TransitionKind::Cut,
            TransitionType::Crossfade { .. }    => TransitionKind::Crossfade,
        }
    }

    /// Duration of the transition in seconds, if applicable.
    pub fn duration_secs(&self) -> f32 {
        match self {
            TransitionType::Cut                          => 0.0,
            TransitionType::Crossfade { duration_secs } => *duration_secs,
        }
    }
}

/// Stored in `ProjectState` — serialized with the project.
/// Keyed by the TimelineClip UUID that precedes the transition so it survives
/// clip reordering without becoming stale.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimelineTransition {
    pub after_clip_id: Uuid,
    pub kind: TransitionType,
}

/// Encode-only — built by `begin_render`, passed in `EncodeSpec`. Not serialized.
#[derive(Clone, Debug)]
pub struct ClipTransition {
    /// Index into the sorted clip Vec (0 = between clips[0] and clips[1]).
    pub after_clip_index: usize,
    pub kind: TransitionType,
}

// ── VideoTransition trait ─────────────────────────────────────────────────────

/// Algorithm contract for all video transitions.
///
/// Implementors are zero-size or config structs — they hold no per-clip state.
/// Runtime parameters (duration, direction, etc.) come via the call site
/// through `TransitionType`; the trait only receives the data it needs to blend.
///
/// # Buffer contract
/// Both `frame_a` and `frame_b` are packed YUV420P byte slices with layout:
///   `[Y plane: w×h] ++ [U plane: (w/2)×(h/2)] ++ [V plane: (w/2)×(h/2)]`
/// No stride padding. Use `velocut_media::helpers::yuv::extract_yuv` to produce
/// them and `write_yuv` to write the result back into a VideoFrame.
///
/// # Alpha convention
/// `alpha = 0.0` → 100% frame_a (outgoing clip)
/// `alpha = 1.0` → 100% frame_b (incoming clip)
/// The caller computes alpha from frame position and total frame count.
///
/// # Performance contract
/// `apply()` is called once per frame. All inner loops must live *inside* the
/// impl — do not make repeated trait calls from within a pixel loop.
pub trait VideoTransition: Send + Sync {
    /// Discriminant identifying this transition in the registry.
    fn kind(&self) -> TransitionKind;

    /// Human-readable label used in the UI picker.
    fn label(&self) -> &'static str;

    /// Default duration in seconds shown in the UI when the user picks this transition.
    fn default_duration_secs(&self) -> f32 { 0.5 }

    /// Blend `frame_a` and `frame_b` at the given `alpha` and return the result.
    ///
    /// `width` and `height` are the luma dimensions. UV dims are `(width/2, height/2)`.
    fn apply(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        width:   u32,
        height:  u32,
        alpha:   f32,
    ) -> Vec<u8>;
}

// ── Registry ──────────────────────────────────────────────────────────────────

/// Return a map of all registered transitions keyed by `TransitionKind`.
///
/// Called once by the encoder and once by the preview module — cheap to
/// construct since all impls are zero-size structs. Consider caching with
/// `std::sync::OnceLock` if profiling shows it in a hot path.
///
/// The `Cut` variant has no corresponding `VideoTransition` entry — callers
/// should short-circuit on `TransitionKind::Cut` before hitting the registry.
pub fn registry() -> HashMap<TransitionKind, Box<dyn VideoTransition>> {
    let entries: Vec<Box<dyn VideoTransition>> = vec![
        Box::new(crossfade::Crossfade),
        // Box::new(wipe::Wipe),  ← add here
    ];
    entries.into_iter().map(|t| (t.kind(), t)).collect()
}
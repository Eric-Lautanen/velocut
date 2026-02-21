// crates/velocut-core/src/transitions/mod.rs
//
// Transition system for VeloCut.
//
// ╔══════════════════════════════════════════════════════════════╗
// ║  HOW TO ADD A TRANSITION — one line, everything else is auto ║
// ╚══════════════════════════════════════════════════════════════╝
//
//   1. Create `transitions/my_transition.rs`, impl `VideoTransition`
//      (all 5 required methods: kind, label, icon, build, apply).
//
//   2. Add ONE line to `declare_transitions!` below:
//        my_transition::MyTransition,
//
//   Done. Badge, tooltip, popup button, duration slider, encode,
//   and preview all pick it up automatically. No other changes needed.
//
// ── Architecture ─────────────────────────────────────────────────────────────
//
//   Layer 1 — Serialized types  (`TransitionKind`, `TransitionType`,
//             `TimelineTransition`, `ClipTransition`)
//             These are written to the project file. Never rename/remove
//             existing variants without a migration path.
//
//   Layer 2 — `VideoTransition` trait
//             Pure pixel algorithm. Receives packed YUV420P slices + alpha,
//             returns blended packed slice. No FFmpeg types cross this boundary.
//
//   Layer 3 — Registry  (`registered()` for UI, `registry()` for encode)
//             Built from `declare_transitions!`. UI iterates `registered()`;
//             encode does O(1) lookup via `registry()`. Cut is never in either
//             — callers short-circuit on `TransitionKind::Cut`.

use std::collections::HashMap;
use uuid::Uuid;
use serde::{Deserialize, Serialize};

// ── Drop-in registration ──────────────────────────────────────────────────────
//
// To add a transition: append `module_name::StructName,` here.
// To remove/disable: comment it out (serialized TransitionType variants must
// stay in the enums below even if the impl is removed — deserialization safety).

macro_rules! declare_transitions {
    ( $( $module:ident :: $struct:ident ),* $(,)? ) => {
        $( mod $module; )*

        fn make_entries() -> Vec<Box<dyn VideoTransition>> {
            vec![ $( Box::new($module::$struct) ),* ]
        }
    };
}

declare_transitions! {
    crossfade::Crossfade,
    // wipe::Wipe,
}

// ── Serialized project types ──────────────────────────────────────────────────

/// Discriminant-only enum used as the registry key.
///
/// Carries no runtime parameters — identifies *which algorithm* to look up,
/// not how it is configured for a particular clip boundary. `Copy` so the
/// registry can be keyed on it without cloning.
///
/// Add a variant here whenever you add a transition to `declare_transitions!`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransitionKind {
    Cut,
    Crossfade,
    // Wipe,
}

/// Serialized transition variant stored in `ProjectState`.
///
/// Carries runtime parameters (duration, direction, etc.) for a specific clip
/// boundary. Written to the project file — **never rename or remove existing
/// variants** without a migration path. Unused variants are harmless on disk.
///
/// Add a variant here whenever you add a transition to `declare_transitions!`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransitionType {
    /// Hard cut — no blending, zero encode overhead.
    Cut,
    /// Dissolve. Both clips overlap by `duration_secs`; total output duration
    /// is preserved.
    Crossfade { duration_secs: f32 },
    // Wipe { duration_secs: f32, direction: WipeDirection },
}

impl Default for TransitionType {
    fn default() -> Self { TransitionType::Cut }
}

impl TransitionType {
    /// Strip runtime params, return the discriminant for registry lookup.
    pub fn kind(&self) -> TransitionKind {
        match self {
            TransitionType::Cut              => TransitionKind::Cut,
            TransitionType::Crossfade { .. } => TransitionKind::Crossfade,
        }
    }

    /// Duration of the overlap in seconds. Returns 0.0 for Cut.
    pub fn duration_secs(&self) -> f32 {
        match self {
            TransitionType::Cut                          => 0.0,
            TransitionType::Crossfade { duration_secs } => *duration_secs,
        }
    }
}

/// Stored in `ProjectState`, serialized with the project.
///
/// Keyed by the UUID of the preceding `TimelineClip` so it survives clip
/// reordering without going stale.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimelineTransition {
    pub after_clip_id: Uuid,
    pub kind: TransitionType,
}

/// Encode-only — built by `begin_render`, passed through `EncodeSpec`.
/// Not serialized.
#[derive(Clone, Debug)]
pub struct ClipTransition {
    /// Index into the sorted clip Vec (0 = between clips[0] and clips[1]).
    pub after_clip_index: usize,
    pub kind: TransitionType,
}

// ── VideoTransition trait ─────────────────────────────────────────────────────

/// Algorithm contract for all video transitions.
///
/// Implementors are zero-size (or config) structs — they hold no per-clip state.
/// Runtime parameters arrive through `TransitionType` at the call site; the
/// trait only receives what it needs to blend two frames.
///
/// # Buffer contract
/// `frame_a` and `frame_b` are packed YUV420P byte slices:
///   `[Y: w×h bytes][U: (w/2)×(h/2) bytes][V: (w/2)×(h/2) bytes]`
/// No stride padding. Produce them with `velocut_media::helpers::yuv::extract_yuv`
/// and write results back with `write_yuv`.
///
/// # Alpha convention
/// `alpha = 0.0` → 100 % frame_a (outgoing clip)  
/// `alpha = 1.0` → 100 % frame_b (incoming clip)  
/// Caller computes alpha from frame index and total overlap frame count via
/// `transitions::helpers::frame_alpha`.
///
/// # Performance
/// `apply()` is called once per blended frame. All inner loops must live
/// *inside* the impl — never make repeated trait calls from a pixel loop.
pub trait VideoTransition: Send + Sync {
    /// Discriminant for registry lookup. Must match the `TransitionKind`
    /// variant declared alongside this transition.
    fn kind(&self) -> TransitionKind;

    /// Human-readable label shown in the UI picker (e.g. `"Dissolve"`).
    fn label(&self) -> &'static str;

    /// Emoji badge shown on the timeline clip block when this transition is
    /// active (e.g. `"🌫️"`). Keep it one glyph wide.
    fn icon(&self) -> &'static str;

    /// Default overlap duration in seconds pre-filled in the UI when the user
    /// first selects this transition.
    fn default_duration_secs(&self) -> f32 { 0.5 }

    /// Construct the serialized `TransitionType` for this transition.
    ///
    /// Called by the UI when the user selects or adjusts a transition. The UI
    /// **never** constructs `TransitionType` variants directly — always calls
    /// this so the variant shape is encapsulated in the impl.
    fn build(&self, duration_secs: f32) -> TransitionType;

    /// Blend `frame_a` and `frame_b` at `alpha` and return the packed result.
    ///
    /// `width` / `height` are luma plane dimensions in pixels.
    /// UV dims are `(width / 2, height / 2)`.
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

/// All registered transitions in stable display order.
///
/// Use for UI iteration (picker buttons, badge lookup). Cut is not included —
/// it is always a hardcoded "remove transition" action in the UI.
pub fn registered() -> Vec<Box<dyn VideoTransition>> {
    make_entries()
}

/// All registered transitions keyed by `TransitionKind` for O(1) lookup.
///
/// Use during encode and preview. Cut has no entry — short-circuit on
/// `TransitionKind::Cut` before calling this.
pub fn registry() -> HashMap<TransitionKind, Box<dyn VideoTransition>> {
    make_entries().into_iter().map(|t| (t.kind(), t)).collect()
}
// crates/velocut-core/src/transitions.rs
//
// Two transition types with different lifetimes:
//
//   TimelineTransition — serialized, stored in ProjectState, keyed by the
//       timeline clip UUID that comes BEFORE the transition. UUID-keyed so
//       transitions survive clip reordering without index invalidation.
//
//   ClipTransition — encode-only, NOT serialized, keyed by sorted clip index.
//       Built by app.rs::begin_render from TimelineTransitions.
//       Lives only for the duration of one encode job.

use uuid::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransitionType {
    /// Hard cut — no blending, zero encode overhead.
    Cut,
    /// Linear dissolve. Both clips are shortened by duration_secs so total
    /// output duration is preserved.
    Crossfade { duration_secs: f32 },
}

impl Default for TransitionType {
    fn default() -> Self { TransitionType::Cut }
}

/// Stored in ProjectState — serialized with the project.
/// Keyed by the TimelineClip UUID before the transition, not by index,
/// so it survives clip reordering without becoming stale.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimelineTransition {
    pub after_clip_id: Uuid,
    pub kind: TransitionType,
}

/// Encode-only — built by begin_render, passed in EncodeSpec. Not serialized.
#[derive(Clone, Debug)]
pub struct ClipTransition {
    /// Index into the sorted clip Vec (0 = between clips[0] and clips[1]).
    pub after_clip_index: usize,
    pub kind: TransitionType,
}
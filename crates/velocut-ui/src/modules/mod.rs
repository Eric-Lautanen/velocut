// crates/velocut-ui/src/modules/mod.rs
//
// Module registry. To add a new panel:
//   1. Create modules/mypanel.rs implementing EditorModule
//   2. Add `pub mod mypanel;` below
//   3. Add one line to the modules vec in app.rs

pub mod audio_module;
pub mod export_module;
pub mod library;
pub mod preview_module;
pub mod timeline;
pub mod video_module;

use egui::{TextureHandle, Ui};
use std::collections::HashMap;
use uuid::Uuid;
use velocut_core::commands::EditorCommand;
use velocut_core::state::ProjectState;

/// GPU-resident thumbnail cache: LibraryClip ID → loaded texture
pub type ThumbnailCache = HashMap<Uuid, TextureHandle>;

/// Every editor panel implements this trait.
/// Modules read state, emit commands — they never mutate state directly.
///
/// `thumb_cache` is passed to every `ui()` implementation for uniformity even
/// though not all modules use it (prefixed `_thumb_cache` in those impls).
/// This avoids a per-module parameter set and keeps the dispatch in `app.rs`
/// trivial. If the trait signature ever needs to evolve, wrap params in a
/// `UiContext` struct instead of adding more arguments.
pub trait EditorModule {
    fn ui(
        &mut self,
        ui: &mut Ui,
        state: &ProjectState,
        thumb_cache: &mut ThumbnailCache,
        cmd: &mut Vec<EditorCommand>,
    );
    // Non-rendering modules (AudioModule, VideoModule) expose an inherent
    // `tick()` that `app.rs` calls on the concrete type — it is deliberately not
    // part of this trait, whose only dispatch is `ui()`.
}

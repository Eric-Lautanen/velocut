// crates/velocut-ui/src/modules/mod.rs
//
// Module registry. To add a new panel:
//   1. Create modules/mypanel.rs implementing EditorModule
//   2. Add `pub mod mypanel;` below
//   3. Add one line to the modules vec in app.rs

pub mod timeline;
pub mod preview_module;
pub mod library;
pub mod export_module;
pub mod video_module;
pub mod audio_module;

use velocut_core::state::ProjectState;
use velocut_core::commands::EditorCommand;
use egui::{Ui, TextureHandle};
use std::collections::HashMap;
use uuid::Uuid;

/// GPU-resident thumbnail cache: LibraryClip ID → loaded texture
pub type ThumbnailCache = HashMap<Uuid, TextureHandle>;

/// Every editor panel implements this trait.
/// Modules read state, emit commands — they never mutate state directly.
pub trait EditorModule {
    fn name(&self) -> &str;
    fn ui(
        &mut self,
        ui:         &mut Ui,
        state:      &ProjectState,
        thumb_cache: &mut ThumbnailCache,
        cmd:        &mut Vec<EditorCommand>,
    );
    /// Called every frame after commands are processed.
    /// Non-rendering modules (e.g. AudioModule) use this instead of ui().
    /// Default is a no-op so existing modules don't need to implement it.
    fn tick(&mut self, _state: &ProjectState, _ctx: &mut crate::context::AppContext) {}
}
// src/modules/mod.rs
//
// Module registry — add new editor panels here.
// Each module implements EditorModule and lives in its own file.
// To add a new panel:
//   1. Create src/modules/mypanel.rs implementing EditorModule
//   2. Add `pub mod mypanel;` below
//   3. Register it in app.rs modules vec

pub mod timeline;
pub mod preview;
pub mod library;
pub mod export;

use crate::state::ProjectState;
use egui::{Ui, TextureHandle};
use std::collections::HashMap;
use uuid::Uuid;

/// GPU-resident thumbnail cache: LibraryClip ID → loaded texture
pub type ThumbnailCache = HashMap<Uuid, TextureHandle>;

/// Every editor panel implements this trait.
/// Add optional lifecycle hooks here as needed in the future.
pub trait EditorModule {
    /// Unique name — used to look up the module in app.rs panels
    fn name(&self) -> &str;

    /// Called every frame for the panel's content area
    fn ui(&mut self, ui: &mut Ui, state: &mut ProjectState, thumb_cache: &mut ThumbnailCache);
}
// crates/velocut-ui/src/modules/mod.rs
//
// Module registry. To add a new panel:
//   1. Create modules/mypanel.rs implementing EditorModule
//   2. Add `pub mod mypanel;` below
//   3. Add one line to register_modules() in app.rs

pub mod timeline;
pub mod preview;
pub mod library;
pub mod export;

use velocut_core::state::ProjectState;
use egui::{Ui, TextureHandle};
use std::collections::HashMap;
use uuid::Uuid;

/// GPU-resident thumbnail cache: LibraryClip ID → loaded texture
pub type ThumbnailCache = HashMap<Uuid, TextureHandle>;

/// Every editor panel implements this trait.
pub trait EditorModule {
    fn name(&self) -> &str;
    fn ui(&mut self, ui: &mut Ui, state: &mut ProjectState, thumb_cache: &mut ThumbnailCache);
}
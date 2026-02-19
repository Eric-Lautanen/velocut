// crates/velocut-ui/src/modules/library.rs
//
// MediaLibrary panel — thumbnail grid with multi-select and drag-to-timeline.
//
// Selection model:
//   • Plain click      → single-select (clears multi-select)
//   • Ctrl+click       → toggle clip in multi-select set
//   • Shift+click      → range-select from last single-selected to clicked
//   • Delete/Backspace → delete all selected (multi or single)
//   • Drag             → always drags only the card under the pointer
//   • Bg click         → clear all selection
//
// Multi-select state lives on LibraryModule (pure UI, not serialised).
// ProjectState::selected_library_clip is kept as the "anchor" for range
// selection and for downstream modules that only care about one clip.
//
// Grid layout uses manual row chunking — the only approach that wraps
// reliably inside a vertical ScrollArea regardless of egui version.

use super::EditorModule;
use velocut_core::state::{ProjectState, ClipType};
use velocut_core::commands::EditorCommand;
use velocut_core::helpers::time::format_duration;
use crate::helpers::format::truncate;
use crate::modules::ThumbnailCache;
use crate::theme::{
    ACCENT, DARK_BG_0, DARK_BG_2, DARK_BG_3, DARK_BG_4,
    DARK_BORDER, DARK_TEXT, DARK_TEXT_DIM,
};
use egui::{Align, Color32, Id, LayerId, Layout, Order, RichText, Sense, Stroke, Ui};
use rfd::FileDialog;
use std::collections::HashSet;
use uuid::Uuid;

// ── Layout constants ──────────────────────────────────────────────────────────
const CARD_W:   f32 = 96.0;   // outer width  (includes border)
const CARD_H:   f32 = 94.0;   // outer height
const THUMB_W:  f32 = 86.0;   // image width  inside card
const THUMB_H:  f32 = 48.0;   // image height inside card
const CARD_GAP: f32 = 6.0;    // gap between cards
const CARD_PAD: f32 = 8.0;    // left / right inset of the grid

// ── Multi-select colour ───────────────────────────────────────────────────────
const SEL_MULTI: Color32 = Color32::from_rgb(255, 160, 50);   // same as ACCENT
const SEL_CHECK: Color32 = Color32::from_rgb(255, 200, 80);   // check-mark tint

// ── Module ────────────────────────────────────────────────────────────────────

pub struct LibraryModule {
    /// All clips currently in the multi-selection set.
    /// Single-click replaces this with exactly one id (or clears it).
    pub multi_selection: HashSet<Uuid>,
    /// Clip IDs whose cards fell within the scroll viewport last frame.
    /// Populated during ui() via `ui.is_rect_visible(card_rect)` and
    /// consumed by app.rs::poll_media the following frame to sort
    /// pending_probes — visible clips are dispatched to the probe semaphore
    /// first so their thumbnails appear before off-screen clips.
    pub visible_ids: HashSet<Uuid>,
}

impl LibraryModule {
    pub fn new() -> Self {
        Self { multi_selection: HashSet::new(), visible_ids: HashSet::new() }
    }

    /// True if `id` is considered "selected" in any mode.
    fn is_any_selected(&self, id: Uuid, state: &ProjectState) -> bool {
        self.multi_selection.contains(&id)
            || state.selected_library_clip == Some(id)
    }

    /// Clear everything — both the module set and the state anchor.
    fn clear_selection(&mut self, cmd: &mut Vec<EditorCommand>) {
        self.multi_selection.clear();
        cmd.push(EditorCommand::SelectLibraryClip(None));
        cmd.push(EditorCommand::SelectTimelineClip(None));
    }
}

impl Default for LibraryModule {
    fn default() -> Self { Self::new() }
}

impl EditorModule for LibraryModule {
    fn name(&self) -> &str { "Media Library" }

    fn ui(
        &mut self,
        ui:          &mut Ui,
        state:       &ProjectState,
        thumb_cache: &mut ThumbnailCache,
        cmd:         &mut Vec<EditorCommand>,
    ) {
        // Refresh visible-card set each frame so poll_media has an up-to-date
        // list for probe prioritisation. Must be cleared before the layout pass
        // because paint_card re-inserts every card that is actually on-screen.
        self.visible_ids.clear();

        let ctrl  = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
        let shift = ui.input(|i| i.modifiers.shift);

        // ── Hotkeys ──────────────────────────────────────────────────────────
        if ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
            if !self.multi_selection.is_empty() {
                // Delete every selected clip
                for id in self.multi_selection.drain() {
                    cmd.push(EditorCommand::DeleteLibraryClip(id));
                }
                cmd.push(EditorCommand::SelectLibraryClip(None));
            } else if let Some(id) = state.selected_library_clip {
                cmd.push(EditorCommand::DeleteLibraryClip(id));
            }
        }

        // Ctrl-A → select all
        if ctrl && ui.input(|i| i.key_pressed(egui::Key::A)) {
            for clip in &state.library {
                self.multi_selection.insert(clip.id);
            }
            if let Some(last) = state.library.last() {
                cmd.push(EditorCommand::SelectLibraryClip(Some(last.id)));
            }
        }

        // Escape → clear selection
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.clear_selection(cmd);
        }

        // ── Column count (must be before ScrollArea) ─────────────────────────
        let usable_w = (ui.available_width() - CARD_PAD * 2.0).max(CARD_W);
        let cols     = ((usable_w + CARD_GAP) / (CARD_W + CARD_GAP)).floor() as usize;
        let cols     = cols.max(1);

        // ── Layout ───────────────────────────────────────────────────────────
        ui.vertical(|ui| {
            header_bar(ui, cmd);
            status_strip(ui, state, &self.multi_selection);
            ui.add_space(1.0);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Background click → clear all selection
                    let bg = ui.interact(
                        ui.available_rect_before_wrap(),
                        Id::new("library_bg"),
                        Sense::click(),
                    );
                    if bg.clicked() && !ctrl {
                        self.clear_selection(cmd);
                    }

                    if state.library.is_empty() {
                        empty_state(ui);
                        return;
                    }

                    ui.add_space(CARD_PAD);

                    // Collect ids in display order for range selection
                    let ids: Vec<Uuid> = state.library.iter().map(|c| c.id).collect();
                    let anchor_idx = state.selected_library_clip
                        .and_then(|a| ids.iter().position(|&x| x == a));

                    let mut to_delete:    Vec<Uuid>    = Vec::new();
                    let mut new_single:   Option<Uuid> = None;
                    let mut toggle_id:    Option<Uuid> = None;
                    let mut range_to_idx: Option<usize> = None;
                    let mut drag_started_id: Option<Uuid> = None;

                    for row in state.library.chunks(cols) {
                        ui.horizontal(|ui| {
                            ui.add_space(CARD_PAD);
                            for clip in row {
                                let id          = clip.id;
                                let item_id     = Id::new("lib_clip").with(id);
                                let in_multi    = self.multi_selection.contains(&id);
                                let is_anchor   = state.selected_library_clip == Some(id);
                                let is_selected = in_multi || is_anchor;
                                let is_dragging = ui.ctx().is_being_dragged(item_id);

                                if is_dragging {
                                    paint_drag_ghost(ui, id, &clip.name, clip.clip_type, thumb_cache);
                                    ui.memory_mut(|m| m.data.insert_temp(Id::new("DND_PAYLOAD"), id));
                                }

                                let card_resp = paint_card(ui, clip, is_selected, in_multi, is_dragging, thumb_cache);

                                // Record whether this card is within the scroll
                                // viewport so poll_media can probe it first.
                                if ui.is_rect_visible(card_resp.rect) {
                                    self.visible_ids.insert(id);
                                }

                                let interact = ui.interact(
                                    card_resp.rect,
                                    item_id,
                                    Sense::click_and_drag(),
                                );

                                // ── Click handling ────────────────────────────
                                if interact.clicked() {
                                    if shift {
                                        // Range select: anchor → clicked
                                        if let Some(to) = ids.iter().position(|&x| x == id) {
                                            range_to_idx = Some(to);
                                        }
                                    } else if ctrl {
                                        toggle_id = Some(id);
                                    } else {
                                        // Plain click: single select
                                        new_single = Some(id);
                                    }
                                }

                                if interact.drag_started() {
                                    drag_started_id = Some(id);
                                }

                                ui.ctx().set_cursor_icon(if interact.dragged() {
                                    egui::CursorIcon::Grabbing
                                } else if interact.hovered() {
                                    egui::CursorIcon::Grab
                                } else {
                                    egui::CursorIcon::Default
                                });

                                // ── Context menu ──────────────────────────────
                                interact.context_menu(|ui| {
                                    context_menu(
                                        ui, clip, is_selected, &self.multi_selection, &mut to_delete,
                                    );
                                });

                                ui.add_space(CARD_GAP);
                            }
                        });
                        ui.add_space(CARD_GAP);
                    }

                    // ── Apply deferred mutations ──────────────────────────────
                    // (egui forbids state mutation inside layout closures above)

                    if let Some(id) = drag_started_id {
                        // Drag always uses the card under pointer, regardless of multi-select
                        cmd.push(EditorCommand::SelectLibraryClip(Some(id)));
                        ui.memory_mut(|m| m.data.insert_temp(Id::new("DND_PAYLOAD"), id));
                    }

                    if let Some(id) = toggle_id {
                        if self.multi_selection.contains(&id) {
                            self.multi_selection.remove(&id);
                            if state.selected_library_clip == Some(id) {
                                // Move anchor to another selected clip if possible
                                let next = self.multi_selection.iter().next().copied();
                                cmd.push(EditorCommand::SelectLibraryClip(next));
                            }
                        } else {
                            self.multi_selection.insert(id);
                            // Keep anchor on the last toggled-in clip
                            cmd.push(EditorCommand::SelectLibraryClip(Some(id)));
                        }
                    } else if let Some(to) = range_to_idx {
                        if let Some(from) = anchor_idx {
                            let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
                            for &range_id in &ids[lo..=hi] {
                                self.multi_selection.insert(range_id);
                            }
                            cmd.push(EditorCommand::SelectLibraryClip(Some(ids[to])));
                        }
                    } else if let Some(id) = new_single {
                        // Plain click clears multi-select and sets single anchor
                        self.multi_selection.clear();
                        cmd.push(EditorCommand::SelectLibraryClip(Some(id)));
                        cmd.push(EditorCommand::SelectTimelineClip(None));
                    }

                    for id in &to_delete {
                        self.multi_selection.remove(id);
                        cmd.push(EditorCommand::DeleteLibraryClip(*id));
                    }
                    if !to_delete.is_empty() {
                        cmd.push(EditorCommand::SelectLibraryClip(None));
                    }

                    ui.add_space(CARD_PAD);
                });
        });
    }
}

// ── Header bar ────────────────────────────────────────────────────────────────

fn header_bar(ui: &mut Ui, cmd: &mut Vec<EditorCommand>) {
    egui::Frame::new()
        .fill(DARK_BG_2)
        .inner_margin(egui::Margin { left: 10, right: 8, top: 7, bottom: 7 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("MEDIA BIN")
                        .size(10.0).strong()
                        .color(DARK_TEXT_DIM)
                        .extra_letter_spacing(1.5),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let btn = egui::Button::new(RichText::new("＋ Import").size(11.0))
                        .corner_radius(egui::CornerRadius::same(4))
                        .fill(DARK_BG_4);
                    if ui.add(btn).clicked() {
                        if let Some(paths) = FileDialog::new()
                            .add_filter("Media", &["mp4","mov","mkv","avi","mp3","wav","webm","m4v"])
                            .pick_files()
                        {
                            for path in paths {
                                cmd.push(EditorCommand::ImportFile(path));
                            }
                        }
                    }
                });
            });
        });
}

// ── Status strip ─────────────────────────────────────────────────────────────

fn status_strip(ui: &mut Ui, state: &ProjectState, multi: &HashSet<Uuid>) {
    if state.library.is_empty() { return; }

    egui::Frame::new()
        .fill(DARK_BG_0)
        .inner_margin(egui::Margin { left: 10, right: 8, top: 3, bottom: 3 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let n = state.library.len();
                ui.label(
                    RichText::new(format!("{n} clip{}", if n == 1 { "" } else { "s" }))
                        .size(10.0).color(DARK_TEXT_DIM),
                );

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if multi.len() > 1 {
                        // Multi-select hint
                        ui.label(
                            RichText::new(format!("{} selected  ⌫", multi.len()))
                                .size(9.5)
                                .color(ACCENT),
                        );
                    } else if !multi.is_empty() || state.selected_library_clip.is_some() {
                        ui.label(
                            RichText::new("⌫ delete  •  ⇧click range  •  ⌃click multi")
                                .size(8.5)
                                .color(Color32::from_rgb(72, 72, 88)),
                        );
                    }
                });
            });
        });
}

// ── Card ──────────────────────────────────────────────────────────────────────

fn paint_card(
    ui:          &mut Ui,
    clip:        &velocut_core::state::LibraryClip,
    is_selected: bool,
    in_multi:    bool,
    is_dragging: bool,
    thumb_cache: &ThumbnailCache,
) -> egui::Response {
    let highlight  = is_selected || is_dragging;
    let fill_col   = if highlight { DARK_BG_4 } else { DARK_BG_3 };
    let border_col = if highlight { SEL_MULTI } else { DARK_BORDER };
    let border_w   = if highlight { 1.5 } else { 1.0 };
    let name_col   = if is_selected { DARK_TEXT } else { DARK_TEXT_DIM };
    let dur_col    = if is_selected { ACCENT } else { Color32::from_rgb(130, 90, 35) };

    let resp = egui::Frame::new()
        .fill(fill_col)
        .stroke(Stroke::new(border_w, border_col))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(4))
        .show(ui, |ui| {
            ui.set_width(CARD_W - 8.0);
            ui.set_height(CARD_H - 8.0);
            ui.vertical_centered(|ui| {
                // Thumbnail / placeholder
                if let Some(tex) = thumb_cache.get(&clip.id) {
                    ui.add(
                        egui::Image::new((tex.id(), egui::vec2(THUMB_W, THUMB_H)))
                            .corner_radius(egui::CornerRadius::same(3)),
                    );
                } else {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(THUMB_W, THUMB_H), Sense::hover());
                    ui.painter().rect_filled(rect, 3.0, Color32::from_rgb(14, 14, 20));
                    ui.painter().text(
                        rect.center(), egui::Align2::CENTER_CENTER,
                        clip_icon(clip.clip_type), egui::FontId::proportional(20.0),
                        Color32::from_gray(50),
                    );
                }
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(clip.name.as_str()).size(10.0).color(name_col)).truncate());
                let dur = if clip.duration_probed { format_duration(clip.duration) } else { "⏳".into() };
                ui.label(RichText::new(dur).size(9.0).monospace().color(dur_col));
            });
        })
        .response;

    // Multi-select check badge — top-right corner tick ✓
    if in_multi {
        let badge_size = 14.0_f32;
        let badge_rect = egui::Rect::from_min_size(
            egui::pos2(resp.rect.max.x - badge_size, resp.rect.min.y),
            egui::vec2(badge_size, badge_size),
        );
        let p = ui.painter();
        p.rect_filled(badge_rect, egui::CornerRadius::same(3), ACCENT);
        p.text(
            badge_rect.center(),
            egui::Align2::CENTER_CENTER,
            "✓",
            egui::FontId::proportional(9.0),
            SEL_CHECK,
        );
    }

    resp
}

// ── Context menu ──────────────────────────────────────────────────────────────

fn context_menu(
    ui:        &mut Ui,
    clip:      &velocut_core::state::LibraryClip,
    is_sel:    bool,
    multi:     &HashSet<Uuid>,
    to_delete: &mut Vec<Uuid>,
) {
    ui.set_min_width(160.0);

    // Clip info header
    ui.label(RichText::new(truncate(&clip.name, 28)).size(10.5).color(DARK_TEXT));
    if clip.duration_probed {
        ui.label(RichText::new(format_duration(clip.duration)).size(10.0).color(DARK_TEXT_DIM));
    }
    if let Some((w, h)) = clip.video_size {
        ui.label(RichText::new(format!("{w} × {h}")).size(9.5).color(DARK_TEXT_DIM));
    }

    ui.separator();

    let multi_count = multi.len();
    if is_sel && multi_count > 1 {
        // Offer to delete all selected
        if ui.button(format!("🗑  Remove {multi_count} clips")).clicked() {
            to_delete.extend(multi.iter().copied());
            ui.close();
        }
    }

    if ui.button("🗑  Remove from project").clicked() {
        to_delete.push(clip.id);
        ui.close();
    }
}

// ── Drag ghost ────────────────────────────────────────────────────────────────

fn paint_drag_ghost(
    ui:          &Ui,
    id:          Uuid,
    name:        &str,
    clip_type:   ClipType,
    thumb_cache: &ThumbnailCache,
) {
    let Some(ptr) = ui.ctx().pointer_interact_pos() else { return };
    let rect  = egui::Rect::from_center_size(ptr, egui::vec2(CARD_W + 10.0, THUMB_H + 20.0));
    let layer = LayerId::new(Order::Tooltip, Id::new("drag_ghost"));
    let p     = ui.ctx().layer_painter(layer);

    // Shadow
    p.rect_filled(rect.translate(egui::vec2(3.0, 4.0)), egui::CornerRadius::same(6),
        Color32::from_black_alpha(80));
    // Body
    p.rect_filled(rect, egui::CornerRadius::same(6),
        Color32::from_rgba_unmultiplied(34, 62, 120, 220));
    p.rect_stroke(rect, egui::CornerRadius::same(6),
        Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);

    let thumb_rect = rect.shrink2(egui::vec2(5.0, 3.0)).with_max_y(rect.max.y - 16.0);
    if let Some(tex) = thumb_cache.get(&id) {
        p.image(tex.id(), thumb_rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
            Color32::from_rgba_unmultiplied(255, 255, 255, 210));
    } else {
        p.rect_filled(thumb_rect, 3.0, Color32::from_rgb(14, 14, 20));
        p.text(thumb_rect.center(), egui::Align2::CENTER_CENTER,
            clip_icon(clip_type), egui::FontId::proportional(18.0), Color32::WHITE);
    }
    p.text(
        egui::pos2(rect.center().x, rect.max.y - 9.0),
        egui::Align2::CENTER_CENTER,
        truncate(name, 18),
        egui::FontId::proportional(10.0),
        Color32::from_rgba_unmultiplied(210, 210, 225, 220),
    );
}

// ── Empty state ───────────────────────────────────────────────────────────────

fn empty_state(ui: &mut Ui) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("🎬").size(36.0));
        ui.add_space(8.0);
        ui.label(RichText::new("Drop files here").size(12.0).color(DARK_TEXT_DIM));
        ui.label(RichText::new("or click  ＋ Import  above").size(10.5).color(Color32::from_gray(62)));
        ui.add_space(16.0);
        ui.label(RichText::new("Supports MP4, MOV, MKV, AVI, MP3, WAV").size(9.0).color(Color32::from_gray(48)));
    });
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn clip_icon(t: ClipType) -> &'static str {
    match t { ClipType::Video => "🎬", ClipType::Audio => "🎵" }
}
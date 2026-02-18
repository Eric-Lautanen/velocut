// crates/velocut-ui/src/modules/library.rs
use super::EditorModule;
use velocut_core::state::{ProjectState, ClipType};
use velocut_core::commands::EditorCommand;
use crate::modules::ThumbnailCache;
use crate::theme::{ACCENT, DARK_BG_2, DARK_BG_3, DARK_BG_4, DARK_BORDER, DARK_TEXT_DIM};
use egui::{Ui, RichText, Layout, Align, Id, Sense, Color32, Stroke, Order, LayerId};
use rfd::FileDialog;

pub struct LibraryModule;

impl EditorModule for LibraryModule {
    fn name(&self) -> &str { "Media Library" }

    fn ui(&mut self, ui: &mut Ui, state: &ProjectState, thumb_cache: &mut ThumbnailCache, cmd: &mut Vec<EditorCommand>) {
        // ── Hotkeys ──────────────────────────────────────────────────────────
        if ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
            if let Some(id) = state.selected_library_clip {
                cmd.push(EditorCommand::DeleteLibraryClip(id));
            }
        }

        ui.vertical(|ui| {
            // ── Header ──────────────────────────────────────────────────────
            egui::Frame::new()
                .fill(DARK_BG_2)
                .inner_margin(egui::Margin { left: 8, right: 8, top: 6, bottom: 6 })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("🗂 Media Bin").size(12.0).strong());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button(RichText::new("＋ Import").size(11.0)).clicked() {
                                if let Some(path) = FileDialog::new()
                                    .add_filter("Media", &["mp4","mov","mkv","avi","mp3","wav","webm","m4v"])
                                    .pick_file()
                                {
                                    cmd.push(EditorCommand::ImportFile(path));
                                }
                            }
                        });
                    });
                });

            ui.separator();

            if !state.library.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new(format!("{} clips", state.library.len()))
                        .size(10.0).color(DARK_TEXT_DIM));
                    if state.selected_library_clip.is_some() {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add_space(6.0);
                            ui.label(RichText::new("Del to remove").size(9.0).color(DARK_TEXT_DIM));
                        });
                    }
                });
            }

            // ── Clip Grid ───────────────────────────────────────────────────
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(4.0);

                let scroll_resp = ui.interact(
                    ui.available_rect_before_wrap(),
                    egui::Id::new("library_bg"),
                    egui::Sense::click(),
                );
                if scroll_resp.clicked() {
                    cmd.push(EditorCommand::SelectLibraryClip(None));
                    cmd.push(EditorCommand::SelectTimelineClip(None));
                }

                if state.library.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("🎬").size(32.0));
                        ui.add_space(6.0);
                        ui.label(RichText::new("Drop files here\nor use Import")
                            .size(11.0).color(DARK_TEXT_DIM));
                    });
                    return;
                }

                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);

                    let mut to_delete: Option<uuid::Uuid> = None;

                    for clip in &state.library {
                        let id         = clip.id;
                        let name       = &clip.name;
                        let clip_type  = clip.clip_type;
                        let duration   = clip.duration;
                        let probed     = clip.duration_probed;
                        let item_id          = Id::new("lib_clip").with(id);
                        let is_selected      = state.selected_library_clip == Some(id);
                        let is_being_dragged = ui.ctx().is_being_dragged(item_id);

                        // ── Drag ghost ───────────────────────────────────────
                        if is_being_dragged {
                            if let Some(ptr) = ui.ctx().pointer_interact_pos() {
                                let ghost_size  = egui::vec2(96.0, 54.0);
                                let ghost_rect  = egui::Rect::from_center_size(ptr, ghost_size);
                                let ghost_layer = LayerId::new(Order::Tooltip, Id::new("drag_ghost"));
                                let gp = ui.ctx().layer_painter(ghost_layer);
                                gp.rect_filled(ghost_rect, egui::CornerRadius::same(4),
                                    Color32::from_rgba_unmultiplied(52, 98, 168, 180));
                                gp.rect_stroke(ghost_rect, egui::CornerRadius::same(4),
                                    Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
                                if let Some(texture) = thumb_cache.get(&id) {
                                    gp.image(texture.id(), ghost_rect.shrink(3.0),
                                        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0,1.0)),
                                        Color32::from_rgba_unmultiplied(255,255,255,200));
                                } else {
                                    let icon = match clip_type { ClipType::Video => "🎬", ClipType::Audio => "🎵" };
                                    gp.text(ghost_rect.center(), egui::Align2::CENTER_CENTER, icon,
                                        egui::FontId::proportional(20.0), Color32::WHITE);
                                }
                                gp.text(ghost_rect.center_bottom() + egui::vec2(0.0, 4.0),
                                    egui::Align2::CENTER_TOP,
                                    format!("  {}  ", truncate(name, 16)),
                                    egui::FontId::proportional(10.0),
                                    Color32::from_rgba_unmultiplied(220,220,230,200));
                                ui.memory_mut(|mem| mem.data.insert_temp(Id::new("DND_PAYLOAD"), id));
                            }
                        }

                        // ── Card ─────────────────────────────────────────────
                        let border_color = if is_selected || is_being_dragged { ACCENT } else { DARK_BORDER };
                        let card_fill    = if is_selected || is_being_dragged { DARK_BG_4 } else { DARK_BG_3 };

                        let card_resp = egui::Frame::new()
                            .fill(card_fill)
                            .stroke(Stroke::new(if is_selected { 1.5 } else { 1.0 }, border_color))
                            .corner_radius(egui::CornerRadius::same(5))
                            .inner_margin(egui::Margin::same(4))
                            .show(ui, |ui| {
                                ui.set_width(92.0);
                                ui.set_height(90.0);
                                ui.vertical_centered(|ui| {
                                    if let Some(texture) = thumb_cache.get(&id) {
                                        ui.add(
                                            egui::Image::new((texture.id(), egui::vec2(84.0, 47.0)))
                                                .corner_radius(egui::CornerRadius::same(3))
                                        );
                                    } else {
                                        let (ph_rect, _) = ui.allocate_exact_size(
                                            egui::vec2(84.0, 47.0), Sense::hover());
                                        ui.painter().rect_filled(ph_rect, 3.0, Color32::from_rgb(18,18,26));
                                        let icon = match clip_type { ClipType::Video => "🎬", ClipType::Audio => "🎵" };
                                        ui.painter().text(ph_rect.center(), egui::Align2::CENTER_CENTER,
                                            icon, egui::FontId::proportional(22.0), Color32::from_gray(70));
                                    }
                                    ui.add_space(3.0);
                                    ui.add(egui::Label::new(
                                        RichText::new(name.as_str()).size(10.0).color(DARK_TEXT_DIM)
                                    ).truncate());
                                    let dur_text = if probed { format!("{:.1}s", duration) } else { "…".into() };
                                    ui.label(RichText::new(dur_text).size(9.0).color(ACCENT).monospace());
                                });
                            }).response;

                        // ── Interact ─────────────────────────────────────────
                        let interact = ui.interact(card_resp.rect, item_id, Sense::click_and_drag());

                        if interact.clicked() {
                            cmd.push(EditorCommand::SelectLibraryClip(Some(id)));
                            cmd.push(EditorCommand::SelectTimelineClip(None));
                        }
                        if interact.drag_started() {
                            cmd.push(EditorCommand::SelectLibraryClip(Some(id)));
                            ui.memory_mut(|mem| mem.data.insert_temp(Id::new("DND_PAYLOAD"), id));
                        }
                        if interact.dragged() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                        } else if interact.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                        }

                        // ── Right-click context menu ──────────────────────────
                        interact.context_menu(|ui| {
                            ui.set_min_width(140.0);
                            if ui.button("🗑  Delete clip").clicked() {
                                to_delete = Some(id);
                                ui.close();
                            }
                            ui.separator();
                            ui.label(RichText::new(truncate(name, 24)).size(10.0).color(DARK_TEXT_DIM));
                            if probed {
                                ui.label(RichText::new(format!("Duration: {duration:.2}s")).size(10.0).color(DARK_TEXT_DIM));
                            }
                        });
                    }

                    if let Some(del_id) = to_delete {
                        cmd.push(EditorCommand::DeleteLibraryClip(del_id));
                    }
                });
                ui.add_space(8.0);
            });
        });
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { return s; }
    let end = s.char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max)
        .last()
        .unwrap_or(0);
    &s[..end]
}
// crates/velocut-ui/src/modules/export.rs
use super::EditorModule;
use velocut_core::state::ProjectState;
use velocut_core::commands::EditorCommand;
use crate::modules::ThumbnailCache;
use crate::theme::{ACCENT, DARK_BG_2, DARK_BG_3, DARK_BORDER, DARK_TEXT_DIM};
use egui::{Ui, RichText, Stroke};

pub struct ExportModule {
    filename:   String,
    resolution: ResolutionPreset,
    fps:        u32,
}

#[derive(PartialEq, Clone, Copy)]
enum ResolutionPreset { HD, FHD, UHD }

impl Default for ExportModule {
    fn default() -> Self {
        Self { filename: "sequence_01".into(), resolution: ResolutionPreset::FHD, fps: 30 }
    }
}

impl EditorModule for ExportModule {
    fn name(&self) -> &str { "Export" }

    fn ui(&mut self, ui: &mut Ui, state: &ProjectState, _thumb_cache: &mut ThumbnailCache, cmd: &mut Vec<EditorCommand>) {
        ui.vertical(|ui| {
            // Header
            egui::Frame::new()
                .fill(DARK_BG_2)
                .inner_margin(egui::Margin { left: 8, right: 8, top: 6, bottom: 6 })
                .show(ui, |ui| {
                    ui.label(RichText::new("🚀 Export").size(12.0).strong());
                });

            ui.separator();
            ui.add_space(6.0);

            ui.vertical(|ui| {
                ui.add_space(4.0);

                // Filename
                ui.label(RichText::new("Output Name").size(11.0).color(DARK_TEXT_DIM));
                ui.add_space(2.0);
                ui.add(egui::TextEdit::singleline(&mut self.filename)
                    .desired_width(f32::INFINITY)
                    .hint_text("filename…")
                );

                ui.add_space(10.0);

                // Resolution
                ui.label(RichText::new("Resolution").size(11.0).color(DARK_TEXT_DIM));
                ui.add_space(2.0);
                egui::ComboBox::from_id_salt("resolution_preset")
                    .selected_text(match self.resolution {
                        ResolutionPreset::HD  => "1280 × 720  (HD)",
                        ResolutionPreset::FHD => "1920 × 1080 (FHD)",
                        ResolutionPreset::UHD => "3840 × 2160 (4K)",
                    })
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.resolution, ResolutionPreset::HD,  "1280 × 720  (HD)");
                        ui.selectable_value(&mut self.resolution, ResolutionPreset::FHD, "1920 × 1080 (FHD)");
                        ui.selectable_value(&mut self.resolution, ResolutionPreset::UHD, "3840 × 2160 (4K)");
                    });

                ui.add_space(10.0);

                // FPS
                ui.label(RichText::new("Frame Rate").size(11.0).color(DARK_TEXT_DIM));
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    for &rate in &[24u32, 30, 60] {
                        let selected = self.fps == rate;
                        let btn = egui::Button::new(
                            RichText::new(format!("{} fps", rate))
                                .size(11.0)
                                .color(if selected { ACCENT } else { DARK_TEXT_DIM })
                        )
                        .stroke(Stroke::new(1.0, if selected { ACCENT } else { DARK_BORDER }))
                        .fill(if selected { DARK_BG_3 } else { DARK_BG_2 });

                        if ui.add(btn).clicked() {
                            self.fps = rate;
                        }
                    }
                });

                ui.add_space(10.0);

                // Stats
                egui::Frame::new()
                    .fill(DARK_BG_3)
                    .stroke(Stroke::new(1.0, DARK_BORDER))
                    .corner_radius(egui::CornerRadius::same(4))
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let total = state.timeline.iter()
                            .map(|c| c.start_time + c.duration)
                            .fold(0.0_f64, f64::max);
                        let clips = state.timeline.len();
                        ui.label(RichText::new(format!("Duration:  {:.1}s", total)).size(11.0).monospace());
                        ui.label(RichText::new(format!("Clips:     {}", clips)).size(11.0).monospace());
                        ui.label(RichText::new("Format:    MP4 / H.264").size(11.0).monospace());
                    });

                ui.add_space(12.0);

                // Render button
                let render_btn = egui::Button::new(
                    RichText::new("⚡ Render MP4").size(13.0).strong().color(egui::Color32::BLACK)
                )
                .fill(ACCENT)
                .stroke(Stroke::NONE)
                .min_size(egui::vec2(ui.available_width(), 34.0));

                if ui.add(render_btn).clicked() {
                    let (width, height) = match self.resolution {
                        ResolutionPreset::HD  => (1280, 720),
                        ResolutionPreset::FHD => (1920, 1080),
                        ResolutionPreset::UHD => (3840, 2160),
                    };
                    cmd.push(EditorCommand::RenderMP4 {
                        filename: self.filename.clone(),
                        width,
                        height,
                        fps: self.fps,
                    });
                }
            });
        });
    }
}
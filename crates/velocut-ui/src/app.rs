// src/app.rs (velocut-ui)
use velocut_core::state::ProjectState;
use velocut_core::commands::EditorCommand;
use velocut_media::MediaWorker;
use velocut_media::audio::cleanup_audio_temp;
use crate::context::AppContext;
use crate::theme::configure_style;
use crate::modules::{
    EditorModule,
    timeline::TimelineModule,
    preview_module::PreviewModule,
    library::LibraryModule,
    export_module::ExportModule,
    audio_module::AudioModule,
    video_module::VideoModule,
};
use eframe::egui;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use rfd::FileDialog;

#[derive(Serialize, Deserialize)]
struct AppStorage {
    project: ProjectState,
}

// ── App ───────────────────────────────────────────────────────────────────────

pub struct VeloCutApp {
    state:        ProjectState,
    context:      AppContext,
    // Panel modules as concrete types — eliminates per-frame name-string lookup
    // and makes typos a compile error instead of a silently blank panel.
    library:      LibraryModule,
    preview:      PreviewModule,
    timeline:     TimelineModule,
    export:       ExportModule,
    /// Stored separately so tick() calls the concrete method, not the trait default no-op.
    audio:        AudioModule,
    video:        VideoModule,
    /// Commands emitted by modules each frame, processed after the UI pass
    pending_cmds: Vec<EditorCommand>,
}

impl VeloCutApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        configure_style(&cc.egui_ctx);
        // Pin to dark mode — prevents egui overwriting our theme on OS light/dark changes.
        cc.egui_ctx.options_mut(|o| {
            o.theme_preference = egui::ThemePreference::Dark;
        });

        let state = cc.storage
            .and_then(|s| eframe::get_value::<AppStorage>(s, eframe::APP_KEY))
            .map(|d| d.project)
            .unwrap_or_default();

        let media_worker = MediaWorker::new();
        for clip in &state.library {
            // Always re-probe on startup — thumbnails and audio are runtime-only
            // (temp WAVs deleted on exit, textures not serialized). Duration and
            // waveform peaks are already in state so probe just refreshes visuals.
            media_worker.probe_clip(clip.id, clip.path.clone());
        }

        let context = AppContext::new(media_worker);

        Self {
            state,
            context,
            library:      LibraryModule,
            preview:      PreviewModule::new(),
            timeline:     TimelineModule,
            export:       ExportModule::default(),
            audio:        AudioModule::new(),
            video:        VideoModule::new(),
            pending_cmds: Vec::new(),
        }
    }

    fn process_command(&mut self, cmd: EditorCommand) {
        match cmd {
            // ── Playback ─────────────────────────────────────────────────────
            EditorCommand::Play => {
                let total = self.state.total_duration();
                if total > 0.0 && self.state.current_time >= total - 0.1 {
                    self.state.current_time = 0.0;
                }
                self.state.is_playing = true;
            }
            EditorCommand::Pause => {
                self.state.is_playing = false;
            }
            EditorCommand::Stop => {
                self.state.is_playing   = false;
                self.state.current_time = 0.0;
            }
            EditorCommand::SetPlayhead(t) => {
                self.state.current_time = t;
                self.context.audio_sinks.clear();
                self.context.audio_was_playing = false;
                self.context.pending_pb_frame = None;
            }
            EditorCommand::SetVolume(v) => {
                self.state.volume = v;
            }
            EditorCommand::ToggleMute => {
                self.state.muted = !self.state.muted;
            }

            // ── Library ──────────────────────────────────────────────────────
            EditorCommand::ImportFile(path) => {
                self.state.add_to_library(path);
            }
            EditorCommand::DeleteLibraryClip(id) => {
                self.state.selected_library_clip = None;
                if let Some(apath) = self.state.library.iter()
                    .find(|c| c.id == id)
                    .and_then(|c| c.audio_path.clone())
                {
                    self.state.pending_audio_cleanup.push(apath);
                }
                self.state.library.retain(|c| c.id != id);
                self.state.timeline.retain(|c| c.media_id != id);
            }
            EditorCommand::SelectLibraryClip(id) => {
                self.state.selected_library_clip = id;
            }

            // ── Timeline ─────────────────────────────────────────────────────
            EditorCommand::AddToTimeline { media_id, at_time } => {
                self.state.add_to_timeline(media_id, at_time);
            }
            EditorCommand::DeleteTimelineClip(id) => {
                self.state.timeline.retain(|c| c.id != id);
                if self.state.selected_timeline_clip == Some(id) {
                    self.state.selected_timeline_clip = None;
                }
            }
            EditorCommand::SelectTimelineClip(id) => {
                self.state.selected_timeline_clip = id;
            }
            EditorCommand::MoveTimelineClip { id, new_start } => {
                if let Some(tc) = self.state.timeline.iter_mut().find(|c| c.id == id) {
                    tc.start_time = new_start;
                }
            }
            EditorCommand::SplitClipAt(_t) => {
                // TODO: implement split
            }
            EditorCommand::TrimClipStart { id, new_source_offset, new_duration } => {
                if let Some(tc) = self.state.timeline.iter_mut().find(|c| c.id == id) {
                    tc.source_offset = new_source_offset;
                    tc.duration      = new_duration;
                }
            }
            EditorCommand::TrimClipEnd { id, new_duration } => {
                if let Some(tc) = self.state.timeline.iter_mut().find(|c| c.id == id) {
                    tc.duration = new_duration;
                }
            }

            // ── Export ───────────────────────────────────────────────────────
            EditorCommand::RenderMP4 { filename, width, height, fps } => {
                eprintln!("[export] Render requested: {filename} {width}x{height} @ {fps}fps");
                // TODO: kick off ffmpeg render pipeline
            }

            // ── View / UI ────────────────────────────────────────────────────
            EditorCommand::SetAspectRatio(ar) => {
                self.state.aspect_ratio = ar;
            }
            EditorCommand::SetTimelineZoom(z) => {
                self.state.timeline_zoom = z;
            }
            EditorCommand::ClearSaveStatus => {
                self.state.save_status = None;
            }
            EditorCommand::RequestSaveFramePicker { path, timestamp } => {
                self.state.pending_save_pick = Some((path, timestamp));
            }
            EditorCommand::SaveFrameToDisk { path, timestamp } => {
                // Direct save (no dialog) — used for programmatic frame export
                if let Some(lib) = self.state.library.iter().find(|l| l.path == path) {
                    self.context.media_worker.extract_frame_hq(lib.id, path, timestamp, std::path::PathBuf::new());
                }
            }
        }
    }

    fn poll_media(&mut self, ctx: &egui::Context) {
        // ── Pre-frame housekeeping ────────────────────────────────────────────
        for path in self.state.pending_audio_cleanup.drain(..) {
            cleanup_audio_temp(&path);
        }

        let pending: Vec<_> = self.state.pending_probes.drain(..).collect();
        for (id, path) in pending {
            self.context.media_worker.probe_clip(id, path);
        }
        let extracts: Vec<_> = self.state.pending_extracts.drain(..).collect();
        for (id, path, ts, dest) in extracts {
            self.context.media_worker.extract_frame_hq(id, path, ts, dest);
        }

        // NOTE: The save-frame dialog ideally belongs in ExportModule since it is
        // purely an export concern and rfd is already imported there. It lives here
        // because EditorModule::ui() receives &ProjectState (read-only), so modules
        // cannot own or drain pending_save_pick themselves. If the trait is ever
        // widened to &mut ProjectState, move this block to ExportModule::ui().
        if let Some((path, ts)) = self.state.pending_save_pick.take() {
            let stem = path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let ts_label = format!("{:.3}", ts).replace('.', "_");
            let default_name = format!("{stem}_t{ts_label}.png");

            if let Some(dest) = FileDialog::new()
                .set_file_name(&default_name)
                .add_filter("PNG", &["png"])
                .save_file()
            {
                self.context.media_worker.extract_frame_hq(Uuid::nil(), path, ts, dest);
            }
        }

        // ── Playback frame consumption (PTS-gated) ────────────────────────────
        VideoModule::poll_playback(&self.state, &mut self.context, ctx);

        // ── Dispatch all queued MediaWorker results into caches / state ───────
        self.context.ingest_media_results(&mut self.state, ctx);
    }

    fn handle_drag_and_drop(&mut self, ctx: &egui::Context) {
        let files = ctx.input(|i| i.raw.dropped_files.clone());
        for file in files {
            if let Some(path) = file.path {
                self.state.add_to_library(path);
            }
        }
    }

}

// ── eframe::App ───────────────────────────────────────────────────────────────

impl eframe::App for VeloCutApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        // audio_path points to temp WAVs that are deleted on exit — clear them
        // before serializing so they don't resurrect as dead paths on next launch.
        let mut project = self.state.clone();
        for clip in &mut project.library {
            clip.audio_path = None;
        }
        eframe::set_value(storage, eframe::APP_KEY, &AppStorage { project });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.context.media_worker.shutdown();
        self.context.audio_sinks.clear();
        for clip in &self.state.library {
            if let Some(apath) = &clip.audio_path {
                cleanup_audio_temp(apath);
            }
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_drag_and_drop(ctx);
        self.poll_media(ctx);

        egui::TopBottomPanel::top("top_panel")
            .exact_height(36.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        egui::RichText::new("⚡ VeloCut")
                            .strong().size(15.0).color(crate::theme::ACCENT),
                    );
                    ui.separator();
                    ui.label(egui::RichText::new("Drop video files to import").size(12.0).weak());
                });
            });

        egui::TopBottomPanel::bottom("timeline_panel")
            .resizable(true)
            .min_height(160.0)
            .default_height(220.0)
            .show(ctx, |ui| {
                self.timeline.ui(ui, &self.state, &mut self.context.thumbnail_cache, &mut self.pending_cmds);
            });

        egui::SidePanel::left("library_panel")
            .resizable(true)
            .default_width(220.0)
            .min_width(160.0)
            .show(ctx, |ui| {
                self.library.ui(ui, &self.state, &mut self.context.thumbnail_cache, &mut self.pending_cmds);
            });

        egui::SidePanel::right("inspector_panel")
            .resizable(true)
            .default_width(220.0)
            .min_width(160.0)
            .show(ctx, |ui| {
                self.export.ui(ui, &self.state, &mut self.context.thumbnail_cache, &mut self.pending_cmds);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            // Give PreviewModule the current live frame (if any) so it can render
            // it directly. No thumbnail_cache mutation needed — the cache stays
            // pure thumbnails and PreviewModule owns its frame reference.
            let active_id = VideoModule::active_media_id(&self.state);
            self.preview.current_frame = active_id
                .and_then(|id| self.context.frame_cache.get(&id).cloned());

            self.preview.ui(ui, &self.state, &mut self.context.thumbnail_cache, &mut self.pending_cmds);
        });

        // ── Process commands emitted by modules this frame ────────────────────
        let cmds: Vec<EditorCommand> = self.pending_cmds.drain(..).collect();
        for cmd in cmds {
            self.process_command(cmd);
        }

        // ── Tick non-rendering modules (concrete calls bypass trait no-op) ────
        VideoModule::tick(&self.state, &mut self.context);
        self.audio.tick(&self.state, &mut self.context);

        if self.state.is_playing {
            let dt = ctx.input(|i| i.stable_dt as f64);
            self.state.current_time += dt;
            let total = self.state.total_duration();
            if total > 0.0 && self.state.current_time >= total {
                self.state.current_time = total - 0.001;
                self.state.is_playing   = false;
            }
            ctx.request_repaint();
        }
    }
}
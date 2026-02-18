// src/app.rs (velocut-ui)
use velocut_core::state::ProjectState;
use velocut_core::commands::EditorCommand;
use velocut_core::media_types::PlaybackFrame;
use velocut_media::{MediaWorker, MediaResult};
use velocut_media::audio::cleanup_audio_temp;
use crate::context::AppContext;
use crate::theme::{configure_style, ACCENT, DARK_BG_2, DARK_BORDER};
use crate::modules::{
    EditorModule,
    timeline::TimelineModule,
    preview::PreviewModule,
    library::LibraryModule,
    export::ExportModule,
    audio::AudioModule,
    ThumbnailCache,
};
use crate::paths::app_ffmpeg_dir;
use eframe::egui;
use ffmpeg_sidecar::command::ffmpeg_is_installed;
use ffmpeg_sidecar::download::unpack_ffmpeg;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;
use rfd::FileDialog;

#[derive(Serialize, Deserialize)]
struct AppStorage {
    project: ProjectState,
}

// ── Download progress (written by bg thread, polled by UI) ───────────────────

#[derive(Clone)]
struct DownloadProgress {
    status:        String,
    downloaded_mb: f32,
    total_mb:      f32,   // 0 = not yet known
    speed_mbps:    f32,
    done:          bool,
    error:         Option<String>,
    /// The sidecar_dir path, shown in the UI so users know where it goes
    save_path:     String,
}

impl Default for DownloadProgress {
    fn default() -> Self {
        let save_path = app_ffmpeg_dir().display().to_string();
        Self {
            status:        "Connecting…".into(),
            downloaded_mb: 0.0,
            total_mb:      0.0,
            speed_mbps:    0.0,
            done:          false,
            error:         None,
            save_path,
        }
    }
}

// ── App ───────────────────────────────────────────────────────────────────────

pub struct VeloCutApp {
    state:        ProjectState,
    context:      AppContext,
    modules:      Vec<Box<dyn EditorModule>>,
    /// Commands emitted by modules each frame, processed after the UI pass
    pending_cmds: Vec<EditorCommand>,
    ffmpeg_ready: bool,
    dl_progress:  Option<Arc<Mutex<DownloadProgress>>>,
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

        // Prepend our AppData ffmpeg dir to PATH
        let ffmpeg_dir = app_ffmpeg_dir();
        let current_path = std::env::var("PATH").unwrap_or_default();
        #[cfg(target_os = "windows")]
        std::env::set_var("PATH", format!("{};{current_path}", ffmpeg_dir.display()));
        #[cfg(not(target_os = "windows"))]
        std::env::set_var("PATH", format!("{}:{current_path}", ffmpeg_dir.display()));
        let ffmpeg_ready = ffmpeg_is_installed();

        let dl_progress = if !ffmpeg_ready {
            let prog     = Arc::new(Mutex::new(DownloadProgress::default()));
            let prog_tx  = prog.clone();
            let ctx_tx   = cc.egui_ctx.clone();
            std::thread::spawn(move || run_download(prog_tx, ctx_tx));
            Some(prog)
        } else {
            None
        };

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
            modules: vec![
                Box::new(LibraryModule),
                Box::new(PreviewModule),
                Box::new(TimelineModule),
                Box::new(ExportModule::default()),
                Box::new(AudioModule::new()),
            ],
            pending_cmds: Vec::new(),
            ffmpeg_ready,
            dl_progress,
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
                // Clear audio sinks whenever the playhead is moved manually —
                // the existing sink decoder is at the wrong position and must
                // be rebuilt with a fresh seek when playback resumes.
                self.context.audio_sinks.clear();
                self.context.audio_was_playing = false;
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
        // Clean up temp WAVs for deleted clips
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
        // ── Drain playback frame buffer ───────────────────────────────────────
        // Collect all available frames; upload only the last one (avoids
        // uploading stale frames that arrive in a burst). Using the same texture
        // name per media id means egui reuses the GPU allocation each frame.
        {
            let mut last: Option<PlaybackFrame> = None;
            while let Ok(f) = self.context.media_worker.pb_rx.try_recv() { last = Some(f); }
            if let Some(f) = last {
                let tex = ctx.load_texture(
                    format!("pb-{}", f.id),
                    egui::ColorImage::from_rgba_unmultiplied(
                        [f.width as usize, f.height as usize], &f.data,
                    ),
                    egui::TextureOptions::LINEAR,
                );
                self.context.frame_cache.insert(f.id, tex);
                ctx.request_repaint();
            }
        }

        while let Ok(result) = self.context.media_worker.rx.try_recv() {
            match result {
                MediaResult::AudioPath { id, path } => {
                    eprintln!("[audio] AudioPath arrived id={id} path={}", path.display());
                    if let Some(clip) = self.state.library.iter_mut().find(|c| c.id == id) {
                        clip.audio_path = Some(path);
                    }
                }
                MediaResult::Duration { id, seconds } => {
                    self.state.update_clip_duration(id, seconds);
                    ctx.request_repaint();
                }
                MediaResult::Thumbnail { id, width, height, data } => {
                    let tex = ctx.load_texture(
                        format!("thumb-{id}"),
                        egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize], &data,
                        ),
                        egui::TextureOptions::LINEAR,
                    );
                    self.context.thumbnail_cache.insert(id, tex);
                    ctx.request_repaint();
                }
                MediaResult::Waveform { id, peaks } => {
                    self.state.update_waveform(id, peaks);
                    ctx.request_repaint();
                }
                MediaResult::VideoSize { id, width, height } => {
                    if let Some(clip) = self.state.library.iter_mut().find(|c| c.id == id) {
                        clip.video_size = Some((width, height));
                    }
                    let is_first = self.state.library.iter()
                        .filter(|c| c.video_size.is_some()).count() == 1;
                    if is_first && width > 0 && height > 0 {
                        use velocut_core::state::AspectRatio;
                        let r = width as f32 / height as f32;
                        self.state.aspect_ratio =
                            if      (r - 16.0/9.0 ).abs() < 0.05 { AspectRatio::SixteenNine   }
                            else if (r - 9.0/16.0 ).abs() < 0.05 { AspectRatio::NineSixteen   }
                            else if (r - 2.0/3.0  ).abs() < 0.05 { AspectRatio::TwoThree      }
                            else if (r - 3.0/2.0  ).abs() < 0.05 { AspectRatio::ThreeTwo      }
                            else if (r - 4.0/3.0  ).abs() < 0.05 { AspectRatio::FourThree     }
                            else if (r - 1.0      ).abs() < 0.05 { AspectRatio::OneOne        }
                            else if (r - 4.0/5.0  ).abs() < 0.05 { AspectRatio::FourFive      }
                            else if (r - 21.0/9.0 ).abs() < 0.10 { AspectRatio::TwentyOneNine }
                            else if (r - 2.39     ).abs() < 0.05 { AspectRatio::Anamorphic    }
                            else if r > 1.0 { AspectRatio::SixteenNine }
                            else            { AspectRatio::NineSixteen };
                        eprintln!("[app] aspect ratio auto-set from {width}x{height}");
                        ctx.request_repaint();
                    }
                }
                MediaResult::FrameSaved { path } => {
                    eprintln!("[app] frame PNG saved → {:?}", path);
                    let name = path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "frame".into());
                    self.state.save_status = Some(format!("✓ Saved: {}", name));
                    ctx.request_repaint();
                }
                MediaResult::VideoFrame { id, width, height, data } => {
                    let tex = ctx.load_texture(
                        format!("frame-{id}"),
                        egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize], &data,
                        ),
                        egui::TextureOptions::LINEAR,
                    );
                    // Always store in bucket cache keyed by the fine bucket (1/4 s grid)
                    // so that both coarse prefetch and fine decode populate the same lookup.
                    // The timestamp we used to request is reconstructed from last_frame_req;
                    // for coarse results we use scrub_coarse_req bucket scaled back up.
                    let bucket = self.context.last_frame_req
                        .filter(|(rid, _)| *rid == id)
                        .map(|(_, b)| b)
                        .or_else(|| self.context.scrub_coarse_req
                            .filter(|(rid, _)| *rid == id)
                            .map(|(_, cb)| cb * 8)) // coarse bucket (2 s) → fine bucket (¼ s)
                        .unwrap_or_else(|| {
                            // Fallback: derive from current playhead position
                            self.state.timeline.iter()
                                .find(|c| c.media_id == id)
                                .map(|c| {
                                    let lt = (self.state.current_time - c.start_time).max(0.0);
                                    (lt * 4.0) as u32
                                })
                                .unwrap_or(0)
                        });
                    if self.context.frame_bucket_cache.len() >= 128 {
                        let to_remove: Vec<_> = self.context.frame_bucket_cache.keys()
                            .take(32).copied().collect();
                        for k in to_remove { self.context.frame_bucket_cache.remove(&k); }
                    }
                    self.context.frame_bucket_cache.insert((id, bucket), tex.clone());
                    self.context.frame_cache.insert(id, tex);
                    ctx.request_repaint();
                }
                MediaResult::Error { id, msg } => {
                    eprintln!("[media] {id}: {msg}");
                }
            }
        }
    }

    fn tick_preview_frame(&mut self) {
        let just_started  = self.state.is_playing && !self.context.prev_playing;
        let just_stopped  = !self.state.is_playing && self.context.prev_playing;
        self.context.prev_playing = self.state.is_playing;

        let current_clip = self.state.timeline.iter().find(|c| {
            self.state.current_time >= c.start_time
                && self.state.current_time < c.start_time + c.duration
        }).cloned();

        if self.state.is_playing {
            if let Some(clip) = &current_clip {
                let clip_changed = Some(clip.media_id) != self.context.playback_media_id;
                if just_started || clip_changed {
                    self.context.playback_media_id = Some(clip.media_id);
                    if let Some(lib) = self.state.library.iter().find(|l| l.id == clip.media_id) {
                        let local_ts = (self.state.current_time - clip.start_time + clip.source_offset).max(0.0);
                        let aspect   = self.state.active_video_ratio();
                        self.context.media_worker.start_playback(lib.id, lib.path.clone(), local_ts, aspect);
                    }
                }
            }
            return;
        }

        if just_stopped {
            self.context.media_worker.stop_playback();
            self.context.playback_media_id = None;
            self.context.last_frame_req    = None;
            self.context.scrub_last_moved  = None;
            self.context.scrub_coarse_req  = None;
        }

        let Some(clip) = current_clip else {
            self.context.last_frame_req   = None;
            self.context.scrub_last_moved = None;
            self.context.scrub_coarse_req = None;
            return;
        };

        let local_t       = (self.state.current_time - clip.start_time + clip.source_offset).max(0.0);
        let fine_bucket   = (local_t * 4.0) as u32;          // ¼s grid
        let coarse_bucket = (local_t / 2.0) as u32;          // 2s grid
        let fine_key      = (clip.media_id, fine_bucket);

        let scrub_moved = self.context.last_frame_req.map(|k| k != fine_key).unwrap_or(true);

        if scrub_moved {
            self.context.scrub_last_moved = Some(std::time::Instant::now());

            if let Some((prev_id, _)) = self.context.last_frame_req {
                if prev_id != clip.media_id {
                    self.context.frame_cache.remove(&prev_id);
                    self.context.scrub_coarse_req = None;
                }
            }
            self.context.last_frame_req = Some(fine_key);

            // ── Layer 1 (0ms): show nearest cached frame instantly ───────────
            let found_nearby = (0..=8u32).find_map(|delta| {
                let b = fine_bucket.saturating_sub(delta);
                self.context.frame_bucket_cache.get(&(clip.media_id, b)).cloned()
            });
            if let Some(cached) = found_nearby {
                self.context.frame_cache.insert(clip.media_id, cached);
            }

            // ── Layer 2 (per fine bucket): decode exact current position ─────
            // Fire on every ¼s bucket change — the latest-wins slot in MediaWorker
            // means rapid scrubbing only ever decodes the most recent position.
            if !self.context.frame_bucket_cache.contains_key(&fine_key) {
                if let Some(lib) = self.state.library.iter().find(|m| m.id == clip.media_id) {
                    let aspect = self.state.active_video_ratio();
                    let ts     = fine_bucket as f64 / 4.0;
                    self.context.media_worker.request_frame(lib.id, lib.path.clone(), ts, aspect);
                }
            }

            // ── Layer 2b (per 2s): coarse warm-up prefetch ──────────────────
            // Pre-fills the bucket cache ahead of the scrub head so Layer 1
            // gets more hits when scrubbing into new territory.
            let coarse_key = (clip.media_id, coarse_bucket);
            if self.context.scrub_coarse_req != Some(coarse_key)
                && !self.context.frame_bucket_cache.contains_key(&fine_key)
            {
                self.context.scrub_coarse_req = Some(coarse_key);
                if let Some(lib) = self.state.library.iter().find(|m| m.id == clip.media_id) {
                    let aspect = self.state.active_video_ratio();
                    let ts     = coarse_bucket as f64 * 2.0;
                    self.context.media_worker.request_frame(lib.id, lib.path.clone(), ts, aspect);
                }
            }
        } else {
            // ── Layer 3 (150ms idle): precise frame after scrub stops ────────
            if self.context.frame_cache.contains_key(&clip.media_id) {
                let idle = self.context.scrub_last_moved
                    .map(|t| t.elapsed() >= std::time::Duration::from_millis(150))
                    .unwrap_or(false);
                if !idle { return; }
                if self.context.frame_bucket_cache.contains_key(&fine_key) { return; }
                if let Some(lib) = self.state.library.iter().find(|m| m.id == clip.media_id) {
                    let aspect = self.state.active_video_ratio();
                    let ts     = fine_bucket as f64 / 4.0;
                    self.context.media_worker.request_frame(lib.id, lib.path.clone(), ts, aspect);
                }
            }
        }
    }

    fn handle_drag_and_drop(&mut self, ctx: &egui::Context) {
        let files = ctx.input(|i| i.raw.dropped_files.clone());
        for file in files {
            if let Some(path) = file.path {
                self.state.add_to_library(path);
            }
        }
    }

    fn show_download_overlay(&mut self, ctx: &egui::Context) -> bool {
        if self.ffmpeg_ready { return false; }

        let prog = match &self.dl_progress {
            Some(p) => p.lock().unwrap().clone(),
            None    => return false,
        };

        if prog.done && prog.error.is_none() {
            self.ffmpeg_ready = true;
            self.dl_progress  = None;
            return false;
        }

        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Background, egui::Id::new("dl_bg"),
        ));
        painter.rect_filled(ctx.viewport_rect(), 0.0, egui::Color32::from_black_alpha(230));

        egui::Area::new(egui::Id::new("dl_card"))
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(DARK_BG_2)
                    .stroke(egui::Stroke::new(1.0, DARK_BORDER))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(32))
                    .show(ui, |ui| {
                        ui.set_min_width(420.0);

                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new("⚡ First-Run Setup")
                                    .size(18.0).strong().color(ACCENT),
                            );
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new(
                                    "VeloCut needs FFmpeg for video decoding & thumbnails.\n\
                                     Downloading once — won't happen again on this machine."
                                ).size(12.0).color(egui::Color32::from_gray(170)),
                            );
                        });

                        ui.add_space(20.0);

                        if let Some(err) = &prog.error {
                            ui.colored_label(egui::Color32::from_rgb(240, 80, 80), "❌  Download failed:");
                            ui.add_space(4.0);
                            egui::ScrollArea::vertical().max_height(60.0).show(ui, |ui| {
                                ui.label(egui::RichText::new(err).size(11.0)
                                    .color(egui::Color32::from_gray(160)));
                            });
                            ui.add_space(12.0);
                            ui.vertical_centered(|ui| {
                                if ui.button("  ↺  Retry  ").clicked() {
                                    let p2 = Arc::new(Mutex::new(DownloadProgress::default()));
                                    let ps = p2.clone();
                                    let cs = ctx.clone();
                                    std::thread::spawn(move || run_download(ps, cs));
                                    self.dl_progress = Some(p2);
                                }
                            });
                            return;
                        }

                        if prog.done {
                            ui.vertical_centered(|ui| {
                                ui.label(egui::RichText::new("✅  Complete! Launching VeloCut…")
                                    .size(13.0).color(egui::Color32::from_rgb(100, 220, 100)));
                            });
                            return;
                        }

                        ui.label(
                            egui::RichText::new(&prog.status)
                                .size(12.0).color(egui::Color32::from_gray(160)),
                        );
                        ui.add_space(8.0);

                        let fraction = if prog.total_mb > 0.0 {
                            (prog.downloaded_mb / prog.total_mb).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        ui.add(
                            egui::ProgressBar::new(fraction)
                                .desired_width(356.0)
                                .fill(ACCENT)
                                .animate(prog.total_mb == 0.0),
                        );

                        ui.add_space(8.0);

                        ui.horizontal(|ui| {
                            ui.set_min_width(356.0);
                            if prog.total_mb > 0.0 {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{:.1} / {:.0} MB   {:.0}%",
                                        prog.downloaded_mb, prog.total_mb,
                                        fraction * 100.0,
                                    )).monospace().size(12.0).color(ACCENT),
                                );
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if prog.speed_mbps > 0.01 {
                                        let remaining_mb = prog.total_mb - prog.downloaded_mb;
                                        let eta_s = (remaining_mb / prog.speed_mbps) as u32;
                                        let eta_str = if eta_s >= 60 {
                                            format!("~{}m {}s left", eta_s / 60, eta_s % 60)
                                        } else {
                                            format!("~{}s left", eta_s)
                                        };
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{:.1} MB/s  {}", prog.speed_mbps, eta_str
                                            )).monospace().size(11.0)
                                            .color(egui::Color32::from_gray(140)),
                                        );
                                    }
                                });
                            } else {
                                ui.label(
                                    egui::RichText::new("Measuring…")
                                        .size(11.0).color(egui::Color32::from_gray(110)),
                                );
                            }
                        });

                        ui.add_space(14.0);
                        ui.separator();
                        ui.add_space(6.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(format!("📁  {}", prog.save_path))
                                    .size(10.0).color(egui::Color32::from_gray(90)),
                            );
                        });
                    });
            });

        ctx.request_repaint();
        true
    }
}

// ── Download thread ───────────────────────────────────────────────────────────

fn run_download(progress: Arc<Mutex<DownloadProgress>>, ctx: egui::Context) {
    let dest = app_ffmpeg_dir();

    if let Err(e) = std::fs::create_dir_all(&dest) {
        let mut p = progress.lock().unwrap();
        p.error = Some(format!("Cannot create directory: {e}"));
        p.done  = true;
        ctx.request_repaint();
        return;
    }

    let url = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
    let archive_path = dest.join("ffmpeg-release-essentials.zip");

    eprintln!("[setup] GET {url}");
    eprintln!("[setup] → {}", archive_path.display());

    let resp = match ureq::get(url).call() {
        Ok(r)  => r,
        Err(e) => {
            let mut p = progress.lock().unwrap();
            p.error = Some(format!("HTTP request failed: {e}"));
            p.done  = true;
            ctx.request_repaint();
            return;
        }
    };

    let total_bytes = resp.headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let total_mb = total_bytes as f32 / 1_000_000.0;

    {
        let mut p = progress.lock().unwrap();
        p.total_mb = total_mb;
        p.status   = "Downloading FFmpeg…".into();
    }
    ctx.request_repaint();

    let mut file = match std::fs::File::create(&archive_path) {
        Ok(f)  => f,
        Err(e) => {
            let mut p = progress.lock().unwrap();
            p.error = Some(format!("Cannot create archive file: {e}"));
            p.done  = true;
            ctx.request_repaint();
            return;
        }
    };

    let mut body   = resp.into_body();
    let mut buf    = [0u8; 65536];
    let mut written = 0u64;
    let mut last_report = Instant::now();
    let mut last_bytes  = 0u64;

    loop {
        let n = match std::io::Read::read(&mut body.as_reader(), &mut buf) {
            Ok(0)  => break,
            Ok(n)  => n,
            Err(e) => {
                let mut p = progress.lock().unwrap();
                p.error = Some(format!("Download read error: {e}"));
                p.done  = true;
                ctx.request_repaint();
                return;
            }
        };

        if let Err(e) = std::io::Write::write_all(&mut file, &buf[..n]) {
            let mut p = progress.lock().unwrap();
            p.error = Some(format!("Write error: {e}"));
            p.done  = true;
            ctx.request_repaint();
            return;
        }

        written += n as u64;

        let elapsed = last_report.elapsed();
        if elapsed >= std::time::Duration::from_millis(250) {
            let speed = (written - last_bytes) as f32 / elapsed.as_secs_f32() / 1_000_000.0;
            last_bytes  = written;
            last_report = Instant::now();

            let dl_mb = written as f32 / 1_000_000.0;
            let mut p = progress.lock().unwrap();
            p.downloaded_mb = dl_mb;
            p.speed_mbps    = speed;
            if total_mb > 0.0 {
                p.total_mb = total_mb;
                let pct = (dl_mb / total_mb * 100.0) as u32;
                p.status = format!("Downloading FFmpeg… {pct}%");
            } else {
                p.status = format!("Downloading FFmpeg… {dl_mb:.0} MB");
            }
            ctx.request_repaint();
        }
    }

    eprintln!("[setup] Download complete ({} bytes)", written);

    {
        let mut p = progress.lock().unwrap();
        p.status        = "Extracting archive…".into();
        p.downloaded_mb = total_mb.max(written as f32 / 1_000_000.0);
        p.speed_mbps    = 0.0;
    }
    ctx.request_repaint();

    if let Err(e) = unpack_ffmpeg(&archive_path, &dest) {
        let mut p = progress.lock().unwrap();
        p.error = Some(format!("Extraction failed: {e}"));
        p.done  = true;
        ctx.request_repaint();
        return;
    }

    let _ = std::fs::remove_file(&archive_path);

    let mut p = progress.lock().unwrap();
    p.status        = "Done!".into();
    p.downloaded_mb = total_mb;
    p.done          = true;
    ctx.request_repaint();
    eprintln!("[setup] FFmpeg ready ✓");
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

        if self.show_download_overlay(ctx) { return; }

        self.tick_preview_frame();

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
                if let Some(m) = self.modules.iter_mut().find(|m| m.name() == "Timeline") {
                    m.ui(ui, &self.state, &mut self.context.thumbnail_cache, &mut self.pending_cmds);
                }
            });

        egui::SidePanel::left("library_panel")
            .resizable(true)
            .default_width(220.0)
            .min_width(160.0)
            .show(ctx, |ui| {
                if let Some(m) = self.modules.iter_mut().find(|m| m.name() == "Media Library") {
                    m.ui(ui, &self.state, &mut self.context.thumbnail_cache, &mut self.pending_cmds);
                }
            });

        egui::SidePanel::right("inspector_panel")
            .resizable(true)
            .default_width(220.0)
            .min_width(160.0)
            .show(ctx, |ui| {
                if let Some(m) = self.modules.iter_mut().find(|m| m.name() == "Export") {
                    m.ui(ui, &self.state, &mut self.context.thumbnail_cache, &mut self.pending_cmds);
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            // Preview only needs the current playback/scrub frame — find the active
            // clip id and insert just that one entry rather than cloning every texture.
            let active_id = self.state.timeline.iter().find(|c| {
                self.state.current_time >= c.start_time
                    && self.state.current_time < c.start_time + c.duration
            }).map(|c| c.media_id);

            // If we have a live frame for the active clip, temporarily swap it in
            // over the thumbnail so preview shows the decoded frame. We save the
            // displaced thumbnail and restore it afterward so the library card
            // doesn't lose its image.
            let swapped = if let Some(id) = active_id {
                if let Some(frame_tex) = self.context.frame_cache.get(&id) {
                    let displaced = self.context.thumbnail_cache
                        .insert(id, frame_tex.clone());
                    Some((id, displaced))
                } else { None }
            } else { None };

            if let Some(m) = self.modules.iter_mut().find(|m| m.name() == "Preview") {
                m.ui(ui, &self.state, &mut self.context.thumbnail_cache, &mut self.pending_cmds);
            }

            // Restore cache to its pre-frame state.
            if let Some((id, Some(thumb))) = swapped {
                // Thumbnail existed before — put it back.
                self.context.thumbnail_cache.insert(id, thumb);
            } else if let Some((id, None)) = swapped {
                // No thumbnail existed — remove the frame entry we injected.
                self.context.thumbnail_cache.remove(&id);
            }
        });

        // ── Process commands emitted by modules this frame ────────────────────
        let cmds: Vec<EditorCommand> = self.pending_cmds.drain(..).collect();
        for cmd in cmds {
            self.process_command(cmd);
        }

        // ── Tick non-UI modules (sees final state after commands) ─────────────
        // Split borrow: pull the module out, tick it, put it back.
        if let Some(pos) = self.modules.iter().position(|m| m.name() == "Audio") {
            let mut m = self.modules.swap_remove(pos);
            m.tick(&self.state, &mut self.context);
            self.modules.push(m);
        }

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
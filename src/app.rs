// src/app.rs (full updated)
use crate::state::ProjectState;
use crate::theme::{configure_style, ACCENT, DARK_BG_2, DARK_BORDER};
use crate::media::{MediaWorker, MediaResult};
use crate::modules::{
    EditorModule,
    timeline::TimelineModule,
    preview::PreviewModule,
    library::LibraryModule,
    export::ExportModule,
    ThumbnailCache,
};
use crate::paths::app_ffmpeg_dir;
use eframe::egui;
use ffmpeg_sidecar::command::ffmpeg_is_installed;
use ffmpeg_sidecar::download::unpack_ffmpeg;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;
use rodio::{OutputStream, OutputStreamBuilder, Sink, Decoder};
use std::fs::File;
use std::collections::HashMap as RodioHashMap;
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
    state:           ProjectState,
    modules:         Vec<Box<dyn EditorModule>>,
    thumbnail_cache: ThumbnailCache,
    frame_cache:     HashMap<Uuid, egui::TextureHandle>,
    frame_bucket_cache: HashMap<(Uuid, u32), egui::TextureHandle>,
    media_worker:    MediaWorker,
    last_frame_req:  Option<(Uuid, u32)>,
    prev_playing:    bool,
    ffmpeg_ready:    bool,
    dl_progress:     Option<Arc<Mutex<DownloadProgress>>>,
    // Audio (rodio 0.21)
    audio_stream:    Option<OutputStream>,
    audio_sinks:     RodioHashMap<Uuid, Sink>,
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
            if !clip.duration_probed {
                media_worker.probe_clip(clip.id, clip.path.clone());
            } else if let Some(thumb) = &clip.thumbnail_path {
                media_worker.reload_thumbnail(clip.id, thumb.clone());
            }
        }

        // rodio 0.21 audio setup
        let audio_stream = OutputStreamBuilder::open_default_stream().ok();
        let audio_sinks = RodioHashMap::new();

        Self {
            state,
            modules: vec![
                Box::new(LibraryModule),
                Box::new(PreviewModule),
                Box::new(TimelineModule),
                Box::new(ExportModule::default()),
            ],
            thumbnail_cache: HashMap::new(),
            frame_cache:     HashMap::new(),
            frame_bucket_cache: HashMap::new(),
            media_worker,
            last_frame_req:  None,
            prev_playing:    false,
            ffmpeg_ready,
            dl_progress,
            audio_stream,
            audio_sinks,
        }
    }

    fn poll_media(&mut self, ctx: &egui::Context) {
        let pending: Vec<_> = self.state.pending_probes.drain(..).collect();
        for (id, path) in pending {
            self.media_worker.probe_clip(id, path);
        }
        let extracts: Vec<_> = self.state.pending_extracts.drain(..).collect();
        for (id, path, ts, dest) in extracts {
            self.media_worker.extract_frame_hq(id, path, ts, dest);
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
                self.media_worker.extract_frame_hq(Uuid::nil(), path, ts, dest);
            }
        }
        while let Ok(result) = self.media_worker.rx.try_recv() {
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
                    self.thumbnail_cache.insert(id, tex);
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
                        use crate::state::AspectRatio;
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
                    // Cache by bucket so scrub revisits are instant
                    if let Some((req_id, bucket)) = self.last_frame_req {
                        if req_id == id {
                            // Prune oldest entries beyond 300 frames (~10s at 30fps)
                            if self.frame_bucket_cache.len() >= 300 {
                                self.frame_bucket_cache.clear();
                            }
                            self.frame_bucket_cache.insert((id, bucket), tex.clone());
                        }
                    }
                    self.frame_cache.insert(id, tex);

                    // Pipeline: immediately pre-request the next frame so the decode
                    // thread never idles waiting for the next UI tick.
                    if self.state.is_playing {
                        if let Some((req_id, bucket)) = self.last_frame_req {
                            if req_id == id {
                                let next_bucket = bucket + 1;
                                let next_ts     = next_bucket as f64 / 30.0;
                                if let Some(lib) = self.state.library.iter().find(|l| l.id == id) {
                                    let aspect = self.state.active_video_ratio();
                                    self.last_frame_req = Some((id, next_bucket));
                                    self.media_worker.request_frame(
                                        lib.id, lib.path.clone(), next_ts, aspect,
                                    );
                                }
                            }
                        }
                    }

                    ctx.request_repaint();
                }
                MediaResult::Error { id, msg } => {
                    eprintln!("[media] {id}: {msg}");
                }
            }
        }
    }

    fn tick_preview_frame(&mut self) {
        // Reset dedup when playback starts so first frame fires immediately.
        let just_started = self.state.is_playing && !self.prev_playing;
        self.prev_playing = self.state.is_playing;
        if just_started { self.last_frame_req = None; }

        let current = self.state.timeline.iter().find(|c| {
            self.state.current_time >= c.start_time
                && self.state.current_time < c.start_time + c.duration
        }).cloned();

        if let Some(clip) = current {
            let local_t = (self.state.current_time - clip.start_time + clip.source_offset).max(0.0);
            let bucket  = (local_t * 30.0) as u32;  // ~30fps granularity
            let key     = (clip.media_id, bucket);

            if self.last_frame_req == Some(key) { return; }
            self.last_frame_req = Some(key);

            if let Some(lib) = self.state.library.iter().find(|m| m.id == clip.media_id) {
                let aspect = self.state.active_video_ratio();
                let ts     = bucket as f64 / 30.0;

                // Serve from cache instantly — avoids re-decode on scrub revisit
                if let Some(cached) = self.frame_bucket_cache.get(&key) {
                    self.frame_cache.insert(lib.id, cached.clone());
                    return;
                }

                self.media_worker.request_frame(lib.id, lib.path.clone(), ts, aspect);
            }
        } else {
            self.last_frame_req = None;
        }
    }

    fn tick_audio(&mut self) {
        let Some(stream) = &self.audio_stream else { return };

        if !self.state.is_playing {
            self.audio_sinks.clear();
            return;
        }

        let t = self.state.current_time;

        if let Some(clip) = self.state.timeline.iter().find(|c| {
            c.track_row == 0 && c.start_time <= t && t < c.start_time + c.duration
        }) {
            if let Some(lib) = self.state.library.iter().find(|l| l.id == clip.media_id) {
                if let Some(apath) = &lib.audio_path {
                    let seek_t = t - clip.start_time + clip.source_offset;

                    if !self.audio_sinks.contains_key(&clip.id) {
                        self.audio_sinks.clear();
                        if let Ok(file) = File::open(apath) {
                            if let Ok(decoder) = Decoder::new(file) {
                                let sink = Sink::connect_new(&stream.mixer());
                                sink.append(decoder);
                                let _ = sink.try_seek(std::time::Duration::from_secs_f64(seek_t.max(0.0)));
                                sink.set_volume(if self.state.muted { 0.0 } else { self.state.volume });
                                sink.play();
                                self.audio_sinks.insert(clip.id, sink);
                            }
                        }
                    }
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
        eframe::set_value(storage, eframe::APP_KEY, &AppStorage {
            project: ProjectState::default(),
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.media_worker.shutdown();
        self.audio_sinks.clear();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_drag_and_drop(ctx);
        self.poll_media(ctx);

        if self.show_download_overlay(ctx) { return; }

        self.tick_preview_frame();
        self.tick_audio();

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
                    m.ui(ui, &mut self.state, &mut self.thumbnail_cache);
                }
            });

        egui::SidePanel::left("library_panel")
            .resizable(true)
            .default_width(220.0)
            .min_width(160.0)
            .show(ctx, |ui| {
                if let Some(m) = self.modules.iter_mut().find(|m| m.name() == "Media Library") {
                    m.ui(ui, &mut self.state, &mut self.thumbnail_cache);
                }
            });

        egui::SidePanel::right("inspector_panel")
            .resizable(true)
            .default_width(220.0)
            .min_width(160.0)
            .show(ctx, |ui| {
                if let Some(m) = self.modules.iter_mut().find(|m| m.name() == "Export") {
                    m.ui(ui, &mut self.state, &mut self.thumbnail_cache);
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let mut merged: ThumbnailCache = self.thumbnail_cache.clone();
            merged.extend(self.frame_cache.iter().map(|(k, v)| (*k, v.clone())));
            if let Some(m) = self.modules.iter_mut().find(|m| m.name() == "Preview") {
                m.ui(ui, &mut self.state, &mut merged);
            }
        });

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
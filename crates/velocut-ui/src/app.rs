// src/app.rs (velocut-ui)
use velocut_core::state::{ProjectState, TimelineClip};
use velocut_core::commands::EditorCommand;
use velocut_media::{MediaWorker, ClipSpec, EncodeSpec};
use velocut_media::audio::cleanup_audio_temp;
use velocut_core::transitions::{ClipTransition, TimelineTransition, TransitionType};
use crate::context::AppContext;
use crate::theme::configure_style;
use crate::helpers::clip_query;
use crate::modules::{
    EditorModule,  // must be in scope for .ui() calls on concrete module types
    timeline::TimelineModule,
    preview_module::PreviewModule,
    library::LibraryModule,
    export_module::ExportModule,
    audio_module::AudioModule,
    video_module::VideoModule,
};
use eframe::egui;
use egui_desktop::{TitleBar, TitleBarOptions, render_resize_handles};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use rfd::FileDialog;

#[derive(Serialize, Deserialize)]
struct AppStorage {
    project: ProjectState,
}

// ── Undo / Redo ───────────────────────────────────────────────────────────────
// Hard cap on undo history depth. Each entry is a full ProjectState clone —
// dominated by Vec<TimelineClip> and Vec<LibraryClip>. At typical project sizes
// (< 200 clips) each snapshot is well under 1 MB, so 50 entries ≈ ≤ 50 MB worst
// case. If memory pressure ever becomes a concern, lower this constant; no other
// code needs to change.
const MAX_UNDO_DEPTH: usize = 50;

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

    // ── Undo / Redo stacks ────────────────────────────────────────────────────
    // Snapshots of ProjectState pushed before any user-visible mutation.
    // Only serialisable, user-meaningful state is snapshotted (library +
    // timeline + transitions). Runtime-only fields (encode progress, temp
    // audio paths, playback position) are restored from the live state after
    // each undo/redo so they are unaffected by history navigation.
    undo_stack: Vec<ProjectState>,
    redo_stack: Vec<ProjectState>,
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
        let library  = LibraryModule::new();
        let timeline = TimelineModule::new();

        Self {
            state,
            context,
            library,
            preview:      PreviewModule::new(),
            timeline,
            export:       ExportModule::default(),
            audio:        AudioModule::new(),
            video:        VideoModule::new(),
            pending_cmds: Vec::new(),
            undo_stack:   Vec::new(),
            redo_stack:   Vec::new(),
        }
    }

    // ── Undo / Redo helpers ───────────────────────────────────────────────────

    /// Push the current state onto the undo stack and clear the redo stack.
    /// Called in response to `EditorCommand::PushUndoSnapshot`. Enforces the
    /// depth cap by discarding the oldest entry when over limit.
    fn push_undo_snapshot(&mut self) {
        if self.undo_stack.len() >= MAX_UNDO_DEPTH {
            self.undo_stack.remove(0); // drop oldest — O(N) but infrequent
        }
        self.undo_stack.push(self.state.clone());
        self.redo_stack.clear();
        self.sync_undo_len();
    }

    /// Restore the most recent undo snapshot. Preserves runtime-only fields
    /// (encode progress, pending cleanup, playback time) from the live state
    /// so history navigation never interrupts an ongoing encode or playback.
    fn apply_undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop() {
            let before = self.state.clone();
            self.redo_stack.push(before);
            self.restore_snapshot(snapshot);
        }
    }

    fn apply_redo(&mut self) {
        if let Some(snapshot) = self.redo_stack.pop() {
            let before = self.state.clone();
            self.undo_stack.push(before);
            self.restore_snapshot(snapshot);
        }
    }

    /// Replace `self.state` with `snapshot` while keeping runtime-only fields
    /// from the current live state intact.
    fn restore_snapshot(&mut self, mut snapshot: ProjectState) {
        // Preserve runtime-only fields that must not be rewound by undo/redo.
        snapshot.current_time          = self.state.current_time;
        snapshot.is_playing            = self.state.is_playing;
        snapshot.volume                = self.state.volume;
        snapshot.muted                 = self.state.muted;
        snapshot.encode_job            = self.state.encode_job;
        snapshot.encode_progress       = self.state.encode_progress.clone();
        snapshot.encode_done           = self.state.encode_done.clone();
        snapshot.encode_error          = self.state.encode_error.clone();
        // Drain pending queues from live state into the snapshot so they aren't lost.
        snapshot.pending_probes        = std::mem::take(&mut self.state.pending_probes);
        snapshot.pending_extracts      = std::mem::take(&mut self.state.pending_extracts);
        snapshot.pending_audio_cleanup = std::mem::take(&mut self.state.pending_audio_cleanup);
        snapshot.pending_save_pick     = self.state.pending_save_pick.take();
        snapshot.save_status           = self.state.save_status.take();

        // Re-queue probes for any library clips whose waveform_peaks are empty
        // in the restored snapshot. This happens when the snapshot was taken while
        // a probe was still in flight (peaks not yet returned from the worker).
        // Without this, undoing ExtractAudioTrack before the first probe completes
        // leaves the video clip with no waveform even though audio_muted is cleared.
        for lib_clip in &snapshot.library {
            if lib_clip.waveform_peaks.is_empty() {
                let already_queued = snapshot.pending_probes.iter().any(|(id, _)| *id == lib_clip.id);
                if !already_queued {
                    snapshot.pending_probes.push((lib_clip.id, lib_clip.path.clone()));
                }
            }
        }

        self.state = snapshot;
        self.sync_undo_len();
    }

    /// Write undo/redo stack depths back into ProjectState so the timeline
    /// module can read them for button enable/disable without needing extra
    /// parameters threaded through the EditorModule trait.
    fn sync_undo_len(&mut self) {
        self.state.undo_len = self.undo_stack.len();
        self.state.redo_len = self.redo_stack.len();
    }

    // ── Command processing ────────────────────────────────────────────────────

    fn process_command(&mut self, cmd: EditorCommand) {
        match cmd {
            // ── Undo / Redo ──────────────────────────────────────────────────
            EditorCommand::PushUndoSnapshot => {
                self.push_undo_snapshot();
            }
            EditorCommand::Undo => {
                self.apply_undo();
            }
            EditorCommand::Redo => {
                self.apply_redo();
            }

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
                self.context.playback.audio_was_playing = false;
                self.context.cache.pending_pb_frame = None;
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
            EditorCommand::AddToTimeline { media_id, at_time, track_row } => {
                // Auto-set aspect ratio from the first clip placed on the timeline.
                // Check emptiness *before* add_to_timeline mutates the vec.
                let is_first_clip = self.state.timeline.is_empty();
                self.state.add_to_timeline(media_id, at_time, track_row);
                if is_first_clip {
                    if let Some((width, height)) = self.state.library.iter()
                        .find(|c| c.id == media_id)
                        .and_then(|c| c.video_size)
                    {
                        if width > 0 && height > 0 {
                            use velocut_core::state::AspectRatio;
                            let r = width as f32 / height as f32;
                            self.state.aspect_ratio =
                                if      (r - 16.0/9.0).abs() < 0.05 { AspectRatio::SixteenNine   }
                                else if (r - 9.0/16.0).abs() < 0.05 { AspectRatio::NineSixteen   }
                                else if (r - 2.0/3.0 ).abs() < 0.05 { AspectRatio::TwoThree      }
                                else if (r - 3.0/2.0 ).abs() < 0.05 { AspectRatio::ThreeTwo      }
                                else if (r - 4.0/3.0 ).abs() < 0.05 { AspectRatio::FourThree     }
                                else if (r - 1.0     ).abs() < 0.05 { AspectRatio::OneOne        }
                                else if (r - 4.0/5.0 ).abs() < 0.05 { AspectRatio::FourFive      }
                                else if (r - 21.0/9.0).abs() < 0.10 { AspectRatio::TwentyOneNine }
                                else if (r - 2.39    ).abs() < 0.05 { AspectRatio::Anamorphic    }
                                else if r > 1.0 { AspectRatio::SixteenNine }
                                else            { AspectRatio::NineSixteen };
                            eprintln!("[app] aspect ratio auto-set from first timeline clip {width}x{height}");
                        }
                    }
                }
            }
            EditorCommand::DeleteTimelineClip(id) => {
                // If this clip is linked to a partner (extract audio pair),
                // un-mute the partner so it doesn't silently stay muted.
                if let Some(partner_id) = self.state.timeline.iter()
                    .find(|c| c.id == id)
                    .and_then(|c| c.linked_clip_id)
                {
                    if let Some(partner) = self.state.timeline.iter_mut().find(|c| c.id == partner_id) {
                        partner.linked_clip_id = None;
                        partner.audio_muted    = false;
                    }
                }
                self.state.timeline.retain(|c| c.id != id);
                if self.state.selected_timeline_clip == Some(id) {
                    self.state.selected_timeline_clip = None;
                }
            }
            EditorCommand::ExtractAudioTrack(clip_id) => {
                self.state.extract_audio_track(clip_id);
            }
            EditorCommand::SetClipVolume { id, volume } => {
                if let Some(tc) = self.state.timeline.iter_mut().find(|c| c.id == id) {
                    tc.volume = volume.clamp(0.0, 2.0);
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
            EditorCommand::SplitClipAt(t) => {
                // Find a clip that contains t with enough room on each side to be
                // worth splitting (> 2 frames from either edge at 30fps).
                let min_dur = 2.0 / 30.0;
                if let Some(clip) = self.state.timeline.iter()
                    .find(|c| t > c.start_time + min_dur
                           && t < c.start_time + c.duration - min_dur)
                    .cloned()
                {
                    let split_offset = t - clip.start_time; // seconds into clip

                    // Shorten the original clip to become the first half.
                    if let Some(c) = self.state.timeline.iter_mut().find(|c| c.id == clip.id) {
                        c.duration = split_offset;
                    }

                    // Push the second half as a new clip immediately after.
                    self.state.timeline.push(TimelineClip {
                        id:             Uuid::new_v4(),
                        media_id:       clip.media_id,
                        start_time:     t,
                        duration:       clip.duration - split_offset,
                        source_offset:  clip.source_offset + split_offset,
                        track_row:      clip.track_row,
                        volume:         clip.volume,
                        linked_clip_id: None,
                        audio_muted:    clip.audio_muted,
                    });
                    // Any transition keyed on clip.id (original → its successor)
                    // remains valid — the badge system renders from clip positions,
                    // so the badge will now appear between new_clip and the old
                    // successor. No transition cleanup needed.
                }
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
                self.begin_render(filename, width, height, fps);
            }
            EditorCommand::CancelEncode(job_id) => {
                self.context.media_worker.cancel_encode(job_id);
                // Do NOT clear encode state here — wait for the EncodeError result
                // ("cancelled") to arrive over the channel so the UI transition is
                // driven by the same path as a real error (avoids race conditions).
            }
            EditorCommand::ClearEncodeStatus => {
                self.state.encode_job      = None;
                self.state.encode_progress = None;
                self.state.encode_done     = None;
                self.state.encode_error    = None;
            }

            // ── Project reset ─────────────────────────────────────────────────
            EditorCommand::ClearProject => {
                // ── Step 1: queue temp WAV deletions before wiping the library.
                // Once library is cleared there's no way to recover the paths.
                for clip in &self.state.library {
                    if let Some(apath) = &clip.audio_path {
                        self.state.pending_audio_cleanup.push(apath.clone());
                    }
                }

                // ── Step 2: stop the playback thread and drain its channel.
                // Must happen before touching ProjectState — the decode loop
                // holds clip references and races if state changes under it.
                self.context.media_worker.stop_playback();

                // ── Step 3: drop audio sinks before clearing state.
                // rodio decode threads reference the WAV path; dropping sinks
                // first lets them finish cleanly before the path becomes invalid.
                self.context.audio_sinks.clear();

                // ── Step 4: evict all GPU textures and reset the byte budget.
                self.context.cache.clear_all();

                // ── Step 5: reset scrub / playback tracking.
                self.context.playback.reset();

                // ── Step 6: wipe serialisable project data.
                self.state.library.clear();
                self.state.timeline.clear();
                self.state.transitions.clear();
                self.state.selected_timeline_clip = None;
                self.state.selected_library_clip  = None;
                self.state.current_time           = 0.0;
                self.state.is_playing             = false;

                // ── Step 7: zero encode state (may have been mid-encode).
                self.state.encode_job      = None;
                self.state.encode_progress = None;
                self.state.encode_done     = None;
                self.state.encode_error    = None;

                // ── Step 8: clear undo/redo — there is nothing meaningful to
                // undo after a full wipe and stale snapshots waste memory.
                self.undo_stack.clear();
                self.redo_stack.clear();
                self.sync_undo_len();
            }
            EditorCommand::SetCrossfadeDuration(secs) => {
                // Batch operation: set crossfade on every touching adjacent pair,
                // or clear all transitions if secs == 0.
                if secs <= 0.0 {
                    self.state.transitions.clear();
                } else {
                    // Sort clips by start time to find adjacent pairs.
                    let mut sorted = self.state.timeline.clone();
                    sorted.sort_by(|a, b| a.start_time.partial_cmp(&b.start_time).unwrap());
                    self.state.transitions.clear();
                    for i in 0..sorted.len().saturating_sub(1) {
                        let a = &sorted[i];
                        let b = &sorted[i + 1];
                        // Only pair clips on the same track that are touching.
                        if a.track_row != b.track_row { continue; }
                        let gap = b.start_time - (a.start_time + a.duration);
                        if gap.abs() > 0.1 { continue; }
                        self.state.transitions.push(TimelineTransition {
                            after_clip_id: a.id,
                            kind: TransitionType::Crossfade { duration_secs: secs },
                        });
                    }
                }
            }
            EditorCommand::SetTransition { after_clip_id, kind } => {
                if let Some(t) = self.state.transitions.iter_mut()
                    .find(|t| t.after_clip_id == after_clip_id)
                {
                    t.kind = kind;
                } else if kind != TransitionType::Cut {
                    self.state.transitions.push(TimelineTransition { after_clip_id, kind });
                }
            }
            EditorCommand::RemoveTransition(after_clip_id) => {
                self.state.transitions.retain(|t| t.after_clip_id != after_clip_id);
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

    /// Open an rfd save dialog, build the EncodeSpec from the current timeline,
    /// and hand it to the media worker. Called from process_command for RenderMP4.
    ///
    /// This mirrors the pattern used by pending_save_pick / RequestSaveFramePicker:
    /// blocking OS dialogs are fine here because process_command runs after the UI
    /// pass, not inside an egui callback.
    fn begin_render(&mut self, filename: String, width: u32, height: u32, fps: u32) {
        // Abort silently if an encode is already running.
        // ExportModule disables the button while is_encoding, but guard here too.
        if self.state.encode_job.is_some() {
            eprintln!("[export] ignoring RenderMP4: encode already in progress");
            return;
        }

        let default_name = format!("{filename}.mp4");
        let dest = match FileDialog::new()
            .set_file_name(&default_name)
            .add_filter("MP4 Video", &["mp4"])
            .save_file()
        {
            Some(p) => p,
            None    => return, // user cancelled the dialog — no-op
        };

        // Resolve timeline clips to ClipSpec by joining with the library.
        // Clips are sorted by start_time so the output timeline is correct even
        // if the Vec is not stored in order.
        let mut timeline = self.state.timeline.clone();
        timeline.sort_by(|a, b| a.start_time.partial_cmp(&b.start_time).unwrap());

        let clip_specs: Vec<ClipSpec> = timeline
            .iter()
            .filter(|tc| {
                // Exclude extracted audio clips (A-row clips linked to a V-row partner).
                // Their audio contribution is encoded via the V-row clip below, using
                // the A-row clip's volume. Including them separately would write
                // duplicate video frames AND duplicate audio into the output file.
                !clip_query::is_extracted_audio_clip(tc)
            })
            .filter_map(|tc| {
                self.state.library.iter()
                    .find(|lc| lc.id == tc.media_id)
                    .map(|lc| {
                        // When a V-row clip's audio was extracted to the A track below
                        // it, the user may have independently adjusted the A-row clip's
                        // volume. Use that volume so the export reflects those changes.
                        // The source file and offset are identical, so no path change
                        // is needed — only the gain differs.
                        let effective_volume = if tc.audio_muted {
                            clip_query::linked_audio_clip(&self.state, tc)
                                .map(|ac| ac.volume)
                                .unwrap_or(tc.volume)
                        } else {
                            tc.volume
                        };
                        ClipSpec {
                            path:          lc.path.clone(),
                            source_offset: tc.source_offset,
                            duration:      tc.duration,
                            volume:        effective_volume,
                            skip_audio:    false,
                        }
                    })
            })
            .collect();

        if clip_specs.is_empty() {
            eprintln!("[export] no resolvable clips — aborting render");
            return;
        }

        let job_id = Uuid::new_v4();

        // Map TimelineTransitions (UUID-keyed) to ClipTransitions (index-keyed)
        // by finding each after_clip_id's position in the sorted timeline vec.
        // Clips that resolve out of range (e.g. last clip) are silently dropped.
        let encode_transitions: Vec<ClipTransition> = self.state.transitions.iter()
            .filter_map(|t| {
                let idx = timeline.iter().position(|tc| tc.id == t.after_clip_id)?;
                if idx + 1 < clip_specs.len() {
                    Some(ClipTransition { after_clip_index: idx, kind: t.kind.clone() })
                } else {
                    None
                }
            })
            .collect();

        let spec = EncodeSpec {
            job_id,
            clips: clip_specs,
            width,
            height,
            fps,
            output: dest,
            transitions: encode_transitions,
        };

        // Arm encode state before handing to the worker so ingest_media_results
        // can route EncodeProgress into the right fields immediately.
        self.state.encode_job      = Some(job_id);
        self.state.encode_progress = Some((0, (self.state.total_duration() * fps as f64).ceil() as u64));
        self.state.encode_done     = None;
        self.state.encode_error    = None;

        self.context.media_worker.start_encode(spec);
    }

    fn poll_media(&mut self, ctx: &egui::Context) {
        // ── Pre-frame housekeeping ────────────────────────────────────────────
        for path in self.state.pending_audio_cleanup.drain(..) {
            cleanup_audio_temp(&path);
        }

        let mut pending: Vec<_> = self.state.pending_probes.drain(..).collect();
        // ── Visible-first probe ordering ──────────────────────────────────────
        // Sort so clips whose cards are on-screen (last frame's visible set) are
        // dispatched to probe_clip before off-screen ones. The gatekeeper threads
        // race for the semaphore in spawn order, so earlier = higher priority.
        // Uses the previous frame's visible_ids — always valid after the first
        // render, which is when real batch imports happen.
        {
            let vis = &self.library.visible_ids;
            pending.sort_by_key(|(id, _)| if vis.contains(id) { 0u8 } else { 1u8 });
        }
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

        const BOLT_ICON: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><polygon points="9,1 4,9 8,9 7,15 12,7 8,7" fill="rgb(255,160,50)"/></svg>"#;
        TitleBar::new(
            TitleBarOptions::new()
                .with_title("VeloCut")
                .with_background_color(crate::theme::DARK_BG_1)
                .with_hover_color(crate::theme::DARK_BG_4)
                .with_close_hover_color(egui::Color32::from_rgb(232, 17, 35))
                .with_title_color(crate::theme::ACCENT)
                .with_title_font_size(15.0)
                .with_app_icon(BOLT_ICON, "bolt.svg"),
        )
        .show(ctx);

        render_resize_handles(ctx);

        egui::TopBottomPanel::bottom("timeline_panel")
            .resizable(false)
            .exact_height(340.0)
            .show(ctx, |ui| {
                self.timeline.ui(ui, &self.state, &mut self.context.cache.thumbnail_cache, &mut self.pending_cmds);
            });

        egui::SidePanel::left("library_panel")
            .resizable(true)
            .min_width(240.0)
            .default_width(240.0)
            .show(ctx, |ui| {
                self.library.ui(ui, &self.state, &mut self.context.cache.thumbnail_cache, &mut self.pending_cmds);
            });

        egui::SidePanel::right("export_panel")
            .resizable(true)
            .default_width(220.0)
            .min_width(160.0)
            .show(ctx, |ui| {
                self.export.ui(ui, &self.state, &mut self.context.cache.thumbnail_cache, &mut self.pending_cmds);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            // Give PreviewModule the current live frame (if any) so it can render
            // it directly. No thumbnail_cache mutation needed — the cache stays
            // pure thumbnails and PreviewModule owns its frame reference.
            let active_id = VideoModule::active_media_id(&self.state);
            self.preview.current_frame = active_id
                .and_then(|id| self.context.cache.frame_cache.get(&id).cloned());

            self.preview.ui(ui, &self.state, &mut self.context.cache.thumbnail_cache, &mut self.pending_cmds);
        });

        // ── Render modal overlay ──────────────────────────────────────────────
        // Must be called after all panels so the scrim and card paint above
        // the entire UI. show_render_modal is a no-op while encode_job is None.
        self.export.show_render_modal(ctx, &self.state, &mut self.pending_cmds);

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

        // Keep repainting while an encode is running so the progress bar stays live.
        if self.state.encode_job.is_some()
            && self.state.encode_done.is_none()
            && self.state.encode_error.is_none()
        {
            ctx.request_repaint();
        }
    }
}
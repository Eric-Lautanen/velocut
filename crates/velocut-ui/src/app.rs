// src/app.rs (velocut-ui)
use crate::context::AppContext;
use crate::helpers::clip_query;
use crate::helpers::memory_manager::MemoryManager;
use crate::modules::{
    audio_module::AudioModule,
    export_module::ExportModule,
    library::LibraryModule,
    preview_module::PreviewModule,
    timeline::TimelineModule,
    video_module::VideoModule,
    EditorModule, // must be in scope for .ui() calls on concrete module types
};
use crate::theme::configure_style;
use crate::velocut_log;
use eframe::egui;
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::PathBuf;
use uuid::Uuid;
use velocut_core::commands::EditorCommand;
use velocut_core::state::{ClipType, LibraryClip, ProjectState, TimelineClip};
use velocut_core::transitions::{
    ClipTransition, TimelineTransition, TransitionKind, TransitionType,
};
use velocut_media::audio::cleanup_audio_temp;
use velocut_media::encode::AudioOverlay;
use velocut_media::{ClipSpec, EncodeSpec, MediaWorker};

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
    state: ProjectState,
    context: AppContext,
    // Panel modules as concrete types — eliminates per-frame name-string lookup
    // and makes typos a compile error instead of a silently blank panel.
    library: LibraryModule,
    preview: PreviewModule,
    timeline: TimelineModule,
    export: ExportModule,
    /// Stored separately so tick() calls the concrete method, not the trait default no-op.
    audio: AudioModule,
    /// Commands emitted by modules each frame, processed after the UI pass
    pending_cmds: Vec<EditorCommand>,

    // ── Undo / Redo stacks ────────────────────────────────────────────────────
    // Snapshots of ProjectState pushed before any user-visible mutation.
    // Only serialisable, user-meaningful state is snapshotted (library +
    // timeline + transitions). Runtime-only fields (encode progress, temp
    // audio paths, playback position) are restored from the live state after
    // each undo/redo so they are unaffected by history navigation.
    // VecDeque so cap-eviction uses pop_front() — O(1) vs Vec::remove(0) O(N).
    undo_stack: VecDeque<ProjectState>,
    redo_stack: VecDeque<ProjectState>,

    /// True after the first rendered frame has checked the window size.
    /// eframe persists geometry between sessions — if a previous run left the
    /// window at near-zero dimensions, we snap it back to the default on
    /// the very first frame. Runs exactly once per process lifetime.
    startup_size_checked: bool,

    /// True once fix_taskbar_icon() has been called from update().
    /// Must be deferred to update() — new() runs before the OS window exists,
    /// so EnumThreadWindows finds nothing to patch if called there.
    taskbar_icon_fixed: bool,

    /// Set to true by ClearProject and checked in save() and on_exit().
    ///
    /// eframe calls save() automatically after the export_module deletes the
    /// app-data directory, which would recreate %APPDATA%\VeloCut\data\ and
    /// undo the deletion. When this flag is true save() becomes a no-op, and
    /// on_exit() re-runs delete_app_data_dir() as a backstop in case eframe
    /// squeezed in an autosave between the button click and the exit.
    reset_done: bool,

    memory_manager: MemoryManager,
}

impl VeloCutApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // egui_extras::install_image_loaders is called in main() before this
        // runs — removed the duplicate call that was here previously.
        configure_style(&cc.egui_ctx);
        cc.egui_ctx.options_mut(|o| {
            o.theme_preference = egui::ThemePreference::Dark;
        });

        // fix_taskbar_icon() is intentionally NOT called here.
        // new() is the app factory — it runs before the OS window is created,
        // so EnumThreadWindows would find nothing to patch there. The call is deferred
        // to the first update() frame via the taskbar_icon_fixed flag.

        let mut state = cc
            .storage
            .and_then(|s| eframe::get_value::<AppStorage>(s, eframe::APP_KEY))
            .map(|d| d.project)
            .unwrap_or_default();

        let media_worker = MediaWorker::new();
        let repairs = pending_audio_repairs(&state);
        for (id, source) in &repairs {
            // The re-extraction writes a full-file WAV, so the seek base moves
            // back to 0 — a persisted trimmed offset would overshoot.
            if let Some(lib) = state.library.iter_mut().find(|lib| lib.id == *id) {
                lib.path = source.clone();
                lib.audio_trimmed_offset = 0.0;
            }
            media_worker.extract_audio_trimmed(*id, source.clone(), 0.0, f64::MAX);
        }
        let repaired: HashSet<Uuid> = repairs.iter().map(|(id, _)| *id).collect();
        for clip in &state.library {
            if repaired.contains(&clip.id) {
                // Duration and waveform peaks are persisted; only audio is missing.
                continue;
            }
            media_worker.probe_clip(clip.id, clip.path.clone());
        }

        let context = AppContext::new(media_worker);
        let library = LibraryModule::new();
        let timeline = TimelineModule::new();
        let memory_manager = MemoryManager::new();

        Self {
            state,
            context,
            library,
            preview: PreviewModule::new(),
            timeline,
            export: ExportModule::default(),
            audio: AudioModule::new(),
            pending_cmds: Vec::new(),
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            startup_size_checked: false,
            taskbar_icon_fixed: false,
            reset_done: false,
            memory_manager,
        }
    }

    // ── Undo / Redo helpers ───────────────────────────────────────────────────

    /// Push the current state onto the undo stack and clear the redo stack.
    /// Called in response to `EditorCommand::PushUndoSnapshot`. Enforces the
    /// depth cap by discarding the oldest entry when over limit.
    fn push_undo_snapshot(&mut self) {
        if self.undo_stack.len() >= MAX_UNDO_DEPTH {
            self.undo_stack.pop_front(); // drop oldest — O(1) with VecDeque
        }
        self.undo_stack.push_back(self.state.clone());
        self.redo_stack.clear();
        self.sync_undo_len();
    }

    /// Restore the most recent undo snapshot. Preserves runtime-only fields
    /// (encode progress, pending cleanup, playback time) from the live state
    /// so history navigation never interrupts an ongoing encode or playback.
    fn apply_undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop_back() {
            let before = self.state.clone();
            self.redo_stack.push_back(before);
            self.restore_snapshot(snapshot);
        }
    }

    fn apply_redo(&mut self) {
        if let Some(snapshot) = self.redo_stack.pop_back() {
            let before = self.state.clone();
            self.undo_stack.push_back(before);
            self.restore_snapshot(snapshot);
        }
    }

    /// Replace `self.state` with `snapshot` while keeping runtime-only fields
    /// from the current live state intact.
    fn restore_snapshot(&mut self, mut snapshot: ProjectState) {
        // Preserve runtime-only fields that must not be rewound by undo/redo.
        snapshot.current_time = self.state.current_time;
        snapshot.is_playing = self.state.is_playing;
        snapshot.volume = self.state.volume;
        snapshot.muted = self.state.muted;
        snapshot.encode_job = self.state.encode_job;
        snapshot.encode_progress = self.state.encode_progress;
        snapshot.encode_done = self.state.encode_done.clone();
        snapshot.encode_error = self.state.encode_error.clone();
        // Drain pending queues from live state into the snapshot so they aren't lost.
        snapshot.pending_probes = std::mem::take(&mut self.state.pending_probes);
        snapshot.pending_extracts = std::mem::take(&mut self.state.pending_extracts);
        snapshot.pending_audio_cleanup = std::mem::take(&mut self.state.pending_audio_cleanup);
        snapshot.pending_save_pick = self.state.pending_save_pick.take();
        snapshot.save_status = self.state.save_status.take();

        // Re-queue probes for any library clips whose waveform_peaks are empty
        // in the restored snapshot. This happens when the snapshot was taken while
        // a probe was still in flight (peaks not yet returned from the worker).
        // Without this, undoing ExtractAudioTrack before the first probe completes
        // leaves the video clip with no waveform even though audio_muted is cleared.
        for lib_clip in &snapshot.library {
            if lib_clip.waveform_peaks.is_empty() {
                let already_queued = snapshot
                    .pending_probes
                    .iter()
                    .any(|(id, _)| *id == lib_clip.id);
                if !already_queued {
                    snapshot
                        .pending_probes
                        .push((lib_clip.id, lib_clip.path.clone()));
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

    fn process_command(&mut self, cmd: EditorCommand, ctx: &egui::Context) {
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
                self.state.is_playing = false;
                self.state.current_time = 0.0;
            }
            EditorCommand::SetPlayhead(t) => {
                self.state.current_time = t;
                // Rebuild audio at the new position. Draining (not dropping)
                // prevents a click on every scrub / ruler click.
                self.audio.drain_for_seek(&mut self.context);
                self.context.cache.pending_pb_frame = None;
                ctx.request_repaint();
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
                // Removing the entry also removes its timeline clips. Core
                // un-links/un-mutes a surviving V partner and only releases the
                // temp WAV when no other entry still plays from it.
                let doomed: Vec<Uuid> = self
                    .state
                    .timeline
                    .iter()
                    .filter(|c| c.media_id == id)
                    .map(|c| c.id)
                    .collect();
                if let Some(wav) = self.state.remove_library_clip(id) {
                    self.state.pending_audio_cleanup.push(wav);
                }
                if self
                    .state
                    .selected_timeline_clip
                    .is_some_and(|sel| doomed.contains(&sel))
                {
                    self.state.selected_timeline_clip = None;
                }
                // Evict cached frames and the thumbnail: the entry is gone, so
                // keeping them only leaks GPU memory and can leave a deleted
                // clip's last frame on screen.
                self.context.cache.frame_cache.remove(&id);
                self.context
                    .cache
                    .frame_bucket_cache
                    .retain(|(bucket_mid, _, _), _| *bucket_mid != id);
                self.preview.current_frame = None;
                self.context.cache.remove_thumbnail(&id);
            }
            EditorCommand::SelectLibraryClip(id) => {
                self.state.selected_library_clip = id;
            }

            // ── Timeline ─────────────────────────────────────────────────────
            EditorCommand::AddToTimeline {
                media_id,
                at_time,
                track_row,
            } => {
                // Auto-set aspect ratio from the first clip placed on the timeline.
                // Check emptiness *before* add_to_timeline mutates the vec.
                let is_first_clip = self.state.timeline.is_empty();
                self.state.add_to_timeline(media_id, at_time, track_row);
                if is_first_clip {
                    if let Some((width, height)) = self
                        .state
                        .library
                        .iter()
                        .find(|c| c.id == media_id)
                        .and_then(|c| c.video_size)
                    {
                        if width > 0 && height > 0 {
                            use velocut_core::state::AspectRatio;
                            let r = width as f32 / height as f32;
                            self.state.aspect_ratio = AspectRatio::from_ratio(r);
                            velocut_log!("[app] aspect ratio auto-set from first timeline clip {width}x{height}");
                        }
                    }
                }
            }
            EditorCommand::DeleteTimelineClip(id) => {
                // If this clip is linked to a partner (extract audio pair),
                // un-mute the partner so it doesn't silently stay muted.
                if let Some(partner_id) = self
                    .state
                    .timeline
                    .iter()
                    .find(|c| c.id == id)
                    .and_then(|c| c.linked_clip_id)
                {
                    if let Some(partner) =
                        self.state.timeline.iter_mut().find(|c| c.id == partner_id)
                    {
                        partner.linked_clip_id = None;
                        partner.audio_muted = false;
                    }
                }
                // Capture the deleted clip's media_id before removal so we can
                // evict its cached frame if no other timeline clip shares it.
                let deleted_media_id = self
                    .state
                    .timeline
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| c.media_id);
                self.state.timeline.retain(|c| c.id != id);
                // Evict frame cache for this media_id if it's now unreferenced.
                // Without this the preview keeps showing the deleted clip's last
                // frame even after the clip is gone.
                if let Some(mid) = deleted_media_id {
                    let still_used = self.state.timeline.iter().any(|c| c.media_id == mid);
                    if !still_used {
                        self.context.cache.frame_cache.remove(&mid);
                        self.context
                            .cache
                            .frame_bucket_cache
                            .retain(|(bucket_mid, _, _), _| *bucket_mid != mid);
                        self.preview.current_frame = None;
                    }
                }
                if self.state.selected_timeline_clip == Some(id) {
                    self.state.selected_timeline_clip = None;
                }
            }
            EditorCommand::ExtractAudioTrack(clip_id) => {
                // Gather library info BEFORE mutating state.
                let lib_info = self
                    .state
                    .timeline
                    .iter()
                    .find(|c| c.id == clip_id)
                    .and_then(|tc| self.state.library.iter().find(|l| l.id == tc.media_id))
                    .map(|lib| {
                        (
                            lib.path.clone(),
                            lib.audio_path.clone(),
                            lib.audio_trimmed_offset,
                            lib.waveform_peaks.clone(),
                            lib.duration,
                            lib.name.clone(),
                        )
                    });

                if let Some((src_path, wav_path, trimmed_offset, peaks, duration, src_name)) =
                    lib_info
                {
                    if let Some(audio_clip_id) = self.state.extract_audio_track(clip_id) {
                        // Create a dedicated Audio library entry so the A-row clip
                        // has the correct type (no thumbnails, no video decode).
                        // `path` must stay the ORIGINAL media file: it is the
                        // entry's identity for probing/re-extraction. Only
                        // `audio_path` may point at the temp WAV, which is
                        // cleared on save and deleted on exit.
                        let audio_lib_id = Uuid::new_v4();
                        let (audio_entry, needs_extract) = separated_audio_entry(
                            audio_lib_id,
                            src_path.clone(),
                            format!("[Audio] {src_name}"),
                            duration,
                            peaks,
                            wav_path,
                            trimmed_offset,
                        );
                        self.state.library.push(audio_entry);
                        // Rewire the A-row timeline clip to the audio library entry.
                        if let Some(tc) = self
                            .state
                            .timeline
                            .iter_mut()
                            .find(|c| c.id == audio_clip_id)
                        {
                            tc.media_id = audio_lib_id;
                        }
                        // The probe pipeline keys AudioPath results by library id.
                        // Extracting before the probe's WAV lands leaves the new
                        // entry with audio_path = None forever (that result goes to
                        // the old id), so the separated track would stay silent.
                        // Ask for a full-file WAV under the new id.
                        if needs_extract {
                            self.context.media_worker.extract_audio_trimmed(
                                audio_lib_id,
                                src_path,
                                0.0,
                                f64::MAX,
                            );
                        }
                    }
                }
            }
            EditorCommand::SetClipVolume { id, volume } => {
                if let Some(tc) = self.state.timeline.iter_mut().find(|c| c.id == id) {
                    tc.volume = volume.clamp(0.0, 2.0);
                }
                self.state.sync_linked_volume(id);
            }
            EditorCommand::SetClipFadeIn { id, secs } => {
                if let Some(tc) = self.state.timeline.iter_mut().find(|c| c.id == id) {
                    tc.fade_in_secs = secs.max(0.0);
                }
                self.state.sync_linked_fades(id);
            }
            EditorCommand::SetClipFadeInStart { id, secs } => {
                if let Some(tc) = self.state.timeline.iter_mut().find(|c| c.id == id) {
                    tc.fade_in_start_secs = secs.max(0.0);
                }
                self.state.sync_linked_fades(id);
            }
            EditorCommand::SetClipFadeOut { id, secs } => {
                if let Some(tc) = self.state.timeline.iter_mut().find(|c| c.id == id) {
                    tc.fade_out_secs = secs.max(0.0);
                }
                self.state.sync_linked_fades(id);
            }
            EditorCommand::SetClipFadeOutEnd { id, secs } => {
                if let Some(tc) = self.state.timeline.iter_mut().find(|c| c.id == id) {
                    tc.fade_out_end_secs = secs.max(0.0);
                }
                self.state.sync_linked_fades(id);
            }
            EditorCommand::SetClipFilter { id, filter } => {
                let filter = filter.validated();
                if let Some(tc) = self.state.timeline.iter_mut().find(|c| c.id == id) {
                    tc.filter = filter;
                }
                if let Some(mid) = self
                    .state
                    .timeline
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| c.media_id)
                {
                    self.context.cache.frame_cache.remove(&mid);
                    self.context
                        .cache
                        .frame_bucket_cache
                        .retain(|(bid, _, _), _| *bid != mid);
                }
                self.context.playback.last_frame_req = None;
                ctx.request_repaint();
            }
            EditorCommand::SelectTimelineClip(id) => {
                self.state.selected_timeline_clip = id;
            }
            EditorCommand::MoveTimelineClip {
                id,
                new_start,
                new_row,
            } => {
                move_clip(&mut self.state, id, new_start, new_row);
            }
            EditorCommand::SplitClipAt(t) => {
                if let Some(clip) = split_target_at(&self.state, t) {
                    split_clip_and_partners(&mut self.state, &clip, t);
                }
            }
            EditorCommand::TrimClipStart {
                id,
                new_source_offset,
                new_duration,
            } => {
                trim_clip_start(&mut self.state, id, new_source_offset, new_duration);
            }
            EditorCommand::TrimClipEnd { id, new_duration } => {
                trim_clip_end(&mut self.state, id, new_duration);
            }

            // ── Export ───────────────────────────────────────────────────────
            EditorCommand::RenderMP4 {
                filename,
                width,
                height,
                fps,
            } => {
                self.begin_render(filename, width, height, fps);
            }
            EditorCommand::CancelEncode(job_id) => {
                self.context.media_worker.cancel_encode(job_id);
                // Do NOT clear encode state here — wait for the EncodeError result
                // ("cancelled") to arrive over the channel so the UI transition is
                // driven by the same path as a real error (avoids race conditions).
            }
            EditorCommand::ClearEncodeStatus => {
                self.state.encode_job = None;
                self.state.encode_progress = None;
                self.state.encode_done = None;
                self.state.encode_error = None;
            }

            // ── Project reset ─────────────────────────────────────────────────
            EditorCommand::ClearProject => {
                // Context teardown — stops playback, drops audio handles, evicts
                // all GPU textures, resets scrub/playback tracking, and wipes egui
                // widget state. Worker threads stay alive so the user can continue
                // using the app on the "keep using" path after reset.
                crate::helpers::reset::reset_context(&mut self.context, Some(ctx));

                // Wipe serialisable project data.
                self.state.library.clear();
                self.state.timeline.clear();
                self.state.transitions.clear();
                self.state.selected_timeline_clip = None;
                self.state.selected_library_clip = None;
                self.state.current_time = 0.0;
                self.state.is_playing = false;

                // Zero encode state (may have been mid-encode).
                self.state.encode_job = None;
                self.state.encode_progress = None;
                self.state.encode_done = None;
                self.state.encode_error = None;

                // Clear undo/redo — stale snapshots waste memory and there is
                // nothing meaningful to undo after a full wipe.
                self.undo_stack = VecDeque::new();
                self.redo_stack = VecDeque::new();
                self.sync_undo_len();

                // Clear the deferred audio-cleanup queue. export_module called
                // reset::delete_temp_files() before pushing this command, so every
                // velocut_* file is already gone. Clearing prevents poll_media from
                // attempting a second deletion next frame and logging a spurious
                // "file not found".
                self.state.pending_audio_cleanup.clear();

                // Re-arm the startup size guard so the timeline height is
                // re-injected on the next frame after egui memory is fresh.
                self.startup_size_checked = false;
                self.reset_done = true;
                self.preview.current_frame = None;
                // held_frame is private, so either make it pub or add a reset method on PreviewModule

                self.library.multi_selection = HashSet::new();
                self.library.visible_ids = HashSet::new();

                self.preview.reset();
            }
            EditorCommand::SetCrossfadeDuration(secs) => {
                // Batch operation: set crossfade on every touching adjacent pair,
                // or clear all transitions if secs == 0.
                if secs <= 0.0 {
                    self.state.transitions.clear();
                } else {
                    // Collect refs sorted by start_time — avoids cloning clip data,
                    // only allocates a Vec of pointers (8 bytes each).
                    let mut sorted: Vec<&TimelineClip> = self.state.timeline.iter().collect();
                    sorted.sort_unstable_by(|a, b| a.start_time.total_cmp(&b.start_time));
                    self.state.transitions.clear();
                    for i in 0..sorted.len().saturating_sub(1) {
                        let a = sorted[i];
                        let b = sorted[i + 1];
                        // Only pair clips on the same track that are touching.
                        if a.track_row != b.track_row {
                            continue;
                        }
                        let gap = b.start_time - (a.start_time + a.duration);
                        if gap.abs() > 0.1 {
                            continue;
                        }
                        self.state.transitions.push(TimelineTransition {
                            after_clip_id: a.id,
                            kind: TransitionType::new(TransitionKind::Crossfade, secs),
                        });
                    }
                }
            }
            EditorCommand::SetTransition {
                after_clip_id,
                kind,
            } => {
                // Clamp the stored duration so the centered blend zone fits
                // inside the two adjacent clips. The duration slider allows up
                // to 3s; on a 0.4s clip an unclamped zone spilled outside both
                // clips, putting the playhead on a neighbouring clip while a
                // transition was reported — wrong blends on screen. Trimming a
                // clip shorter later is handled by `clamped_secs` at use sites.
                let kind = clip_query::transition_boundary(&self.state, after_clip_id)
                    .map(|(clip_a, clip_b)| {
                        let dur = kind.clamped_secs(clip_a.duration, clip_b.duration) as f32;
                        TransitionType::new(kind.kind, dur)
                    })
                    .unwrap_or(kind);
                if let Some(t) = self
                    .state
                    .transitions
                    .iter_mut()
                    .find(|t| t.after_clip_id == after_clip_id)
                {
                    t.kind = kind;
                } else if kind != TransitionType::cut() {
                    self.state.transitions.push(TimelineTransition {
                        after_clip_id,
                        kind,
                    });
                }
            }
            EditorCommand::RemoveTransition(after_clip_id) => {
                self.state
                    .transitions
                    .retain(|t| t.after_clip_id != after_clip_id);
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
                    self.context.media_worker.extract_frame_hq(
                        lib.id,
                        path,
                        timestamp,
                        std::path::PathBuf::new(),
                    );
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
            velocut_log!("[export] ignoring RenderMP4: encode already in progress");
            return;
        }

        let default_name = format!("{filename}.mp4");
        let dest = match FileDialog::new()
            .set_file_name(&default_name)
            .add_filter("MP4 Video", &["mp4"])
            .save_file()
        {
            Some(p) => p,
            None => return, // user cancelled the dialog — no-op
        };

        // Sort by start_time using refs — avoids cloning all clip data.
        let mut sorted: Vec<&TimelineClip> = self.state.timeline.iter().collect();
        sorted.sort_unstable_by(|a, b| a.start_time.total_cmp(&b.start_time));

        let Some((clip_specs, encode_transitions, audio_overlays)) =
            build_encode_plan(&self.state, &sorted)
        else {
            velocut_log!("[export] no resolvable clips — aborting render");
            return;
        };

        let job_id = Uuid::new_v4();
        let spec = EncodeSpec {
            job_id,
            clips: clip_specs,
            width,
            height,
            fps,
            output: dest,
            transitions: encode_transitions,
            audio_overlays,
        };

        // Arm encode state before handing to the worker so ingest_media_results
        // can route EncodeProgress into the right fields immediately.
        self.state.encode_job = Some(job_id);
        self.state.encode_progress =
            Some((0, (self.state.total_duration() * fps as f64).ceil() as u64));
        self.state.encode_done = None;
        self.state.encode_error = None;

        self.context.media_worker.start_encode(spec);
    }

    fn poll_media(&mut self, ctx: &egui::Context) {
        // ── Pre-frame housekeeping ────────────────────────────────────────────
        for path in self.state.pending_audio_cleanup.drain(..) {
            cleanup_audio_temp(&path);
        }

        let mut pending = std::mem::take(&mut self.state.pending_probes);
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
        let extracts = std::mem::take(&mut self.state.pending_extracts);
        for (id, path, ts, dest) in extracts {
            self.context
                .media_worker
                .extract_frame_hq(id, path, ts, dest);
        }

        // NOTE: The save-frame dialog ideally belongs in ExportModule since it is
        // purely an export concern and rfd is already imported there. It lives here
        // because EditorModule::ui() receives &ProjectState (read-only), so modules
        // cannot own or drain pending_save_pick themselves. If the trait is ever
        // widened to &mut ProjectState, move this block to ExportModule::ui().
        if let Some((path, ts)) = self.state.pending_save_pick.take() {
            let stem = path
                .file_stem()
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
                self.context
                    .media_worker
                    .extract_frame_hq(Uuid::nil(), path, ts, dest);
            }
        }

        // ── Playback frame consumption (PTS-gated) ────────────────────────────
        VideoModule::poll_playback(&self.state, &mut self.context, ctx);

        // ── Dispatch all queued MediaWorker results into caches / state ───────
        self.context.ingest_media_results(&mut self.state, ctx);
        if self.context.cache.pending_pb_frame.is_some()
            || self.context.playback.last_frame_req.is_some()
        {
            ctx.request_repaint();
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
}

// ── eframe::App ───────────────────────────────────────────────────────────────

impl eframe::App for VeloCutApp {
    fn persist_egui_memory(&self) -> bool {
        // After a reset both of eframe's storage writes must be suppressed:
        //   1. App::save()           — guarded by the early return below.
        //   2. egui internal memory  — guarded here (panel sizes, scroll
        //                             positions, open-popup flags).
        // Without this, eframe still flushes egui's Memory to disk even when
        // save() is a no-op, recreating %APPDATA%\VeloCut\data\ on its own.
        !self.reset_done
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        // After a reset the app-data directory has been deleted. Skip writing
        // so eframe does not recreate %APPDATA%\VeloCut\data\ and undo that.
        if self.reset_done {
            return;
        }
        // audio_path points to temp WAVs that are deleted on exit — clear them
        // before serializing so they don't resurrect as dead paths on next launch.
        let mut project = self.state.clone();
        for clip in &mut project.library {
            clip.audio_path = None;
        }
        eframe::set_value(storage, eframe::APP_KEY, &AppStorage { project });
    }

    fn on_exit(&mut self) {
        self.context.media_worker.shutdown();
        self.context.audio_sinks.clear();
        self.context.audio_overlay_sinks.clear();
        for clip in &self.state.library {
            if let Some(apath) = &clip.audio_path {
                cleanup_audio_temp(apath);
            }
        }
        // eframe flushes its own storage (window geometry, egui panel state)
        // after on_exit() returns, which recreates %APPDATA%\VeloCut\data\
        // even if our save() is a no-op. schedule_app_data_dir_deletion()
        // hard-exits the process before that flush can run.
        if self.reset_done {
            crate::helpers::reset::schedule_app_data_dir_deletion(&mut self.context);
        }
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── Taskbar icon fix (Windows only, fires once) ───────────────────────
        // Must run from logic(), not new() — the OS window doesn't exist yet
        // when new() is called, so EnumThreadWindows finds nothing to patch there.
        if !self.taskbar_icon_fixed {
            crate::fix_taskbar_icon();
            self.taskbar_icon_fixed = true;
        }

        // ── Startup size guard ────────────────────────────────────────────────
        // eframe restores persisted window geometry. If a previous run left the
        // window at tiny/zero dimensions (e.g. the resizable-panel bug), we snap
        // back to the default on the very first rendered frame. Fires once only.
        // ── Startup layout guard ──────────────────────────────────────────────
        // On the first frame the native window may report a near-zero viewport
        // before the OS has actually sized it. A bottom panel that tries to
        // claim 340 px gets clamped to whatever tiny space exists, stores that
        // in egui memory, and every later frame uses the stored tiny value —
        // even after clearing storage (the race re-fires on the next cold start).
        //
        // Fix: spin here until the viewport has settled to a real size, then
        // actively insert_temp the desired timeline height. insert_temp wins over
        // any previously persisted value for this key, so `default_height` is
        // irrelevant — we own the initial height explicitly.
        if !self.startup_size_checked {
            let screen = ctx.viewport_rect();
            const TARGET_W: f32 = 1465.0;
            const TARGET_H: f32 = 965.0;
            const SETTLE_MIN_H: f32 = 900.0;

            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                TARGET_W, TARGET_H,
            )));

            if screen.height() >= SETTLE_MIN_H {
                self.startup_size_checked = true;

                let timeline_h = (screen.height() * 0.35).clamp(300.0, 420.0);
                ctx.memory_mut(|mem| {
                    mem.data
                        .insert_temp(egui::Id::new("timeline_panel"), timeline_h);
                    mem.data
                        .insert_persisted(egui::Id::new("timeline_panel"), timeline_h);
                });
            }

            ctx.request_repaint();
            return;
        }
        self.handle_drag_and_drop(ctx);
        self.poll_media(ctx);

        // ── Process commands emitted by modules this frame ────────────────────
        let cmds = std::mem::take(&mut self.pending_cmds);
        for cmd in cmds {
            self.process_command(cmd, ctx);
        }

        self.tick_modules(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render_panels(ui);
    }
}

// ── Timeline split ───────────────────────────────────────────────────────────
//
// Split is one atomic operation over `ProjectState` so that a cut never leaves
// the project internally inconsistent: the colour filter, volume, fades and the
// A/V link survive the cut, the transition keyed on the clip follows it to the
// tail, and an extracted-audio partner is cut at the same instant.

/// Minimum room on each side of the cut, in seconds, for a split to be worth
/// doing (2 frames at the 30 fps timeline rate).
const MIN_SPLIT: f64 = 2.0 / 30.0;

/// Pick the clip to split at `t`.
///
/// The selected clip wins, so overlapping lanes (V1/V2) split the clip the user
/// is actually working on; otherwise the first clip under the playhead is used.
fn split_target_at(state: &ProjectState, t: f64) -> Option<TimelineClip> {
    let fits = |c: &TimelineClip| {
        t > c.start_time + MIN_SPLIT && t < c.start_time + c.duration - MIN_SPLIT
    };
    state
        .timeline
        .iter()
        .find(|c| Some(c.id) == state.selected_timeline_clip && fits(c))
        .or_else(|| state.timeline.iter().find(|c| fits(c)))
        .cloned()
}

/// Split `clip` at absolute timeline time `t`, returning the id of the new tail
/// half, or `None` when `t` leaves less than `MIN_SPLIT` on either side.
///
/// The head half is shortened in place. Everything describing the source
/// (volume, filter, `audio_muted`, A/V link, fade-out) is carried onto the tail
/// so both halves render identically; only the fades owned by each end differ.
fn split_clip_at(state: &mut ProjectState, clip: &TimelineClip, t: f64) -> Option<Uuid> {
    let split_offset = t - clip.start_time; // seconds into the clip
    if split_offset <= MIN_SPLIT || split_offset >= clip.duration - MIN_SPLIT {
        return None;
    }

    // Head half: shortened in place. Its fade-out belongs to the tail now.
    if let Some(head) = state.timeline.iter_mut().find(|c| c.id == clip.id) {
        head.duration = split_offset;
        head.fade_out_secs = 0.0;
        head.fade_out_end_secs = 0.0;
    }

    let tail_id = Uuid::new_v4();
    state.timeline.push(TimelineClip {
        id: tail_id,
        media_id: clip.media_id,
        start_time: t,
        duration: clip.duration - split_offset,
        source_offset: clip.source_offset + split_offset,
        track_row: clip.track_row,
        volume: clip.volume,
        linked_clip_id: clip.linked_clip_id,
        audio_muted: clip.audio_muted,
        fade_in_secs: 0.0,
        fade_in_start_secs: 0.0,
        fade_out_secs: clip.fade_out_secs,
        fade_out_end_secs: clip.fade_out_end_secs,
        filter: clip.filter.clone(),
    });

    // Transitions are keyed by the clip that comes *before* the cut. The effect
    // belongs to the clip's end, so the key moves to the tail — leaving it on
    // the head would fire the transition at the split point instead of the real
    // boundary.
    for tr in state.transitions.iter_mut() {
        if tr.after_clip_id == clip.id {
            tr.after_clip_id = tail_id;
        }
    }

    Some(tail_id)
}

/// Split `clip` and, if it is paired with an extracted audio track, split the
/// partner at the same instant so the two halves stay in sync. The halves are
/// re-paired head↔head and tail↔tail.
fn split_clip_and_partners(state: &mut ProjectState, clip: &TimelineClip, t: f64) {
    // Capture partners *before* mutating: the pairing fields are rewritten
    // below, so searching afterwards would miss them.
    let partners: Vec<Uuid> = state
        .timeline
        .iter()
        .filter(|c| {
            c.id != clip.id
                && (clip.linked_clip_id == Some(c.id) || c.linked_clip_id == Some(clip.id))
        })
        .map(|c| c.id)
        .collect();

    let Some(tail_id) = split_clip_at(state, clip, t) else {
        return;
    };

    for pid in partners {
        let Some(partner) = state.timeline.iter().find(|c| c.id == pid).cloned() else {
            continue;
        };
        // Same relative instant inside the partner's own range.
        let partner_t = partner.start_time + (t - clip.start_time);
        match split_clip_at(state, &partner, partner_t) {
            Some(partner_tail) => {
                for (id, linked) in [(tail_id, partner_tail), (partner_tail, tail_id)] {
                    if let Some(c) = state.timeline.iter_mut().find(|c| c.id == id) {
                        c.linked_clip_id = Some(linked);
                    }
                }
            }
            None => {
                // The partner was nudged off the split instant, so the pair
                // cannot be cut together. The new half inherited the head's
                // one-sided link and its muted flag; clear both so it plays its
                // own audio instead of pointing at the head's partner and
                // going silent. The head keeps the real pair.
                if let Some(c) = state.timeline.iter_mut().find(|c| c.id == tail_id) {
                    c.linked_clip_id = None;
                    c.audio_muted = false;
                }
            }
        }
    }
}

// ── Timeline edit helpers ────────────────────────────────────────────────────

// ── Same-track overlap policy ─────────────────────────────────
//
// Moves ripple, trims prevent. A moved clip keeps its snapped position; the
// first clip its span actually lands on — and everything behind that clip —
// shifts right just enough to clear it. Gaps absorb a move, so distant clips
// are never shoved (a free-position timeline is not a gapless magnetic one),
// and because the push is overlap-gated it is idempotent across the per-frame
// move commands. Nothing is destroyed, and no silent overlap is created.
//
// Trims never shove other clips around: a head that would cross the left
// neighbour stops at the neighbour's end, a tail that would cross the right
// neighbour stops at the neighbour's start. Butt-joins against a neighbour
// remain legal, so an overlap left over from an older project is still
// reachable by undo, and the lookup helpers break those ties deterministically
// (later start wins).

// The policy is deterministic because the row-lookup helpers below sort by
// start time and fold with a well-defined tie-break, so the same state always
// yields the same clamped edge.

/// Clips on `row`, other than `except_id`, sorted by start time.
///
/// Shared by the trim clamps and the move ripple so all three operations read
/// the row through one definition of "the other clips on this track".
fn row_clips(state: &ProjectState, row: usize, except_id: Uuid) -> Vec<&TimelineClip> {
    let mut row_clips: Vec<&TimelineClip> = state
        .timeline
        .iter()
        .filter(|c| c.track_row == row && c.id != except_id)
        .collect();
    row_clips.sort_unstable_by(|a, b| a.start_time.total_cmp(&b.start_time));
    row_clips
}

/// The furthest a head trim may reach: the right-most end of any other clip on
/// the row that ends at or before `head` (0.0 when there is none). `ADJACENCY_EPS`
/// keeps a butt-join exactly zero-gap without epsilon drift reading as overlap.
fn trim_start_time_limit(state: &ProjectState, row: usize, except_id: Uuid, head: f64) -> f64 {
    row_clips(state, row, except_id)
        .iter()
        .filter(|c| c.start_time + c.duration <= head + clip_query::ADJACENCY_EPS)
        .map(|c| c.start_time + c.duration)
        .fold(0.0_f64, f64::max)
}

/// The furthest a tail trim may reach: the left-most start of any other clip on
/// the row (INFINITY when there is none). Clips whose start sits left of
/// `tail` — a legacy overlap — still bound the tail so the trim cannot grow
/// the overlap.
fn trim_end_time_limit(state: &ProjectState, row: usize, except_id: Uuid, head: f64) -> f64 {
    row_clips(state, row, except_id)
        .iter()
        .filter(|c| c.start_time > head + clip_query::ADJACENCY_EPS)
        .map(|c| c.start_time)
        .fold(f64::INFINITY, f64::min)
}

/// Trim a clip's head by setting its in-point and length. The left edge stops
/// at the left neighbour's end — trims must not create (or grow) an overlap.
/// The remaining geometry is mirrored onto the linked audio partner.
///
/// `new_source_offset` both sources the trim and anchors it: the delta it moved
/// is exactly how far the start slides, so the **tail does not move** unless
/// the head had to clamp against a neighbour (which eats into the clip instead
/// of shoving it).
fn trim_clip_start(state: &mut ProjectState, id: Uuid, new_source_offset: f64, new_duration: f64) {
    let (row, head, tail, source_offset) = match state.timeline.iter().find(|c| c.id == id) {
        Some(c) => (
            c.track_row,
            c.start_time,
            c.start_time + c.duration,
            c.source_offset,
        ),
        None => return,
    };
    let new_start = head + (new_source_offset - source_offset);
    let clamped_start = new_start.max(trim_start_time_limit(state, row, id, head));
    // The edge was clamped right of where the trim wanted it, so the trimmed
    // head keeps a little extra: add back at most what was given away by the
    // clamp and never run past the unmoved tail.
    let duration = (new_duration + (clamped_start - new_start))
        .max(0.0)
        .min(tail - clamped_start);

    // The in-point follows the edge that actually moved, not the requested one:
    // a head clamped by a neighbour must not slip the source content under a
    // stationary span. Without the clamp this is exactly `new_source_offset`.
    let actual_source_offset = (source_offset + (clamped_start - head)).max(0.0);

    if let Some(c) = state.timeline.iter_mut().find(|c| c.id == id) {
        c.start_time = clamped_start;
        c.source_offset = actual_source_offset;
        c.duration = duration;
    }
    state.sync_linked_clip(id);
}

/// Trim a clip's tail, keeping its head fixed. The tail stops at the next
/// clip's start — trims must not create (or grow) an overlap.
fn trim_clip_end(state: &mut ProjectState, id: Uuid, new_duration: f64) {
    let (row, head) = match state.timeline.iter().find(|c| c.id == id) {
        Some(c) => (c.track_row, c.start_time),
        None => return,
    };
    let limit = trim_end_time_limit(state, row, id, head);
    let duration = new_duration.min(limit - head).max(0.0);

    if let Some(c) = state.timeline.iter_mut().find(|c| c.id == id) {
        c.duration = duration;
    }
    state.sync_linked_clip(id);
}

/// The clip mutually linked to `id` (either side may hold the pointer) —
/// None when the link is one-sided or stale.
fn linked_partner_id(state: &ProjectState, id: Uuid) -> Option<Uuid> {
    let me = state.timeline.iter().find(|c| c.id == id)?;
    let partner_id = me.linked_clip_id.or_else(|| {
        state
            .timeline
            .iter()
            .find(|c| c.linked_clip_id == Some(id))
            .map(|c| c.id)
    })?;
    // A real pair points both ways; a stale link must not teleport a stranger.
    let partner_points_back = state
        .timeline
        .iter()
        .any(|c| c.id == partner_id && c.linked_clip_id == Some(id));
    partner_points_back.then_some(partner_id)
}

/// Move a clip to a new start time and track row, leaving the target row
/// overlap-free by ripple: any clip the moved span would land on — and
/// everything to its right — shifts right just enough to clear it. The moved
/// clip keeps its snapped position; nothing is deleted, split or silently
/// overlapped.
///
/// The linked audio partner follows only when the moved clip is the V-row
/// anchor (`sync_linked_clip` deliberately treats an A-row origin as a manual
/// nudge, so an A-only drag must not shuffle the video row). The partner's row
/// is made room on before the sync writes its new span.
///
/// `MoveTimelineClip` arrives every frame during a drag, so this must be
/// idempotent: `make_room_on_row` only shifts by the overlap that still exists,
/// never by the whole clip duration per frame.
fn move_clip(state: &mut ProjectState, id: Uuid, new_start: f64, new_row: usize) {
    let Some((moved_row, moved_duration)) = state
        .timeline
        .iter()
        .find(|c| c.id == id)
        .map(|c| (c.track_row, c.duration))
    else {
        return;
    };
    let start = new_start.max(0.0);

    // A-row origins are manual nudges: they never move (or ripple) the V row.
    let partner = if moved_row % 2 == 0 {
        linked_partner_id(state, id)
    } else {
        None
    };

    if let Some(pid) = partner {
        if let Some(partner_row) = state
            .timeline
            .iter()
            .find(|c| c.id == pid)
            .map(|c| c.track_row)
        {
            // The partner is relocated by the sync below, so exclude it: it
            // must not also be pushed as a bystander.
            state.make_room_on_row(partner_row, start, moved_duration, &[id, pid]);
        }
    }

    let mut except = vec![id];
    if let Some(pid) = partner {
        except.push(pid);
    }
    state.make_room_on_row(new_row, start, moved_duration, &except);

    if let Some(c) = state.timeline.iter_mut().find(|c| c.id == id) {
        c.start_time = start;
        c.track_row = new_row;
    }
    // Mirror to the partner last, once both rows have been re-laid.
    state.sync_linked_clip(id);
}

// ── Separated audio helpers ──────────────────────────────────────────────────

/// Separated-audio entries whose temp WAV has to be rebuilt on startup.
///
/// Those entries keep the WAV in `audio_path`, which `save()` clears because
/// the files live in the temp dir and are deleted on exit. The entry's own
/// `path` is the original media file, so the source can be re-derived through
/// the V↔A link and re-extracted. Probing the recorded temp path would fail and
/// leave the A row permanently silent.
///
/// Returns `(library id, source media path)` pairs. Entries that still have a
/// live WAV, standalone audio, and broken links are left alone.
fn pending_audio_repairs(state: &ProjectState) -> Vec<(Uuid, PathBuf)> {
    state
        .library
        .iter()
        .filter(|lib| {
            lib.clip_type == ClipType::Audio
                && lib.audio_path.as_ref().is_none_or(|path| !path.exists())
        })
        .filter_map(|lib| state.audio_library_source(lib.id).map(|src| (lib.id, src)))
        .collect()
}

/// Build the library entry for an extracted audio track.
///
/// The entry's `path` stays the ORIGINAL media file: it is the identity used to
/// re-probe or re-extract after a restart, when the temp WAV recorded in
/// `audio_path` has been cleared and deleted. Reusing a WAV that still exists
/// keeps its trimmed offset; a fresh full-file extraction resets it to 0 so the
/// seek base matches the file the worker is about to write. Returns the entry
/// and whether that extraction still has to be requested.
fn separated_audio_entry(
    id: Uuid,
    src_path: PathBuf,
    name: String,
    duration: f64,
    peaks: Vec<f32>,
    wav_path: Option<PathBuf>,
    wav_trimmed_offset: f64,
) -> (LibraryClip, bool) {
    let reuse = wav_path.as_ref().is_some_and(|path| path.exists());
    let entry = LibraryClip {
        id,
        path: src_path,
        name,
        duration,
        clip_type: ClipType::Audio,
        thumbnail_path: None,
        duration_probed: true,
        waveform_peaks: peaks,
        video_size: None,
        audio_path: if reuse { wav_path } else { None },
        audio_trimmed_offset: if reuse { wav_trimmed_offset } else { 0.0 },
    };
    (entry, !reuse)
}

// ── Encode plan builder ──────────────────────────────────────────────────────
//
// Pure function — takes state + a pre-sorted slice of timeline refs and returns
// the (ClipSpec, ClipTransition) vecs needed to build an EncodeSpec, or None
// when no resolvable clips exist.
//
// Extracted from begin_render so it's independently testable and so begin_render
// itself stays focused on I/O (file dialog, worker dispatch, state arming).

/// Window tolerance for "this extracted A partner is still in sync with its V
/// anchor". Values copied by `sync_linked_clip` are bit-identical; this only
/// absorbs float noise from serialization.
const AUDIO_WINDOW_EPS: f64 = 1.0e-6;

/// Build the encoder's absolute-time audio overlay for `tc`.
///
/// Standalone A-row clips always go through this. An extracted-audio partner
/// also does when its window was nudged/trimmed away from the V anchor: the
/// preview plays that A clip at its own `start_time`/`source_offset`/`duration`
/// and its own volume/fade envelope, so the export has to mix it at the same
/// absolute time instead of encoding the V clip's window.
fn audio_overlay_for(
    state: &velocut_core::state::ProjectState,
    tc: &TimelineClip,
) -> Option<AudioOverlay> {
    let lib = clip_query::library_entry_for(state, tc)?;
    Some(AudioOverlay {
        path: lib.path.clone(),
        source_offset: tc.source_offset,
        timeline_start: tc.start_time,
        duration: tc.duration,
        volume: tc.volume,
        fade_in_secs: tc.fade_in_secs,
        fade_in_start_secs: tc.fade_in_start_secs,
        fade_out_secs: tc.fade_out_secs,
        fade_out_end_secs: tc.fade_out_end_secs,
    })
}

fn build_encode_plan(
    state: &velocut_core::state::ProjectState,
    sorted: &[&TimelineClip],
) -> Option<(Vec<ClipSpec>, Vec<ClipTransition>, Vec<AudioOverlay>)> {
    // Only V-row clips (even track_row) drive the sequential video/audio encode.
    //
    // Two categories of A-row clips must be excluded from this list:
    //   1. Extracted-audio clips (odd row + linked_clip_id) — their audio is
    //      already encoded via the paired V-row clip using the A-row's volume.
    //   2. Standalone audio clips (odd row, no linked_clip_id) — these are
    //      independent audio files dragged to an A-row track.  Including them
    //      here causes encode_clip to open an audio-only file, find no video
    //      stream, and abort the entire export with "no video stream in '...'".
    //      Their audio is handled separately as AudioOverlay entries below.
    // Also drop clips whose library entry is missing, before any index math:
    // clip_specs and filtered must stay index-aligned or a transition would be
    // attached to the wrong cut.
    let filtered: Vec<&TimelineClip> = sorted
        .iter()
        .copied()
        .filter(|tc| tc.track_row % 2 == 0) // V-row clips only
        .filter(|tc| state.library.iter().any(|lc| lc.id == tc.media_id))
        .collect();

    let mut extracted_overlays: Vec<AudioOverlay> = Vec::new();

    let clip_specs: Vec<ClipSpec> = filtered
        .iter()
        .copied()
        .filter_map(|tc| {
            state
                .library
                .iter()
                .find(|lc| lc.id == tc.media_id)
                .map(|lc| {
                    // When audio was extracted to an A-row partner (audio_muted=true),
                    // use that partner's volume AND fade envelope so adjustments made
                    // via the 🔊 badge on the A-row clip are reflected in the export.
                    // For plain clips (audio_muted=false) fall back to the V-row values.
                    let linked_audio = if tc.audio_muted {
                        clip_query::linked_audio_clip(state, tc)
                    } else {
                        None
                    };

                    // The V→A link is one-directional: V anchors, but the A row may
                    // be nudged or trimmed to correct drift. `sync_linked_clip` never
                    // mirrors A→V, so a diverged window means the preview is playing
                    // the A clip at its own absolute time while the export would still
                    // encode the V window. Route that case through the overlay path
                    // (and silence the V clip) to keep preview and export identical.
                    let audio_diverged = linked_audio.is_some_and(|ac| {
                        (ac.start_time - tc.start_time).abs() > AUDIO_WINDOW_EPS
                            || (ac.duration - tc.duration).abs() > AUDIO_WINDOW_EPS
                            || (ac.source_offset - tc.source_offset).abs() > AUDIO_WINDOW_EPS
                    });
                    if audio_diverged {
                        if let Some(overlay) =
                            linked_audio.and_then(|ac| audio_overlay_for(state, ac))
                        {
                            extracted_overlays.push(overlay);
                        }
                    }

                    // A muted V clip is silent in the preview when its partner is
                    // missing or cannot be resolved; a resolvable in-sync partner is
                    // encoded through this spec (the content is the same source).
                    let skip_clip_audio = if tc.audio_muted {
                        match linked_audio {
                            None => true,
                            Some(ac) => {
                                audio_diverged || clip_query::library_entry_for(state, ac).is_none()
                            }
                        }
                    } else {
                        false
                    };

                    let effective_volume = linked_audio.map(|ac| ac.volume).unwrap_or(tc.volume);

                    let (effective_fi, effective_fi_start, effective_fo, effective_fo_end) =
                        linked_audio
                            .map(|ac| {
                                (
                                    ac.fade_in_secs,
                                    ac.fade_in_start_secs,
                                    ac.fade_out_secs,
                                    ac.fade_out_end_secs,
                                )
                            })
                            .unwrap_or((
                                tc.fade_in_secs,
                                tc.fade_in_start_secs,
                                tc.fade_out_secs,
                                tc.fade_out_end_secs,
                            ));

                    ClipSpec {
                        path: lc.path.clone(),
                        timeline_start: tc.start_time,
                        source_offset: tc.source_offset,
                        duration: tc.duration,
                        volume: effective_volume,
                        skip_audio: skip_clip_audio,
                        fade_in_secs: effective_fi,
                        fade_in_start_secs: effective_fi_start,
                        fade_out_secs: effective_fo,
                        fade_out_end_secs: effective_fo_end,
                        filter: tc.filter.clone(),
                    }
                })
        })
        .collect();

    if clip_specs.is_empty() {
        return None;
    }

    // Map TimelineTransitions (UUID-keyed) → ClipTransitions (index-keyed).
    // Uses position in *filtered* vec, not raw timeline, so A-row clips don't
    // shift transition indices.
    //
    // A transition only exists where two clips on the same video lane actually
    // meet. A gap, or the next clip being on a different V row, has no shared
    // boundary — the preview shows a hard cut there (`transition_boundary`), so
    // the export must not blend across it either.
    let encode_transitions: Vec<ClipTransition> = state
        .transitions
        .iter()
        .filter_map(|t| {
            let idx = filtered
                .iter()
                .copied()
                .position(|tc| tc.id == t.after_clip_id)?;
            let clip_a = filtered.get(idx)?;
            let clip_b = filtered.get(idx + 1)?;
            if clip_b.track_row != clip_a.track_row
                || (clip_b.start_time - (clip_a.start_time + clip_a.duration)).abs()
                    > clip_query::ADJACENCY_EPS
            {
                return None;
            }
            Some(ClipTransition {
                after_clip_index: idx,
                kind: t.kind.clone(),
            })
        })
        .collect();

    // Collect standalone audio clips — odd track_row with no linked_clip_id.
    //
    // These are NOT extracted-audio partners; they are independent audio files
    // (e.g. a music track) dragged to an A-row.  Each one becomes an
    // AudioOverlay: its PCM is pre-decoded once in run_encode and then mixed
    // into the AAC encoder FIFO at the correct output-timeline position as
    // video frames are encoded.  The mixing is additive: if a V-row clip also
    // has un-muted audio, both sources contribute to the output.
    //
    // `extracted_overlays` holds linked A partners whose window diverged from
    // the V anchor (see the clip-spec loop above); preview plays those at their
    // own absolute time too.
    //
    // Note: `sorted` contains ALL timeline clips, so we iterate it directly
    // rather than `filtered` (which only contains V-row clips).
    let audio_overlays: Vec<AudioOverlay> = sorted
        .iter()
        .copied()
        .filter(|tc| tc.track_row % 2 == 1 && tc.linked_clip_id.is_none())
        .filter_map(|tc| audio_overlay_for(state, tc))
        .chain(extracted_overlays)
        .collect();

    if !audio_overlays.is_empty() {
        eprintln!(
            "[render] {} audio overlay(s) included in export",
            audio_overlays.len()
        );
    }

    Some((clip_specs, encode_transitions, audio_overlays))
}

// ── Sub-frame helpers (called from update) ────────────────────────────────────

impl VeloCutApp {
    /// Render all panels and the encode modal overlay.
    /// Called from `ui()` after pre-frame housekeeping.
    fn render_panels(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();

        egui::Panel::bottom("timeline_panel")
            .resizable(true)
            .min_size(340.0)
            .default_size(340.0)
            .show_inside(ui, |ui| {
                self.timeline.ui(
                    ui,
                    &self.state,
                    &mut self.context.cache.thumbnail_cache,
                    &mut self.pending_cmds,
                );
            });

        egui::Panel::left("library_panel")
            .resizable(true)
            .min_size(240.0)
            .default_size(240.0)
            .show_inside(ui, |ui| {
                self.library.ui(
                    ui,
                    &self.state,
                    &mut self.context.cache.thumbnail_cache,
                    &mut self.pending_cmds,
                );
            });

        egui::Panel::right("export_panel")
            .resizable(true)
            .default_size(220.0)
            .min_size(220.0)
            .show_inside(ui, |ui| {
                self.export.ui(
                    ui,
                    &self.state,
                    &mut self.context.cache.thumbnail_cache,
                    &mut self.pending_cmds,
                );
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            let active_id = VideoModule::active_media_id(&self.state);
            self.preview.current_frame =
                active_id.and_then(|id| self.context.cache.frame_cache.get(&id).cloned());

            self.preview.ui(
                ui,
                &self.state,
                &mut self.context.cache.thumbnail_cache,
                &mut self.pending_cmds,
            );
        });

        self.export
            .show_render_modal(&ctx, &self.state, &mut self.pending_cmds);
        crate::helpers::reset::show_uninstall_modal(&ctx, &mut self.export.show_reset_complete);
    }

    /// Tick non-rendering modules and advance the playback clock.
    /// Called from `update()` after command processing so ticks see the
    /// freshest state (including commands processed this frame).
    fn tick_modules(&mut self, ctx: &egui::Context) {
        // preview.last_canvas_size is written by preview.ui() inside render_panels(),
        // which runs before tick_modules() — so this is always the current panel size.
        VideoModule::tick(
            &self.state,
            &mut self.context,
            ctx,
            self.preview.last_canvas_size,
        );
        self.audio.tick(&self.state, &mut self.context);
        self.memory_manager
            .tick(ctx, &self.state, &mut self.context);
        if self.state.is_playing {
            let dt = ctx.input(|i| i.stable_dt as f64);
            self.state.current_time += dt;
            let total = self.state.total_duration();
            if total > 0.0 && self.state.current_time >= total {
                self.state.current_time = total - 0.001;
                self.state.is_playing = false;
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

#[cfg(test)]
mod split_tests {
    use super::*;
    use velocut_core::filters::{FilterKind, FilterParams};
    use velocut_core::transitions::TransitionKind;

    fn clip(id: Uuid, start: f64, dur: f64, row: usize, media_id: Uuid) -> TimelineClip {
        TimelineClip {
            id,
            media_id,
            start_time: start,
            duration: dur,
            track_row: row,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: None,
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: FilterParams::none(),
        }
    }

    fn state_with(clips: Vec<TimelineClip>) -> ProjectState {
        ProjectState {
            timeline: clips,
            ..Default::default()
        }
    }

    #[test]
    fn split_moves_transition_key_to_tail() {
        let media = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut state = state_with(vec![
            clip(a, 0.0, 4.0, 0, media),
            clip(b, 4.0, 4.0, 0, media),
        ]);
        state.transitions.push(TimelineTransition {
            after_clip_id: a,
            kind: TransitionType {
                kind: TransitionKind::Crossfade,
                duration_secs: 0.5,
            },
        });

        let target = split_target_at(&state, 2.0).expect("clip under playhead");
        split_clip_and_partners(&mut state, &target, 2.0);

        assert_eq!(state.timeline.len(), 3);
        assert_eq!(
            state.timeline.iter().find(|c| c.id == a).unwrap().duration,
            2.0
        );
        // The transition must still describe the a→b boundary, i.e. its key is
        // now the tail clip that actually ends at 4.0.
        let key = state.transitions[0].after_clip_id;
        assert_ne!(key, a, "transition key stayed on the head half");
        let keyed = state.timeline.iter().find(|c| c.id == key).unwrap();
        assert!((keyed.start_time + keyed.duration - 4.0).abs() < 1e-9);
    }

    #[test]
    fn split_with_a_shorter_audio_partner_unlinks_the_new_half() {
        // The A row was trimmed independently (allowed as a nudge), so the V
        // split instant falls past its end and the pair cannot be cut together.
        let media = Uuid::new_v4();
        let v = Uuid::new_v4();
        let a = Uuid::new_v4();
        let mut vc = clip(v, 0.0, 4.0, 0, media);
        vc.audio_muted = true;
        vc.linked_clip_id = Some(a);
        let mut ac = clip(a, 0.0, 2.0, 1, media);
        ac.linked_clip_id = Some(v);
        let mut state = state_with(vec![vc, ac]);
        state.selected_timeline_clip = Some(v);

        let target = split_target_at(&state, 3.8).expect("V has room at 3.8");
        split_clip_and_partners(&mut state, &target, 3.8);

        let tail = state
            .timeline
            .iter()
            .find(|c| c.track_row == 0 && c.id != v)
            .expect("V tail created");
        assert_eq!(tail.linked_clip_id, None, "stale one-sided link survived");
        assert!(
            !tail.audio_muted,
            "the unpaired half must play its own audio, not stay silent"
        );
        // The head still owns the real pair, and the partner is untouched.
        assert_eq!(
            state
                .timeline
                .iter()
                .find(|c| c.id == v)
                .unwrap()
                .linked_clip_id,
            Some(a)
        );
        assert_eq!(
            state
                .timeline
                .iter()
                .find(|c| c.id == a)
                .unwrap()
                .linked_clip_id,
            Some(v)
        );
    }

    #[test]
    fn split_copies_filter_and_source_offset() {
        let media = Uuid::new_v4();
        let a = Uuid::new_v4();
        let mut state = state_with(vec![clip(a, 0.0, 4.0, 0, media)]);
        let filter = FilterParams {
            kind: FilterKind::Cinematic,
            ..FilterParams::none()
        };
        {
            let c = state.timeline.iter_mut().find(|c| c.id == a).unwrap();
            c.filter = filter.clone();
            c.volume = 0.5;
            c.fade_out_secs = 0.4;
            c.source_offset = 10.0;
        }

        let target = split_target_at(&state, 1.0).unwrap();
        split_clip_and_partners(&mut state, &target, 1.0);

        let tail = state
            .timeline
            .iter()
            .find(|c| c.id != a)
            .expect("tail half created");
        assert_eq!(tail.filter, filter, "tail lost the clip's colour filter");
        assert_eq!(tail.volume, 0.5);
        assert_eq!(tail.fade_out_secs, 0.4);
        assert!((tail.source_offset - 11.0).abs() < 1e-9);
        // Fade-out belongs to the tail only; the head keeps the fade-in side.
        assert_eq!(
            state
                .timeline
                .iter()
                .find(|c| c.id == a)
                .unwrap()
                .fade_out_secs,
            0.0
        );
    }

    #[test]
    fn split_cuts_extracted_audio_partner_in_lockstep() {
        let media = Uuid::new_v4();
        let video = Uuid::new_v4();
        let audio = Uuid::new_v4();
        let mut v = clip(video, 1.0, 5.0, 0, media);
        v.audio_muted = true;
        v.linked_clip_id = Some(audio);
        let mut a = clip(audio, 1.0, 5.0, 1, media);
        a.linked_clip_id = Some(video);
        let mut state = state_with(vec![v, a]);

        let target = split_target_at(&state, 3.5).unwrap();
        assert_eq!(target.id, video);
        split_clip_and_partners(&mut state, &target, 3.5);

        assert_eq!(state.timeline.len(), 4, "both halves of the pair expected");
        let video_tail = state
            .timeline
            .iter()
            .find(|c| c.id != video && c.track_row == 0)
            .unwrap();
        let audio_tail = state
            .timeline
            .iter()
            .find(|c| c.id != audio && c.track_row == 1)
            .unwrap();
        // Same instant, same length → the halves stay frame-locked.
        assert!((video_tail.start_time - audio_tail.start_time).abs() < 1e-9);
        assert!((video_tail.duration - audio_tail.duration).abs() < 1e-9);
        assert!((video_tail.start_time - 3.5).abs() < 1e-9);
        // Re-paired head↔head, tail↔tail.
        assert_eq!(audio_tail.linked_clip_id, Some(video_tail.id));
        assert_eq!(video_tail.linked_clip_id, Some(audio_tail.id));
        assert_eq!(
            state
                .timeline
                .iter()
                .find(|c| c.id == video)
                .unwrap()
                .linked_clip_id,
            Some(audio)
        );
        assert_eq!(
            state
                .timeline
                .iter()
                .find(|c| c.id == audio)
                .unwrap()
                .linked_clip_id,
            Some(video)
        );
    }

    #[test]
    fn split_prefers_the_selected_clip_on_overlapping_lanes() {
        let media = Uuid::new_v4();
        let v1 = Uuid::new_v4();
        let v2 = Uuid::new_v4();
        let mut state = state_with(vec![
            clip(v1, 0.0, 6.0, 0, media),
            clip(v2, 2.0, 3.0, 2, media),
        ]);
        state.selected_timeline_clip = Some(v2);

        let target = split_target_at(&state, 3.0).unwrap();
        assert_eq!(target.id, v2, "selected clip must win over stack order");
        split_clip_and_partners(&mut state, &target, 3.0);
        assert_eq!(
            state.timeline.iter().find(|c| c.id == v1).unwrap().duration,
            6.0
        );
    }

    #[test]
    fn split_refuses_cuts_too_close_to_a_boundary() {
        let media = Uuid::new_v4();
        let a = Uuid::new_v4();
        let mut state = state_with(vec![clip(a, 0.0, 4.0, 0, media)]);

        // 1 frame in — below MIN_SPLIT.
        assert!(split_target_at(&state, 1.0 / 30.0).is_none());
        let target = split_target_at(&state, 2.0).unwrap();
        assert!(split_clip_at(&mut state, &target, 0.0).is_none());
        assert!(split_clip_at(&mut state, &target, 4.0).is_none());
        assert_eq!(state.timeline.len(), 1, "no half may be created");
    }
}

#[cfg(test)]
mod timeline_edit_tests {
    use super::*;
    use velocut_core::filters::FilterParams;

    fn clip(id: Uuid, start: f64, dur: f64, row: usize) -> TimelineClip {
        TimelineClip {
            id,
            media_id: Uuid::new_v4(),
            start_time: start,
            duration: dur,
            track_row: row,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: None,
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: FilterParams::none(),
        }
    }

    fn state_with(clips: Vec<TimelineClip>) -> ProjectState {
        ProjectState {
            timeline: clips,
            ..Default::default()
        }
    }

    fn get(state: &ProjectState, id: Uuid) -> &TimelineClip {
        state.timeline.iter().find(|c| c.id == id).unwrap()
    }

    fn linked_video_with_audio(start: f64, dur: f64) -> (ProjectState, Uuid, Uuid) {
        let v = Uuid::new_v4();
        let a = Uuid::new_v4();
        let mut vclip = clip(v, start, dur, 0);
        let mut aclip = clip(a, start, dur, 1);
        vclip.linked_clip_id = Some(a);
        aclip.linked_clip_id = Some(v);
        (state_with(vec![vclip, aclip]), v, a)
    }

    #[test]
    fn head_trim_moves_the_start_and_keeps_the_tail() {
        let mut state = state_with(vec![clip(Uuid::new_v4(), 4.0, 8.0, 0)]);
        let id = state.timeline[0].id;

        // Trim 1s off the head, exactly what the UI sends.
        trim_clip_start(&mut state, id, 1.0, 7.0);

        let c = get(&state, id);
        assert_eq!(c.start_time, 5.0, "left edge must follow the trim");
        assert_eq!(c.source_offset, 1.0);
        assert_eq!(c.duration, 7.0);
        assert_eq!(c.start_time + c.duration, 12.0, "tail must not move");
    }

    #[test]
    fn head_trim_keeps_the_separated_audio_in_sync() {
        let (mut state, v, a) = linked_video_with_audio(4.0, 8.0);

        trim_clip_start(&mut state, v, 2.0, 6.0);

        let audio = get(&state, a);
        assert_eq!(audio.start_time, 6.0);
        assert_eq!(audio.source_offset, 2.0);
        assert_eq!(audio.duration, 6.0);
    }

    #[test]
    fn tail_trim_keeps_the_separated_audio_in_sync() {
        let (mut state, v, a) = linked_video_with_audio(4.0, 8.0);

        trim_clip_end(&mut state, v, 5.0);

        let audio = get(&state, a);
        assert_eq!(audio.start_time, 4.0, "head stays put");
        assert_eq!(audio.duration, 5.0);
    }

    #[test]
    fn move_carries_the_separated_audio_without_changing_its_row() {
        let (mut state, v, a) = linked_video_with_audio(4.0, 8.0);

        move_clip(&mut state, v, 20.0, 0);

        let audio = get(&state, a);
        assert_eq!(audio.start_time, 20.0);
        assert_eq!(audio.track_row, 1, "audio must stay on A1");
    }

    #[test]
    fn moving_an_audio_clip_does_not_drag_the_video() {
        let (mut state, v, a) = linked_video_with_audio(4.0, 8.0);

        move_clip(&mut state, a, 9.0, 1);

        assert_eq!(get(&state, v).start_time, 4.0);
    }

    // ── Same-track overlap policy: trims prevent, moves ripple ────────────

    #[test]
    fn head_trim_stops_at_the_left_neighbour() {
        // Neighbour ends at 6.0. The clip already wears 3s off its head; the
        // trim wants to continue to source_offset 2.0, i.e. the head moving to
        // 5.0 — into the neighbour.
        let id = Uuid::new_v4();
        let mut trimmed = clip(id, 6.0, 4.0, 0);
        trimmed.source_offset = 3.0;
        let mut state = state_with(vec![clip(Uuid::new_v4(), 2.0, 4.0, 0), trimmed]);

        trim_clip_start(&mut state, id, 2.0, 5.0);

        let c = get(&state, id);
        assert_eq!(c.start_time, 6.0, "head must stop at the neighbour's end");
        assert_eq!(
            c.source_offset, 3.0,
            "a blocked head must not slip the source under a stationary span"
        );
        // The tail must not move, so what the clamp ate from the head is kept.
        assert_eq!(c.duration, 4.0, "tail stays at 10.0");
    }

    #[test]
    fn tail_trim_stops_at_the_right_neighbour() {
        let id = Uuid::new_v4();
        let mut state = state_with(vec![
            clip(id, 0.0, 8.0, 0),
            clip(Uuid::new_v4(), 5.0, 3.0, 0),
        ]);

        trim_clip_end(&mut state, id, 7.5);

        assert_eq!(
            get(&state, id).duration,
            5.0,
            "tail must stop at the neighbour's start"
        );
    }

    #[test]
    fn tail_trim_over_a_legacy_overlap_cannot_grow_the_overlap() {
        // A legacy overlap: the neighbour already starts inside the clip.
        let id = Uuid::new_v4();
        let mut state = state_with(vec![
            clip(id, 0.0, 8.0, 0),
            clip(Uuid::new_v4(), 3.0, 5.0, 0),
        ]);

        trim_clip_end(&mut state, id, 9.0);

        assert_eq!(
            get(&state, id).duration,
            3.0,
            "tail must not pass the neighbour's start"
        );
    }

    #[test]
    fn move_pushes_what_it_lands_on_and_the_tail_behind_it() {
        let moved = Uuid::new_v4();
        let blocker = Uuid::new_v4();
        let tail = Uuid::new_v4();
        let other_row = Uuid::new_v4();
        let mut state = state_with(vec![
            clip(moved, 0.0, 2.0, 0),
            clip(blocker, 3.0, 1.0, 0),
            clip(tail, 10.0, 4.0, 0),
            // Another row: never touched by this move.
            clip(other_row, 3.0, 4.0, 1),
        ]);

        // [2, 4) overlaps the blocker's [3, 4): it must clear the moved span.
        move_clip(&mut state, moved, 2.0, 0);

        assert_eq!(get(&state, moved).start_time, 2.0);
        assert_eq!(
            get(&state, blocker).start_time,
            4.0,
            "the blocker starts where the moved clip ends"
        );
        assert_eq!(
            get(&state, tail).start_time,
            11.0,
            "relative order after the blocker is preserved"
        );
        assert_eq!(
            get(&state, other_row).start_time,
            3.0,
            "other rows untouched"
        );
    }

    #[test]
    fn move_across_a_gap_leaves_distant_clips_alone() {
        // Free-position timeline: a gap between the landing spot and the next
        // clip absorbs the move. Only a real collision may push anything.
        let moved = Uuid::new_v4();
        let later = Uuid::new_v4();
        let mut state = state_with(vec![clip(moved, 4.0, 2.0, 0), clip(later, 8.0, 4.0, 0)]);

        move_clip(&mut state, moved, 2.0, 0);

        assert_eq!(get(&state, moved).start_time, 2.0);
        assert_eq!(
            get(&state, later).start_time,
            8.0,
            "a distant clip behind a gap must not be shoved by a move"
        );
    }

    #[test]
    fn moving_video_pushes_a_colliding_audio_neighbour_clear() {
        let (mut state, v, a) = linked_video_with_audio(4.0, 8.0);
        let a_later = Uuid::new_v4();
        state.timeline.push(clip(a_later, 15.0, 4.0, 1));

        move_clip(&mut state, v, 12.0, 0);

        let audio = get(&state, a);
        assert_eq!(
            audio.start_time, 12.0,
            "the partner mirrors the video's landing point"
        );
        // The mirrored span [12, 20) overlaps [15, 19): push it clear to 20.
        assert_eq!(
            get(&state, a_later).start_time,
            20.0,
            "A-row blockers must clear the moved pair"
        );
    }

    #[test]
    fn move_pushes_colliders_in_start_order_not_document_order() {
        // Document order vs. start order: two A-row clips that differ in document
        // order must both move by the same amount.
        let (mut state, v, _) = linked_video_with_audio(4.0, 8.0);
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        // Document order: second first, then first. Start order: first, then second.
        state.timeline.push(clip(second, 20.0, 2.0, 1));
        let idx = state.timeline.iter().position(|c| c.id == second).unwrap();
        state.timeline.insert(idx, clip(first, 15.0, 2.0, 1));

        move_clip(&mut state, v, 14.0, 0);

        // Mirrored span [14, 22) collides with the earliest blocker at 15;
        // both are pushed by the same 7s, in start order.
        assert_eq!(get(&state, first).start_time, 22.0);
        assert_eq!(get(&state, second).start_time, 27.0);
    }

    #[test]
    fn slow_move_drag_does_not_accumulate_ripple() {
        let moving = Uuid::new_v4();
        let blocker = Uuid::new_v4();
        let mut state = state_with(vec![clip(moving, 0.0, 2.0, 0), clip(blocker, 10.0, 5.0, 0)]);

        // A drag re-sends the same absolute move every frame. 100 frames of a
        // slow 10s drag must leave the blocker at the moved clip's end (12.0),
        // not add a full clip duration to it on every frame.
        for frame in 1..=100 {
            move_clip(&mut state, moving, frame as f64 * 0.1, 0);
        }

        assert_eq!(get(&state, moving).start_time, 10.0);
        assert!(
            (get(&state, blocker).start_time - 12.0).abs() < 1e-6,
            "blocker ended at {}, expected 12.0",
            get(&state, blocker).start_time
        );
    }

    #[test]
    fn moving_an_audio_clip_does_not_ripple_the_video_row() {
        let (mut state, v, a) = linked_video_with_audio(0.0, 4.0);
        let v_neighbour = Uuid::new_v4();
        state.timeline.push(clip(v_neighbour, 10.0, 2.0, 0));

        move_clip(&mut state, a, 5.0, 1);

        assert_eq!(get(&state, a).start_time, 5.0, "A row follows the drag");
        assert_eq!(get(&state, v).start_time, 0.0, "V is not the A's puppet");
        assert_eq!(
            get(&state, v_neighbour).start_time,
            10.0,
            "V-row bystanders must not be pushed by an A-only drag"
        );
    }

    #[test]
    fn moving_video_makes_room_on_the_partner_row_too() {
        let (mut state, v, a) = linked_video_with_audio(0.0, 4.0);
        let a_neighbour = Uuid::new_v4();
        state.timeline.push(clip(a_neighbour, 5.0, 2.0, 1));

        move_clip(&mut state, v, 4.0, 0);

        assert_eq!(get(&state, v).start_time, 4.0);
        assert_eq!(get(&state, a).start_time, 4.0, "partner mirrors the move");
        assert_eq!(
            get(&state, a_neighbour).start_time,
            8.0,
            "the mirrored A span must push its own neighbours clear"
        );
    }

    #[test]
    fn move_cross_row_cascade_never_leaves_the_landing_span_overlapped() {
        // The two make_room_on_row passes deliberately do NOT share their
        // `moved` set: a partner-row push can cascade onto the target row
        // (pushed X carries its partner Y along), and if Y's carried position
        // now covers the landing span, the second pass MUST re-push it.
        // Geometry: V+A move to [5,7). The partner-row pass pushes audio X
        // [4,6) by 3 → [7,9), and the cascade carries X's video partner Y
        // from [1,4) to [4,7) — covering [5,7) on the target row. A shared
        // set would stop there and leave Y overlapping the moved clip; a
        // fresh set re-pushes Y to [7,10) (carrying X to [10,12) to stay
        // locked) so every span stays clear.
        let v = Uuid::new_v4();
        let a = Uuid::new_v4();
        let x = Uuid::new_v4();
        let y = Uuid::new_v4();
        let mut vc = clip(v, 0.0, 2.0, 0);
        vc.linked_clip_id = Some(a);
        let mut ac = clip(a, 0.0, 2.0, 1);
        ac.linked_clip_id = Some(v);
        let mut xc = clip(x, 4.0, 2.0, 1);
        xc.linked_clip_id = Some(y);
        let mut yc = clip(y, 1.0, 3.0, 0);
        yc.linked_clip_id = Some(x);
        let mut state = state_with(vec![vc, ac, xc, yc]);

        move_clip(&mut state, v, 5.0, 0);

        assert_eq!(get(&state, v).start_time, 5.0);
        assert_eq!(get(&state, a).start_time, 5.0, "partner mirrors the move");
        // The cascade-pushed pair must be cleared out of [5,7) on both rows,
        // and the pair stays locked while it happens.
        let y_start = get(&state, y).start_time;
        let x_start = get(&state, x).start_time;
        assert!(
            y_start >= 7.0 - 1e-6,
            "cascade-pushed Y ({y_start}) must not cover the landing span"
        );
        assert!(
            x_start >= 7.0 - 1e-6,
            "cascade-pushed X ({x_start}) must not cover the mirrored A span"
        );
        assert_eq!(
            x_start - y_start,
            3.0,
            "the pushed pair keeps its original 3s offset (locked, not co-located)"
        );
        // Same invariant, stated directly: nothing on the V row overlaps [5,7).
        for c in &state.timeline {
            if c.track_row == 0 && c.id != v {
                assert!(
                    c.start_time >= 7.0 - 1e-6 || c.start_time + c.duration <= 5.0 + 1e-6,
                    "V-row clip [{}, {}) overlaps the landing span",
                    c.start_time,
                    c.start_time + c.duration
                );
            }
        }
    }
}

#[cfg(test)]
mod split_trim_composition_tests {
    //! Split and trim are separate operations owned by different changes, but
    //! users chain them. These pin the seam: a separated A/V pair stays locked
    //! after a cut *and* a subsequent trim.
    use super::*;

    fn clip(id: Uuid, start: f64, dur: f64, row: usize, media_id: Uuid) -> TimelineClip {
        TimelineClip {
            id,
            media_id,
            start_time: start,
            duration: dur,
            track_row: row,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: None,
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: Default::default(),
        }
    }

    #[test]
    fn trimming_a_tail_half_after_a_split_keeps_its_audio_half_locked() {
        let media = Uuid::new_v4();
        let video = Uuid::new_v4();
        let audio = Uuid::new_v4();
        let mut v = clip(video, 0.0, 4.0, 0, media);
        v.audio_muted = true;
        v.linked_clip_id = Some(audio);
        let mut a = clip(audio, 0.0, 4.0, 1, media);
        a.linked_clip_id = Some(video);
        let mut state = ProjectState {
            timeline: vec![v, a],
            ..Default::default()
        };

        let target = split_target_at(&state, 2.0).unwrap();
        split_clip_and_partners(&mut state, &target, 2.0);

        let video_tail = state
            .timeline
            .iter()
            .find(|c| c.id != video && c.track_row == 0)
            .unwrap()
            .id;

        // Now trim the video tail half — the extracted audio tail must follow.
        trim_clip_end(&mut state, video_tail, 0.5);

        let vt = state.timeline.iter().find(|c| c.id == video_tail).unwrap();
        let at_id = vt.linked_clip_id.expect("tail half still paired");
        let at = state.timeline.iter().find(|c| c.id == at_id).unwrap();
        assert!((vt.duration - 0.5).abs() < 1e-9);
        assert!(
            (at.duration - 0.5).abs() < 1e-9,
            "audio tail did not follow the trim"
        );
        assert!((at.start_time - vt.start_time).abs() < 1e-9);
        assert!((at.source_offset - vt.source_offset).abs() < 1e-9);

        // The head halves are untouched by the tail trim.
        let vh = state.timeline.iter().find(|c| c.id == video).unwrap();
        assert!((vh.duration - 2.0).abs() < 1e-9);
        let ah_id = vh.linked_clip_id.expect("head half still paired");
        let ah = state.timeline.iter().find(|c| c.id == ah_id).unwrap();
        assert!((ah.duration - 2.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod separated_audio_tests {
    use super::*;

    fn write_temp_wav(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("velocut_test_{tag}_{}.wav", Uuid::new_v4()));
        std::fs::write(&path, b"RIFF").unwrap();
        path
    }

    #[test]
    fn separated_entry_keeps_source_path_and_requests_extraction() {
        let src = PathBuf::from("C:\\media\\clip.mp4");
        let (entry, needs_extract) = separated_audio_entry(
            Uuid::new_v4(),
            src.clone(),
            "[Audio] clip".into(),
            12.0,
            vec![0.5; 10],
            None,
            3.5,
        );

        assert_eq!(entry.path, src, "entry identity must stay the media file");
        assert_eq!(entry.audio_path, None);
        assert_eq!(
            entry.audio_trimmed_offset, 0.0,
            "a full-file re-extraction starts at 0"
        );
        assert!(needs_extract);
        assert_eq!(entry.clip_type, ClipType::Audio);
        assert!(entry.duration_probed);
    }

    #[test]
    fn separated_entry_reuses_a_live_wav_and_its_offset() {
        let wav = write_temp_wav("shared");
        let src = PathBuf::from("C:\\media\\clip.mp4");
        let (entry, needs_extract) = separated_audio_entry(
            Uuid::new_v4(),
            src.clone(),
            "[Audio] clip".into(),
            12.0,
            Vec::new(),
            Some(wav.clone()),
            2.5,
        );

        assert_eq!(
            entry.path, src,
            "the temp WAV must never become the identity"
        );
        assert_eq!(entry.audio_path, Some(wav.clone()));
        assert_eq!(entry.audio_trimmed_offset, 2.5);
        assert!(!needs_extract);

        std::fs::remove_file(wav).ok();
    }

    fn library_entry(
        id: Uuid,
        path: &str,
        clip_type: ClipType,
        audio_path: Option<PathBuf>,
    ) -> LibraryClip {
        LibraryClip {
            id,
            path: path.into(),
            name: path.to_string(),
            duration: 10.0,
            clip_type,
            thumbnail_path: None,
            duration_probed: true,
            waveform_peaks: Vec::new(),
            video_size: None,
            audio_path,
            audio_trimmed_offset: 0.0,
        }
    }

    fn timeline_clip(
        id: Uuid,
        media_id: Uuid,
        track_row: usize,
        link: Option<Uuid>,
        audio_muted: bool,
    ) -> TimelineClip {
        TimelineClip {
            id,
            media_id,
            start_time: 0.0,
            duration: 10.0,
            track_row,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: link,
            audio_muted,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: velocut_core::filters::FilterParams::none(),
        }
    }

    #[test]
    fn pending_audio_repairs_recovers_a_separated_track_after_save() {
        let video_lib = Uuid::new_v4();
        let audio_lib = Uuid::new_v4();
        let video_clip = Uuid::new_v4();
        let audio_clip = Uuid::new_v4();
        let dead_wav = std::env::temp_dir().join(format!("velocut_gone_{}.wav", Uuid::new_v4()));
        let dead_wav_str = dead_wav.to_string_lossy().into_owned();

        let mut state = ProjectState::default();
        state.library.push(library_entry(
            video_lib,
            "C:\\media\\clip.mp4",
            ClipType::Video,
            None,
        ));
        // What save() leaves behind: audio_path cleared, path still the temp WAV.
        state.library.push(library_entry(
            audio_lib,
            &dead_wav_str,
            ClipType::Audio,
            None,
        ));
        state.timeline.push(timeline_clip(
            video_clip,
            video_lib,
            0,
            Some(audio_clip),
            true,
        ));
        state.timeline.push(timeline_clip(
            audio_clip,
            audio_lib,
            1,
            Some(video_clip),
            false,
        ));

        assert_eq!(
            pending_audio_repairs(&state),
            vec![(audio_lib, PathBuf::from("C:\\media\\clip.mp4"))]
        );
    }

    #[test]
    fn pending_audio_repairs_skips_live_wavs_and_standalone_audio() {
        let wav = write_temp_wav("live");
        let video_lib = Uuid::new_v4();
        let audio_lib = Uuid::new_v4();
        let standalone_lib = Uuid::new_v4();
        let video_clip = Uuid::new_v4();
        let audio_clip = Uuid::new_v4();

        let mut state = ProjectState::default();
        state.library.push(library_entry(
            video_lib,
            "C:\\media\\clip.mp4",
            ClipType::Video,
            None,
        ));
        state.library.push(library_entry(
            audio_lib,
            "C:\\media\\clip.mp4",
            ClipType::Audio,
            Some(wav.clone()),
        ));
        state.library.push(library_entry(
            standalone_lib,
            "C:\\media\\music.mp3",
            ClipType::Audio,
            None,
        ));
        state.timeline.push(timeline_clip(
            video_clip,
            video_lib,
            0,
            Some(audio_clip),
            true,
        ));
        state.timeline.push(timeline_clip(
            audio_clip,
            audio_lib,
            1,
            Some(video_clip),
            false,
        ));

        assert!(pending_audio_repairs(&state).is_empty());
        std::fs::remove_file(wav).ok();
    }

    #[test]
    fn separated_entry_ignores_a_wav_that_was_cleaned_up() {
        let missing = std::env::temp_dir().join(format!("velocut_gone_{}.wav", Uuid::new_v4()));
        let (entry, needs_extract) = separated_audio_entry(
            Uuid::new_v4(),
            PathBuf::from("C:\\media\\clip.mp4"),
            "[Audio] clip".into(),
            12.0,
            Vec::new(),
            Some(missing),
            2.5,
        );

        assert_eq!(entry.audio_path, None);
        assert_eq!(entry.audio_trimmed_offset, 0.0);
        assert!(needs_extract, "a stale WAV must be re-extracted");
    }
}

#[cfg(test)]
mod encode_plan_tests {
    use super::*;

    fn media(id: Uuid) -> LibraryClip {
        LibraryClip {
            id,
            path: PathBuf::from("C:/media/test.mp4"),
            name: "test.mp4".into(),
            duration: 30.0,
            clip_type: ClipType::Video,
            thumbnail_path: None,
            duration_probed: true,
            waveform_peaks: Vec::new(),
            video_size: Some((1920, 1080)),
            audio_path: None,
            audio_trimmed_offset: 0.0,
        }
    }

    fn clip(id: Uuid, start: f64, dur: f64, row: usize, media_id: Uuid) -> TimelineClip {
        TimelineClip {
            id,
            media_id,
            start_time: start,
            duration: dur,
            track_row: row,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: None,
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: velocut_core::filters::FilterParams::none(),
        }
    }

    fn plan(state: &ProjectState) -> (Vec<ClipSpec>, Vec<ClipTransition>, Vec<AudioOverlay>) {
        let mut sorted: Vec<&TimelineClip> = state.timeline.iter().collect();
        sorted.sort_unstable_by(|a, b| a.start_time.total_cmp(&b.start_time));
        build_encode_plan(state, &sorted).expect("plan should resolve")
    }

    fn crossfade_after(id: Uuid) -> TimelineTransition {
        TimelineTransition {
            after_clip_id: id,
            kind: TransitionType {
                kind: TransitionKind::Crossfade,
                duration_secs: 0.5,
            },
        }
    }

    #[test]
    fn plan_keeps_absolute_timeline_positions() {
        let media_id = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let state = ProjectState {
            library: vec![media(media_id)],
            timeline: vec![
                clip(a, 0.0, 2.0, 0, media_id),
                clip(b, 3.0, 1.0, 0, media_id),
            ],
            ..Default::default()
        };

        let (specs, _, _) = plan(&state);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].timeline_start, 0.0);
        assert_eq!(
            specs[1].timeline_start, 3.0,
            "a gap must survive into the encode plan, or every later clip shifts earlier"
        );
    }

    #[test]
    fn transition_across_a_gap_is_not_exported() {
        let media_id = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut state = ProjectState {
            library: vec![media(media_id)],
            timeline: vec![
                clip(a, 0.0, 2.0, 0, media_id),
                clip(b, 3.0, 1.0, 0, media_id),
            ],
            ..Default::default()
        };
        state.transitions.push(crossfade_after(a));

        let (_, transitions, _) = plan(&state);
        assert!(
            transitions.is_empty(),
            "the preview shows a hard cut across a gap; the export must not blend"
        );
    }

    #[test]
    fn transition_between_video_rows_is_not_exported() {
        let media_id = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut state = ProjectState {
            library: vec![media(media_id)],
            timeline: vec![
                clip(a, 0.0, 2.0, 0, media_id),
                clip(b, 2.0, 2.0, 2, media_id),
            ],
            ..Default::default()
        };
        state.transitions.push(crossfade_after(a));

        let (_, transitions, _) = plan(&state);
        assert!(
            transitions.is_empty(),
            "clips on different V lanes never share a preview boundary"
        );
    }

    #[test]
    fn butt_joined_same_row_transition_survives() {
        let media_id = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut state = ProjectState {
            library: vec![media(media_id)],
            timeline: vec![
                clip(a, 0.0, 2.0, 0, media_id),
                clip(b, 2.0, 2.0, 0, media_id),
            ],
            ..Default::default()
        };
        state.transitions.push(crossfade_after(a));

        let (_, transitions, _) = plan(&state);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].after_clip_index, 0);
    }

    #[test]
    fn standalone_audio_overlay_keeps_its_timeline_position() {
        let media_id = Uuid::new_v4();
        let a = Uuid::new_v4();
        let overlay = Uuid::new_v4();
        let state = ProjectState {
            library: vec![media(media_id)],
            timeline: vec![
                clip(a, 0.0, 2.0, 0, media_id),
                clip(overlay, 3.0, 2.0, 1, media_id),
            ],
            ..Default::default()
        };

        let (_, _, overlays) = plan(&state);
        assert_eq!(overlays.len(), 1);
        assert_eq!(
            overlays[0].timeline_start, 3.0,
            "overlays mix at absolute timeline time, and the gap frames keep video aligned"
        );
    }

    /// Build a muted V clip and its extracted A partner, linked both ways.
    fn linked_pair(v: TimelineClip, mut a: TimelineClip) -> (TimelineClip, TimelineClip) {
        let mut v = v;
        v.audio_muted = true;
        v.linked_clip_id = Some(a.id);
        a.linked_clip_id = Some(v.id);
        (v, a)
    }

    #[test]
    fn in_sync_extracted_pair_keeps_encoding_audio_through_the_v_clip() {
        let media_id = Uuid::new_v4();
        let (v, a) = linked_pair(
            clip(Uuid::new_v4(), 0.0, 4.0, 0, media_id),
            clip(Uuid::new_v4(), 0.0, 4.0, 1, media_id),
        );
        let state = ProjectState {
            library: vec![media(media_id)],
            timeline: vec![v, a],
            ..Default::default()
        };

        let (specs, _, overlays) = plan(&state);
        assert_eq!(specs.len(), 1);
        assert!(
            !specs[0].skip_audio,
            "the normal pair must keep the single-ClipSpec audio path"
        );
        assert!(overlays.is_empty());
    }

    #[test]
    fn nudged_extracted_partner_is_exported_as_an_overlay() {
        let media_id = Uuid::new_v4();
        let (v, mut a) = linked_pair(
            clip(Uuid::new_v4(), 0.0, 4.0, 0, media_id),
            clip(Uuid::new_v4(), 0.5, 4.0, 1, media_id),
        );
        a.volume = 0.5;
        a.fade_in_secs = 0.25;
        let state = ProjectState {
            library: vec![media(media_id)],
            timeline: vec![v, a],
            ..Default::default()
        };

        let (specs, _, overlays) = plan(&state);
        assert_eq!(specs.len(), 1);
        assert!(
            specs[0].skip_audio,
            "the V window would play the uncorrected audio; the A overlay owns it instead"
        );
        assert_eq!(overlays.len(), 1);
        assert_eq!(
            overlays[0].timeline_start, 0.5,
            "the export must honour the manual nudge the preview plays"
        );
        assert_eq!(overlays[0].duration, 4.0);
        assert_eq!(overlays[0].volume, 0.5);
        assert_eq!(overlays[0].fade_in_secs, 0.25);
    }

    #[test]
    fn shortened_extracted_partner_keeps_its_own_overlay_window() {
        let media_id = Uuid::new_v4();
        let (v, a) = linked_pair(
            clip(Uuid::new_v4(), 0.0, 4.0, 0, media_id),
            clip(Uuid::new_v4(), 0.0, 2.0, 1, media_id),
        );
        let state = ProjectState {
            library: vec![media(media_id)],
            timeline: vec![v, a],
            ..Default::default()
        };

        let (specs, _, overlays) = plan(&state);
        assert!(specs[0].skip_audio);
        assert_eq!(overlays.len(), 1);
        assert_eq!(
            overlays[0].duration, 2.0,
            "the preview falls silent after the trimmed A ends, and so must the export"
        );
    }

    #[test]
    fn unresolvable_extracted_partner_exports_silence() {
        let media_id = Uuid::new_v4();
        let (v, a) = linked_pair(
            clip(Uuid::new_v4(), 0.0, 4.0, 0, media_id),
            // No library entry for this media id.
            clip(Uuid::new_v4(), 0.0, 4.0, 1, Uuid::new_v4()),
        );
        let state = ProjectState {
            library: vec![media(media_id)],
            timeline: vec![v, a],
            ..Default::default()
        };

        let (specs, _, overlays) = plan(&state);
        assert!(
            specs[0].skip_audio,
            "preview plays nothing when the partner cannot resolve; export must not fall back to V audio"
        );
        assert!(overlays.is_empty());
    }
}

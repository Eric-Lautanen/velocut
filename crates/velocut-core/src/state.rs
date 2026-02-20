// crates/velocut-core/src/state.rs
// Pure project data — no egui, no ffmpeg, no runtime handles.
// Serializable via serde. Used by both velocut-ui and velocut-core consumers.
use std::path::PathBuf;
use uuid::Uuid;
use serde::{Deserialize, Serialize};
use crate::transitions::TimelineTransition;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum AspectRatio {
    SixteenNine,    // 16:9  — YouTube / HD
    NineSixteen,    // 9:16  — TikTok / Reels / Shorts
    TwoThree,       // 2:3   — Portrait photo
    ThreeTwo,       // 3:2   — Landscape photo
    FourThree,      // 4:3   — Classic TV
    OneOne,         // 1:1   — Instagram square
    FourFive,       // 4:5   — Instagram portrait
    TwentyOneNine,  // 21:9  — Ultrawide / cinema
    Anamorphic,     // 2.39:1 — Anamorphic widescreen
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClipType {
    Video,
    Audio,
}

/// Source file in the media bin
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryClip {
    pub id:              Uuid,
    pub path:            PathBuf,
    pub name:            String,
    pub duration:        f64,
    pub clip_type:       ClipType,
    pub thumbnail_path:  Option<PathBuf>,
    pub duration_probed: bool,
    /// Normalised amplitude peaks [0, 1] for waveform display
    #[serde(default)]
    pub waveform_peaks:  Vec<f32>,
    #[serde(default)]
    pub video_size:      Option<(u32, u32)>,
    #[serde(default)]
    pub audio_path:      Option<PathBuf>,
}

/// An instance of a LibraryClip placed on the timeline
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimelineClip {
    pub id:            Uuid,
    pub media_id:      Uuid,
    pub start_time:    f64,
    pub duration:      f64,
    pub track_row:     usize,
    pub source_offset: f64,
    /// Per-clip gain multiplier (0.0–2.0, default 1.0). Applied on top of
    /// the global volume in audio_module. Serialized so project saves retain gains.
    #[serde(default = "default_clip_volume")]
    pub volume:         f32,
    /// ID of the paired clip created by Extract Audio Track. Set on both the
    /// muted video clip and the extracted audio clip so they can be found
    /// relative to each other for display and deletion.
    #[serde(default)]
    pub linked_clip_id: Option<Uuid>,
    /// True when this video clip's audio has been extracted to a separate
    /// audio track. audio_module skips these clips for audio playback.
    #[serde(default)]
    pub audio_muted:    bool,
}

fn default_clip_volume() -> f32 { 1.0 }

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProjectState {
    pub library:                Vec<LibraryClip>,
    pub timeline:               Vec<TimelineClip>,
    pub aspect_ratio:           AspectRatio,
    pub current_time:           f64,
    pub selected_timeline_clip: Option<Uuid>,
    pub selected_library_clip:  Option<Uuid>,
    pub is_playing:             bool,
    pub timeline_zoom:          f32,
    #[serde(default = "default_volume")]
    pub volume:                 f32,
    #[serde(default)]
    pub muted:                  bool,
    /// Transitions between adjacent timeline clips.
    /// Per-boundary transitions stored by clip UUID so they survive reordering.
    /// Keyed by the TimelineClip ID that comes BEFORE the transition.
    #[serde(default)]
    pub transitions: Vec<TimelineTransition>,
    pub pending_probes:         Vec<(Uuid, PathBuf)>,
    /// (clip_id, source_path, timestamp, dest_path)
    #[serde(skip)]
    pub pending_extracts:       Vec<(Uuid, PathBuf, f64, PathBuf)>,
    /// Temp WAV paths queued for deletion (populated when a library clip is removed)
    #[serde(skip)]
    pub pending_audio_cleanup: Vec<std::path::PathBuf>,
    /// Queued frame-save: source_path + timestamp, waiting for file dialog result
    #[serde(skip)]
    pub pending_save_pick:      Option<(PathBuf, f64)>,
    /// Brief status message shown in timeline toolbar after a frame save
    #[serde(skip)]
    pub save_status:            Option<String>,

    // ── Encode status (runtime-only, not serialized) ──────────────────────────
    /// UUID of the currently running encode job, or None when idle.
    /// Set by app.rs::begin_render before calling media_worker.start_encode.
    #[serde(skip)]
    pub encode_job:      Option<Uuid>,
    /// (frames_done, total_frames) — updated each EncodeProgress result.
    /// Used by ExportModule to render the progress bar.
    #[serde(skip)]
    pub encode_progress: Option<(u64, u64)>,
    /// Set to the output PathBuf on EncodeDone. ExportModule shows a ✓ banner.
    #[serde(skip)]
    pub encode_done:     Option<PathBuf>,
    /// Set to the error/cancel message on EncodeError. ExportModule shows a ✕ banner.
    /// The string "cancelled" is the sentinel for a user-initiated cancel.
    #[serde(skip)]
    pub encode_error:    Option<String>,

    // ── Undo / Redo lengths (runtime-only) ───────────────────────────────────
    /// Number of snapshots on the undo stack. Written by app.rs::sync_undo_len()
    /// each time the stacks change. Read by timeline.rs to enable/disable buttons
    /// without changing the EditorModule trait signature.
    #[serde(skip)]
    pub undo_len: usize,
    /// Number of snapshots on the redo stack.
    #[serde(skip)]
    pub redo_len: usize,
}

fn default_volume() -> f32 { 1.0 }

impl Default for ProjectState {
    fn default() -> Self {
        Self {
            library:                Vec::new(),
            timeline:               Vec::new(),
            aspect_ratio:           AspectRatio::SixteenNine,
            current_time:           0.0,
            selected_timeline_clip: None,
            selected_library_clip:  None,
            is_playing:             false,
            timeline_zoom:          50.0,
            volume:                 1.0,
            muted:                  false,
            transitions:            Vec::new(),
            pending_probes:         Vec::new(),
            pending_extracts:       Vec::new(),
            pending_audio_cleanup:  Vec::new(),
            pending_save_pick:      None,
            save_status:            None,
            encode_job:             None,
            encode_progress:        None,
            encode_done:            None,
            encode_error:           None,
            undo_len:               0,
            redo_len:               0,
        }
    }
}

impl ProjectState {
    /// Import a file into the library. Duration = 0 until ffprobe returns.
    pub fn add_to_library(&mut self, path: PathBuf) -> Uuid {
        // Avoid duplicates
        if let Some(existing) = self.library.iter().find(|c| c.path == path) {
            return existing.id;
        }

        let name = path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let ext = path.extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();

        let is_audio = matches!(ext.as_str(), "mp3"|"wav"|"aac"|"flac"|"ogg"|"m4a");
        let id = Uuid::new_v4();

        self.library.push(LibraryClip {
            id,
            path: path.clone(),
            name,
            duration:        0.0,
            clip_type:       if is_audio { ClipType::Audio } else { ClipType::Video },
            thumbnail_path:  None,
            duration_probed: false,
            waveform_peaks:  Vec::new(),
            video_size:      None,
            audio_path:      None,
        });
        self.pending_probes.push((id, path));
        id
    }

    pub fn update_clip_duration(&mut self, id: Uuid, duration: f64) {
        if let Some(clip) = self.library.iter_mut().find(|c| c.id == id) {
            clip.duration        = duration;
            clip.duration_probed = true;
        }
        // Sync any timeline clips that were added before duration was known
        for tc in self.timeline.iter_mut() {
            if tc.media_id == id {
                // Only extend if the timeline clip is still at its placeholder length
                if tc.duration <= 1.0 {
                    tc.duration = duration;
                }
            }
        }
    }

    pub fn update_waveform(&mut self, id: Uuid, peaks: Vec<f32>) {
        if let Some(clip) = self.library.iter_mut().find(|c| c.id == id) {
            clip.waveform_peaks = peaks;
        }
    }

    /// Place a library clip on the timeline.
    /// `preferred_row` is the track row the user targeted (from DnD hover y).
    /// Enforces type safety: video clips land on even rows (V1=0, V2=2),
    /// audio clips land on odd rows (A1=1, A2=3). The preferred row is
    /// corrected to the nearest valid row if the user drops on the wrong type.
    /// Snaps to 0 if dropped within 0.5 s of the start.
    /// Snaps to just after the previous clip on the same track if within 1 s gap.
    pub fn add_to_timeline(&mut self, media_id: Uuid, at_time: f64, preferred_row: usize) {
        let lib_clip = match self.library.iter().find(|c| c.id == media_id) {
            Some(c) => c.clone(),
            None    => return,
        };

        // ── Track enforcement ─────────────────────────────────────────────
        // Video → even rows only (0, 2); Audio → odd rows only (1, 3).
        let row = match lib_clip.clip_type {
            ClipType::Video => {
                let r = if preferred_row % 2 == 0 { preferred_row } else { preferred_row.saturating_sub(1) };
                r.min(2)
            }
            ClipType::Audio => {
                let r = if preferred_row % 2 == 1 { preferred_row } else { preferred_row + 1 };
                r.min(3)
            }
        };

        let duration = lib_clip.duration.max(1.0);

        // ── Snapping ───────────────────────────────────────────────────────
        // 1. Snap to timeline zero
        let mut snapped = at_time;
        if snapped < 0.5 {
            snapped = 0.0;
        }

        // 2. Snap to 0 if track is empty; otherwise snap to end of last clip
        //    on the same track (within a 1-second snap radius)
        let track_end: f64 = self.timeline.iter()
            .filter(|c| c.track_row == row)
            .map(|c| c.start_time + c.duration)
            .fold(f64::NEG_INFINITY, f64::max);

        if !track_end.is_finite() {
            snapped = 0.0; // track is empty — always start at 0
        } else if (snapped - track_end).abs() < 1.0 {
            snapped = track_end; // butt up directly after previous clip
        }

        self.timeline.push(TimelineClip {
            id: Uuid::new_v4(),
            media_id,
            start_time:     snapped,
            duration,
            track_row:      row,
            source_offset:  0.0,
            volume:         1.0,
            linked_clip_id: None,
            audio_muted:    false,
        });
    }

    /// Extracts the audio from a video timeline clip onto the A track directly
    /// below it. Creates a linked audio clip, mutes the original video clip's
    /// audio, and returns the new audio clip's UUID.
    /// Returns None if the clip doesn't exist or isn't a video clip.
    pub fn extract_audio_track(&mut self, clip_id: Uuid) -> Option<Uuid> {
        let clip_idx = self.timeline.iter().position(|c| c.id == clip_id)?;
        let clip = self.timeline[clip_idx].clone();

        // Only makes sense for video clips on a V row.
        let lib = self.library.iter().find(|l| l.id == clip.media_id)?;
        if lib.clip_type != ClipType::Video { return None; }
        if clip.audio_muted { return None; } // already extracted

        // Audio row is always one below the video row: V1(0)→A1(1), V2(2)→A2(3)
        let audio_row = (clip.track_row + 1).min(3);

        let audio_id = Uuid::new_v4();
        let audio_clip = TimelineClip {
            id:             audio_id,
            media_id:       clip.media_id,
            start_time:     clip.start_time,
            duration:       clip.duration,
            track_row:      audio_row,
            source_offset:  clip.source_offset,
            volume:         1.0,
            linked_clip_id: Some(clip_id),
            audio_muted:    false,
        };

        // Mute audio on the video clip and link it to the new audio clip.
        self.timeline[clip_idx].audio_muted    = true;
        self.timeline[clip_idx].linked_clip_id = Some(audio_id);

        self.timeline.push(audio_clip);
        Some(audio_id)
    }

    pub fn delete_selected(&mut self) {
        if let Some(id) = self.selected_timeline_clip.take() {
            self.timeline.retain(|c| c.id != id);
        }
    }

    pub fn delete_selected_library(&mut self) -> Option<std::path::PathBuf> {
        if let Some(id) = self.selected_library_clip.take() {
            if let Some(apath) = self.library.iter()
                .find(|c| c.id == id)
                .and_then(|c| c.audio_path.clone())
            {
                self.pending_audio_cleanup.push(apath.clone());
            }
            self.library.retain(|c| c.id != id);
            self.timeline.retain(|c| c.media_id != id);
        }
        None
    }

    pub fn total_duration(&self) -> f64 {
        self.timeline.iter()
            .map(|c| c.start_time + c.duration)
            .fold(0.0_f64, f64::max)
    }

    pub fn active_video_ratio(&self) -> f32 {
        match self.aspect_ratio {
            AspectRatio::SixteenNine => 16.0 / 9.0,
            AspectRatio::NineSixteen => 9.0 / 16.0,
            AspectRatio::TwoThree    => 2.0 / 3.0,
            AspectRatio::ThreeTwo    => 3.0 / 2.0,
            AspectRatio::FourThree => 4.0 / 3.0,
            AspectRatio::OneOne => 1.0,
            AspectRatio::FourFive => 4.0 / 5.0,
            AspectRatio::TwentyOneNine => 21.0 / 9.0,
            AspectRatio::Anamorphic => 2.39,
        }
    }
}
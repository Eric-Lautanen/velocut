// src/state.rs
use std::path::PathBuf;
use uuid::Uuid;
use serde::{Deserialize, Serialize};

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
}

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
    #[serde(skip)]
    pub pending_probes:         Vec<(Uuid, PathBuf)>,
    /// (clip_id, source_path, timestamp, dest_path)
    #[serde(skip)]
    pub pending_extracts:       Vec<(Uuid, PathBuf, f64, PathBuf)>,
    /// Queued frame-save: source_path + timestamp, waiting for file dialog result
    #[serde(skip)]
    pub pending_save_pick:      Option<(PathBuf, f64)>,
    /// Brief status message shown in timeline toolbar after a frame save
    #[serde(skip)]
    pub save_status:            Option<String>,
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
            pending_probes:         Vec::new(),
            pending_extracts:       Vec::new(),
            pending_save_pick:      None,
            save_status:            None,
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
    /// Snaps to 0 if dropped within 0.5 s of the start.
    /// Snaps to just after the previous clip on the same track if within 1 s gap.
    pub fn add_to_timeline(&mut self, media_id: Uuid, at_time: f64) {
        let lib_clip = match self.library.iter().find(|c| c.id == media_id) {
            Some(c) => c.clone(),
            None    => return,
        };

        let row = if lib_clip.clip_type == ClipType::Audio { 1 } else { 0 };
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
            start_time:    snapped,
            duration,
            track_row:     row,
            source_offset: 0.0,
        });
    }

    pub fn delete_selected(&mut self) {
        if let Some(id) = self.selected_timeline_clip.take() {
            self.timeline.retain(|c| c.id != id);
        }
    }

    pub fn delete_selected_library(&mut self) {
        if let Some(id) = self.selected_library_clip.take() {
            self.library.retain(|c| c.id != id);
            // Also remove any timeline clips referencing this media
            self.timeline.retain(|c| c.media_id != id);
        }
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
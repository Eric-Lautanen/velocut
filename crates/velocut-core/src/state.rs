// crates/velocut-core/src/state.rs
// Pure project data — no egui, no ffmpeg, no runtime handles.
// Serializable via serde. Used by both velocut-ui and velocut-core consumers.
use crate::transitions::TimelineTransition;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum AspectRatio {
    SixteenNine,   // 16:9  — YouTube / HD
    NineSixteen,   // 9:16  — TikTok / Reels / Shorts
    TwoThree,      // 2:3   — Portrait photo
    ThreeTwo,      // 3:2   — Landscape photo
    FourThree,     // 4:3   — Classic TV
    OneOne,        // 1:1   — Instagram square
    FourFive,      // 4:5   — Instagram portrait
    TwentyOneNine, // 21:9  — Ultrawide / cinema
    Anamorphic,    // 2.39:1 — Anamorphic widescreen
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClipType {
    Video,
    Audio,
}

/// Source file in the media bin
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryClip {
    pub id: Uuid,
    pub path: PathBuf,
    pub name: String,
    pub duration: f64,
    pub clip_type: ClipType,
    pub thumbnail_path: Option<PathBuf>,
    pub duration_probed: bool,
    /// Normalised amplitude peaks [0, 1] for waveform display
    #[serde(default)]
    pub waveform_peaks: Vec<f32>,
    #[serde(default)]
    pub video_size: Option<(u32, u32)>,
    #[serde(default)]
    pub audio_path: Option<PathBuf>,
    /// The source-file offset (seconds) at which the current `audio_path` WAV
    /// begins. 0.0 for full-file extractions (probe_clip path); equal to
    /// `source_offset` for trimmed extractions (extract_audio_trimmed path).
    ///
    /// Used by AudioModule to avoid double-counting source_offset when seeking
    /// into the WAV: `seek_t = elapsed + source_offset - audio_trimmed_offset`.
    #[serde(default)]
    pub audio_trimmed_offset: f64,
}

/// An instance of a LibraryClip placed on the timeline
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimelineClip {
    pub id: Uuid,
    pub media_id: Uuid,
    pub start_time: f64,
    pub duration: f64,
    pub track_row: usize,
    pub source_offset: f64,
    /// Per-clip gain multiplier (0.0–2.0, default 1.0). Applied on top of
    /// the global volume in audio_module. Serialized so project saves retain gains.
    #[serde(default = "default_clip_volume")]
    pub volume: f32,
    /// ID of the paired clip created by Extract Audio Track. Set on both the
    /// muted video clip and the extracted audio clip so they can be found
    /// relative to each other for display and deletion.
    #[serde(default)]
    pub linked_clip_id: Option<Uuid>,
    /// True when this video clip's audio has been extracted to a separate
    /// audio track. audio_module skips these clips for audio playback.
    #[serde(default)]
    pub audio_muted: bool,
    /// Duration of audio fade-in ramp (seconds). Ramp begins after `fade_in_start_secs` of silence.
    #[serde(default)]
    pub fade_in_secs: f32,
    /// Seconds of silence before the fade-in ramp starts (0 = ramp starts immediately).
    #[serde(default)]
    pub fade_in_start_secs: f32,
    /// Duration of audio fade-out ramp (seconds). After the ramp, silence continues to clip end.
    #[serde(default)]
    pub fade_out_secs: f32,
    /// Seconds of silence after fade-out ramp, before clip end (0 = ramp ends at clip boundary).
    #[serde(default)]
    pub fade_out_end_secs: f32,
    #[serde(default)]
    pub filter: crate::filters::FilterParams,
}

impl TimelineClip {
    /// True when this clip has a non-identity filter applied.
    pub fn has_filter(&self) -> bool {
        !self.filter.is_identity()
    }
}

fn default_clip_volume() -> f32 {
    1.0
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProjectState {
    /// Serialization format version. Increment when the project format changes
    /// in a way that older versions cannot read. Currently: 1.
    #[serde(default = "default_version")]
    pub version: u32,
    pub library: Vec<LibraryClip>,
    pub timeline: Vec<TimelineClip>,
    pub aspect_ratio: AspectRatio,
    pub current_time: f64,
    pub selected_timeline_clip: Option<Uuid>,
    pub selected_library_clip: Option<Uuid>,
    pub is_playing: bool,
    pub timeline_zoom: f32,
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default)]
    pub muted: bool,
    /// Transitions between adjacent timeline clips.
    /// Per-boundary transitions stored by clip UUID so they survive reordering.
    /// Keyed by the TimelineClip ID that comes BEFORE the transition.
    #[serde(default)]
    pub transitions: Vec<TimelineTransition>,
    pub pending_probes: Vec<(Uuid, PathBuf)>,
    /// (clip_id, source_path, timestamp, dest_path)
    #[serde(skip)]
    pub pending_extracts: Vec<(Uuid, PathBuf, f64, PathBuf)>,
    /// Temp WAV paths queued for deletion (populated when a library clip is removed)
    #[serde(skip)]
    pub pending_audio_cleanup: Vec<std::path::PathBuf>,
    /// Queued frame-save: source_path + timestamp, waiting for file dialog result
    #[serde(skip)]
    pub pending_save_pick: Option<(PathBuf, f64)>,
    /// Brief status message shown in timeline toolbar after a frame save
    #[serde(skip)]
    pub save_status: Option<String>,

    // ── Encode status (runtime-only, not serialized) ──────────────────────────
    /// UUID of the currently running encode job, or None when idle.
    /// Set by app.rs::begin_render before calling media_worker.start_encode.
    #[serde(skip)]
    pub encode_job: Option<Uuid>,
    /// (frames_done, total_frames) — updated each EncodeProgress result.
    /// Used by ExportModule to render the progress bar.
    #[serde(skip)]
    pub encode_progress: Option<(u64, u64)>,
    /// Set to the output PathBuf on EncodeDone. ExportModule shows a ✓ banner.
    #[serde(skip)]
    pub encode_done: Option<PathBuf>,
    /// Set to the error/cancel message on EncodeError. ExportModule shows a ✕ banner.
    /// The string "cancelled" is the sentinel for a user-initiated cancel.
    #[serde(skip)]
    pub encode_error: Option<String>,

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

fn default_version() -> u32 {
    1
}

fn default_volume() -> f32 {
    1.0
}

impl Default for ProjectState {
    fn default() -> Self {
        Self {
            version: 1,
            library: Vec::new(),
            timeline: Vec::new(),
            aspect_ratio: AspectRatio::SixteenNine,
            current_time: 0.0,
            selected_timeline_clip: None,
            selected_library_clip: None,
            is_playing: false,
            timeline_zoom: 50.0,
            volume: 1.0,
            muted: false,
            transitions: Vec::new(),
            pending_probes: Vec::new(),
            pending_extracts: Vec::new(),
            pending_audio_cleanup: Vec::new(),
            pending_save_pick: None,
            save_status: None,
            encode_job: None,
            encode_progress: None,
            encode_done: None,
            encode_error: None,
            undo_len: 0,
            redo_len: 0,
        }
    }
}

impl AspectRatio {
    /// Detect the closest named aspect ratio from a width/height pixel ratio.
    ///
    /// Tolerances are ±0.05 for all ratios except 21:9 (±0.10, wider spread
    /// in practice due to non-standard ultrawide implementations).  Falls back
    /// to `SixteenNine` for unknown landscape and `NineSixteen` for portrait.
    ///
    /// Extracted from `app.rs::process_command(AddToTimeline)` so the logic is
    /// testable and reusable without touching the command handler.
    pub fn from_ratio(r: f32) -> Self {
        if (r - 16.0 / 9.0).abs() < 0.05 {
            Self::SixteenNine
        } else if (r - 9.0 / 16.0).abs() < 0.05 {
            Self::NineSixteen
        } else if (r - 2.0 / 3.0).abs() < 0.05 {
            Self::TwoThree
        } else if (r - 3.0 / 2.0).abs() < 0.05 {
            Self::ThreeTwo
        } else if (r - 4.0 / 3.0).abs() < 0.05 {
            Self::FourThree
        } else if (r - 1.0).abs() < 0.05 {
            Self::OneOne
        } else if (r - 4.0 / 5.0).abs() < 0.05 {
            Self::FourFive
        } else if (r - 21.0 / 9.0).abs() < 0.10 {
            Self::TwentyOneNine
        } else if (r - 2.39).abs() < 0.05 {
            Self::Anamorphic
        } else if r > 1.0 {
            Self::SixteenNine
        } else {
            Self::NineSixteen
        }
    }
}

impl ProjectState {
    /// Import a file into the library. Duration = 0 until ffprobe returns.
    pub fn add_to_library(&mut self, path: PathBuf) -> Uuid {
        // Avoid duplicates — compare canonical paths when possible,
        // fall back to the raw path when canonicalization fails.
        let path_key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if let Some(existing) = self.library.iter().find(|c| {
            std::fs::canonicalize(&c.path)
                .map(|cp| cp == path_key)
                .unwrap_or(false)
                || c.path == path
        }) {
            return existing.id;
        }

        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let ext = path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();

        let is_audio = matches!(ext.as_str(), "mp3" | "wav" | "aac" | "flac" | "ogg" | "m4a");
        let id = Uuid::new_v4();

        self.library.push(LibraryClip {
            id,
            path: path.clone(),
            name,
            duration: 0.0,
            clip_type: if is_audio {
                ClipType::Audio
            } else {
                ClipType::Video
            },
            thumbnail_path: None,
            duration_probed: false,
            waveform_peaks: Vec::new(),
            video_size: None,
            audio_path: None,
            audio_trimmed_offset: 0.0,
        });
        self.pending_probes.push((id, path));
        id
    }

    pub fn update_clip_duration(&mut self, id: Uuid, duration: f64) {
        if let Some(clip) = self.library.iter_mut().find(|c| c.id == id) {
            clip.duration = duration;
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

    /// Set the extracted WAV path and the source offset at which it starts.
    /// `trimmed_offset` is 0.0 for full-file extractions (probe_clip) and
    /// equal to the clip's source_offset for trimmed extractions
    /// (extract_audio_trimmed). AudioModule subtracts this from seek_t so
    /// both code paths land at the correct position in the WAV.
    pub fn set_audio_path(&mut self, id: Uuid, path: PathBuf, trimmed_offset: f64) {
        if let Some(clip) = self.library.iter_mut().find(|c| c.id == id) {
            clip.audio_path = Some(path);
            clip.audio_trimmed_offset = trimmed_offset;
        }
    }

    /// Resolve the source media file behind a separated-audio library entry.
    ///
    /// `extract_audio_track` gives the A-row clip its own library entry so the
    /// bin can show it as audio. That entry is derived data: its real source is
    /// the video file of the V-row clip it is linked to. `audio_path` (the WAV
    /// in the temp dir) is transient and cleared on save, so callers that need
    /// to re-extract audio after a restart must go through this lookup instead
    /// of trusting the entry's own `path`.
    ///
    /// Returns `None` for standalone audio entries, broken links, or clips that
    /// no longer sit on the timeline.
    pub fn audio_library_source(&self, lib_id: Uuid) -> Option<PathBuf> {
        let audio_clip = self
            .timeline
            .iter()
            .find(|c| c.media_id == lib_id && c.track_row % 2 == 1)?;
        let partner_id = audio_clip.linked_clip_id?;
        let video_partner = self
            .timeline
            .iter()
            .find(|c| c.id == partner_id && c.linked_clip_id == Some(audio_clip.id))?;
        self.library
            .iter()
            .find(|l| l.id == video_partner.media_id)
            .map(|l| l.path.clone())
    }

    /// Whether any library entry other than `except` still plays from `path`.
    ///
    /// The V row entry and its separated A row entry reference the same temp
    /// WAV. Deleting one of them must not queue that WAV for cleanup while the
    /// other still depends on it, or the survivor goes silent.
    pub fn audio_path_in_use(&self, path: &Path, except: Uuid) -> bool {
        self.library
            .iter()
            .any(|c| c.id != except && c.audio_path.as_deref() == Some(path))
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
            None => return,
        };

        // ── Track enforcement ─────────────────────────────────────────────
        // Video → even rows only (0, 2); Audio → odd rows only (1, 3).
        let row = match lib_clip.clip_type {
            ClipType::Video => {
                let r = if preferred_row.is_multiple_of(2) {
                    preferred_row
                } else {
                    preferred_row.saturating_sub(1)
                };
                r.min(2)
            }
            ClipType::Audio => {
                let r = if preferred_row % 2 == 1 {
                    preferred_row
                } else {
                    preferred_row + 1
                };
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
        let track_end: f64 = self
            .timeline
            .iter()
            .filter(|c| c.track_row == row)
            .map(|c| c.start_time + c.duration)
            .fold(f64::NEG_INFINITY, f64::max);

        if !track_end.is_finite() {
            snapped = 0.0; // track is empty — always start at 0
        } else if (snapped - track_end).abs() < 1.0 {
            snapped = track_end; // butt up directly after previous clip
        }

        // A drop is a ripple too: make room under the snapped span instead of
        // letting the new clip land on top of an existing one. Ripple (rather
        // than clamping the drop) keeps the clip where the user aimed; the row
        // moves out of its way.
        self.make_room_on_row(row, snapped, duration, &[]);

        self.timeline.push(TimelineClip {
            id: Uuid::new_v4(),
            media_id,
            start_time: snapped,
            duration,
            track_row: row,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: None,
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: crate::filters::FilterParams::none(),
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
        if lib.clip_type != ClipType::Video {
            return None;
        }
        if clip.audio_muted {
            return None;
        } // already extracted

        // Audio row is always one below the video row: V1(0)→A1(1), V2(2)→A2(3)
        let audio_row = (clip.track_row + 1).min(3);

        let audio_id = Uuid::new_v4();
        let audio_clip = TimelineClip {
            id: audio_id,
            media_id: clip.media_id,
            start_time: clip.start_time,
            duration: clip.duration,
            track_row: audio_row,
            source_offset: clip.source_offset,
            // Carry the video clip's gain across: the user set it before
            // extracting, and the A row clip is what actually produces audio
            // afterwards. Resetting to 1.0 made the separated track louder.
            volume: clip.volume,
            linked_clip_id: Some(clip_id),
            audio_muted: false,
            fade_in_secs: clip.fade_in_secs,
            fade_in_start_secs: clip.fade_in_start_secs,
            fade_out_secs: clip.fade_out_secs,
            fade_out_end_secs: clip.fade_out_end_secs,
            filter: crate::filters::FilterParams::none(),
        };

        // Mute audio on the video clip and link it to the new audio clip.
        self.timeline[clip_idx].audio_muted = true;
        self.timeline[clip_idx].linked_clip_id = Some(audio_id);

        self.timeline.push(audio_clip);
        Some(audio_id)
    }

    /// Keep a separated audio track locked to its video clip after a trim or move.
    ///
    /// `from` is the clip whose timeline geometry just changed. Its partner is
    /// updated to the same `start_time`, `duration` and `source_offset`. The V
    /// row clip is the anchor, so calling this with an A row clip is a no-op —
    /// that leaves room to nudge audio manually when correcting drift.
    ///
    /// The update only happens when both clips still point at each other, so a
    /// stale `linked_clip_id` (partner deleted or undone) is ignored rather than
    /// teleporting an unrelated clip.
    pub fn sync_linked_clip(&mut self, from: Uuid) {
        let Some(src) = self.timeline.iter().find(|c| c.id == from) else {
            return;
        };
        let (start_time, duration, source_offset) =
            (src.start_time, src.duration, src.source_offset);
        let Some(partner) = self.linked_partner_mut(from) else {
            return;
        };
        partner.start_time = start_time;
        partner.duration = duration;
        partner.source_offset = source_offset;
    }

    /// The partner of `from`, only while both clips still point at each other.
    ///
    /// The V row clip is the anchor: an A row origin returns `None` so the
    /// separated audio can still be nudged independently. Stale links (partner
    /// deleted or undone) are ignored instead of moving an unrelated clip.
    fn linked_partner_mut(&mut self, from: Uuid) -> Option<&mut TimelineClip> {
        let src = self.timeline.iter().find(|c| c.id == from)?;
        if src.track_row % 2 == 1 {
            return None;
        }
        let partner_id = src.linked_clip_id?;
        self.timeline
            .iter_mut()
            .find(|c| c.id == partner_id && c.linked_clip_id == Some(from))
    }

    /// Mirror the video clip's fade envelope onto its separated audio clip.
    ///
    /// The fade sliders live on the V row clip in the UI, but after
    /// `extract_audio_track` the audio is produced by the A row clip — both the
    /// preview mixer and the encoder read the A row envelope. Without this the
    /// sliders look active yet change nothing audible.
    pub fn sync_linked_fades(&mut self, from: Uuid) {
        let Some(src) = self.timeline.iter().find(|c| c.id == from) else {
            return;
        };
        let (fade_in_secs, fade_in_start_secs, fade_out_secs, fade_out_end_secs) = (
            src.fade_in_secs,
            src.fade_in_start_secs,
            src.fade_out_secs,
            src.fade_out_end_secs,
        );
        let Some(partner) = self.linked_partner_mut(from) else {
            return;
        };
        partner.fade_in_secs = fade_in_secs;
        partner.fade_in_start_secs = fade_in_start_secs;
        partner.fade_out_secs = fade_out_secs;
        partner.fade_out_end_secs = fade_out_end_secs;
    }

    /// Mirror the video clip's gain onto its separated audio clip.
    ///
    /// After `extract_audio_track` the A row clip is what produces audio and
    /// what the encoder reads for clip volume, so dragging the V clip's volume
    /// badge would otherwise be a silent no-op — the same trap the fade
    /// envelope had before `sync_linked_fades`.
    pub fn sync_linked_volume(&mut self, from: Uuid) {
        let Some(volume) = self
            .timeline
            .iter()
            .find(|c| c.id == from)
            .map(|c| c.volume)
        else {
            return;
        };
        if let Some(partner) = self.linked_partner_mut(from) {
            partner.volume = volume;
        }
    }

    /// Shift clips on `row` right just far enough to fit `[start, start + duration)`.
    ///
    /// Used by drop and by the per-frame move ripple. The push is deliberately
    /// collision-gated and minimal: only the first clip that overlaps the new
    /// span — and everything to its right — is shifted, and only by the overlap
    /// it actually has. That makes the operation idempotent, which matters
    /// because a drag re-sends its move every frame: an unconditional "push
    /// everything at/after the anchor by the clip duration" would add the full
    /// duration on each frame and send the row to infinity.
    ///
    /// A clip entirely left of `start` is treated as a legacy overlap: it is
    /// pushed right so it begins exactly at the new span's end, never pulled
    /// left (moving further left later cannot drag it back).
    ///
    /// A shifted clip carries its separated-audio partner with it: the pair is
    /// one unit when the row moves out of someone else's way, even though a
    /// direct drag of the A clip is still a manual nudge (`sync_linked_clip`).
    /// Without this, rippling a row that holds a V clip would leave its A clip
    /// behind and silently desync the export.
    ///
    /// Returns the distance shifted, 0.0 when the span already fits.
    pub fn make_room_on_row(
        &mut self,
        row: usize,
        start: f64,
        duration: f64,
        except: &[Uuid],
    ) -> f64 {
        // Everything this call moves, so a partner reached from two directions
        // moves once and the cascade terminates.
        let mut moved = except.to_vec();
        let mut total = 0.0;
        self.make_room_inner(row, start, duration, &mut moved, &mut total, true);
        total
    }

    fn make_room_inner(
        &mut self,
        row: usize,
        start: f64,
        duration: f64,
        moved: &mut Vec<Uuid>,
        total: &mut f64,
        count_shift: bool,
    ) {
        // Same tolerance the UI uses for butt-joins, so an exact adjacency
        // (`end == neighbour.start`) never reads as an overlap.
        const EPS: f64 = 1.0e-6;
        let end = start + duration;

        let blocker = self
            .timeline
            .iter()
            .filter(|c| c.track_row == row && !moved.contains(&c.id))
            .filter(|c| c.start_time < end - EPS && c.start_time + c.duration > start + EPS)
            .map(|c| c.start_time)
            .fold(f64::INFINITY, f64::min);

        if !blocker.is_finite() {
            return;
        }
        let shift = end - blocker;
        // Every overlapping clip moves by this one shift. It looks like more
        // movement than "just clear the overlap": a clip that overlaps the
        // landing span only from the LEFT (a legacy overlap reaching well past
        // `start`) is pushed all the way to `end`, not merely nudged. That is
        // deliberate — shifting by `overlap` instead would leave the pushed
        // pair still covering part of the span, and clearing it would need a
        // second pass over the same clip.
        if shift <= EPS {
            return;
        }
        if count_shift {
            *total += shift;
        }

        let ids: Vec<Uuid> = self
            .timeline
            .iter()
            .filter(|c| {
                c.track_row == row && !moved.contains(&c.id) && c.start_time >= blocker - EPS
            })
            .map(|c| c.id)
            .collect();

        let mut shifted: Vec<(Uuid, usize, f64, f64)> = Vec::new();
        for id in ids {
            if let Some(c) = self.timeline.iter_mut().find(|c| c.id == id) {
                c.start_time += shift;
                moved.push(id);
                shifted.push((id, c.track_row, c.start_time, c.duration));
            }
        }

        // Carry each shifted clip's partner along, then let the partner's row
        // ripple for its new span. The partner keeps exactly `shift` — only its
        // bystanders move — so the pair's geometry stays locked.
        for (id, _, _, _) in shifted {
            let Some(partner) = self.mutual_partner(id) else {
                continue;
            };
            if moved.contains(&partner) {
                continue;
            }
            let promoted = self.timeline.iter_mut().find(|c| c.id == partner).map(|c| {
                c.start_time += shift;
                (c.track_row, c.start_time, c.duration)
            });
            moved.push(partner);
            if let Some((prow, pstart, pduration)) = promoted {
                self.make_room_inner(prow, pstart, pduration, moved, total, false);
            }
        }
    }

    /// The clip `id` points at, only when that clip points back.
    ///
    /// Unlike [`Self::linked_partner_mut`] this works from either side: ripple
    /// needs to carry the pair regardless of which row it is shifting.
    fn mutual_partner(&self, id: Uuid) -> Option<Uuid> {
        let partner = self.timeline.iter().find(|c| c.id == id)?.linked_clip_id?;
        self.timeline
            .iter()
            .any(|c| c.id == partner && c.linked_clip_id == Some(id))
            .then_some(partner)
    }

    pub fn delete_selected(&mut self) {
        if let Some(id) = self.selected_timeline_clip.take() {
            self.timeline.retain(|c| c.id != id);
        }
    }

    /// Remove a library entry and every timeline clip that references it.
    ///
    /// Returns the entry's temp WAV when no other entry still plays from it,
    /// so the caller can queue it for deletion. A surviving V clip whose
    /// separated A partner was removed is un-linked and un-muted: leaving it
    /// muted with a dangling link is silent video with no way back but undo.
    pub fn remove_library_clip(&mut self, lib_id: Uuid) -> Option<PathBuf> {
        let wav = self
            .library
            .iter()
            .find(|l| l.id == lib_id)
            .and_then(|l| l.audio_path.clone());
        let doomed: Vec<Uuid> = self
            .timeline
            .iter()
            .filter(|c| c.media_id == lib_id)
            .map(|c| c.id)
            .collect();
        for clip in &mut self.timeline {
            if clip.linked_clip_id.is_some_and(|p| doomed.contains(&p)) {
                clip.linked_clip_id = None;
                clip.audio_muted = false;
            }
        }
        self.library.retain(|l| l.id != lib_id);
        self.timeline.retain(|c| c.media_id != lib_id);
        wav.filter(|path| !self.audio_path_in_use(path, lib_id))
    }

    pub fn delete_selected_library(&mut self) -> Option<PathBuf> {
        let id = self.selected_library_clip.take()?;
        let wav = self.remove_library_clip(id);
        if let Some(path) = wav.clone() {
            self.pending_audio_cleanup.push(path);
        }
        wav
    }

    pub fn total_duration(&self) -> f64 {
        self.timeline
            .iter()
            .map(|c| c.start_time + c.duration)
            .fold(0.0_f64, f64::max)
    }

    pub fn active_video_ratio(&self) -> f32 {
        crate::helpers::geometry::aspect_ratio_value(self.aspect_ratio)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_clip(
        state: &mut ProjectState,
        start_time: f64,
        duration: f64,
        track_row: usize,
    ) -> Uuid {
        let id = Uuid::new_v4();
        state.timeline.push(TimelineClip {
            id,
            media_id: Uuid::new_v4(),
            start_time,
            duration,
            track_row,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: None,
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: crate::filters::FilterParams::none(),
        });
        id
    }

    /// Create a linked V/A pair the way extract_audio_track does.
    fn linked_pair(state: &mut ProjectState) -> (Uuid, Uuid) {
        let video = push_clip(state, 0.0, 10.0, 0);
        let audio = push_clip(state, 0.0, 10.0, 1);
        state
            .timeline
            .iter_mut()
            .find(|c| c.id == video)
            .unwrap()
            .linked_clip_id = Some(audio);
        state
            .timeline
            .iter_mut()
            .find(|c| c.id == audio)
            .unwrap()
            .linked_clip_id = Some(video);
        (video, audio)
    }

    fn clip(state: &ProjectState, id: Uuid) -> &TimelineClip {
        state.timeline.iter().find(|c| c.id == id).unwrap()
    }

    fn push_library(state: &mut ProjectState, path: &str, clip_type: ClipType) -> Uuid {
        let id = Uuid::new_v4();
        state.library.push(LibraryClip {
            id,
            path: path.into(),
            name: path.to_string(),
            duration: 10.0,
            clip_type,
            thumbnail_path: None,
            duration_probed: true,
            waveform_peaks: Vec::new(),
            video_size: None,
            audio_path: None,
            audio_trimmed_offset: 0.0,
        });
        id
    }

    #[test]
    fn sync_linked_clip_copies_trimmed_head_geometry() {
        let mut state = ProjectState::default();
        let (video, audio) = linked_pair(&mut state);

        // Simulate trimming 2s off the head: start and source offset move,
        // duration shrinks, tail stays put.
        let v = state.timeline.iter_mut().find(|c| c.id == video).unwrap();
        v.start_time = 2.0;
        v.source_offset = 2.0;
        v.duration = 8.0;
        state.sync_linked_clip(video);

        let a = clip(&state, audio);
        assert_eq!(a.start_time, 2.0);
        assert_eq!(a.source_offset, 2.0);
        assert_eq!(a.duration, 8.0);
        assert_eq!(a.track_row, 1, "partner must stay on its audio row");
    }

    #[test]
    fn sync_linked_clip_copies_move() {
        let mut state = ProjectState::default();
        let (video, audio) = linked_pair(&mut state);

        state
            .timeline
            .iter_mut()
            .find(|c| c.id == video)
            .unwrap()
            .start_time = 12.5;
        state.sync_linked_clip(video);

        assert_eq!(clip(&state, audio).start_time, 12.5);
    }

    #[test]
    fn sync_linked_clip_ignores_audio_origin() {
        let mut state = ProjectState::default();
        let (video, audio) = linked_pair(&mut state);

        // Nudging the A clip is allowed to break the lock — the video anchor
        // must not be dragged along.
        state
            .timeline
            .iter_mut()
            .find(|c| c.id == audio)
            .unwrap()
            .start_time = 1.0;
        state.sync_linked_clip(audio);

        assert_eq!(clip(&state, video).start_time, 0.0);
    }

    #[test]
    fn sync_linked_clip_ignores_stale_link() {
        let mut state = ProjectState::default();
        let (video, _audio) = linked_pair(&mut state);
        let stranger = push_clip(&mut state, 30.0, 4.0, 1);

        // Point the video at a clip that does not point back.
        state
            .timeline
            .iter_mut()
            .find(|c| c.id == video)
            .unwrap()
            .linked_clip_id = Some(stranger);
        state.sync_linked_clip(video);

        assert_eq!(clip(&state, stranger).start_time, 30.0);
        assert_eq!(clip(&state, stranger).duration, 4.0);
    }

    #[test]
    fn sync_linked_fades_mirrors_video_envelope() {
        let mut state = ProjectState::default();
        let (video, audio) = linked_pair(&mut state);

        let v = state.timeline.iter_mut().find(|c| c.id == video).unwrap();
        v.fade_in_secs = 0.5;
        v.fade_in_start_secs = 0.25;
        v.fade_out_secs = 0.75;
        v.fade_out_end_secs = 0.1;
        state.sync_linked_fades(video);

        let a = clip(&state, audio);
        assert_eq!(a.fade_in_secs, 0.5);
        assert_eq!(a.fade_in_start_secs, 0.25);
        assert_eq!(a.fade_out_secs, 0.75);
        assert_eq!(a.fade_out_end_secs, 0.1);
    }

    #[test]
    fn sync_linked_volume_mirrors_video_gain() {
        let mut state = ProjectState::default();
        let (video, audio) = linked_pair(&mut state);

        state
            .timeline
            .iter_mut()
            .find(|c| c.id == video)
            .unwrap()
            .volume = 0.4;
        state.sync_linked_volume(video);

        assert_eq!(clip(&state, audio).volume, 0.4);
        assert!(
            (clip(&state, video).volume - 0.4).abs() < 1e-6,
            "the video clip keeps its own gain too"
        );
    }

    #[test]
    fn sync_linked_volume_ignores_audio_origin() {
        let mut state = ProjectState::default();
        let (video, audio) = linked_pair(&mut state);

        state
            .timeline
            .iter_mut()
            .find(|c| c.id == audio)
            .unwrap()
            .volume = 0.2;
        state.sync_linked_volume(audio);

        assert_eq!(
            clip(&state, video).volume,
            1.0,
            "editing the separated track must not drag the video clip's gain"
        );
    }

    #[test]
    fn sync_linked_fades_leaves_geometry_alone() {
        let mut state = ProjectState::default();
        let (video, audio) = linked_pair(&mut state);

        state
            .timeline
            .iter_mut()
            .find(|c| c.id == video)
            .unwrap()
            .start_time = 5.0;
        state.sync_linked_fades(video);

        assert_eq!(clip(&state, audio).start_time, 0.0);
    }

    #[test]
    fn extract_audio_carries_gain_and_links_both_ways() {
        let mut state = ProjectState::default();
        let media_id = Uuid::new_v4();
        state.library.push(LibraryClip {
            id: media_id,
            path: "C:\\test\\clip.mp4".into(),
            name: "clip".to_string(),
            duration: 10.0,
            clip_type: ClipType::Video,
            thumbnail_path: None,
            duration_probed: true,
            waveform_peaks: Vec::new(),
            video_size: None,
            audio_path: None,
            audio_trimmed_offset: 0.0,
        });
        let video = push_clip(&mut state, 1.0, 6.0, 0);
        state
            .timeline
            .iter_mut()
            .find(|c| c.id == video)
            .unwrap()
            .media_id = media_id;
        state
            .timeline
            .iter_mut()
            .find(|c| c.id == video)
            .unwrap()
            .volume = 0.35;
        state
            .timeline
            .iter_mut()
            .find(|c| c.id == video)
            .unwrap()
            .fade_in_secs = 0.4;

        let audio = state.extract_audio_track(video).unwrap();
        let a = clip(&state, audio);
        assert_eq!(a.volume, 0.35, "gain must survive separation");
        assert_eq!(a.fade_in_secs, 0.4, "fade envelope must survive separation");
        assert_eq!(a.start_time, 1.0);
        assert_eq!(a.duration, 6.0);
        assert_eq!(a.linked_clip_id, Some(video));
        assert_eq!(clip(&state, video).linked_clip_id, Some(audio));
        assert!(clip(&state, video).audio_muted);
    }

    /// Build the V/A library + timeline shape `ExtractAudioTrack` leaves behind
    /// in the app: the A clip points at its own audio library entry, whose real
    /// source is the linked video file.
    fn separated_pair(state: &mut ProjectState) -> (Uuid, Uuid, Uuid, Uuid) {
        let video_lib = push_library(state, "C:\\test\\clip.mp4", ClipType::Video);
        let audio_lib = push_library(state, "C:\\test\\clip.mp4", ClipType::Audio);
        let video_clip = push_clip(state, 0.0, 10.0, 0);
        let audio_clip = push_clip(state, 0.0, 10.0, 1);
        for (clip_id, media_id, link, muted) in [
            (video_clip, video_lib, Some(audio_clip), true),
            (audio_clip, audio_lib, Some(video_clip), false),
        ] {
            let c = state.timeline.iter_mut().find(|c| c.id == clip_id).unwrap();
            c.media_id = media_id;
            c.linked_clip_id = link;
            c.audio_muted = muted;
        }
        (video_lib, audio_lib, video_clip, audio_clip)
    }

    #[test]
    fn audio_library_source_follows_the_link_to_the_video_file() {
        let mut state = ProjectState::default();
        let (video_lib, audio_lib, _video, _audio) = separated_pair(&mut state);
        state.set_audio_path(video_lib, "C:\\temp\\velocut_audio_v.wav".into(), 0.0);

        assert_eq!(
            state.audio_library_source(audio_lib),
            Some(PathBuf::from("C:\\test\\clip.mp4")),
            "separated audio must be re-derivable from the linked video clip"
        );
    }

    #[test]
    fn audio_library_source_ignores_standalone_audio() {
        let mut state = ProjectState::default();
        let lib = push_library(&mut state, "C:\\test\\music.mp3", ClipType::Audio);
        let a1 = push_clip(&mut state, 0.0, 10.0, 1);
        let a2 = push_clip(&mut state, 0.0, 10.0, 3);
        for clip_id in [a1, a2] {
            state
                .timeline
                .iter_mut()
                .find(|c| c.id == clip_id)
                .unwrap()
                .media_id = lib;
        }

        assert_eq!(state.audio_library_source(lib), None);
    }

    #[test]
    fn audio_path_in_use_reports_shared_wavs_only() {
        let mut state = ProjectState::default();
        let first = push_library(&mut state, "C:\\test\\a.mp4", ClipType::Video);
        let second = push_library(&mut state, "C:\\test\\a.mp4", ClipType::Audio);
        let wav = PathBuf::from("C:\\temp\\velocut_audio_shared.wav");
        state.set_audio_path(first, wav.clone(), 0.0);
        state.set_audio_path(second, wav.clone(), 0.0);

        assert!(state.audio_path_in_use(&wav, first));
        assert!(state.audio_path_in_use(&wav, second));

        state.library.retain(|l| l.id != first && l.id != second);
        assert!(!state.audio_path_in_use(&wav, first));
    }

    #[test]
    fn remove_library_clip_unmutes_surviving_video_partner() {
        let mut state = ProjectState::default();
        let (_video_lib, audio_lib, video_clip, audio_clip) = separated_pair(&mut state);

        let wav = state.remove_library_clip(audio_lib);

        assert!(wav.is_none(), "no WAV was attached to the A entry");
        assert!(state.timeline.iter().all(|c| c.id != audio_clip));
        let video = clip(&state, video_clip);
        assert_eq!(video.linked_clip_id, None);
        assert!(
            !video.audio_muted,
            "deleting the separated track must restore V audio"
        );
    }

    #[test]
    fn remove_library_clip_keeps_a_wav_another_entry_still_uses() {
        let mut state = ProjectState::default();
        let (video_lib, audio_lib, _video, _audio) = separated_pair(&mut state);
        let wav = PathBuf::from("C:\\temp\\velocut_audio_shared.wav");
        state.set_audio_path(video_lib, wav.clone(), 0.0);
        state.set_audio_path(audio_lib, wav.clone(), 0.0);

        assert_eq!(state.remove_library_clip(audio_lib), None);
        assert_eq!(
            state
                .library
                .iter()
                .find(|l| l.id == video_lib)
                .unwrap()
                .audio_path,
            Some(wav)
        );
    }

    #[test]
    fn remove_library_clip_returns_unreferenced_wav() {
        let mut state = ProjectState::default();
        let (video_lib, _audio_lib, _video, audio_clip) = separated_pair(&mut state);
        let wav = PathBuf::from("C:\\temp\\velocut_audio_v.wav");
        state.set_audio_path(video_lib, wav.clone(), 0.0);

        assert_eq!(state.remove_library_clip(video_lib), Some(wav));
        assert!(state.library.iter().all(|l| l.id != video_lib));
        // The separated A clip outlives the video entry as standalone audio.
        assert_eq!(state.timeline.len(), 1);
        assert_eq!(state.timeline[0].id, audio_clip);
        assert_eq!(state.timeline[0].linked_clip_id, None);
    }

    // ── make_room_on_row ──────────────────────────────────────────────────────

    #[test]
    fn make_room_leaves_a_fitting_span_alone() {
        let mut state = ProjectState::default();
        let right = push_clip(&mut state, 10.0, 2.0, 0);

        assert_eq!(state.make_room_on_row(0, 0.0, 5.0, &[]), 0.0);
        assert_eq!(clip(&state, right).start_time, 10.0);
    }

    #[test]
    fn make_room_pushes_the_blocker_and_everything_after_it() {
        let mut state = ProjectState::default();
        let blocker = push_clip(&mut state, 2.0, 4.0, 0);
        let later = push_clip(&mut state, 8.0, 2.0, 0);

        // [1, 4) lands on the blocker; push it (and the later clip) by 2s.
        assert_eq!(state.make_room_on_row(0, 1.0, 3.0, &[]), 2.0);
        assert_eq!(clip(&state, blocker).start_time, 4.0);
        assert_eq!(clip(&state, later).start_time, 10.0);
    }

    #[test]
    fn make_room_only_touches_the_target_row() {
        let mut state = ProjectState::default();
        let other_row = push_clip(&mut state, 2.0, 4.0, 1);

        state.make_room_on_row(0, 0.0, 6.0, &[]);
        assert_eq!(clip(&state, other_row).start_time, 2.0);
    }

    #[test]
    fn make_room_excludes_the_moving_clip_and_its_partner() {
        let mut state = ProjectState::default();
        let moving = push_clip(&mut state, 0.0, 4.0, 0);
        let partner = push_clip(&mut state, 0.0, 4.0, 1);

        let shift = state.make_room_on_row(0, 0.0, 4.0, &[moving, partner]);
        assert_eq!(shift, 0.0);
        assert_eq!(clip(&state, moving).start_time, 0.0);
    }

    #[test]
    fn make_room_is_idempotent_across_drag_frames() {
        let mut state = ProjectState::default();
        let moving = push_clip(&mut state, 0.0, 2.0, 0);
        let blocker = push_clip(&mut state, 10.0, 5.0, 0);
        state.timeline.retain(|c| c.id != moving);

        // Simulate 100 frames of a slow 10s drag. The blocker must follow the
        // moving edge, not gain a full clip duration per frame.
        for frame in 1..=100 {
            state.make_room_on_row(0, frame as f64 * 0.1, 2.0, &[]);
        }
        assert!(
            (clip(&state, blocker).start_time - 12.0).abs() < 1e-6,
            "blocker ended at {}, expected the moved span's end (12.0)",
            clip(&state, blocker).start_time
        );
    }

    #[test]
    fn make_room_never_pulls_a_clip_back_left() {
        let mut state = ProjectState::default();
        let blocker = push_clip(&mut state, 2.0, 4.0, 0);

        // Land inside the blocker: it moves to the new span's end.
        assert_eq!(state.make_room_on_row(0, 1.0, 3.0, &[]), 2.0);
        let after_first = clip(&state, blocker).start_time;
        // Dragging further left shrinks the needed room; the clip stays put.
        assert_eq!(state.make_room_on_row(0, 0.5, 3.0, &[]), 0.0);
        assert_eq!(clip(&state, blocker).start_time, after_first);
    }

    #[test]
    fn make_room_carries_a_linked_pair_together() {
        let mut state = ProjectState::default();
        let (video, audio) = linked_pair(&mut state);
        // The pair sits at 10.0..20.0; a span ending at 15.0 lands on it.
        for id in [video, audio] {
            state
                .timeline
                .iter_mut()
                .find(|c| c.id == id)
                .unwrap()
                .start_time = 10.0;
        }

        let shift = state.make_room_on_row(0, 0.0, 15.0, &[]);

        assert_eq!(shift, 5.0);
        assert_eq!(clip(&state, video).start_time, 15.0);
        assert_eq!(
            clip(&state, audio).start_time,
            15.0,
            "the separated A clip must not be left behind by a ripple"
        );
    }

    #[test]
    fn make_room_cascades_through_a_partner_row() {
        let mut state = ProjectState::default();
        // V1/A1 pair at 10.0, plus a standalone A clip butt-joined at 20.0.
        let (video, audio) = linked_pair(&mut state);
        for id in [video, audio] {
            state
                .timeline
                .iter_mut()
                .find(|c| c.id == id)
                .unwrap()
                .start_time = 10.0;
        }
        let bystander = push_clip(&mut state, 20.0, 1.0, 1);

        // Span [0, 15) pushes the pair by 5; the A's new span [15, 25) then
        // pushes the bystander clear instead of overlapping it.
        state.make_room_on_row(0, 0.0, 15.0, &[]);

        assert_eq!(clip(&state, video).start_time, 15.0);
        assert_eq!(clip(&state, audio).start_time, 15.0);
        assert!(
            clip(&state, bystander).start_time >= 25.0 - 1e-6,
            "A-row bystander was not pushed clear of the moved pair"
        );
    }

    #[test]
    fn add_to_timeline_ripples_off_an_occupied_spot() {
        let mut state = ProjectState::default();
        let existing = push_clip(&mut state, 0.0, 2.0, 0);
        // Library duration 2s, so the drop spans [1, 3) and collides.
        let lib = push_library(&mut state, "C:\\test\\drop.mp4", ClipType::Video);
        state
            .library
            .iter_mut()
            .find(|l| l.id == lib)
            .unwrap()
            .duration = 2.0;

        state.add_to_timeline(lib, 1.0, 0);

        let dropped = state.timeline.iter().find(|c| c.media_id == lib).unwrap();
        assert_eq!(dropped.start_time, 1.0, "the drop keeps its snapped spot");
        assert!(
            clip(&state, existing).start_time >= dropped.start_time + dropped.duration - 1e-6,
            "the existing clip must be pushed clear of the drop"
        );
    }
}

// crates/velocut-ui/src/modules/audio_module.rs
//
// AudioModule owns all audio playback logic.
// Non-rendering module — tick() is called every frame from app.rs
// after commands are processed. No egui panel is shown.

use velocut_core::state::ProjectState;
use velocut_core::commands::EditorCommand;
use crate::context::AppContext;
use crate::modules::ThumbnailCache;
use super::EditorModule;
use egui::Ui;
use rodio::Decoder;
use std::fs::File;
use std::io::BufReader;
use std::collections::HashSet;
use uuid::Uuid;

pub struct AudioModule {
    /// Clips whose extracted WAV has played to completion.
    /// Without this, a sink that drains empty (WAV shorter than clip duration)
    /// triggers a full File::open + Decoder + Sink rebuild on every subsequent tick.
    exhausted: HashSet<Uuid>,
}

impl AudioModule {
    pub fn new() -> Self { Self { exhausted: HashSet::new() } }

    /// Called every frame after commands are processed.
    /// Manages rodio sinks: creates on play, clears on stop/seek.
    pub fn tick(&mut self, state: &ProjectState, ctx: &mut AppContext) {
        // audio_stream must stay alive in AppContext for the device thread to run.
        // We only need it here to call .mixer() — no borrow is stored.
        let Some(stream) = &ctx.audio_stream else { return };

        if !state.is_playing {
            // Clear sinks only on the play→stop transition.
            if ctx.playback.audio_was_playing {
                ctx.playback.audio_was_playing = false;
                ctx.audio_sinks.clear();
                self.exhausted.clear();
            }
            return;
        }
        ctx.playback.audio_was_playing = true;

        // Evict sinks for clip IDs that no longer exist in the timeline.
        // This handles undo/redo during active playback: after an undo the clip
        // that owned the sink may be gone, and its rodio thread would keep
        // playing phantom audio indefinitely without this guard.
        let timeline_ids: std::collections::HashSet<uuid::Uuid> =
            state.timeline.iter().map(|c| c.id).collect();
        let stale: Vec<uuid::Uuid> = ctx.audio_sinks.keys()
            .filter(|id| !timeline_ids.contains(id))
            .copied()
            .collect();
        for id in stale {
            ctx.audio_sinks.remove(&id);
            self.exhausted.remove(&id);
        }

        let t = state.current_time;

        // Search priority:
        // 1. Dedicated audio clips on A rows (1, 3) — these are extracted audio tracks.
        // 2. Video clips on V rows (0, 2) whose audio hasn't been extracted yet.
        // This ensures that after ExtractAudioTrack, the A-row clip plays and the
        // muted V-row clip stays silent.
        let active_clip = state.timeline.iter()
            .find(|c| {
                matches!(c.track_row, 1 | 3)
                    && c.start_time <= t
                    && t < c.start_time + c.duration
            })
            .or_else(|| state.timeline.iter().find(|c| {
                matches!(c.track_row, 0 | 2)
                    && !c.audio_muted
                    && c.start_time <= t
                    && t < c.start_time + c.duration
            }));

        if let Some(clip) = active_clip {
            if let Some(lib) = state.library.iter().find(|l| l.id == clip.media_id) {
                // Prefer the pre-extracted WAV (audio_path). Fall back to the
                // original media file so clips that have never had audio extracted
                // (audio_path == None) still produce sound via rodio's symphonia decoder.
                let apath = lib.audio_path.as_ref().unwrap_or(&lib.path);
                let seek_t = (t - clip.start_time + clip.source_offset).max(0.0);

                // If the sink for this clip already played to completion (WAV shorter
                // than clip duration), don't rebuild it on every tick — just stay silent
                // for the remainder of the clip.
                if self.exhausted.contains(&clip.id) {
                    return;
                }

                // Mark an existing sink as exhausted rather than rebuilding it.
                if let Some(sink) = ctx.audio_sinks.get(&clip.id) {
                    if sink.empty() {
                        self.exhausted.insert(clip.id);
                        return;
                    }
                }

                // Rebuild sink if this clip has no active sink yet.
                // Covers both the fresh-start case (empty map) and the
                // clip-change case (map has a different clip's sink, which
                // the clear() below will remove before creating the new one).
                let needs_sink = !ctx.audio_sinks.contains_key(&clip.id);

                if needs_sink {
                    ctx.audio_sinks.clear();
                    self.exhausted.clear(); // new clip, reset exhaustion tracking
                    match File::open(apath) {
                        Ok(file) => {
                            match Decoder::new(BufReader::new(file)) {
                                Ok(decoder) => {
                                    // Per rodio 0.21 docs: connect_new takes &Mixer
                                    // obtained from OutputStream::mixer().
                                    // stream lives in AppContext so the device stays alive.
                                    let sink = rodio::Sink::connect_new(&stream.mixer());
                                    sink.append(decoder);
                                    let _ = sink.try_seek(
                                        std::time::Duration::from_secs_f64(seek_t));
                                    sink.set_volume(
                                        if state.muted { 0.0 } else { state.volume * clip.volume });
                                    sink.play();
                                    eprintln!("[audio] sink created seek_t={seek_t:.3} vol={}", state.volume);
                                    ctx.audio_sinks.insert(clip.id, sink);
                                }
                                Err(e) => eprintln!("[audio] Decoder failed: {e}"),
                            }
                        }
                        Err(e) => eprintln!("[audio] File::open failed: {e}"),
                    }
                } else {
                    // Sync volume/mute without rebuilding the sink.
                    if let Some(sink) = ctx.audio_sinks.get(&clip.id) {
                        sink.set_volume(if state.muted { 0.0 } else { state.volume * clip.volume });
                    }
                }
            }
        } else {
            // No clip under playhead — silence.
            if !ctx.audio_sinks.is_empty() {
                ctx.audio_sinks.clear();
                self.exhausted.clear();
            }
        }
    }
}

impl EditorModule for AudioModule {
    fn name(&self) -> &str { "Audio" }

    fn ui(
        &mut self,
        _ui:          &mut Ui,
        _state:       &ProjectState,
        _thumb_cache: &mut ThumbnailCache,
        _cmd:         &mut Vec<EditorCommand>,
    ) {
        // No UI panel — driven entirely by tick().
    }
}
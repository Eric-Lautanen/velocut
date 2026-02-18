// crates/velocut-ui/src/modules/audio.rs
//
// AudioModule owns all audio playback logic.
// It is a non-rendering module — tick() is called every frame from app.rs
// after commands are processed. No egui panel is shown.
//
// Keeping audio here means fixing audio bugs = editing ONE file.

use velocut_core::state::ProjectState;
use velocut_core::commands::EditorCommand;
use crate::context::AppContext;
use crate::modules::ThumbnailCache;
use super::EditorModule;
use egui::Ui;
use rodio::Decoder;
use std::fs::File;

pub struct AudioModule;

impl AudioModule {
    pub fn new() -> Self { Self }

    /// Called every frame after commands are processed.
    /// Manages rodio sinks: creates them on play, clears on stop/seek.
    pub fn tick(&mut self, state: &ProjectState, ctx: &mut AppContext) {
        let Some(stream) = &ctx.audio_stream else { return };

        if !state.is_playing {
            // Clear sinks only on the play→stop transition.
            if ctx.audio_was_playing {
                ctx.audio_was_playing = false;
                ctx.audio_sinks.clear();
            }
            return;
        }
        ctx.audio_was_playing = true;

        let t = state.current_time;

        // Find the active clip on track row 1 (dedicated audio track).
        // Track 0 = video, track 1 = audio (auto-placed on import).
        // We also check track 0 for video clips that have embedded audio.
        let active_clip = state.timeline.iter().find(|c| {
            (c.track_row == 0 || c.track_row == 1)
                && c.start_time <= t
                && t < c.start_time + c.duration
        });

        if let Some(clip) = active_clip {
            if let Some(lib) = state.library.iter().find(|l| l.id == clip.media_id) {
                if let Some(apath) = &lib.audio_path {
                    let seek_t = (t - clip.start_time + clip.source_offset).max(0.0);

                    // Rebuild sink if: missing, or existing one has finished (empty queue).
                    let needs_sink = !ctx.audio_sinks.contains_key(&clip.id)
                        || ctx.audio_sinks.get(&clip.id)
                            .map(|s| s.empty())
                            .unwrap_or(true);

                    if needs_sink {
                        ctx.audio_sinks.clear();
                        if let Ok(file) = File::open(apath) {
                            if let Ok(decoder) = Decoder::new(file) {
                                let sink = rodio::Sink::connect_new(&stream.mixer());
                                sink.append(decoder);
                                let _ = sink.try_seek(
                                    std::time::Duration::from_secs_f64(seek_t));
                                sink.set_volume(if state.muted { 0.0 } else { state.volume });
                                sink.play();
                                ctx.audio_sinks.insert(clip.id, sink);
                            }
                        }
                    } else {
                        // Sync volume/mute changes without rebuilding the sink.
                        if let Some(sink) = ctx.audio_sinks.get(&clip.id) {
                            sink.set_volume(if state.muted { 0.0 } else { state.volume });
                        }
                    }
                }
            }
        } else {
            // No clip under playhead — silence.
            if !ctx.audio_sinks.is_empty() {
                ctx.audio_sinks.clear();
            }
        }
    }
}

// EditorModule is implemented for trait-object storage in the modules vec.
// AudioModule renders no UI — ui() is a no-op.
impl EditorModule for AudioModule {
    fn name(&self) -> &str { "Audio" }

    fn ui(
        &mut self,
        _ui:         &mut Ui,
        _state:      &ProjectState,
        _thumb_cache: &mut ThumbnailCache,
        _cmd:        &mut Vec<EditorCommand>,
    ) {
        // No UI panel — audio is driven by tick() not the egui pass.
    }
}
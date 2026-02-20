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
use rodio::{Decoder, OutputStreamBuilder};
use std::fs::File;
use std::io::BufReader;
use std::collections::{HashSet, HashMap};
use std::time::Instant;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Diagnostic logging
//
// eprintln! is swallowed in Windows GUI-subsystem mode (no console attached
// when the exe is double-clicked). Write to a temp file instead so we have
// visibility in all launch modes without changing the subsystem.
// File: %TEMP%\velocut_audio.log — append-only, created on first write.
// ---------------------------------------------------------------------------
fn audio_log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("velocut_audio.log"))
    {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{timestamp}] {msg}");
    }
}

/// Minimum elapsed time (seconds) after a sink is created before it may be
/// marked exhausted via the empty() check.
///
/// Rationale: rodio fills its internal decode buffer asynchronously. In release
/// builds the UI tick loop outruns the decode thread on the very first tick, so
/// sink.empty() can return true for one or two frames immediately after creation,
/// then go false once the buffer fills. Without this guard, that transient empty
/// sets sink_has_played on tick N and exhausted on tick N+1, permanently
/// silencing the clip after less than a second. 1.5s is conservative but safe —
/// any legitimate WAV exhaustion happens near end-of-clip, well past this window.
const MIN_PLAY_SECS: f64 = 1.5;

pub struct AudioModule {
    /// Clips whose extracted WAV has played to completion.
    /// Without this, a sink that drains empty (WAV shorter than clip duration)
    /// triggers a full File::open + Decoder + Sink rebuild on every subsequent tick.
    exhausted: HashSet<Uuid>,

    /// Sinks observed non-empty at least once — only these can be marked exhausted.
    sink_has_played: HashSet<Uuid>,

    /// Wall-clock time each sink was created. Guards against false exhaustion:
    /// in release builds, rodio's decode thread may not have buffered any samples
    /// by the time the next tick runs, so sink.empty() can return true transiently
    /// even though the clip has barely started. We refuse to mark a clip exhausted
    /// until at least MIN_PLAY_SECS have elapsed since sink creation.
    sink_created_at: HashMap<Uuid, Instant>,

    /// Ticks remaining after stream creation before sinks are allowed.
    ///
    /// In Windows GUI-subsystem mode (double-click launch), WASAPI registers its
    /// audio session asynchronously after OutputStreamBuilder succeeds. Creating a
    /// Sink on the same tick as the stream causes the first clip to be silently
    /// dropped. Waiting ~83ms (5 ticks at 60fps) gives WASAPI time to complete
    /// session setup. Terminal / cargo-run launches are unaffected — the stream is
    /// created on tick 1, long before the user presses play.
    stream_warmup_ticks: u8,
}

impl AudioModule {
    pub fn new() -> Self {
        Self {
            exhausted: HashSet::new(),
            sink_has_played: HashSet::new(),
            sink_created_at: HashMap::new(),
            stream_warmup_ticks: 0,
        }
    }

    /// Clear all per-sink tracking state. Called on stop and clip change.
    fn clear_sink_state(&mut self) {
        self.exhausted.clear();
        self.sink_has_played.clear();
        self.sink_created_at.clear();
    }

    /// Remove tracking state for a single clip ID. Called on stale-sink eviction.
    fn remove_sink_state(&mut self, id: Uuid) {
        self.exhausted.remove(&id);
        self.sink_has_played.remove(&id);
        self.sink_created_at.remove(&id);
    }

    /// Called every frame after commands are processed.
    /// Manages rodio sinks: creates on play, clears on stop/seek.
    pub fn tick(&mut self, state: &ProjectState, ctx: &mut AppContext) {
        // Lazy init: create the audio stream on the first tick rather than at
        // AppContext::new() time. In Windows GUI-subsystem mode (double-click),
        // WASAPI requires the Win32 message loop to be running first.
        if ctx.audio_stream.is_none() {
            match rodio::OutputStreamBuilder::open_default_stream() {
                Ok(stream) => {
                    audio_log("stream ready — starting warmup");
                    ctx.audio_stream = Some(stream);
                    // Give WASAPI time to complete async session registration
                    // before we try to connect a sink. 5 ticks ≈ 83ms at 60fps.
                    self.stream_warmup_ticks = 5;
                }
                Err(e) => {
                    audio_log(&format!("stream init failed: {e}"));
                }
            }
        }

        // Don't touch sinks until the warmup window has passed.
        if self.stream_warmup_ticks > 0 {
            self.stream_warmup_ticks -= 1;
            return;
        }

        let Some(stream) = &ctx.audio_stream else { return };

        if !state.is_playing {
            // Clear sinks only on the play→stop transition.
            if ctx.playback.audio_was_playing {
                ctx.playback.audio_was_playing = false;
                ctx.audio_sinks.clear();
                self.clear_sink_state();
            }
            return;
        }
        ctx.playback.audio_was_playing = true;

        // Evict sinks for clip IDs that no longer exist in the timeline.
        // This handles undo/redo during active playback: after an undo the clip
        // that owned the sink may be gone, and its rodio thread would keep
        // playing phantom audio indefinitely without this guard.
        let timeline_ids: HashSet<Uuid> =
            state.timeline.iter().map(|c| c.id).collect();
        let stale: Vec<Uuid> = ctx.audio_sinks.keys()
            .filter(|id| !timeline_ids.contains(id))
            .copied()
            .collect();
        for id in stale {
            audio_log(&format!("evicting stale sink for clip {id}"));
            ctx.audio_sinks.remove(&id);
            self.remove_sink_state(id);
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

                // Check whether an existing sink has finished playing.
                // Three conditions must all be true before we mark a clip exhausted:
                //   1. sink.empty()          — decoder has delivered all samples.
                //   2. sink_has_played       — sink was non-empty at least once
                //                              (confirmed it actually started).
                //   3. elapsed >= MIN_PLAY_SECS — guards against transient empty()
                //                              readings in the first few ticks after
                //                              sink creation in release builds.
                if let Some(sink) = ctx.audio_sinks.get(&clip.id) {
                    let elapsed = self.sink_created_at.get(&clip.id)
                        .map(|t| t.elapsed().as_secs_f64())
                        .unwrap_or(0.0);

                    if !sink.empty() {
                        self.sink_has_played.insert(clip.id);
                    } else if self.sink_has_played.contains(&clip.id) {
                        if elapsed >= MIN_PLAY_SECS {
                            audio_log(&format!(
                                "clip {id} exhausted after {elapsed:.2}s",
                                id = clip.id,
                            ));
                            self.exhausted.insert(clip.id);
                            return;
                        }
                        // elapsed < MIN_PLAY_SECS: transient underrun in release
                        // build — do not mark exhausted yet.
                        audio_log(&format!(
                            "clip {id} empty() at {elapsed:.3}s — ignoring (underrun guard)",
                            id = clip.id,
                        ));
                    }
                    // else: newly created sink not yet buffered — leave it alone.
                }

                // Rebuild sink if this clip has no active sink yet.
                // Covers both the fresh-start case (empty map) and the
                // clip-change case (map has a different clip's sink, which
                // the clear() below will remove before creating the new one).
                let needs_sink = !ctx.audio_sinks.contains_key(&clip.id);

                if needs_sink {
                    ctx.audio_sinks.clear();
                    self.clear_sink_state();
                    audio_log(&format!(
                        "opening sink — clip={id} path={path:?} seek_t={seek_t:.3}",
                        id = clip.id,
                        path = apath,
                    ));
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
                                    audio_log(&format!(
                                        "sink created seek_t={seek_t:.3} vol={}",
                                        state.volume,
                                    ));
                                    self.sink_created_at.insert(clip.id, Instant::now());
                                    ctx.audio_sinks.insert(clip.id, sink);
                                }
                                Err(e) => audio_log(&format!("Decoder failed: {e}")),
                            }
                        }
                        Err(e) => audio_log(&format!("File::open failed for {apath:?}: {e}")),
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
                audio_log("no active clip under playhead — clearing sinks");
                ctx.audio_sinks.clear();
                self.clear_sink_state();
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
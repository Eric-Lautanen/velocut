// crates/velocut-ui/src/modules/audio_module.rs
//
// AudioModule owns all audio playback logic.
// Non-rendering module — tick() is called every frame from app.rs
// after commands are processed. No egui panel is shown.

use velocut_core::state::ProjectState;
use velocut_core::commands::EditorCommand;
use crate::context::AppContext;
use crate::helpers::clip_query;
use crate::modules::ThumbnailCache;
use super::EditorModule;
use egui::Ui;
use rodio::{Decoder, Source};
use std::fs::File;
use std::io::BufReader;
use std::collections::{HashSet, HashMap};
use std::time::{Duration, Instant};
use uuid::Uuid;

// Diagnostic logging: routed through the shared log helper so all VeloCut
// output lands in a single %TEMP%\velocut.log regardless of launch mode.
use crate::helpers::log::vlog as audio_log;

/// Minimum elapsed time (seconds) after a sink is created before it may be
/// marked exhausted via the empty() check.
///
/// Rationale: rodio fills its internal decode buffer asynchronously. In release
/// builds the UI tick loop outruns the decode thread on the very first tick, so
/// sink.empty() can return true for one or two frames immediately after creation,
/// then go false once the buffer fills. Without this guard, that transient empty
/// fires exhausted within the first second. This value must comfortably exceed
/// rodio's internal buffer fill time (~200ms) but stay below the shortest clip
/// we'd ever want to silence correctly (~2s).
const MIN_PLAY_SECS: f64 = 1.5;

/// Crossfade duration applied at every clip boundary (seconds).
///
/// - Outgoing clip: volume is linearly ramped 1.0→0.0 over the last FADE_SECS
///   of the clip.  By the time the clip transition fires, the sink is already
///   near silence → no abrupt amplitude step → no click.
/// - Incoming clip: the new decoder is wrapped in `fade_in(FADE_SECS)` so
///   it ramps 0.0→1.0 from its seek position → no DC-offset pop on entry.
///
/// 40 ms ≈ 2 video frames at 24fps — short enough to be inaudible as a dip,
/// long enough to completely mask any zero-crossing discontinuity.
const FADE_SECS: f64 = 0.040;

/// How long (seconds) an evicted sink is held at vol=0 before being dropped.
/// The OS audio buffer (WASAPI/CoreAudio) is typically 10–20 ms deep.
/// Holding the silent sink alive for 50 ms ensures the buffer fully flushes
/// zeros before the rodio mixer thread releases the handle, preventing the
/// pop that occurs when a playing sink is abruptly deallocated.
const FADE_OUT_HOLD_SECS: f64 = 0.050;

pub struct AudioModule {
    /// Clips whose extracted WAV has played to completion.
    /// Without this, a sink that drains empty (WAV shorter than clip duration)
    /// triggers a full File::open + Decoder + Sink rebuild on every subsequent tick.
    exhausted: HashSet<Uuid>,

    /// Sinks observed non-empty at least once — only these can be marked exhausted.
    sink_has_played: HashSet<Uuid>,

    /// Wall-clock time each sink was created. Guards against false exhaustion from
    /// transient empty() readings in the first frames after sink creation.
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

    /// Exhausted-clip tracking for standalone overlay sinks.
    /// Mirrors `exhausted` / `sink_has_played` / `sink_created_at` for the
    /// secondary sink map `AppContext::audio_overlay_sinks`.
    overlay_exhausted:      HashSet<Uuid>,
    overlay_has_played:     HashSet<Uuid>,
    overlay_created_at:     HashMap<Uuid, Instant>,

    /// Duration of each primary clip at the time its sink was created.
    /// If the clip's duration changes (e.g. after split + delete or trim), the
    /// sink — and any exhausted state for that clip — is torn down and rebuilt
    /// so the shortened clip plays correctly and ghost audio cannot continue
    /// past the new clip boundary.
    sink_clip_durations: HashMap<Uuid, f64>,

    /// Sinks being softly faded out before drop.
    ///
    /// Instead of immediately dropping an evicted or clip-change sink (which
    /// hard-clips any in-flight OS audio buffer and causes a pop), we:
    ///   1. Set the sink's volume to 0.0.
    ///   2. Move it here.
    ///   3. Drop it after FADE_OUT_HOLD_SECS so the OS buffer has time to
    ///      flush the now-silent samples before the rodio thread releases it.
    draining_sinks: Vec<(rodio::Sink, Instant)>,

    /// Last volume value passed to set_volume() for each primary sink.
    /// set_volume() is only called when the new value differs by more than
    /// VOLUME_EPSILON — preventing the 60fps stream of instantaneous amplitude
    /// steps that rodio applies as hard sample-level discontinuities, which
    /// manifests as crackling when mix_factor changes or fade_out ramps.
    sink_last_volume: HashMap<Uuid, f32>,

    /// Same deduplication cache for overlay sinks.
    overlay_last_volume: HashMap<Uuid, f32>,
}

impl AudioModule {
    pub fn new() -> Self {
        Self {
            exhausted: HashSet::new(),
            sink_has_played: HashSet::new(),
            sink_created_at: HashMap::new(),
            stream_warmup_ticks: 0,
            overlay_exhausted:  HashSet::new(),
            overlay_has_played: HashSet::new(),
            overlay_created_at: HashMap::new(),
            sink_clip_durations: HashMap::new(),
            draining_sinks: Vec::new(),
            sink_last_volume: HashMap::new(),
            overlay_last_volume: HashMap::new(),
        }
    }

    /// Clear all per-sink tracking state. Called on stop and clip change.
    fn clear_sink_state(&mut self) {
        self.exhausted.clear();
        self.sink_has_played.clear();
        self.sink_created_at.clear();
        self.overlay_exhausted.clear();
        self.overlay_has_played.clear();
        self.overlay_created_at.clear();
        self.sink_clip_durations.clear();
        self.sink_last_volume.clear();
        self.overlay_last_volume.clear();
    }

    /// Remove tracking state for a single clip ID. Called on stale-sink eviction.
    fn remove_sink_state(&mut self, id: Uuid) {
        self.exhausted.remove(&id);
        self.sink_has_played.remove(&id);
        self.sink_created_at.remove(&id);
        self.overlay_exhausted.remove(&id);
        self.overlay_has_played.remove(&id);
        self.overlay_created_at.remove(&id);
        self.sink_clip_durations.remove(&id);
        self.sink_last_volume.remove(&id);
        self.overlay_last_volume.remove(&id);
    }

    /// Move every primary + overlay sink from `ctx` into the drain pool at vol=0.
    ///
    /// Each sink keeps playing silence for FADE_OUT_HOLD_SECS, giving the OS
    /// audio buffer time to flush zeros before the handle is dropped.  This
    /// prevents the pop that occurs when a playing sink is hard-deallocated.
    /// Also clears all per-sink tracking state.
    fn soft_drain_all(&mut self, ctx: &mut AppContext) {
        let now = Instant::now();
        for (_, sink) in ctx.audio_sinks.drain() {
            sink.set_volume(0.0);
            self.draining_sinks.push((sink, now));
        }
        for (_, sink) in ctx.audio_overlay_sinks.drain() {
            sink.set_volume(0.0);
            self.draining_sinks.push((sink, now));
        }
        self.clear_sink_state();
    }

    /// Move a single primary sink into the drain pool at vol=0, by clip ID.
    fn soft_drain_one_primary(&mut self, ctx: &mut AppContext, id: Uuid) {
        if let Some(sink) = ctx.audio_sinks.remove(&id) {
            sink.set_volume(0.0);
            self.draining_sinks.push((sink, Instant::now()));
        }
    }

    /// Call sink.set_volume() only when the value has changed by more than
    /// VOLUME_EPSILON. rodio applies every set_volume() as an instantaneous
    /// hard step at the sample level — calling it 60 times/sec with floating
    /// point micro-variations produces a constant stream of tiny amplitude
    /// discontinuities that the ear hears as crackling, especially when
    /// mix_factor changes or fade_out ramps are active.
    ///
    /// 0.002 ≈ −54 dB change threshold: inaudible as a volume difference but
    /// large enough to absorb all f32 rounding noise at 60 fps.
    const VOLUME_EPSILON: f32 = 0.002;

    fn set_primary_volume(&mut self, id: Uuid, sink: &rodio::Sink, vol: f32) {
        let last = self.sink_last_volume.get(&id).copied().unwrap_or(-1.0);
        if (vol - last).abs() > Self::VOLUME_EPSILON {
            sink.set_volume(vol);
            self.sink_last_volume.insert(id, vol);
        }
    }

    fn set_overlay_volume(&mut self, id: Uuid, sink: &rodio::Sink, vol: f32) {
        let last = self.overlay_last_volume.get(&id).copied().unwrap_or(-1.0);
        if (vol - last).abs() > Self::VOLUME_EPSILON {
            sink.set_volume(vol);
            self.overlay_last_volume.insert(id, vol);
        }
    }

    /// Called every frame after commands are processed.
    /// Manages rodio sinks: creates on play, clears on stop/seek.
    pub fn tick(&mut self, state: &ProjectState, ctx: &mut AppContext) {
        // Drop draining sinks whose hold window has expired.
        // Each entry was silenced at push time; FADE_OUT_HOLD_SECS of zeros in the
        // OS buffer ensures no pop when the sink handle is finally released.
        self.draining_sinks.retain(|(_, t)| t.elapsed().as_secs_f64() < FADE_OUT_HOLD_SECS);

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

        // Clone the mixer handle out of ctx.audio_stream so that `ctx` is no longer
        // immutably borrowed for the rest of tick(). Without this, the compiler rejects
        // the mutable borrows in soft_drain_all / soft_drain_one_primary because they
        // take &mut AppContext while `stream` (a &OutputStream inside ctx) is still live.
        // Arc::clone() is cheap — it only bumps a reference count.
        let Some(mixer) = ctx.audio_stream.as_ref().map(|s| s.mixer().clone()) else { return };

        if !state.is_playing {
            // Soft-drain sinks only on the play→stop transition.
            if ctx.playback.audio_was_playing {
                ctx.playback.audio_was_playing = false;
                self.soft_drain_all(ctx);
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
            self.soft_drain_one_primary(ctx, id);
            self.remove_sink_state(id);
        }

        let t = state.current_time;

        // Search priority:
        // 1. Dedicated audio clips on A rows (1, 3) — these are extracted audio tracks.
        // 2. Video clips on V rows (0, 2) whose audio hasn't been extracted yet.
        // This ensures that after ExtractAudioTrack, the A-row clip plays and the
        // muted V-row clip stays silent.
        // A-row clips (extracted audio) take priority over V-row clips.
        // Logic lives in clip_query::active_audio_clip — single source of truth.
        let active_clip = clip_query::active_audio_clip(state, t);

        // Compute overlay clip list early so we know the total sink count before
        // setting ANY volume. This is the key input for mix normalization below.
        // (Previously this call happened after the primary sink block; moved up
        // so the count is available for mix_factor before primary volume is set.)
        let overlay_clips = clip_query::active_overlay_clips(state, t);

        // ── Mix normalization ─────────────────────────────────────────────────
        // rodio's mixer sums sample values additively. With N simultaneous sinks
        // each at volume=1.0, the combined output reaches ±N, which hard-clips
        // to ±1.0 at the DAC — producing the harsh crackling heard when an audio
        // overlay plays alongside a primary clip.
        //
        // Equal-power mixing (1/√N per sink) is the standard solution:
        //   N=1 → scale=1.000  (no change when playing alone)
        //   N=2 → scale=0.707  (−3 dB each; combined RMS ≈ 1.0 for random phase)
        //   N=3 → scale=0.577  (−4.8 dB each)
        //
        // Users can still adjust the balance via per-clip volume controls; this
        // factor only prevents unintended clipping from additive summation.
        let n_sinks = active_clip.is_some() as usize + overlay_clips.len();
        let mix_factor = if n_sinks > 1 {
            1.0_f32 / (n_sinks as f32).sqrt()
        } else {
            1.0_f32
        };

        if let Some(clip) = active_clip {
            // Use a labeled block so early exits (WAV not ready, exhausted) fall
            // through to overlay processing instead of returning from tick().
            'primary_sink: {
            if let Some(lib) = clip_query::library_entry_for(state, clip) {

                // --- WAV guard ---------------------------------------------------
                // Only play from a pre-extracted WAV, never from the raw source
                // file directly. Raw MP4/AAC decoding via symphonia is unreliable:
                // these containers have non-standard timescales that cause the
                // symphonia decoder to stall partway through, producing a sink that
                // goes empty after ~0.8s even though the clip is 6s long.
                //
                // The WAV is extracted by the media worker probe pipeline and its
                // path lands in lib.audio_path within a second or two of import.
                // If it hasn't arrived yet, skip this tick silently — the next tick
                // will try again and the WAV will be ready soon.
                //
                // This also means we never need to handle the "symphonia gave up"
                // failure mode at all.
                // -----------------------------------------------------------------
                let Some(apath) = lib.audio_path.as_ref() else {
                    // WAV not ready yet — log once per clip (only when a sink would
                    // otherwise have been created) to make the wait visible.
                    if !ctx.audio_sinks.contains_key(&clip.id) {
                        audio_log(&format!(
                            "clip {id} waiting for WAV extraction (audio_path is None)",
                            id = clip.id,
                        ));
                    }
                    break 'primary_sink; // WAV not ready — overlays can still play
                };

                let seek_t = (t - clip.start_time + clip.source_offset).max(0.0);

                // Detect whether the clip's duration changed since the sink was
                // created (e.g. split + delete of the tail segment, or trim).
                // If so, evict the old exhausted / has-played state so the
                // shortened clip gets a fresh sink rather than staying silent.
                let duration_changed = self.sink_clip_durations
                    .get(&clip.id)
                    .map(|&d| (d - clip.duration).abs() > 1e-9)
                    .unwrap_or(false);

                if duration_changed {
                    audio_log(&format!(
                        "clip {id} duration changed → rebuilding sink (was {old:.3}s, now {new:.3}s)",
                        id = clip.id,
                        old = self.sink_clip_durations.get(&clip.id).copied().unwrap_or(0.0),
                        new = clip.duration,
                    ));
                    self.soft_drain_one_primary(ctx, clip.id);
                    self.exhausted.remove(&clip.id);
                    self.sink_has_played.remove(&clip.id);
                    self.sink_created_at.remove(&clip.id);
                    self.sink_clip_durations.remove(&clip.id);
                }

                // If the sink for this clip already played to completion (WAV shorter
                // than clip duration), don't rebuild it on every tick — just stay silent
                // for the remainder of the clip.
                if self.exhausted.contains(&clip.id) {
                    break 'primary_sink; // exhausted — overlays can still play
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
                            break 'primary_sink; // exhausted — overlays can still play
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
                    self.soft_drain_all(ctx);
                    audio_log(&format!(
                        "opening sink — clip={id} wav={path:?} seek_t={seek_t:.3}",
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
                                    let sink = rodio::Sink::connect_new(&mixer);
                                    // fade_in ramps 0→1 over FADE_SECS from the seek position.
                                    // This masks any DC offset at the seek point and prevents
                                    // the click on play-start and clip transitions.
                                    sink.append(decoder.fade_in(Duration::from_secs_f64(FADE_SECS)));
                                    let _ = sink.try_seek(
                                        std::time::Duration::from_secs_f64(seek_t));
                                    let initial_vol = if state.muted { 0.0 } else { state.volume * clip.volume * mix_factor };
                                    sink.set_volume(initial_vol);
                                    self.sink_last_volume.insert(clip.id, initial_vol);
                                    sink.play();
                                    audio_log(&format!(
                                        "sink created seek_t={seek_t:.3} vol={}",
                                        state.volume,
                                    ));
                                    self.sink_created_at.insert(clip.id, Instant::now());
                                    self.sink_clip_durations.insert(clip.id, clip.duration);
                                    ctx.audio_sinks.insert(clip.id, sink);
                                }
                                Err(e) => audio_log(&format!("Decoder failed for WAV {apath:?}: {e}")),
                            }
                        }
                        Err(e) => audio_log(&format!("File::open failed for WAV {apath:?}: {e}")),
                    }
                } else {
                    // Sync volume/mute. Also apply a pre-emptive linear fade-out
                    // over the last FADE_SECS of the clip so the outgoing sink's
                    // amplitude is already near zero when the transition fires.
                    // This eliminates the click that was caused by soft_drain_all
                    // doing an instantaneous hard cut to silence on a non-zero signal.
                    let time_remaining = (clip.start_time + clip.duration) - state.current_time;
                    let fade_out = if time_remaining < FADE_SECS {
                        (time_remaining / FADE_SECS).clamp(0.0, 1.0) as f32
                    } else {
                        1.0_f32
                    };
                    if let Some(sink) = ctx.audio_sinks.get(&clip.id) {
                        let vol = if state.muted { 0.0 } else { state.volume * clip.volume * fade_out * mix_factor };
                        self.set_primary_volume(clip.id, sink, vol);
                    }
                }
            }
            } // end 'primary_sink
        } else {
            // No clip under playhead — silence.
            if !ctx.audio_sinks.is_empty() {
                audio_log("no active clip under playhead — clearing sinks");
                self.soft_drain_all(ctx);
            }
        }

        // ── Standalone audio overlay sinks ────────────────────────────────────
        // Independent A-row clips (odd track_row, no linked_clip_id) play
        // simultaneously with — not instead of — the primary sink above.
        // Each gets its own entry in ctx.audio_overlay_sinks, keyed by clip ID.
        // The logic mirrors the primary sink block: WAV guard, exhaustion check,
        // create-on-miss, volume sync.
        // overlay_clips was computed above (before the primary block) so that
        // mix_factor could be derived from the total active-sink count.

        // Evict overlay sinks for clips no longer active.
        let active_overlay_ids: HashSet<Uuid> =
            overlay_clips.iter().map(|c| c.id).collect();
        let stale_overlays: Vec<Uuid> = ctx.audio_overlay_sinks.keys()
            .filter(|id| !active_overlay_ids.contains(id))
            .copied()
            .collect();
        for id in stale_overlays {
            audio_log(&format!("evicting stale overlay sink for clip {id}"));
            if let Some(sink) = ctx.audio_overlay_sinks.remove(&id) {
                sink.set_volume(0.0);
                self.draining_sinks.push((sink, Instant::now()));
            }
            self.overlay_exhausted.remove(&id);
            self.overlay_has_played.remove(&id);
            self.overlay_created_at.remove(&id);
        }

        for clip in overlay_clips {
            let Some(lib) = clip_query::library_entry_for(state, clip) else { continue };
            let Some(apath) = lib.audio_path.as_ref() else {
                if !ctx.audio_overlay_sinks.contains_key(&clip.id) {
                    audio_log(&format!(
                        "overlay clip {id} waiting for WAV extraction",
                        id = clip.id,
                    ));
                }
                continue;
            };

            // seek_t within the WAV file.
            //
            // Two cases depending on how the WAV was extracted:
            //   • probe_clip path (always used for newly imported clips): extracts the
            //     FULL source file starting at t=0. The WAV's t=0 = source file's t=0,
            //     so seek to (elapsed + source_offset) is correct.
            //   • extract_audio_trimmed path (overlay trim changed): extracts only
            //     [source_offset, source_offset+duration). WAV's t=0 already represents
            //     source_offset in the source, so adding source_offset again double-counts
            //     it. Correct seek is just elapsed time in the clip.
            //
            // lib.audio_trimmed_offset records which offset the current WAV was
            // extracted at (0.0 for probe_clip, actual source_offset for trimmed).
            // Subtracting it normalises both paths to the same formula.
            let wav_start = lib.audio_trimmed_offset;
            let seek_t = (t - clip.start_time + clip.source_offset - wav_start).max(0.0);

            if self.overlay_exhausted.contains(&clip.id) {
                continue;
            }

            if let Some(sink) = ctx.audio_overlay_sinks.get(&clip.id) {
                let elapsed = self.overlay_created_at.get(&clip.id)
                    .map(|ct| ct.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
                if !sink.empty() {
                    self.overlay_has_played.insert(clip.id);
                } else if self.overlay_has_played.contains(&clip.id) {
                    if elapsed >= MIN_PLAY_SECS {
                        audio_log(&format!(
                            "overlay clip {id} exhausted after {elapsed:.2}s",
                            id = clip.id,
                        ));
                        self.overlay_exhausted.insert(clip.id);
                        continue;
                    }
                    audio_log(&format!(
                        "overlay clip {id} empty() at {elapsed:.3}s — ignoring (underrun guard)",
                        id = clip.id,
                    ));
                }
            }

            if !ctx.audio_overlay_sinks.contains_key(&clip.id) {
                audio_log(&format!(
                    "opening overlay sink — clip={id} wav={path:?} seek_t={seek_t:.3}",
                    id = clip.id,
                    path = apath,
                ));
                match File::open(apath) {
                    Ok(file) => {
                        match Decoder::new(BufReader::new(file)) {
                            Ok(decoder) => {
                                let sink = rodio::Sink::connect_new(&mixer);
                                sink.append(decoder.fade_in(Duration::from_secs_f64(FADE_SECS)));
                                let _ = sink.try_seek(
                                    std::time::Duration::from_secs_f64(seek_t));
                                let initial_vol = if state.muted { 0.0 } else { state.volume * clip.volume * mix_factor };
                                sink.set_volume(initial_vol);
                                self.overlay_last_volume.insert(clip.id, initial_vol);
                                sink.play();
                                audio_log(&format!(
                                    "overlay sink created seek_t={seek_t:.3} vol={}",
                                    state.volume,
                                ));
                                self.overlay_created_at.insert(clip.id, Instant::now());
                                ctx.audio_overlay_sinks.insert(clip.id, sink);
                            }
                            Err(e) => audio_log(&format!(
                                "Decoder failed for overlay WAV {apath:?}: {e}")),
                        }
                    }
                    Err(e) => audio_log(&format!(
                        "File::open failed for overlay WAV {apath:?}: {e}")),
                }
            } else if let Some(sink) = ctx.audio_overlay_sinks.get(&clip.id) {
                let time_remaining = (clip.start_time + clip.duration) - state.current_time;
                let fade_out = if time_remaining < FADE_SECS {
                    (time_remaining / FADE_SECS).clamp(0.0, 1.0) as f32
                } else {
                    1.0_f32
                };
                let vol = if state.muted { 0.0 } else { state.volume * clip.volume * fade_out * mix_factor };
                self.set_overlay_volume(clip.id, sink, vol);
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
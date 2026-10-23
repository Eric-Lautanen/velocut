// crates/velocut-ui/src/modules/audio_module.rs
//
// AudioModule owns all audio playback logic.
// Non-rendering module — tick() is called every frame from app.rs
// after commands are processed. No egui panel is shown.

use super::EditorModule;
use crate::context::AppContext;
use crate::helpers::clip_query;
use crate::modules::ThumbnailCache;
use egui::Ui;
use rodio::{Decoder, Source};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;
use velocut_core::commands::EditorCommand;
use velocut_core::state::{ProjectState, TimelineClip};

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

/// How long a clip whose WAV failed to open/decode is left silent before the
/// sink builder retries it.
///
/// Retries inside the window are skipped entirely instead of being re-attempted
/// and re-logged 60 times a second: a WAV that never appears (failed
/// extraction, deleted temp file) then costs one open per window and one log
/// line for the whole episode (see `AudioModule::note_open_failure`). Two
/// seconds keeps a WAV that lands late — an extraction finishing while the
/// playhead is on the clip — from muting the clip for long.
const OPEN_RETRY: Duration = Duration::from_secs(2);

/// One clip's sink-open failure episode (see `AudioModule::note_open_failure`).
///
/// Sinks are rebuilt from scratch on any tick where they are absent, so without
/// this the module would re-open (and re-log) a broken WAV on every frame.
struct OpenFailure {
    /// The WAV whose open/decode failed. A failure against a different path
    /// starts a new episode: a re-extraction can replace the file behind the
    /// clip, and that deserves its own log line.
    path: PathBuf,
    /// Silent retries before this instant are skipped.
    retry_at: Instant,
}

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
    overlay_exhausted: HashSet<Uuid>,
    overlay_has_played: HashSet<Uuid>,
    overlay_created_at: HashMap<Uuid, Instant>,

    /// Content-identity stamp `(source_offset, duration)` of the extraction
    /// window each primary sink was built for. If the stamp stops matching
    /// (split + delete of the tail, a trim, or a SLIDE that keeps the duration
    /// constant — which a duration-only stamp cannot detect), the sink — and
    /// its exhausted state — is torn down and rebuilt so it plays the NEW
    /// window's content instead of a stale one with a matching clip id.
    sink_clip_shapes: HashMap<Uuid, (f64, f64)>,

    /// Same `(source_offset, duration)` stamp for overlay sinks. Overlays
    /// previously had no window-identity check at all: a slide under the
    /// playhead kept the old sink playing the old audio region.
    overlay_clip_shapes: HashMap<Uuid, (f64, f64)>,

    /// Open/decode failure episodes for primary sinks, keyed by timeline clip.
    /// A clip listed here is left silent until its `retry_at`, which keeps a
    /// permanently broken WAV from re-opening and re-logging every tick.
    sink_open_failures: HashMap<Uuid, OpenFailure>,

    /// Same episodes for overlay sinks — an overlay and a primary sink read the
    /// same WAV but are built independently, so one failing must not suppress
    /// the other's retry or its log line.
    overlay_open_failures: HashMap<Uuid, OpenFailure>,

    /// Sinks being softly faded out before drop.
    ///
    /// Instead of immediately dropping an evicted or clip-change sink (which
    /// hard-clips any in-flight OS audio buffer and causes a pop), we:
    ///   1. Set the sink's volume to 0.0.
    ///   2. Move it here.
    ///   3. Drop it after FADE_OUT_HOLD_SECS so the OS buffer has time to
    ///      flush the now-silent samples before the rodio thread releases it.
    draining_sinks: Vec<(rodio::Player, Instant)>,

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
            overlay_exhausted: HashSet::new(),
            overlay_has_played: HashSet::new(),
            overlay_created_at: HashMap::new(),
            sink_clip_shapes: HashMap::new(),
            overlay_clip_shapes: HashMap::new(),
            sink_open_failures: HashMap::new(),
            overlay_open_failures: HashMap::new(),
            draining_sinks: Vec::new(),
            sink_last_volume: HashMap::new(),
            overlay_last_volume: HashMap::new(),
        }
    }

    /// Clear tracking state for the primary (mixer) sink.
    fn clear_primary_state(&mut self) {
        self.exhausted.clear();
        self.sink_has_played.clear();
        self.sink_created_at.clear();
        self.sink_clip_shapes.clear();
        self.sink_last_volume.clear();
        self.sink_open_failures.clear();
    }

    /// Clear tracking state for independent A-row overlay sinks.
    fn clear_overlay_state(&mut self) {
        self.overlay_exhausted.clear();
        self.overlay_has_played.clear();
        self.overlay_created_at.clear();
        self.overlay_last_volume.clear();
        self.overlay_clip_shapes.clear();
        self.overlay_open_failures.clear();
    }

    /// Clear all per-sink tracking state. Called on stop and seek.
    fn clear_sink_state(&mut self) {
        self.clear_primary_state();
        self.clear_overlay_state();
    }

    /// Remove tracking state for a single clip ID. Called on stale-sink eviction.
    fn remove_sink_state(&mut self, id: Uuid) {
        self.exhausted.remove(&id);
        self.sink_has_played.remove(&id);
        self.sink_created_at.remove(&id);
        self.overlay_exhausted.remove(&id);
        self.overlay_has_played.remove(&id);
        self.overlay_created_at.remove(&id);
        self.sink_clip_shapes.remove(&id);
        self.sink_last_volume.remove(&id);
        self.overlay_last_volume.remove(&id);
        self.overlay_clip_shapes.remove(&id);
        // Eviction / stale-clip removal / a WAV rebuild all mean the old
        // failure episode no longer describes what the clip resolves to.
        self.sink_open_failures.remove(&id);
        self.overlay_open_failures.remove(&id);
    }

    /// Seek a freshly built sink to its clip's start offset, logging instead of
    /// discarding a failure.
    ///
    /// A failed `try_seek` is not fatal — the sink still plays, but from the
    /// WAV's offset 0 while carrying the right clip id, i.e. desynced audio
    /// under a correct-looking identity — so it must not abort sink creation.
    /// It must not vanish silently either, which is why the seek call and the
    /// log sink are injected: `what` names the sink ("clip <id>" or "overlay
    /// clip <id>") and `log` receives the formatted message, making this
    /// failure branch reachable in a test without an audio device.
    ///
    /// Returns true when the seek succeeded.
    fn seek_sink<E: std::fmt::Display>(
        try_seek: impl FnOnce(Duration) -> Result<(), E>,
        seek_t: f64,
        what: &str,
        log: &mut dyn FnMut(&str),
    ) -> bool {
        match try_seek(Duration::from_secs_f64(seek_t)) {
            Ok(()) => true,
            Err(e) => {
                log(&format!(
                    "{what} try_seek({seek_t:.3}s) failed: {e} — playing from WAV start"
                ));
                false
            }
        }
    }

    /// True while this clip's last open failure is still inside its retry
    /// window, in which case the tick must not build a sink for it at all.
    fn open_failure_blocked(&self, id: Uuid, overlay: bool, now: Instant) -> bool {
        let failures = if overlay {
            &self.overlay_open_failures
        } else {
            &self.sink_open_failures
        };
        failures.get(&id).is_some_and(|f| now < f.retry_at)
    }

    /// Record a failed sink open/decode, logging only when the failure starts a
    /// new episode.
    ///
    /// An absent sink is rebuilt on every tick, so an unopenable WAV would
    /// otherwise log — and construct a decoder attempt — 60 times a second.
    /// The episode logs once at onset and then retries silently every
    /// `OPEN_RETRY`; a failure against a different WAV path logs again because
    /// a re-extraction may have replaced the file. Success, a WAV rebuild, or
    /// stop/seek clears the episode (`clear_open_failure`), so the next failure
    /// is reported again.
    fn note_open_failure(
        &mut self,
        id: Uuid,
        path: &Path,
        overlay: bool,
        now: Instant,
        message: &str,
        log: &mut dyn FnMut(&str),
    ) {
        let failures = if overlay {
            &mut self.overlay_open_failures
        } else {
            &mut self.sink_open_failures
        };
        let new_episode = failures.get(&id).is_none_or(|f| f.path != path);
        if new_episode {
            log(message);
        }
        failures.insert(
            id,
            OpenFailure {
                path: path.to_path_buf(),
                retry_at: now + OPEN_RETRY,
            },
        );
    }

    /// Drop the clip's open-failure episode: the next failure logs again and the
    /// next tick may retry immediately.
    fn clear_open_failure(&mut self, id: Uuid, overlay: bool) {
        if overlay {
            self.overlay_open_failures.remove(&id);
        } else {
            self.sink_open_failures.remove(&id);
        }
    }

    /// Content-identity stamp of the extraction window a sink was built for.
    ///
    /// (source_offset, duration) — NOT duration alone (see F4 of the
    /// audio-path audit): a sliding trim that keeps the clip's duration
    /// constant still moves the window, and a duration-only stamp let the old
    /// sink keep playing the OLD audio region while the video showed the NEW
    /// one — wrong content under a matching clip id, the same identity-class
    /// hole the video path closed with the generation gate.
    fn clip_shape(clip: &TimelineClip) -> (f64, f64) {
        (clip.source_offset, clip.duration)
    }

    /// Evict the clip's sink session when its stamped `(source_offset,
    /// duration)` no longer matches the clip's current window. `overlay=false`
    /// handles primary (mixer) sinks, `overlay=true` overlay sinks and their
    /// tracking. Returns true when an eviction happened. Tolerances follow the
    /// old duration check (1e-9). Callers stamp the new shape right after a
    /// fresh sink is created.
    fn evict_shape_stamped_sink(
        &mut self,
        ctx: &mut AppContext,
        clip: &TimelineClip,
        overlay: bool,
    ) -> bool {
        let stamp = if overlay {
            self.overlay_clip_shapes.get(&clip.id).copied()
        } else {
            self.sink_clip_shapes.get(&clip.id).copied()
        };
        let Some((s, d)) = stamp else {
            return false;
        };
        if (s - clip.source_offset).abs() <= 1e-9 && (d - clip.duration).abs() <= 1e-9 {
            return false;
        }
        audio_log(&format!(
            "clip {id} extraction window changed → rebuilding {which} sink (was src={s:.3}/{d:.3}s, now src={so:.3}/{du:.3}s)",
            id = clip.id,
            which = if overlay { "overlay" } else { "primary" },
            so = clip.source_offset,
            du = clip.duration,
        ));
        if overlay {
            if let Some(sink) = ctx.audio_overlay_sinks.remove(&clip.id) {
                sink.set_volume(0.0);
                self.draining_sinks.push((sink, Instant::now()));
            }
            self.overlay_exhausted.remove(&clip.id);
            self.overlay_has_played.remove(&clip.id);
            self.overlay_created_at.remove(&clip.id);
            self.overlay_last_volume.remove(&clip.id);
            self.overlay_clip_shapes.remove(&clip.id);
        } else {
            self.soft_drain_one_primary(ctx, clip.id);
            self.exhausted.remove(&clip.id);
            self.sink_has_played.remove(&clip.id);
            self.sink_created_at.remove(&clip.id);
            self.sink_last_volume.remove(&clip.id);
            self.sink_clip_shapes.remove(&clip.id);
        }
        true
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

    /// Drain the primary sinks and reset primary-only tracking state.
    ///
    /// Used when the primary clip changes. Independent A-row overlay clips are
    /// not affected by that cut: draining them (as `soft_drain_all` does) would
    /// hard-mute a playing overlay to 0 mid-signal, and recreating it a tick
    /// later leaves a gap. They now keep playing across the cut.
    fn soft_drain_primary_only(&mut self, ctx: &mut AppContext) {
        let now = Instant::now();
        for (_, sink) in ctx.audio_sinks.drain() {
            sink.set_volume(0.0);
            self.draining_sinks.push((sink, now));
        }
        self.clear_primary_state();
    }

    /// Move a single primary sink into the drain pool at vol=0, by clip ID.
    fn soft_drain_one_primary(&mut self, ctx: &mut AppContext, id: Uuid) {
        if let Some(sink) = ctx.audio_sinks.remove(&id) {
            sink.set_volume(0.0);
            self.draining_sinks.push((sink, Instant::now()));
        }
    }

    /// Silence every live sink when the playhead jumps.
    ///
    /// Sinks hold audio decoded at the old position, so leaving them running
    /// after a seek plays the wrong part of the clip against the new video
    /// position. They are routed through the zero-volume drain pool rather than
    /// dropped so the jump doesn't click.
    pub fn drain_for_seek(&mut self, ctx: &mut AppContext) {
        self.soft_drain_all(ctx);
        ctx.playback.audio_was_playing = false;
    }

    /// Invalidate the sink sessions of every timeline clip whose WAV was
    /// re-extracted (consumes `ctx.pending_audio_wav_rebuild`).
    ///
    /// A re-extraction rewrites the WAV temp file on disk — in place, under any
    /// open sink decoder. The old sink is therefore a session of the OLD file
    /// content: its decoder reads truncated/re-written bytes from here on, and
    /// its `exhausted` / `sink_has_played` bookkeeping refers to content that
    /// no longer exists. Draining (not dropping) keeps the boundary click-free;
    /// dropping from the sink map forces the next tick to rebuild at the new
    /// `wav_seek_t`, and clearing the tracking lets a newly shorter/longer WAV
    /// be re-judged fresh instead of staying silent as "exhausted".
    ///
    /// Keyed by library id: a re-extraction request targets a `LibraryClip`,
    /// while sinks are keyed by timeline clip id, so the clip ids are resolved
    /// through `TimelineClip::media_id`.
    fn process_wav_rebuild_requests(&mut self, state: &ProjectState, ctx: &mut AppContext) {
        if ctx.pending_audio_wav_rebuild.is_empty() {
            return;
        }
        let lib_ids = std::mem::take(&mut ctx.pending_audio_wav_rebuild);
        let clip_ids: Vec<Uuid> = state
            .timeline
            .iter()
            .filter(|c| lib_ids.contains(&c.media_id))
            .map(|c| c.id)
            .collect();
        audio_log(&format!(
            "wav re-extracted for lib entries {}: invalidating {} sink session(s)",
            lib_ids
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<_>>()
                .join(","),
            clip_ids.len()
        ));
        for clip_id in clip_ids {
            self.soft_drain_one_primary(ctx, clip_id);
            if let Some(sink) = ctx.audio_overlay_sinks.remove(&clip_id) {
                sink.set_volume(0.0);
                self.draining_sinks.push((sink, Instant::now()));
            }
            self.remove_sink_state(clip_id);
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

    fn set_primary_volume(&mut self, id: Uuid, sink: &rodio::Player, vol: f32) {
        let last = self.sink_last_volume.get(&id).copied().unwrap_or(-1.0);
        if (vol - last).abs() > Self::VOLUME_EPSILON {
            sink.set_volume(vol);
            self.sink_last_volume.insert(id, vol);
        }
    }

    fn set_overlay_volume(&mut self, id: Uuid, sink: &rodio::Player, vol: f32) {
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
        self.draining_sinks
            .retain(|(_, t)| t.elapsed().as_secs_f64() < FADE_OUT_HOLD_SECS);

        // WAV re-extraction boundary: drain sinks of re-extracted entries first
        // so they never keep playing the OLD extraction's bytes after their
        // file was rewritten under them.
        self.process_wav_rebuild_requests(state, ctx);

        // Lazy init: create the audio stream on the first tick rather than at
        // AppContext::new() time. In Windows GUI-subsystem mode (double-click),
        // WASAPI requires the Win32 message loop to be running first.
        if ctx.audio_stream.is_none() {
            match rodio::DeviceSinkBuilder::open_default_sink() {
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
        let Some(mixer) = ctx.audio_stream.as_ref().map(|s| s.mixer().clone()) else {
            return;
        };

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
        let timeline_ids: HashSet<Uuid> = state.timeline.iter().map(|c| c.id).collect();
        let stale: Vec<Uuid> = ctx
            .audio_sinks
            .keys()
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

                    let seek_t = clip_query::wav_seek_t(clip, lib, t);

                    // Content-identity check (F4): evict the sink session when
                    // the clip's extraction window changed since it was built —
                    // duration changes, trims, AND slides that keep duration
                    // constant. The old duration-only check missed the slide.
                    self.evict_shape_stamped_sink(ctx, clip, false);

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
                        let elapsed = self
                            .sink_created_at
                            .get(&clip.id)
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
                    // Build into a local first: the sink construction needs
                    // `&mut ctx` (soft_drain_primary_only), which an `entry` slot
                    // would borrow for the whole block. Insertion happens once,
                    // after the fallible open/decode.
                    let mut new_sink = None;

                    // A WAV that just failed to open is left silent until its
                    // retry window expires — reopening it every tick would
                    // re-log (and re-attempt a decoder) 60 times a second.
                    if needs_sink && !self.open_failure_blocked(clip.id, false, Instant::now()) {
                        // Only the primary row changes here; overlay sinks must
                        // keep playing across the cut.
                        self.soft_drain_primary_only(ctx);
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
                                        let sink = rodio::Player::connect_new(&mixer);
                                        sink.append(
                                            decoder.fade_in(Duration::from_secs_f64(FADE_SECS)),
                                        );
                                        // Not fatal, but a failed seek silently
                                        // plays the WAV from offset 0 under the
                                        // right clip id — `seek_sink` logs it.
                                        Self::seek_sink(
                                            |pos| sink.try_seek(pos),
                                            seek_t,
                                            &format!("clip {id}", id = clip.id),
                                            &mut |msg| audio_log(msg),
                                        );
                                        let initial_vol = if state.muted {
                                            0.0
                                        } else {
                                            state.volume * clip.volume * mix_factor
                                        };
                                        sink.set_volume(initial_vol);
                                        self.sink_last_volume.insert(clip.id, initial_vol);
                                        sink.play();
                                        audio_log(&format!(
                                            "sink created seek_t={seek_t:.3} vol={}",
                                            state.volume,
                                        ));
                                        self.sink_created_at.insert(clip.id, Instant::now());
                                        self.sink_clip_shapes
                                            .insert(clip.id, Self::clip_shape(clip));
                                        self.clear_open_failure(clip.id, false);
                                        new_sink = Some(sink);
                                    }
                                    Err(e) => self.note_open_failure(
                                        clip.id,
                                        apath,
                                        false,
                                        Instant::now(),
                                        &format!(
                                            "clip {id} decoder failed for WAV {apath:?} (seek_t={seek_t:.3}s): {e}",
                                            id = clip.id,
                                        ),
                                        &mut |msg| audio_log(msg),
                                    ),
                                }
                            }
                            // Attribution matters here: several clips can read the
                            // same per-library WAV, and a clip with no sink is
                            // simply silent — the clip id is the only handle that
                            // ties the failure to the audio that went missing.
                            Err(e) => self.note_open_failure(
                                clip.id,
                                apath,
                                false,
                                Instant::now(),
                                &format!(
                                    "clip {id} File::open failed for WAV {apath:?}: {e}",
                                    id = clip.id,
                                ),
                                &mut |msg| audio_log(msg),
                            ),
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
                        // Per-clip fade-in / fade-out envelope (set by the user via the
                        // volume popup sliders). Applied on top of the transition fade_out
                        // guard above — they multiply together so neither is cancelled out.
                        let elapsed = state.current_time - clip.start_time;
                        let clip_fade = {
                            let e = elapsed as f32;
                            let r = time_remaining as f32;
                            let in_gain = if e < clip.fade_in_start_secs {
                                0.0
                            } else if clip.fade_in_secs > 0.0 {
                                ((e - clip.fade_in_start_secs) / clip.fade_in_secs)
                                    .clamp(0.0, 1.0)
                                    .sqrt()
                            } else {
                                1.0
                            };
                            let out_gain = if r < clip.fade_out_end_secs {
                                0.0
                            } else if clip.fade_out_secs > 0.0 {
                                ((r - clip.fade_out_end_secs) / clip.fade_out_secs)
                                    .clamp(0.0, 1.0)
                                    .sqrt()
                            } else {
                                1.0
                            };
                            in_gain.min(out_gain)
                        };
                        if let Some(sink) = ctx.audio_sinks.get(&clip.id) {
                            let vol = if state.muted {
                                0.0
                            } else {
                                state.volume * clip.volume * fade_out * clip_fade * mix_factor
                            };
                            self.set_primary_volume(clip.id, sink, vol);
                        }
                    }
                    if let Some(sink) = new_sink {
                        ctx.audio_sinks.insert(clip.id, sink);
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
        let active_overlay_ids: HashSet<Uuid> = overlay_clips.iter().map(|c| c.id).collect();
        let stale_overlays: Vec<Uuid> = ctx
            .audio_overlay_sinks
            .keys()
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
            let Some(lib) = clip_query::library_entry_for(state, clip) else {
                continue;
            };
            let Some(apath) = lib.audio_path.as_ref() else {
                if !ctx.audio_overlay_sinks.contains_key(&clip.id) {
                    audio_log(&format!(
                        "overlay clip {id} waiting for WAV extraction",
                        id = clip.id,
                    ));
                }
                continue;
            };

            // seek_t within the WAV file. A WAV extracted from a trimmed range
            // starts at the clip's source_offset, so the shared helper subtracts
            // lib.audio_trimmed_offset — adding source_offset again would
            // double-count it. See clip_query::wav_seek_t for the full rule.
            let seek_t = clip_query::wav_seek_t(clip, lib, t);

            // Content-identity check (F4), same as for primary sinks — this
            // check did not exist for overlays before: a sliding trim under the
            // playhead used to keep the old sink playing the old window.
            self.evict_shape_stamped_sink(ctx, clip, true);

            if self.overlay_exhausted.contains(&clip.id) {
                continue;
            }

            if let Some(sink) = ctx.audio_overlay_sinks.get(&clip.id) {
                let elapsed = self
                    .overlay_created_at
                    .get(&clip.id)
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

            let needs_overlay = !ctx.audio_overlay_sinks.contains_key(&clip.id);
            // Built locally and inserted once, for the same borrow reason as the
            // primary sink above.
            let mut new_overlay = None;
            // Same retry-window rule as the primary sink.
            if needs_overlay && !self.open_failure_blocked(clip.id, true, Instant::now()) {
                audio_log(&format!(
                    "opening overlay sink — clip={id} wav={path:?} seek_t={seek_t:.3}",
                    id = clip.id,
                    path = apath,
                ));
                match File::open(apath) {
                    Ok(file) => match Decoder::new(BufReader::new(file)) {
                        Ok(decoder) => {
                            let sink = rodio::Player::connect_new(&mixer);
                            sink.append(decoder.fade_in(Duration::from_secs_f64(FADE_SECS)));
                            // Same log-don't-drop rule as the primary sink.
                            Self::seek_sink(
                                |pos| sink.try_seek(pos),
                                seek_t,
                                &format!("overlay clip {id}", id = clip.id),
                                &mut |msg| audio_log(msg),
                            );
                            let initial_vol = if state.muted {
                                0.0
                            } else {
                                state.volume * clip.volume * mix_factor
                            };
                            sink.set_volume(initial_vol);
                            self.overlay_last_volume.insert(clip.id, initial_vol);
                            sink.play();
                            audio_log(&format!(
                                "overlay sink created seek_t={seek_t:.3} vol={}",
                                state.volume,
                            ));
                            self.overlay_created_at.insert(clip.id, Instant::now());
                            self.overlay_clip_shapes
                                .insert(clip.id, Self::clip_shape(clip));
                            self.clear_open_failure(clip.id, true);
                            new_overlay = Some(sink);
                        }
                        Err(e) => self.note_open_failure(
                            clip.id,
                            apath,
                            true,
                            Instant::now(),
                            &format!(
                                "overlay clip {id} decoder failed for WAV {apath:?} (seek_t={seek_t:.3}s): {e}",
                                id = clip.id,
                            ),
                            &mut |msg| audio_log(msg),
                        ),
                    },
                    Err(e) => self.note_open_failure(
                        clip.id,
                        apath,
                        true,
                        Instant::now(),
                        &format!(
                            "overlay clip {id} File::open failed for WAV {apath:?}: {e}",
                            id = clip.id,
                        ),
                        &mut |msg| audio_log(msg),
                    ),
                }
            } else if let Some(sink) = ctx.audio_overlay_sinks.get(&clip.id) {
                let time_remaining = (clip.start_time + clip.duration) - state.current_time;
                let fade_out = if time_remaining < FADE_SECS {
                    (time_remaining / FADE_SECS).clamp(0.0, 1.0) as f32
                } else {
                    1.0_f32
                };
                let elapsed = state.current_time - clip.start_time;
                let clip_fade = {
                    let e = elapsed as f32;
                    let r = time_remaining as f32;
                    let in_gain = if e < clip.fade_in_start_secs {
                        0.0
                    } else if clip.fade_in_secs > 0.0 {
                        ((e - clip.fade_in_start_secs) / clip.fade_in_secs)
                            .clamp(0.0, 1.0)
                            .sqrt()
                    } else {
                        1.0
                    };
                    let out_gain = if r < clip.fade_out_end_secs {
                        0.0
                    } else if clip.fade_out_secs > 0.0 {
                        ((r - clip.fade_out_end_secs) / clip.fade_out_secs)
                            .clamp(0.0, 1.0)
                            .sqrt()
                    } else {
                        1.0
                    };
                    in_gain.min(out_gain)
                };
                let vol = if state.muted {
                    0.0
                } else {
                    state.volume * clip.volume * fade_out * clip_fade * mix_factor
                };
                self.set_overlay_volume(clip.id, sink, vol);
            }
            if let Some(sink) = new_overlay {
                ctx.audio_overlay_sinks.insert(clip.id, sink);
            }
        }
    }
}

impl EditorModule for AudioModule {
    fn ui(
        &mut self,
        _ui: &mut Ui,
        _state: &ProjectState,
        _thumb_cache: &mut ThumbnailCache,
        _cmd: &mut Vec<EditorCommand>,
    ) {
        // No UI panel — driven entirely by tick().
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocut_core::state::TimelineClip;
    use velocut_media::MediaWorker;

    fn id() -> Uuid {
        Uuid::new_v4()
    }

    fn timeline_clip_on_lib(clip_id: Uuid, media_id: Uuid) -> TimelineClip {
        TimelineClip {
            id: clip_id,
            media_id,
            start_time: 0.0,
            duration: 5.0,
            track_row: 0,
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

    /// The WAV re-extraction boundary (see `process_wav_rebuild_requests`):
    /// drains sink sessions and clears tracking state for clips of the
    /// re-extracted library entry ONLY — a different entry's live session
    /// (e.g. an overlay) must keep playing across the rewrite.
    ///
    /// Sink maps stay empty here (rodio players can't be built without a live
    /// output stream); what the test pins is the clip-id resolution via
    /// `media_id`, the pending-set consumption, and the tracking-state reset.
    #[test]
    fn wav_rebuild_drains_only_clips_of_the_reextracted_lib_entry() {
        let mut state = velocut_core::state::ProjectState::default();
        let lib_reextracted = Uuid::new_v4();
        let lib_other = Uuid::new_v4();
        let clip_a = Uuid::new_v4();
        let clip_b = Uuid::new_v4();
        state
            .timeline
            .push(timeline_clip_on_lib(clip_a, lib_reextracted));
        state.timeline.push(timeline_clip_on_lib(clip_b, lib_other));

        // Seed tracking state directly (audio_with_tracking targets its own
        // random ids, not the clips this test controls).
        let mut audio = AudioModule::new();
        audio.exhausted.insert(clip_a);
        audio.exhausted.insert(clip_b);
        audio.sink_has_played.insert(clip_a);
        audio.sink_has_played.insert(clip_b);
        audio.sink_created_at.insert(clip_a, Instant::now());
        audio.sink_created_at.insert(clip_b, Instant::now());
        audio.overlay_exhausted.insert(clip_a);
        audio.overlay_exhausted.insert(clip_b);
        audio.overlay_has_played.insert(clip_a);
        audio.overlay_has_played.insert(clip_b);
        audio.overlay_created_at.insert(clip_a, Instant::now());
        audio.overlay_created_at.insert(clip_b, Instant::now());
        audio.overlay_last_volume.insert(clip_a, 0.4);
        audio.overlay_last_volume.insert(clip_b, 0.4);
        let (primary, overlay) = (clip_a, clip_b);

        let mut ctx = AppContext::new(MediaWorker::new());
        ctx.pending_audio_wav_rebuild.insert(lib_reextracted);

        audio.process_wav_rebuild_requests(&state, &mut ctx);

        // The re-extracted entry's session is invalidated
        // (primary+overlay tracking both keyed by clip id — empty sink maps
        // make the drain loops no-ops).
        assert!(!audio.exhausted.contains(&primary));
        assert!(!audio.sink_has_played.contains(&primary));
        assert!(!audio.sink_created_at.contains_key(&primary));
        assert!(!audio.overlay_exhausted.contains(&primary));
        assert!(!audio.overlay_last_volume.contains_key(&primary));
        assert_ne!(audio.overlay_last_volume.get(&primary), Some(&0.4));

        // The other entry's live session is untouched.
        assert!(audio.exhausted.contains(&overlay));
        assert!(audio.sink_has_played.contains(&overlay));
        assert!(audio.sink_created_at.contains_key(&overlay));
        assert!(audio.overlay_exhausted.contains(&overlay));
        assert_eq!(audio.overlay_last_volume.get(&overlay), Some(&0.4));

        // The pending set is consumed.
        assert!(ctx.pending_audio_wav_rebuild.is_empty());
    }

    fn audio_with_tracking() -> (AudioModule, Uuid, Uuid) {
        let mut audio = AudioModule::new();
        let primary = id();
        let overlay = id();
        audio.exhausted.insert(primary);
        audio.sink_has_played.insert(primary);
        audio.sink_created_at.insert(primary, Instant::now());
        audio.sink_clip_shapes.insert(primary, (0.0, 3.0));
        audio.sink_last_volume.insert(primary, 0.5);
        audio.overlay_exhausted.insert(overlay);
        audio.overlay_has_played.insert(overlay);
        audio.overlay_created_at.insert(overlay, Instant::now());
        audio.overlay_last_volume.insert(overlay, 0.4);
        (audio, primary, overlay)
    }

    /// A primary clip change must not reset overlay state: resetting it would
    /// restart an independent A-row clip on the next tick, which is exactly the
    /// gap/click `soft_drain_primary_only` exists to avoid.
    #[test]
    fn primary_reset_leaves_overlay_tracking_alone() {
        let (mut audio, primary, overlay) = audio_with_tracking();

        audio.clear_primary_state();

        assert!(!audio.exhausted.contains(&primary));
        assert!(!audio.sink_has_played.contains(&primary));
        assert!(!audio.sink_created_at.contains_key(&primary));
        assert!(!audio.sink_clip_shapes.contains_key(&primary));
        assert!(!audio.sink_last_volume.contains_key(&primary));

        assert!(audio.overlay_exhausted.contains(&overlay));
        assert!(audio.overlay_has_played.contains(&overlay));
        assert!(audio.overlay_created_at.contains_key(&overlay));
        assert_eq!(audio.overlay_last_volume.get(&overlay), Some(&0.4));
    }

    /// Stop and seek still clear everything, so a jump cannot leave a stale
    /// overlay sink playing audio from the old position.
    #[test]
    fn full_reset_clears_primary_and_overlay_tracking() {
        let (mut audio, _primary, _overlay) = audio_with_tracking();

        audio.clear_sink_state();

        assert!(audio.exhausted.is_empty());
        assert!(audio.sink_created_at.is_empty());
        assert!(audio.sink_clip_shapes.is_empty());
        assert!(audio.sink_last_volume.is_empty());
        assert!(audio.overlay_exhausted.is_empty());
        assert!(audio.overlay_has_played.is_empty());
        assert!(audio.overlay_created_at.is_empty());
        assert!(audio.overlay_last_volume.is_empty());
    }

    // ── F4: (source_offset, duration) content stamp ─────────────────────────

    fn primary_session_seeded(audio: &mut AudioModule, clip_id: Uuid, shape: (f64, f64)) {
        audio.sink_clip_shapes.insert(clip_id, shape);
        audio.exhausted.insert(clip_id);
        audio.sink_has_played.insert(clip_id);
        audio.sink_created_at.insert(clip_id, Instant::now());
        audio.sink_last_volume.insert(clip_id, 0.7);
    }

    /// F4 audit case: a SLIDING trim that keeps the clip duration constant
    /// (e.g. 0.0s..5.0s slid to 2.0s..7.0s). The duration-only stamp this
    /// replaces passed both, and the old sink kept playing the OLD audio
    /// region against the video's NEW window — wrong content with a matching
    /// clip id. The shape stamp (`clip_shape` / `evict_shape_stamped_sink`)
    /// catches it.
    ///
    /// Sink maps stay empty (rodio players need a live output stream); the
    /// test pins eviction-decision + tracking-state semantics.
    #[test]
    fn slide_at_constant_duration_rebuilds_the_primary_sink() {
        let mut audio = AudioModule::new();
        let clip_id = id();
        primary_session_seeded(&mut audio, clip_id, (0.0, 5.0));

        let mut state = velocut_core::state::ProjectState::default();
        let mut clip = timeline_clip_on_lib(clip_id, Uuid::new_v4());
        clip.source_offset = 2.0; // slid 2s forward...
        clip.duration = 5.0; // ...with duration unchanged
        state.timeline.push(clip);

        let mut ctx = AppContext::new(MediaWorker::new());
        let evicted = audio.evict_shape_stamped_sink(&mut ctx, &state.timeline[0], false);

        assert!(evicted, "a slide with constant duration must evict");
        assert!(!audio.exhausted.contains(&clip_id));
        assert!(!audio.sink_has_played.contains(&clip_id));
        assert!(!audio.sink_created_at.contains_key(&clip_id));
        assert!(!audio.sink_last_volume.contains_key(&clip_id));
        assert!(!audio.sink_clip_shapes.contains_key(&clip_id));
    }

    #[test]
    fn unchanged_window_keeps_the_primary_sink() {
        let mut audio = AudioModule::new();
        let clip_id = id();
        primary_session_seeded(&mut audio, clip_id, (0.0, 5.0));

        let mut state = velocut_core::state::ProjectState::default();
        state
            .timeline
            .push(timeline_clip_on_lib(clip_id, Uuid::new_v4()));

        let mut ctx = AppContext::new(MediaWorker::new());
        assert!(!audio.evict_shape_stamped_sink(&mut ctx, &state.timeline[0], false));
        assert!(audio.exhausted.contains(&clip_id));
        assert!(audio.sink_clip_shapes.contains_key(&clip_id));
        assert_eq!(audio.sink_clip_shapes.get(&clip_id), Some(&(0.0, 5.0)));
    }

    /// The old duration-only check's case must keep working: a trim that
    /// shortens the clip rebuilds the sink.
    #[test]
    fn duration_change_alone_still_rebuilds_the_primary_sink() {
        let mut audio = AudioModule::new();
        let clip_id = id();
        primary_session_seeded(&mut audio, clip_id, (0.0, 5.0));

        let mut state = velocut_core::state::ProjectState::default();
        let mut clip = timeline_clip_on_lib(clip_id, Uuid::new_v4());
        clip.source_offset = 0.0;
        clip.duration = 3.0; // trimmed shorter, window start unchanged
        state.timeline.push(clip);

        let mut ctx = AppContext::new(MediaWorker::new());
        assert!(audio.evict_shape_stamped_sink(&mut ctx, &state.timeline[0], false));
    }

    /// Overlay sinks had NO window-identity check at all before — a sliding
    /// trim under the playhead kept the old overlay sink playing the old
    /// window. The shape stamp now governs both sink maps.
    #[test]
    fn slide_rebuilds_the_overlay_sink_too() {
        let mut audio = AudioModule::new();
        let clip_id = id();
        audio.overlay_clip_shapes.insert(clip_id, (1.0, 4.0));
        audio.overlay_exhausted.insert(clip_id);
        audio.overlay_has_played.insert(clip_id);
        audio.overlay_created_at.insert(clip_id, Instant::now());
        audio.overlay_last_volume.insert(clip_id, 0.4);

        let mut state = velocut_core::state::ProjectState::default();
        let mut clip = timeline_clip_on_lib(clip_id, Uuid::new_v4());
        clip.source_offset = 3.0;
        clip.duration = 4.0;
        state.timeline.push(clip);

        let mut ctx = AppContext::new(MediaWorker::new());
        let evicted = audio.evict_shape_stamped_sink(&mut ctx, &state.timeline[0], true);

        assert!(evicted, "overlay window slide must evict");
        assert!(!audio.overlay_exhausted.contains(&clip_id));
        assert!(!audio.overlay_has_played.contains(&clip_id));
        assert!(!audio.overlay_created_at.contains_key(&clip_id));
        assert!(!audio.overlay_last_volume.contains_key(&clip_id));
        assert!(!audio.overlay_clip_shapes.contains_key(&clip_id));

        // The overlay eviction must not touch primary tracking of the same id.
        primary_session_seeded(&mut audio, clip_id, (1.0, 4.0));
        assert!(audio.exhausted.contains(&clip_id));
    }

    // ── F5: seek failures are logged, not swallowed ─────────────────────────

    /// A failed `try_seek` must not abort sink creation — the clip still plays,
    /// just from the WAV's start — but it has to leave a trace, otherwise the
    /// audible desync (right clip id, wrong content position) has no
    /// diagnostic anywhere. `seek_sink` takes the seek call and the log sink as
    /// parameters so this branch runs without an audio device.
    #[test]
    fn failed_seek_is_logged_with_sink_identity_and_error() {
        let mut logged: Vec<String> = Vec::new();
        let ok = AudioModule::seek_sink(
            |_pos| Err("device refused the seek"),
            12.5,
            "clip 7d1",
            &mut |m| logged.push(m.to_string()),
        );

        assert!(!ok, "seek_sink must report the failure to the caller");
        assert_eq!(logged.len(), 1, "exactly one line per failed seek");
        let msg = &logged[0];
        assert!(msg.contains("clip 7d1"), "sink identity missing: {msg}");
        assert!(msg.contains("12.500"), "seek position missing: {msg}");
        assert!(
            msg.contains("device refused the seek"),
            "decoder error missing: {msg}"
        );
        assert!(
            msg.contains("playing from WAV start"),
            "audible consequence missing: {msg}"
        );
    }

    /// The happy path stays silent and hands the sink exactly the requested
    /// offset.
    #[test]
    fn successful_seek_logs_nothing_and_passes_the_offset() {
        let mut logged: Vec<String> = Vec::new();
        let mut seen: Option<Duration> = None;
        let ok = AudioModule::seek_sink(
            |pos| -> Result<(), &str> {
                seen = Some(pos);
                Ok(())
            },
            4.25,
            "overlay clip 7d1",
            &mut |m| logged.push(m.to_string()),
        );

        assert!(ok);
        assert_eq!(seen, Some(Duration::from_secs_f64(4.25)));
        assert!(
            logged.is_empty(),
            "a successful seek must not log: {logged:?}"
        );
    }

    /// The overlay sink and the primary sink share the helper but must remain
    /// distinguishable in the log — a bare "try_seek failed" line would not
    /// say which of the two sinks desynced.
    #[test]
    fn overlay_seek_failure_names_the_overlay_sink() {
        let mut logged: Vec<String> = Vec::new();
        let ok = AudioModule::seek_sink(|_pos| Err("boom"), 0.0, "overlay clip 7d1", &mut |m| {
            logged.push(m.to_string())
        });

        assert!(!ok);
        assert_eq!(logged.len(), 1);
        assert!(
            logged[0].starts_with("overlay clip 7d1 try_seek(0.000s)"),
            "{}",
            logged[0]
        );
    }

    // ── Open-failure episodes: one log line, not one per tick ───────────────

    fn wav_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    /// The bug this closes: an absent sink is rebuilt every tick, so a WAV that
    /// can never be opened re-logged (and re-tried) 60 times a second. The
    /// episode must log at onset, stay silent for silent retries, and re-arm
    /// the window on each failure.
    #[test]
    fn open_failure_logs_once_and_blocks_retries_inside_the_window() {
        let mut audio = AudioModule::new();
        let clip_id = id();
        let wav = wav_path("velocut_audio_f5fail.wav");
        let t0 = Instant::now();
        let mut logged: Vec<String> = Vec::new();

        audio.note_open_failure(
            clip_id,
            &wav,
            false,
            t0,
            "clip failed to open (1)",
            &mut |m| logged.push(m.to_string()),
        );
        assert_eq!(logged.len(), 1, "the failure onset must be logged");
        assert!(audio.open_failure_blocked(clip_id, false, t0));
        assert!(audio.open_failure_blocked(
            clip_id,
            false,
            t0 + OPEN_RETRY - Duration::from_millis(1)
        ));

        // A retry inside the window re-fails: silent, and the window re-arms.
        let inside = t0 + Duration::from_millis(100);
        audio.note_open_failure(
            clip_id,
            &wav,
            false,
            inside,
            "clip failed to open (2)",
            &mut |m| logged.push(m.to_string()),
        );
        assert_eq!(
            logged.len(),
            1,
            "a retry of the same broken WAV must not log"
        );
        // The retry re-arms the window from its own attempt time.
        assert!(audio.open_failure_blocked(
            clip_id,
            false,
            inside + OPEN_RETRY - Duration::from_millis(1)
        ));

        // Past the window the builder may try again; failing again is still the
        // same episode and stays silent.
        let later = inside + OPEN_RETRY + Duration::from_millis(1);
        assert!(!audio.open_failure_blocked(clip_id, false, later));
        audio.note_open_failure(
            clip_id,
            &wav,
            false,
            later,
            "clip failed to open (3)",
            &mut |m| logged.push(m.to_string()),
        );
        assert_eq!(logged.len(), 1, "a retry failure is not a new episode");
    }

    /// A re-extraction can put a different file behind the clip, so a failure
    /// against a different path is a new episode and logs.
    #[test]
    fn failure_against_a_different_wav_starts_a_new_episode() {
        let mut audio = AudioModule::new();
        let clip_id = id();
        let t0 = Instant::now();
        let mut logged: Vec<String> = Vec::new();

        audio.note_open_failure(
            clip_id,
            &wav_path("velocut_audio_a.wav"),
            false,
            t0,
            "failed against A",
            &mut |m| logged.push(m.to_string()),
        );
        audio.note_open_failure(
            clip_id,
            &wav_path("velocut_audio_b.wav"),
            false,
            t0,
            "failed against B",
            &mut |m| logged.push(m.to_string()),
        );

        assert_eq!(logged.len(), 2, "a new path must be reported: {logged:?}");
        assert_eq!(logged[1], "failed against B");
    }

    /// Success and state removal end the episode, so a later failure is
    /// reported again instead of being swallowed for the rest of the session.
    #[test]
    fn success_and_state_removal_end_the_episode() {
        let mut audio = AudioModule::new();
        let clip_id = id();
        let wav = wav_path("velocut_audio_recover.wav");
        let t0 = Instant::now();
        let mut logged: Vec<String> = Vec::new();

        audio.note_open_failure(clip_id, &wav, false, t0, "first failure", &mut |m| {
            logged.push(m.to_string())
        });
        audio.clear_open_failure(clip_id, false); // the sink was built
        assert!(!audio.open_failure_blocked(clip_id, false, t0));
        audio.note_open_failure(
            clip_id,
            &wav,
            false,
            t0,
            "failure after recovery",
            &mut |m| logged.push(m.to_string()),
        );
        assert_eq!(
            logged.len(),
            2,
            "a post-recovery failure must log: {logged:?}"
        );

        // Eviction / stale-clip removal / WAV rebuild all route through
        // remove_sink_state, which ends the episode too.
        audio.remove_sink_state(clip_id);
        assert!(!audio.open_failure_blocked(clip_id, false, t0));
        audio.note_open_failure(
            clip_id,
            &wav,
            false,
            t0,
            "failure after eviction",
            &mut |m| logged.push(m.to_string()),
        );
        assert_eq!(
            logged.len(),
            3,
            "an evicted clip's failure must log again: {logged:?}"
        );
    }

    /// Primary and overlay sinks read the same WAV but are built separately:
    /// a primary failure must not mute the overlay's retry (and vice versa).
    #[test]
    fn primary_and_overlay_episodes_are_independent() {
        let mut audio = AudioModule::new();
        let clip_id = id();
        let wav = wav_path("velocut_audio_split.wav");
        let t0 = Instant::now();
        let mut logged: Vec<String> = Vec::new();

        audio.note_open_failure(clip_id, &wav, false, t0, "primary failed", &mut |m| {
            logged.push(m.to_string())
        });

        assert!(audio.open_failure_blocked(clip_id, false, t0));
        assert!(
            !audio.open_failure_blocked(clip_id, true, t0),
            "the overlay sink must still be attempted"
        );

        audio.note_open_failure(clip_id, &wav, true, t0, "overlay failed", &mut |msg| {
            logged.push(msg.to_string())
        });
        assert_eq!(logged, vec!["primary failed", "overlay failed"]);
    }

    /// Stop/seek clear the cache: a new playback session reports the failure
    /// once more rather than staying silent from the previous run.
    #[test]
    fn clear_sink_state_resets_open_failure_episodes() {
        let mut audio = AudioModule::new();
        let clip_id = id();
        let wav = wav_path("velocut_audio_session.wav");
        let t0 = Instant::now();
        let mut logged: Vec<String> = Vec::new();

        audio.note_open_failure(clip_id, &wav, false, t0, "primary failed", &mut |m| {
            logged.push(m.to_string())
        });
        audio.note_open_failure(clip_id, &wav, true, t0, "overlay failed", &mut |m| {
            logged.push(m.to_string())
        });
        assert!(audio.open_failure_blocked(clip_id, false, t0));
        assert!(audio.open_failure_blocked(clip_id, true, t0));

        audio.clear_sink_state();

        assert!(!audio.open_failure_blocked(clip_id, false, t0));
        assert!(!audio.open_failure_blocked(clip_id, true, t0));
        assert!(audio.sink_open_failures.is_empty());
        assert!(audio.overlay_open_failures.is_empty());
    }
}

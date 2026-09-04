//! Per-camera frame statistics registry.
//!
//! Each supervisor task owns a shared `Arc<FrameStatsRegistry>` and
//! calls [`FrameStatsRegistry::observe_frame`] every time a frame
//! arrives from the source, and [`FrameStatsRegistry::observe_dropped`]
//! whenever the motion gate (or any later stage) discards a frame.
//!
//! The HTTP layer (`GET /v1/cameras/:id/stats` + the same fields
//! merged into `GET /api/v1/cameras`) reads a cheap snapshot of the
//! map. Same contention model as [`crate::cache::LatestFrameCache`]:
//! one writer per camera, many readers — `parking_lot::RwLock` over
//! a `HashMap` is the right primitive.
//!
//! Why a separate registry instead of squatting on the existing bus
//! `PIPELINE_STATUS` topic: that topic publishes only on supervisor
//! state transitions (Initializing → Running → Stopped), not on
//! every frame, so it can't carry a live fps EMA.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use nexus_types::CameraId;
use parking_lot::RwLock;
use serde::Serialize;

/// Sliding window over which `fps_ema` is averaged. Two seconds is
/// long enough to smooth typical source jitter (camera frame timing,
/// gate back-pressure) without making the reading laggy when fps
/// genuinely changes (camera reconnect, source switch).
const FPS_WINDOW: Duration = Duration::from_secs(2);
/// Hard cap on retained frame timestamps. Bounds memory and CPU at
/// roughly 120 fps within the window; anything faster than that is
/// already past the detector cadence we care about.
const FPS_WINDOW_MAX_SAMPLES: usize = 240;

/// A camera with no observed frame within this window is reported
/// offline. This is the frame-source liveness signal — distinct from
/// `live_view::STALL_AFTER`, which only judges cameras with an active
/// live-view subscriber and would misreport every unwatched camera.
/// Generous relative to the 30s cloud heartbeat cadence so a single
/// slow tick or brief reconnect never flaps the fleet view; a source
/// that is actually gone stays silent far longer than this.
pub const CAMERA_OFFLINE_AFTER_MS: i64 = 90_000;

/// Snapshot returned to API callers. Cheap to clone.
#[derive(Debug, Clone)]
pub struct CameraFrameStats {
    /// Wall-clock timestamp of the most recent frame, in UTC.
    pub last_frame_at: Option<DateTime<Utc>>,
    /// Frames-per-second computed over a fixed wall-clock window
    /// ([`FPS_WINDOW`]). Immune to burst arrivals because the divisor
    /// is the window length, not the inter-frame delta. The field is
    /// kept named `fps_ema` for API stability; the value is no longer
    /// EMA-derived. Zero until two frames have been seen.
    pub fps_ema: f64,
    /// Total frames received from the source since this camera was
    /// last (re)spawned. Counted at the live-view tap, i.e. the real
    /// decode rate — not the rate the analysis loop consumes at.
    pub frames_emitted: u64,
    /// Frames the motion gate (or any later stage) discarded. This is
    /// **not** `frames_emitted` minus the analysed count: since
    /// BUG-136 the analysis loop takes frames latest-wins off a
    /// `watch`, so frames coalesced away before the gate ever saw them
    /// are in neither total.
    pub frames_dropped: u64,
    /// Width of the most recent frame, in pixels. For RTSP this is
    /// the detector frame dimension (currently 960), NOT the camera
    /// native resolution. The UI uses this to scale bbox overlay
    /// coordinates to the displayed video element.
    pub source_width: u32,
    pub source_height: u32,
    /// M_TILE_REINFER (G1) — number of frames on which the tile
    /// cascade fired (stage-1 crowd threshold crossed AND `pick_tiles`
    /// returned a non-empty set AND `run_tile_inference` succeeded).
    pub tile_invocations: u64,
    /// Total stage-2 detections merged into the tracker input across
    /// all `tile_invocations`. Divide by `tile_invocations` for the
    /// mean added detections per cascade.
    pub tile_detections_added: u64,
    /// Cumulative wall-clock spent inside `run_tile_inference` (sum of
    /// per-invocation elapsed-ms). Divide by `tile_invocations` for the
    /// mean per-cascade latency.
    pub tile_inference_ms_total: u64,
}

impl CameraFrameStats {
    /// Milliseconds since the last observed frame, computed against
    /// the supplied wall-clock `now`. `None` if no frame has been
    /// observed yet.
    pub fn last_frame_age_ms(&self, now: DateTime<Utc>) -> Option<i64> {
        self.last_frame_at
            .map(|t| (now - t).num_milliseconds().max(0))
    }

    /// Edge-observed liveness in the last frame-source pass: `true` only
    /// when a frame has actually arrived within [`CAMERA_OFFLINE_AFTER_MS`].
    /// A camera that has never produced a frame (never spawned, or spawned
    /// but not yet decoding) is offline, not unknown — the whole point of
    /// this signal is to say something real instead of a placeholder.
    pub fn is_online(&self, now: DateTime<Utc>) -> bool {
        self.last_frame_age_ms(now)
            .is_some_and(|age_ms| age_ms <= CAMERA_OFFLINE_AFTER_MS)
    }
}

/// Internal mutable record. Tracks the monotonic `Instant` of every
/// frame observed in the current [`FPS_WINDOW`] (wall-clock is unsafe
/// for fps math — operators can drift system time).
struct Entry {
    last_frame_at: Option<DateTime<Utc>>,
    /// Ring of frame arrival `Instant`s, capped at
    /// [`FPS_WINDOW_MAX_SAMPLES`]. Pruned to entries inside
    /// [`FPS_WINDOW`] on every observation.
    recent_instants: VecDeque<Instant>,
    frames_emitted: u64,
    frames_dropped: u64,
    source_width: u32,
    source_height: u32,
    tile_invocations: u64,
    tile_detections_added: u64,
    tile_inference_ms_total: u64,
}

impl Entry {
    /// Compute the sliding-window fps from the retained `Instant`s.
    /// Returns 0.0 when fewer than two frames are in the window
    /// (i.e. no measurable rate yet).
    fn fps(&self, now: Instant) -> f64 {
        if self.recent_instants.len() < 2 {
            return 0.0;
        }
        let oldest = *self.recent_instants.front().expect("len >= 2");
        // Divide by elapsed-from-oldest (capped at the window) so the
        // reading decays naturally when the source stops emitting.
        let elapsed = now
            .saturating_duration_since(oldest)
            .min(FPS_WINDOW)
            .as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }
        (self.recent_instants.len() as f64 - 1.0) / elapsed
    }

    fn snapshot(&self, now: Instant) -> CameraFrameStats {
        CameraFrameStats {
            last_frame_at: self.last_frame_at,
            fps_ema: self.fps(now),
            frames_emitted: self.frames_emitted,
            frames_dropped: self.frames_dropped,
            source_width: self.source_width,
            source_height: self.source_height,
            tile_invocations: self.tile_invocations,
            tile_detections_added: self.tile_detections_added,
            tile_inference_ms_total: self.tile_inference_ms_total,
        }
    }
}

#[derive(Default)]
pub struct FrameStatsRegistry {
    inner: RwLock<HashMap<CameraId, Entry>>,
}

impl FrameStatsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one frame from the source. `captured_at` should be the
    /// wall-clock timestamp on the `Frame` itself. `width`/`height`
    /// are the source frame dimensions.
    pub fn observe_frame(
        &self,
        camera_id: CameraId,
        captured_at: DateTime<Utc>,
        width: u32,
        height: u32,
    ) {
        let now = Instant::now();
        let mut guard = self.inner.write();
        let entry = guard.entry(camera_id).or_insert_with(|| Entry {
            last_frame_at: None,
            recent_instants: VecDeque::with_capacity(FPS_WINDOW_MAX_SAMPLES),
            frames_emitted: 0,
            frames_dropped: 0,
            source_width: 0,
            source_height: 0,
            tile_invocations: 0,
            tile_detections_added: 0,
            tile_inference_ms_total: 0,
        });
        // Prune anything older than the window before appending so the
        // VecDeque stays bounded even at high arrival rates.
        let cutoff = now - FPS_WINDOW;
        while let Some(front) = entry.recent_instants.front() {
            if *front < cutoff {
                entry.recent_instants.pop_front();
            } else {
                break;
            }
        }
        entry.recent_instants.push_back(now);
        // Hard cap on retained samples — protects against pathological
        // burst arrivals that pre-prune leaves longer than the window.
        while entry.recent_instants.len() > FPS_WINDOW_MAX_SAMPLES {
            entry.recent_instants.pop_front();
        }
        entry.last_frame_at = Some(captured_at);
        entry.frames_emitted = entry.frames_emitted.saturating_add(1);
        entry.source_width = width;
        entry.source_height = height;
    }

    pub fn observe_dropped(&self, camera_id: CameraId) {
        let mut guard = self.inner.write();
        if let Some(entry) = guard.get_mut(&camera_id) {
            entry.frames_dropped = entry.frames_dropped.saturating_add(1);
        }
    }

    /// M_TILE_REINFER (G1) — record one successful tile cascade. The
    /// caller passes the number of stage-2 detections merged into the
    /// tracker input (`added`, post-prompts-whitelist) and the
    /// wall-clock elapsed across `run_tile_inference` (`infer_ms`). No
    /// entry is created if the camera has not yet observed a frame —
    /// the cascade always runs after `observe_frame`, so the entry is
    /// guaranteed to exist by the time this is called.
    pub fn observe_tile_invocation(&self, camera_id: CameraId, added: u64, infer_ms: u64) {
        let mut guard = self.inner.write();
        if let Some(entry) = guard.get_mut(&camera_id) {
            entry.tile_invocations = entry.tile_invocations.saturating_add(1);
            entry.tile_detections_added = entry.tile_detections_added.saturating_add(added);
            entry.tile_inference_ms_total = entry.tile_inference_ms_total.saturating_add(infer_ms);
        }
    }

    /// Reset a camera's stats — call this when a supervisor is
    /// stopped (e.g. on `disable` or URL change), so the next spawn
    /// starts from a clean slate.
    pub fn clear(&self, camera_id: CameraId) {
        self.inner.write().remove(&camera_id);
    }

    pub fn snapshot(&self, camera_id: CameraId) -> Option<CameraFrameStats> {
        let now = Instant::now();
        self.inner.read().get(&camera_id).map(|e| e.snapshot(now))
    }

    pub fn snapshot_all(&self) -> HashMap<CameraId, CameraFrameStats> {
        let now = Instant::now();
        self.inner
            .read()
            .iter()
            .map(|(k, v)| (*k, v.snapshot(now)))
            .collect()
    }
}

/// Per-camera decode-health counters, written by the pre-roll ingester's
/// GStreamer threads.
///
/// Deliberately a separate registry from [`FrameStatsRegistry`] rather than
/// more columns on it. That one is written by the supervisor task, *after*
/// the RGB appsink's own `drop=true max-buffers=4` and after any broadcast
/// lag — which is exactly why its `fps_ema` cannot answer "did we lose this
/// frame before the decoder or after it?". These counters are written by the
/// ingester, on either side of the decoder, and answering that question is
/// their whole purpose (BUG-071).
#[derive(Debug, Default)]
pub struct DecodeHealthRegistry {
    inner: RwLock<HashMap<CameraId, DecodeHealth>>,
    /// SPEC-069 Phase 1 — windowed-rate side-state, one [`RateState`] per
    /// camera. Kept in its own map (rather than inline on `DecodeHealth`)
    /// because it holds `Instant`s, which are neither serializable nor
    /// `Copy`-cheap to carry through the public snapshot type.
    rates: RwLock<HashMap<CameraId, RateState>>,
}

/// Snapshot of one camera's decode health. Cheap to clone.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DecodeHealth {
    /// Compressed access units the RGB branch's `leaky=downstream` queue
    /// dropped **ahead of** the decoder.
    ///
    /// That queue sits between `h26Xparse` and the decode chain, so its
    /// buffers are H.26x access units (`alignment=au`), not frames. Losing
    /// one mid-GOP corrupts every picture until the next IDR — smeared
    /// motion and blocky residue, not a dropped frame. Anything above zero
    /// on a sustained basis means the decoder cannot keep up with the
    /// camera and the picture is being damaged to make room.
    pub decoder_input_drops: u64,
    /// Frames the decoder actually produced, counted on its src pad.
    ///
    /// This is the only honest measure of what the video engine managed.
    /// Every chain ends in a `videorate` that pads a starved decoder back
    /// up to the requested framerate by duplicating buffers, so anything
    /// counted downstream reads a flat nominal rate regardless of how
    /// little the hardware delivered. Compare against
    /// [`Self::sampled_frames`]: a large gap means the picture reaching
    /// the engine is mostly duplicated padding.
    pub decoder_output_frames: u64,
    /// Frames delivered to the RGB appsink, i.e. what the engine actually
    /// consumed — **after** `videorate` padding, so this counts duplicates
    /// as real frames. It is the correct denominator for
    /// [`Self::duplicate_frames`] (which is detected here) and the wrong
    /// one for decode throughput.
    pub sampled_frames: u64,
    /// Of [`Self::sampled_frames`], how many repeated a picture seen 2–12
    /// frames earlier, i.e. the decoder re-served a surface it had already
    /// handed over. Repeats at distance 1 (static scene, `videorate`
    /// padding) are excluded by the detector and never counted here.
    ///
    /// The guard only logs once it crosses `FRAME_LOOP_TRIP` within
    /// `FRAME_LOOP_EVAL_WINDOW`, so a loop that stays under that bar was
    /// previously invisible in production. This is the raw rate.
    pub duplicate_frames: u64,
    /// SPEC-069 Phase 1 — `decoder_output_frames` per second, over a
    /// trailing window (see `RATE_WINDOW`). The cumulative counter above
    /// answers "how much has this camera decoded since the supervisor
    /// started"; this answers "how much is it decoding right now", which
    /// is the number decode-capacity ceiling calibration needs. Zero
    /// until at least two src-pad buffers have landed inside one window.
    pub decoder_output_fps: f32,
    /// SPEC-069 Phase 1 — `sampled_frames` per second over the same trailing
    /// [`RATE_WINDOW`] as `decoder_output_fps`. Includes `videorate` padding
    /// (same caveat as the cumulative counter), so the gap between the two is
    /// how much of the output is duplicated rather than decoded — a
    /// comparison that is only valid because they share a window.
    pub sampled_fps: f32,
    /// SPEC-069 Phase 1 — width/height negotiated on the decoder's own
    /// src pad (not the appsink's, which can differ when a `videoscale`
    /// sits downstream). Zero until the first buffer with caps arrives.
    pub decoder_width: u32,
    pub decoder_height: u32,
}

impl DecodeHealth {
    /// Duplicate frames per thousand sampled. Zero when nothing sampled yet.
    #[must_use]
    pub fn duplicate_per_mille(&self) -> u64 {
        self.duplicate_frames
            .saturating_mul(1000)
            .checked_div(self.sampled_frames)
            .unwrap_or(0)
    }
}

/// Trailing window used to turn discrete `observe_*` calls into a
/// per-second rate. 5s balances responsiveness (a stalled decoder should
/// show up quickly) against noise from one slow frame.
const RATE_WINDOW: Duration = Duration::from_secs(5);

/// Rolling event-timestamp window → events/sec.
#[derive(Debug, Default)]
struct RateWindow {
    timestamps: VecDeque<Instant>,
}

impl RateWindow {
    /// Record one event `now` and return the rate (events/sec) over the
    /// trailing [`RATE_WINDOW`]. Needs at least two timestamps in the
    /// window to produce a rate; a single event has no span to divide by.
    fn record(&mut self, now: Instant) -> f32 {
        self.timestamps.push_back(now);
        while let Some(&front) = self.timestamps.front() {
            if now.duration_since(front) > RATE_WINDOW {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
        let n = self.timestamps.len();
        if n < 2 {
            return 0.0;
        }
        let span = self
            .timestamps
            .back()
            .unwrap()
            .duration_since(*self.timestamps.front().unwrap())
            .as_secs_f32();
        if span <= 0.0 {
            return 0.0;
        }
        (n - 1) as f32 / span
    }
}

/// Rate-tracking side-state for one camera, kept out of the public,
/// `Copy` [`DecodeHealth`] snapshot: `Instant` isn't serializable and
/// doesn't need to cross the API boundary, only the fps it produces does.
#[derive(Debug, Default)]
struct RateState {
    decoder_output: RateWindow,
    /// Sampled (post-`videorate`) appsink deliveries. Shares [`RATE_WINDOW`]
    /// with `decoder_output` so the two rates can legitimately be divided —
    /// `decode_verdict` does exactly that to size the padding gap.
    sampled: RateWindow,
}

impl DecodeHealthRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one leaked access unit on the RGB branch's decoder-input queue.
    pub fn observe_decoder_input_drop(&self, camera_id: CameraId) {
        let mut guard = self.inner.write();
        let e = guard.entry(camera_id).or_default();
        e.decoder_input_drops = e.decoder_input_drops.saturating_add(1);
    }

    /// Record one frame emitted by the decoder itself (src-pad probe).
    pub fn observe_decoder_output(&self, camera_id: CameraId) {
        let now = Instant::now();
        let fps = self
            .rates
            .write()
            .entry(camera_id)
            .or_default()
            .decoder_output
            .record(now);
        let mut guard = self.inner.write();
        let e = guard.entry(camera_id).or_default();
        e.decoder_output_frames = e.decoder_output_frames.saturating_add(1);
        e.decoder_output_fps = fps;
    }

    /// Record the width/height negotiated on the decoder's src pad.
    pub fn observe_decoder_geometry(&self, camera_id: CameraId, width: u32, height: u32) {
        let mut guard = self.inner.write();
        let e = guard.entry(camera_id).or_default();
        e.decoder_width = width;
        e.decoder_height = height;
    }

    /// Publish the loop detector's running totals. Absolute, not deltas, so
    /// a session rebuild resets them rather than double-counting. Called once
    /// per appsink delivery, which is what makes the rate window below count
    /// sampled frames.
    pub fn observe_loop_stats(&self, camera_id: CameraId, sampled: u64, duplicates: u64) {
        let now = Instant::now();
        let fps = self
            .rates
            .write()
            .entry(camera_id)
            .or_default()
            .sampled
            .record(now);
        let mut guard = self.inner.write();
        let e = guard.entry(camera_id).or_default();
        e.sampled_frames = sampled;
        e.duplicate_frames = duplicates;
        e.sampled_fps = fps;
    }

    /// Reset one camera. Called when a supervisor stops so the next spawn
    /// starts clean, mirroring [`FrameStatsRegistry::clear`].
    pub fn clear(&self, camera_id: CameraId) {
        self.inner.write().remove(&camera_id);
        self.rates.write().remove(&camera_id);
    }

    #[must_use]
    pub fn snapshot(&self, camera_id: CameraId) -> Option<DecodeHealth> {
        self.inner.read().get(&camera_id).copied()
    }

    /// Every camera the registry currently holds health for. The absent-vs-
    /// zero distinction matters to callers that report a census rather than
    /// a lookup: a camera missing here has no live decode chain, which is
    /// not the same as one decoding cleanly.
    #[must_use]
    pub fn snapshot_all(&self) -> HashMap<CameraId, DecodeHealth> {
        self.inner.read().clone()
    }
}

// ---------------------------------------------------------------------------
// SPEC-069 Phase 1 (P3) — analysis-stream status.
// ---------------------------------------------------------------------------

/// Which URL the analysis pipeline is actually reading right now, and how
/// healthy that choice is. Published on the stats endpoints as
/// `analysis_stream` — `{mode, state, reason, width, height, fps}` — so a
/// column/verdict can explain WHY decode capacity looks the way it does: a
/// camera stuck analysing its 1080p mainstream (because the substream never
/// negotiated, or fell back) consumes far more decode than one analysing a
/// healthy 360p substream, and that's invisible from decode-capacity
/// numbers alone.
#[derive(Debug, Clone, Serialize)]
pub struct AnalysisStreamStatus {
    /// `"substream"` or `"mainstream"` — which URL analysis is reading.
    pub mode: String,
    /// `"active"` (delivering frames at an acceptable rate), `"probing"`
    /// (inside the substream's first-frame grace window, verdict not
    /// judged yet), or `"unavailable"` (a configured substream was tried
    /// and rejected; analysis fell back to the main stream).
    pub state: String,
    /// Present only when `state == "unavailable"`: `"refused"`,
    /// `"no_frames"`, or `"unhealthy"` — see [`crate::source::FallbackReason`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
    /// Geometry of the currently-active stream (from the frame source
    /// actually being read, not necessarily the substream). Zero until
    /// the first frame arrives.
    pub width: u32,
    pub height: u32,
    /// Frames per second observed over the last health window. Zero
    /// while `state == "probing"`.
    pub fps: f32,
}

/// Registry of per-camera [`AnalysisStreamStatus`]. Separate from
/// [`DecodeHealthRegistry`] because it is written from a different call
/// site (`SharedRtspSource::run`'s health tick, not the decoder's src-pad
/// probe) and answers a different question: not "how much is the decoder
/// managing" but "which stream is it managing".
#[derive(Debug, Default)]
pub struct AnalysisStreamRegistry {
    inner: RwLock<HashMap<CameraId, AnalysisStreamStatus>>,
}

impl AnalysisStreamRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A camera with no substream configured at all reads `mainstream` /
    /// `active` from the start — this is the intended, healthy state, NOT
    /// a fallback, so it must never be confused with `unavailable`.
    pub fn observe_mainstream_by_design(&self, camera_id: CameraId) {
        self.inner.write().insert(
            camera_id,
            AnalysisStreamStatus {
                mode: "mainstream".to_string(),
                state: "active".to_string(),
                reason: None,
                width: 0,
                height: 0,
                fps: 0.0,
            },
        );
    }

    /// A substream session exists but hasn't cleared its first-frame grace
    /// window yet — too early to call it healthy or unavailable.
    pub fn observe_probing(&self, camera_id: CameraId) {
        self.inner.write().insert(
            camera_id,
            AnalysisStreamStatus {
                mode: "substream".to_string(),
                state: "probing".to_string(),
                reason: None,
                width: 0,
                height: 0,
                fps: 0.0,
            },
        );
    }

    /// Substream delivering frames at an acceptable rate.
    pub fn observe_active(&self, camera_id: CameraId, width: u32, height: u32, fps: f32) {
        self.inner.write().insert(
            camera_id,
            AnalysisStreamStatus {
                mode: "substream".to_string(),
                state: "active".to_string(),
                reason: None,
                width,
                height,
                fps,
            },
        );
    }

    /// A configured substream was tried and rejected; analysis fell back
    /// to the main stream. `reason` is one of `"refused"`, `"no_frames"`,
    /// `"unhealthy"`.
    pub fn observe_unavailable(&self, camera_id: CameraId, reason: &str) {
        self.inner.write().insert(
            camera_id,
            AnalysisStreamStatus {
                mode: "mainstream".to_string(),
                state: "unavailable".to_string(),
                reason: Some(reason.to_string()),
                width: 0,
                height: 0,
                fps: 0.0,
            },
        );
    }

    pub fn clear(&self, camera_id: CameraId) {
        self.inner.write().remove(&camera_id);
    }

    #[must_use]
    pub fn snapshot(&self, camera_id: CameraId) -> Option<AnalysisStreamStatus> {
        self.inner.read().get(&camera_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_frame_increments_counter_and_records_dims() {
        let reg = FrameStatsRegistry::new();
        reg.observe_frame(1, Utc::now(), 960, 540);
        let s = reg.snapshot(1).unwrap();
        assert_eq!(s.frames_emitted, 1);
        assert_eq!(s.source_width, 960);
        assert_eq!(s.source_height, 540);
        // EMA stays 0 until the second frame.
        assert_eq!(s.fps_ema, 0.0);
    }

    #[test]
    fn second_frame_seeds_fps_ema() {
        let reg = FrameStatsRegistry::new();
        let t = Utc::now();
        reg.observe_frame(1, t, 960, 540);
        std::thread::sleep(std::time::Duration::from_millis(20));
        reg.observe_frame(1, t, 960, 540);
        let s = reg.snapshot(1).unwrap();
        assert!(s.fps_ema > 0.0, "fps_ema should be positive after 2 frames");
        assert_eq!(s.frames_emitted, 2);
    }

    /// Regression: bursty arrivals (gate drain, queue flush) used to
    /// inflate `fps_ema` past 1000 because the EMA observed
    /// microsecond inter-frame deltas. The sliding window divides by
    /// real wall-clock span, so the answer must stay near the true
    /// arrival rate regardless of intra-batch spacing.
    #[test]
    fn bursty_arrivals_do_not_inflate_fps() {
        let reg = FrameStatsRegistry::new();
        let t = Utc::now();
        // 10 frames in <1 ms (tight loop), then sleep so the span the
        // window measures is dominated by the real wall-clock gap.
        for _ in 0..10 {
            reg.observe_frame(1, t, 320, 240);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        reg.observe_frame(1, t, 320, 240);
        let s = reg.snapshot(1).unwrap();
        // Eleven frames over ~50 ms => roughly 200 fps. The exact
        // value is timing-sensitive; what matters is that the prior
        // implementation (instant-fps EMA) would report 5000+.
        assert!(
            s.fps_ema < 600.0,
            "fps should not blow up under bursty arrivals (got {})",
            s.fps_ema
        );
        assert!(s.fps_ema > 0.0);
    }

    #[test]
    fn dropped_frames_do_not_count_emitted() {
        let reg = FrameStatsRegistry::new();
        reg.observe_frame(1, Utc::now(), 320, 240);
        reg.observe_dropped(1);
        reg.observe_dropped(1);
        let s = reg.snapshot(1).unwrap();
        assert_eq!(s.frames_emitted, 1);
        assert_eq!(s.frames_dropped, 2);
    }

    #[test]
    fn clear_resets_camera() {
        let reg = FrameStatsRegistry::new();
        reg.observe_frame(1, Utc::now(), 640, 480);
        reg.clear(1);
        assert!(reg.snapshot(1).is_none());
    }

    #[test]
    fn snapshot_all_returns_one_entry_per_camera() {
        let reg = FrameStatsRegistry::new();
        reg.observe_frame(1, Utc::now(), 320, 240);
        reg.observe_frame(2, Utc::now(), 640, 480);
        let all = reg.snapshot_all();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn last_frame_age_ms_is_non_negative() {
        let reg = FrameStatsRegistry::new();
        let t = Utc::now() - chrono::Duration::milliseconds(500);
        reg.observe_frame(1, t, 16, 16);
        let s = reg.snapshot(1).unwrap();
        let age = s.last_frame_age_ms(Utc::now()).unwrap();
        assert!(age >= 500);
    }

    #[test]
    fn tile_counters_default_to_zero() {
        let reg = FrameStatsRegistry::new();
        reg.observe_frame(1, Utc::now(), 960, 540);
        let s = reg.snapshot(1).unwrap();
        assert_eq!(s.tile_invocations, 0);
        assert_eq!(s.tile_detections_added, 0);
        assert_eq!(s.tile_inference_ms_total, 0);
    }

    /// Each `overrun` on the decoder-input queue is one leaked access unit,
    /// and the count is per camera — a busy camera's leaks must not be
    /// attributed to a quiet one sharing the box.
    #[test]
    fn decoder_input_drops_accumulate_per_camera() {
        let reg = DecodeHealthRegistry::new();
        reg.observe_decoder_input_drop(1);
        reg.observe_decoder_input_drop(1);
        reg.observe_decoder_input_drop(2);
        assert_eq!(reg.snapshot(1).unwrap().decoder_input_drops, 2);
        assert_eq!(reg.snapshot(2).unwrap().decoder_input_drops, 1);
        assert!(reg.snapshot(3).is_none());
    }

    /// `FrameLoopDetector::stats()` returns running totals for the *current
    /// session*, so publishing them must overwrite, not add. Adding would
    /// make every session rebuild double-count and the duplicate rate climb
    /// on its own, which is precisely the kind of self-confirming metric
    /// this instrumentation exists to avoid (BUG-071).
    #[test]
    fn loop_stats_overwrite_rather_than_accumulate() {
        let reg = DecodeHealthRegistry::new();
        reg.observe_loop_stats(1, 90, 30);
        reg.observe_loop_stats(1, 180, 44);
        let s = reg.snapshot(1).unwrap();
        assert_eq!(s.sampled_frames, 180);
        assert_eq!(s.duplicate_frames, 44);
    }

    /// A leak counter and a loop counter for the same camera are independent
    /// writers — the ingester calls them from different callbacks — so
    /// neither may clobber the other's field.
    #[test]
    fn leak_and_loop_counters_are_independent() {
        let reg = DecodeHealthRegistry::new();
        reg.observe_decoder_input_drop(4);
        reg.observe_loop_stats(4, 100, 5);
        reg.observe_decoder_input_drop(4);
        reg.observe_decoder_output(4);
        let s = reg.snapshot(4).unwrap();
        assert_eq!(s.decoder_input_drops, 2);
        assert_eq!(s.decoder_output_frames, 1);
        assert_eq!(s.sampled_frames, 100);
        assert_eq!(s.duplicate_frames, 5);
    }

    /// The gap between what the decoder produced and what the appsink saw is
    /// the whole point of separating these: `videorate` pads a starved
    /// decoder back up to the requested framerate, so a count taken at the
    /// appsink reads nominal no matter how little the hardware managed.
    /// Measured on the 53-camera box: ~7 fps out of the decoder presented as
    /// a flat 15.1 fps at the appsink.
    #[test]
    fn decoder_output_is_counted_separately_from_appsink_deliveries() {
        let reg = DecodeHealthRegistry::new();
        for _ in 0..7 {
            reg.observe_decoder_output(9);
        }
        reg.observe_loop_stats(9, 15, 0);
        let s = reg.snapshot(9).unwrap();
        assert_eq!(s.decoder_output_frames, 7, "true decode rate");
        assert_eq!(s.sampled_frames, 15, "post-videorate delivery rate");
    }

    /// SPEC-069 Phase 1 — a single decoder-output event has no span to
    /// divide by, so the windowed rate must stay 0 rather than divide by
    /// zero or report a bogus instantaneous spike.
    #[test]
    fn decoder_output_fps_is_zero_after_one_sample() {
        let reg = DecodeHealthRegistry::new();
        reg.observe_decoder_output(1);
        let s = reg.snapshot(1).unwrap();
        assert_eq!(s.decoder_output_fps, 0.0);
    }

    /// Two decoder-output events ~50ms apart should read close to 20fps —
    /// this is the "how much is it decoding *right now*" number ceiling
    /// calibration needs, as opposed to the cumulative
    /// `decoder_output_frames` counter (which only answers "since the
    /// supervisor started").
    #[test]
    fn decoder_output_fps_tracks_recent_arrival_rate() {
        let reg = DecodeHealthRegistry::new();
        reg.observe_decoder_output(1);
        std::thread::sleep(std::time::Duration::from_millis(50));
        reg.observe_decoder_output(1);
        let s = reg.snapshot(1).unwrap();
        assert!(
            s.decoder_output_fps > 5.0 && s.decoder_output_fps < 60.0,
            "expected roughly 20fps, got {}",
            s.decoder_output_fps
        );
    }

    /// `observe_loop_stats` fires once per appsink delivery, so the rate is
    /// the call rate over [`RATE_WINDOW`] — the cumulative counter it also
    /// carries is not what the fps is derived from.
    #[test]
    fn sampled_fps_tracks_the_appsink_delivery_rate() {
        let reg = DecodeHealthRegistry::new();
        reg.observe_loop_stats(1, 1, 0);
        std::thread::sleep(std::time::Duration::from_millis(50));
        reg.observe_loop_stats(1, 2, 0);
        let s = reg.snapshot(1).unwrap();
        assert!(
            s.sampled_fps > 5.0 && s.sampled_fps < 60.0,
            "expected roughly 20fps, got {}",
            s.sampled_fps
        );
    }

    /// BUG-174 — `decode_verdict` divides `decoder_output_fps` by
    /// `sampled_fps` to size the padding gap, which is only meaningful if
    /// both are measured over the same window. `sampled_fps` used to be
    /// `1/Δt` for the single most recent inter-frame gap while
    /// `decoder_output_fps` was a [`RATE_WINDOW`] mean, so one jittery
    /// delivery moved the ratio far enough to flip the verdict. Feeding both
    /// counters at the same cadence must leave the ratio at ~1.
    #[test]
    fn sampled_and_decoder_output_rates_share_a_window_so_their_ratio_is_stable() {
        let reg = DecodeHealthRegistry::new();
        for i in 1..=6 {
            reg.observe_decoder_output(1);
            reg.observe_loop_stats(1, i, 0);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // One late delivery: the old single-interval estimator turned this
        // into a large sampled_fps swing all on its own.
        std::thread::sleep(std::time::Duration::from_millis(40));
        reg.observe_decoder_output(1);
        reg.observe_loop_stats(1, 7, 0);

        let s = reg.snapshot(1).unwrap();
        let ratio = s.decoder_output_fps / s.sampled_fps;
        assert!(
            (ratio - 1.0).abs() < 0.05,
            "rates must track each other: output {} / sampled {} = {ratio}",
            s.decoder_output_fps,
            s.sampled_fps
        );
    }

    /// A session rebuild restarts `observe_loop_stats` from a lower
    /// cumulative total. The rate is a call-rate now, so a counter reset
    /// cannot disturb it — two calls inside the window still yield a positive
    /// rate — while the cumulative field follows the counter down.
    #[test]
    fn a_counter_that_goes_backwards_does_not_disturb_the_rate() {
        let reg = DecodeHealthRegistry::new();
        reg.observe_loop_stats(1, 500, 10);
        std::thread::sleep(std::time::Duration::from_millis(20));
        reg.observe_loop_stats(1, 20, 1); // session rebuilt, counter reset
        let s = reg.snapshot(1).unwrap();
        assert!(
            s.sampled_fps > 0.0,
            "a counter reset must not zero the rate, got {}",
            s.sampled_fps
        );
        assert_eq!(s.sampled_frames, 20);
    }

    /// SPEC-069 Phase 1 — geometry read off the decoder's own src pad.
    #[test]
    fn decoder_geometry_is_recorded_per_camera() {
        let reg = DecodeHealthRegistry::new();
        reg.observe_decoder_geometry(1, 1920, 1080);
        reg.observe_decoder_geometry(2, 640, 360);
        assert_eq!(
            (
                reg.snapshot(1).unwrap().decoder_width,
                reg.snapshot(1).unwrap().decoder_height
            ),
            (1920, 1080)
        );
        assert_eq!(
            (
                reg.snapshot(2).unwrap().decoder_width,
                reg.snapshot(2).unwrap().decoder_height
            ),
            (640, 360)
        );
    }

    /// The API serves this on every stats request, including for a camera
    /// whose tap has not produced a frame yet. A naive divide would panic.
    #[test]
    fn duplicate_per_mille_handles_a_zero_denominator() {
        assert_eq!(DecodeHealth::default().duplicate_per_mille(), 0);
        let one_in_three = DecodeHealth {
            decoder_input_drops: 0,
            decoder_output_frames: 0,
            sampled_frames: 90,
            duplicate_frames: 30,
            ..Default::default()
        };
        assert_eq!(one_in_three.duplicate_per_mille(), 333);
    }

    #[test]
    fn clearing_decode_health_resets_only_that_camera() {
        let reg = DecodeHealthRegistry::new();
        reg.observe_decoder_input_drop(1);
        reg.observe_decoder_input_drop(2);
        reg.clear(1);
        assert!(reg.snapshot(1).is_none());
        assert_eq!(reg.snapshot(2).unwrap().decoder_input_drops, 1);
    }

    /// SPEC-069 Phase 1 (P3) — a camera with no substream configured must
    /// read `mainstream`/`active` by design, never `unavailable`: absence
    /// of a substream is normal, not a failure.
    #[test]
    fn mainstream_by_design_is_active_not_unavailable() {
        let reg = AnalysisStreamRegistry::new();
        reg.observe_mainstream_by_design(1);
        let s = reg.snapshot(1).unwrap();
        assert_eq!(s.mode, "mainstream");
        assert_eq!(s.state, "active");
        assert!(s.reason.is_none());
    }

    #[test]
    fn probing_reports_substream_with_no_verdict_yet() {
        let reg = AnalysisStreamRegistry::new();
        reg.observe_probing(1);
        let s = reg.snapshot(1).unwrap();
        assert_eq!(s.mode, "substream");
        assert_eq!(s.state, "probing");
    }

    #[test]
    fn active_substream_carries_geometry_and_fps() {
        let reg = AnalysisStreamRegistry::new();
        reg.observe_active(1, 640, 360, 14.8);
        let s = reg.snapshot(1).unwrap();
        assert_eq!(s.mode, "substream");
        assert_eq!(s.state, "active");
        assert_eq!((s.width, s.height), (640, 360));
        assert!((s.fps - 14.8).abs() < 0.01);
    }

    /// A fallback must fall through to `mainstream` (that's what the
    /// source actually reads afterwards) with `unavailable` + a reason —
    /// this is the state `decode_verdict`'s `substream_unavailable` derives
    /// from.
    #[test]
    fn unavailable_falls_back_to_mainstream_with_a_reason() {
        let reg = AnalysisStreamRegistry::new();
        reg.observe_unavailable(1, "unhealthy");
        let s = reg.snapshot(1).unwrap();
        assert_eq!(s.mode, "mainstream");
        assert_eq!(s.state, "unavailable");
        assert_eq!(s.reason.as_deref(), Some("unhealthy"));
    }

    #[test]
    fn analysis_stream_clear_resets_only_that_camera() {
        let reg = AnalysisStreamRegistry::new();
        reg.observe_active(1, 640, 360, 15.0);
        reg.observe_active(2, 640, 360, 15.0);
        reg.clear(1);
        assert!(reg.snapshot(1).is_none());
        assert!(reg.snapshot(2).is_some());
    }

    #[test]
    fn observe_tile_invocation_increments_counters() {
        let reg = FrameStatsRegistry::new();
        reg.observe_frame(1, Utc::now(), 960, 540);
        reg.observe_tile_invocation(1, 7, 12);
        reg.observe_tile_invocation(1, 3, 8);
        let s = reg.snapshot(1).unwrap();
        assert_eq!(s.tile_invocations, 2);
        assert_eq!(s.tile_detections_added, 10);
        assert_eq!(s.tile_inference_ms_total, 20);
    }

    #[test]
    fn observe_tile_invocation_without_entry_is_noop() {
        let reg = FrameStatsRegistry::new();
        reg.observe_tile_invocation(42, 5, 4);
        assert!(reg.snapshot(42).is_none());
    }

    #[test]
    fn clear_resets_tile_counters() {
        let reg = FrameStatsRegistry::new();
        reg.observe_frame(1, Utc::now(), 960, 540);
        reg.observe_tile_invocation(1, 4, 6);
        reg.clear(1);
        reg.observe_frame(1, Utc::now(), 960, 540);
        let s = reg.snapshot(1).unwrap();
        assert_eq!(s.tile_invocations, 0);
        assert_eq!(s.tile_detections_added, 0);
        assert_eq!(s.tile_inference_ms_total, 0);
    }

    /// Frame-source liveness (`CameraFrameStats::is_online`) must not depend
    /// on any live-view subscriber — this is the signal that also covers a
    /// camera nobody is currently watching, unlike `live_view::stalled_cameras`.
    #[test]
    fn camera_with_recent_frame_is_online_with_no_live_view_subscriber() {
        let reg = FrameStatsRegistry::new();
        let now = Utc::now();
        reg.observe_frame(1, now, 960, 540);
        let s = reg.snapshot(1).unwrap();
        assert!(s.is_online(now));
    }

    /// A camera that stops producing frames must flip to offline once its
    /// last frame ages past [`CAMERA_OFFLINE_AFTER_MS`].
    #[test]
    fn camera_flips_offline_after_frames_stop() {
        let reg = FrameStatsRegistry::new();
        let last_frame = Utc::now();
        reg.observe_frame(1, last_frame, 960, 540);
        let s = reg.snapshot(1).unwrap();
        assert!(s.is_online(last_frame), "just observed a frame");
        let long_after = last_frame + chrono::Duration::milliseconds(CAMERA_OFFLINE_AFTER_MS + 1);
        assert!(
            !s.is_online(long_after),
            "no frame in over CAMERA_OFFLINE_AFTER_MS must report offline"
        );
    }

    /// A camera that has never produced a frame (never spawned, or spawned
    /// but not yet decoding) is offline, not merely absent from the map.
    #[test]
    fn camera_never_seen_is_offline() {
        let reg = FrameStatsRegistry::new();
        assert!(reg.snapshot(99).is_none());
    }
}

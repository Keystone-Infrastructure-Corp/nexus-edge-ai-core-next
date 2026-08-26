//! Phase 10 Live View — the edge LBR (Low-Bit-Rate) snapshot pump + manager.
//!
//! The always-on default transport for the cloud live wall: every grid cell
//! paints from this. The cloud `LiveHub` ref-counts browser subscribers and
//! sends exactly one `lbr_subscribe` per `(core, camera)` down the tunnel
//! (re-sent only when the tile size or fps tier changes) and one
//! `lbr_unsubscribe` when the last viewer leaves. The edge runs **one encode
//! task per camera** regardless of viewer count — the cloud fans that single
//! JPEG stream out to every viewer, so multiple operators never multiply
//! edge cost (encode-once fan-out).
//!
//! Governing principle (PHASE_10_LIVE_VIEW.md §5): spend the operator's
//! browser CPU generously and the edge's CPU stingily — only when the scene
//! actually changes. Concretely:
//!
//! * **Sample, never re-decode.** The pump reads the already-decoded
//!   supervisor frame straight out of [`LatestFrameCache`]; it never opens a
//!   second capture/decode, and it skips ticks where the pipeline has not
//!   produced a new `frame_id`.
//! * **Adaptive fps gated on the existing tracked-object set** — the
//!   confirmed reuse of the detector's output, no new analysis. A static
//!   scene emits a ~1 fps keepalive; a changing scene bursts up to the tier
//!   ceiling (`grid` ~4 fps, `focus` ~8 fps).
//! * **Per-core encode budget** (the hard backstop that yields to
//!   inference): a shared token bucket caps total encodes/sec across every
//!   camera, so many simultaneously-moving cameras degrade each other's fps
//!   round-robin rather than starving the detection pipeline.
//! * **Clean, tile-sized frames.** The emitted JPEG is the clean supervisor
//!   frame (no detection overlays) resized down to the browser's on-screen
//!   tile — never upscaled past native.
//!
//! LBR rides the existing WSS tunnel via [`TunnelOutbox`]; it never touches
//! WebRTC / coturn. It is fire-and-forget and fail-open: a send error just
//! means the tunnel is momentarily down and the next snapshot supersedes the
//! dropped one.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use image::ImageEncoder;
use nexus_cloud_client::{build_lbr_frame_envelope, TunnelOutbox};
use nexus_cloud_protocol::v1::{LbrSubscribePayload, LbrUnsubscribePayload};
use nexus_pipeline::LatestFrameCache;
use nexus_types::{CameraId, Frame, PixelFormat, TrackedObject};
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// JPEG quality for LBR snapshots. PHASE_10_LIVE_VIEW.md §5 calls for
/// q70–75; 72 is the middle of that band.
const LBR_JPEG_QUALITY: u8 = 72;

/// Keepalive cadence: even a perfectly static scene emits one frame per
/// second so a freshly-subscribed cell paints promptly and client-side
/// stale-frame detection stays simple.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// A subscribed camera that has produced no new `frame_id` for this long is
/// reported as stalled. Generous relative to the slowest tier (`GRID_FPS`)
/// so a merely slow detector never trips it; a genuinely dead pipeline is
/// silent for minutes, not seconds.
pub(crate) const STALL_AFTER: Duration = Duration::from_secs(5);
/// Adaptive-fps ceiling for a normal grid cell.
const GRID_FPS: u32 = 4;
/// Adaptive-fps ceiling for the hovered / selected ("focus") cell — a
/// warmer preview before the operator decides to expand to HD.
const FOCUS_FPS: u32 = 8;

/// Per-core global LBR encode budget (encodes/sec summed across every
/// engaged camera). The hard backstop that makes LBR yield to inference —
/// deliberately conservative; tuned per hardware profile later (lower-power
/// boxes tighter). Encode (resize + JPEG) is the expensive step the budget
/// governs; the per-tick "did the scene change?" check is cheap + unbounded.
const DEFAULT_BUDGET_PER_SEC: u32 = 24;

/// How many recently-seen frame content hashes each pump remembers when
/// looking for a repeating cycle. Field-observed periods were 2–6; eight
/// gives headroom without making the scan meaningful work.
const LOOP_WINDOW: usize = 8;

/// How many recent samples the log decision is evaluated over. The pump
/// counts cycle hits *within* this window rather than requiring an unbroken
/// run: one genuinely fresh frame slipping through a recycled pool used to
/// reset the counter, which kept a persistent loop permanently unreported.
///
/// Note the window counts *new-frame* observations, not pump ticks, so it
/// spans `LOOP_EVAL_WINDOW / cache_update_rate` seconds — roughly 25 s at
/// the gate's 2 fps LBR baseline, not the 12 s a naive read against
/// [`GRID_FPS`] would suggest.
const LOOP_EVAL_WINDOW: usize = 48;

/// Cycle hits within [`LOOP_EVAL_WINDOW`] before the pump logs. Keeps an
/// incidental repeat quiet while a sustained loop gets reported exactly
/// once per episode.
const LOOP_LOG_TRIP: u32 = 12;

/// Hash of the decoded frame's pixels.
///
/// Deliberately over the *frame* rather than the encoded JPEG: `encode_lbr`
/// is deterministic, so identical pixels imply an identical payload, and
/// hashing first lets a looping camera skip the resize+JPEG entirely
/// (saving both the CPU and the budget token the encode would have spent).
fn frame_content_hash(frame: &Frame) -> u64 {
    let mut h = DefaultHasher::new();
    frame.data.hash(&mut h);
    h.finish()
}

/// Shared token bucket capping total LBR encodes/sec across all pumps.
struct LbrBudget {
    max_per_sec: u32,
    window: Mutex<BudgetWindow>,
}

struct BudgetWindow {
    started: Instant,
    used: u32,
}

impl LbrBudget {
    fn new(max_per_sec: u32) -> Self {
        Self {
            max_per_sec,
            window: Mutex::new(BudgetWindow {
                started: Instant::now(),
                used: 0,
            }),
        }
    }

    /// Claim one encode token for the current 1-second window. Returns
    /// `false` when the budget is exhausted — the caller skips this tick
    /// (round-robin degradation under load).
    fn try_acquire(&self) -> bool {
        let mut w = self.window.lock();
        if w.started.elapsed() >= Duration::from_secs(1) {
            w.started = Instant::now();
            w.used = 0;
        }
        if w.used < self.max_per_sec {
            w.used += 1;
            true
        } else {
            false
        }
    }
}

/// Whether this tick should encode and send.
///
/// The shared budget governs **motion bursts only**; the 1 fps keepalive
/// bypasses it. A single bucket shared across `N` cameras cannot satisfy `N`
/// keepalives once `N` exceeds [`DEFAULT_BUDGET_PER_SEC`], and because
/// [`LbrBudget::try_acquire`] is first-come-first-served rather than
/// round-robin — each pump sleeps a fixed `sample_interval`, so tick phases
/// are stable — the cells that lose the race lose it permanently rather than
/// degrading evenly. A 53-camera wall against a budget of 24 therefore leaves
/// ~29 cells frozen indefinitely.
///
/// Bypassing is bounded: the keepalive is at most one encode per camera per
/// second, so worst-case load is `N + max_per_sec` encodes/sec, and an encode
/// is a small-tile resize + JPEG, not a decode.
fn should_emit(
    new_frame: bool,
    scene_changed: bool,
    keepalive_due: bool,
    stale_dup: bool,
    budget: &LbrBudget,
) -> bool {
    if !new_frame || stale_dup || !(scene_changed || keepalive_due) {
        return false;
    }
    keepalive_due || budget.try_acquire()
}

/// Live-tunable per-camera pump parameters. A re-`lbr_subscribe` (sent by
/// the cloud when the tile size or fps tier changes) updates these in place
/// so the running pump retargets without a restart / frame gap.
#[derive(Clone, Copy)]
struct PumpParams {
    /// On-screen tile width in device px; drives the resize (height is
    /// derived from the native aspect so a slightly-off tile never
    /// distorts). `None` = encode at native supervisor resolution.
    tile_w: Option<u32>,
    /// Adaptive-fps ceiling for this camera (grid vs focus tier).
    ceiling_fps: u32,
}

struct PumpEntry {
    params: Arc<Mutex<PumpParams>>,
    state: Arc<Mutex<PumpState>>,
    task: JoinHandle<()>,
}

/// What a running pump is currently doing. Written by the pump task, read by
/// the admin `live/status` surface and the heartbeat health roll-up.
///
/// This exists because the pump's normal failure mode is *silence*: it only
/// emits on a new `frame_id`, so a stalled source produces no frames, no
/// keepalive, and no log after the first one. Silence is indistinguishable
/// from a quiet scene, which is why BUG-057 took a live box to diagnose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PumpState {
    /// No new `frame_id` for at least [`STALL_AFTER`] — the source is dead
    /// or wedged, and whatever the wall is showing for this cell is stale.
    stalled: bool,
    /// Decoded frames are repeating on a fixed cycle, so sends are being
    /// suppressed. See the loop guard in [`spawn_pump`].
    suppressed: bool,
}

/// One camera's pump state, flattened for the admin surface.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct LivePumpStatus {
    pub camera_id: CameraId,
    pub ceiling_fps: u32,
    pub tile_w: Option<u32>,
    pub stalled: bool,
    pub suppressed: bool,
}

/// Owns every running LBR pump. Constructed once at engine boot and shared
/// with the tunnel supervisor's inbound dispatch, which drives it from the
/// `lbr_subscribe` / `lbr_unsubscribe` envelopes.
pub struct LiveViewManager {
    cache: Arc<LatestFrameCache>,
    outbox: Arc<TunnelOutbox>,
    budget: Arc<LbrBudget>,
    pumps: Mutex<HashMap<CameraId, PumpEntry>>,
}

impl LiveViewManager {
    /// Build the manager over the engine's frame cache + cloud outbox.
    #[must_use]
    pub fn new(cache: Arc<LatestFrameCache>, outbox: Arc<TunnelOutbox>) -> Arc<Self> {
        Arc::new(Self {
            cache,
            outbox,
            budget: Arc::new(LbrBudget::new(DEFAULT_BUDGET_PER_SEC)),
            pumps: Mutex::new(HashMap::new()),
        })
    }

    /// Handle an inbound `lbr_subscribe`: ensure exactly one pump is running
    /// for the camera, (re)setting its tile size + fps ceiling. Idempotent —
    /// a subscribe for an already-pumping camera just retargets it, so we
    /// never run more than one encode task per camera.
    pub fn on_subscribe(&self, payload: &LbrSubscribePayload) {
        let Ok(camera_id) = CameraId::try_from(payload.camera_id) else {
            warn!(
                camera_id = payload.camera_id,
                "lbr_subscribe: camera_id out of range; ignoring"
            );
            return;
        };
        let ceiling_fps = if payload.fps_tier.as_deref() == Some("focus") {
            FOCUS_FPS
        } else {
            GRID_FPS
        };
        let params = PumpParams {
            tile_w: payload.tile_w.and_then(|w| u32::try_from(w).ok()),
            ceiling_fps,
        };
        let mut pumps = self.pumps.lock();
        if let Some(entry) = pumps.get(&camera_id) {
            *entry.params.lock() = params; // retarget the running pump in place
            return;
        }
        let shared = Arc::new(Mutex::new(params));
        let state = Arc::new(Mutex::new(PumpState::default()));
        let task = spawn_pump(
            camera_id,
            payload.camera_id,
            self.cache.clone(),
            self.outbox.clone(),
            self.budget.clone(),
            shared.clone(),
            state.clone(),
        );
        pumps.insert(
            camera_id,
            PumpEntry {
                params: shared,
                state,
                task,
            },
        );
        debug!(camera_id, ceiling_fps, "LBR pump started");
    }

    /// Handle an inbound `lbr_unsubscribe`: stop the camera's pump. The
    /// aborted task stops within one sample interval (≤ ~1 s).
    pub fn on_unsubscribe(&self, payload: &LbrUnsubscribePayload) {
        let Ok(camera_id) = CameraId::try_from(payload.camera_id) else {
            return;
        };
        self.stop(camera_id);
    }

    /// Stop one camera's pump, whatever the reason. Called from
    /// [`Self::on_unsubscribe`] and from the camera lifecycle: a camera that
    /// has been stopped or deleted has no frames to pump, and leaving the
    /// task running just polls a cache entry that will never be refilled.
    pub fn stop(&self, camera_id: CameraId) {
        if let Some(entry) = self.pumps.lock().remove(&camera_id) {
            entry.task.abort();
            debug!(camera_id, "LBR pump stopped");
        }
    }

    /// Abort every pump. Called when the tunnel disconnects: the cloud
    /// `LiveHub` re-issues `lbr_subscribe` for still-active viewers on
    /// reconnect, so this bounds orphaned encode work to the connected
    /// window without leaking tasks.
    pub fn clear_all(&self) {
        let mut pumps = self.pumps.lock();
        let n = pumps.len();
        for (_, entry) in pumps.drain() {
            entry.task.abort();
        }
        if n > 0 {
            debug!(stopped = n, "LBR pumps cleared (tunnel down)");
        }
    }

    /// Number of cameras currently being pumped.
    #[must_use]
    pub fn active_pump_count(&self) -> usize {
        self.pumps.lock().len()
    }

    /// Per-camera pump state for `GET /api/v1/admin/live/status`, sorted by
    /// camera so the output is stable between polls.
    #[must_use]
    pub fn status(&self) -> Vec<LivePumpStatus> {
        let mut out: Vec<LivePumpStatus> = self
            .pumps
            .lock()
            .iter()
            .map(|(&camera_id, entry)| {
                let params = *entry.params.lock();
                let state = *entry.state.lock();
                LivePumpStatus {
                    camera_id,
                    ceiling_fps: params.ceiling_fps,
                    tile_w: params.tile_w,
                    stalled: state.stalled,
                    suppressed: state.suppressed,
                }
            })
            .collect();
        out.sort_unstable_by_key(|s| s.camera_id);
        out
    }

    /// Cameras whose source has stalled, for the heartbeat health roll-up.
    #[must_use]
    pub fn stalled_cameras(&self) -> Vec<CameraId> {
        let mut out: Vec<CameraId> = self
            .pumps
            .lock()
            .iter()
            .filter(|(_, entry)| entry.state.lock().stalled)
            .map(|(&camera_id, _)| camera_id)
            .collect();
        out.sort_unstable();
        out
    }
}

/// Spawn the single per-camera encode loop. Wakes at the tier cadence,
/// samples the frame cache, and emits a fresh `lbr_frame` only when there is
/// new content to send AND (the scene changed OR a keepalive is due) AND the
/// shared budget has a token spare.
fn spawn_pump(
    camera_id: CameraId,
    wire_camera_id: u64,
    cache: Arc<LatestFrameCache>,
    outbox: Arc<TunnelOutbox>,
    budget: Arc<LbrBudget>,
    params: Arc<Mutex<PumpParams>>,
    state: Arc<Mutex<PumpState>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // The runtime's clock, not the OS clock, so `tokio::time::pause()`
        // can drive the stall detector in tests without real waiting.
        use tokio::time::Instant;

        let mut last_frame_id: Option<u64> = None;
        let mut last_sig: u64 = 0;
        // Rolling window of recent frame content hashes + the current
        // run of suppressed cycle hits. See the loop-guard comment below.
        let mut recent: VecDeque<u64> = VecDeque::with_capacity(LOOP_WINDOW);
        let mut loop_outcomes: VecDeque<bool> = VecDeque::with_capacity(LOOP_EVAL_WINDOW);
        let mut loop_hits: u32 = 0;
        let mut loop_logged = false;
        // Stall clock. Advanced on every new `frame_id`; drives the stalled
        // flag when it goes quiet. Starts now so a camera that never
        // produces a first frame still trips after STALL_AFTER.
        let mut last_new_frame = Instant::now();
        let mut stalled = false;
        // Force the first available frame to emit immediately so a fresh
        // cell paints without waiting for motion or the keepalive tick.
        let mut last_emit = Instant::now()
            .checked_sub(KEEPALIVE_INTERVAL)
            .unwrap_or_else(Instant::now);
        loop {
            let (tile_w, ceiling_fps) = {
                let p = params.lock();
                (p.tile_w, p.ceiling_fps)
            };
            let sample_interval = Duration::from_millis(1000 / u64::from(ceiling_fps.max(1)));

            if let Some(entry) = cache.get(camera_id) {
                let frame_id = entry.frame.frame_id;
                let new_frame = last_frame_id != Some(frame_id);
                let sig = objects_signature(entry.objects.as_slice());
                let now = Instant::now();
                if new_frame {
                    last_new_frame = now;
                }
                let scene_changed = new_frame && sig != last_sig;
                let keepalive_due = now.duration_since(last_emit) >= KEEPALIVE_INTERVAL;

                // Loop guard. A decoder that re-serves already-delivered
                // surfaces produces a short rotating cycle of pixel-identical
                // frames while `frame_id` keeps advancing — which the checks
                // above cannot see, so the wall renders convincing "live"
                // video that is actually seconds stale (each tile arriving
                // with a fresh `now_unix_ms()` wire timestamp).
                //
                // Distance matters. A repeat at distance 1 is an unchanged
                // scene (H.265 skip blocks decode bit-identically) and is
                // legitimate — the keepalive path below exists for exactly
                // that. A repeat at distance >= 2 means content we already
                // sent came back *after other frames*, which live video
                // cannot do. Suppress those so the cloud stops replaying
                // them, but still honour the keepalive so the tile stays
                // alive (stale-but-1fps beats a dead cell, and beats a
                // fake-smooth loop).
                let cycle = if new_frame {
                    let h = frame_content_hash(&entry.frame);
                    let d = recent
                        .iter()
                        .rev()
                        .position(|&p| p == h)
                        .map(|i| i + 1)
                        .filter(|&d| d >= 2);
                    if recent.len() == LOOP_WINDOW {
                        recent.pop_front();
                    }
                    recent.push_back(h);
                    d
                } else {
                    None
                };

                if new_frame {
                    loop_outcomes.push_back(cycle.is_some());
                    if loop_outcomes.len() > LOOP_EVAL_WINDOW
                        && loop_outcomes.pop_front() == Some(true)
                    {
                        loop_hits = loop_hits.saturating_sub(1);
                    }
                }
                if let Some(period) = cycle {
                    loop_hits = loop_hits.saturating_add(1);
                    if loop_hits >= LOOP_LOG_TRIP && !loop_logged {
                        loop_logged = true;
                        warn!(
                            camera_id,
                            period,
                            hits = loop_hits,
                            "live view: decoded frames are repeating on a fixed cycle; \
                             suppressing duplicate lbr_frame sends (video is stale \
                             even though frame ids advance)"
                        );
                    }
                } else if new_frame && loop_logged && loop_hits == 0 {
                    // Only declare the episode over once the whole
                    // evaluation window has drained of cycle hits. Clearing
                    // on the first fresh frame re-armed the log on every
                    // gap in an ongoing loop. Closes at WARN because it
                    // opened at WARN — an episode that only ever announces
                    // its start reads as still-running forever.
                    warn!(camera_id, "live view: frame cycle cleared");
                    loop_logged = false;
                }
                let stale_dup = cycle.is_some() && !keepalive_due;

                // Encode only when there is a NEW frame to send and either
                // the scene changed or a keepalive is due. See
                // `should_emit`: the shared budget governs motion bursts,
                // while the keepalive bypasses it so a wall wider than the
                // budget cannot freeze the cells that lose the token race.
                if should_emit(new_frame, scene_changed, keepalive_due, stale_dup, &budget) {
                    match encode_lbr(entry.frame.as_ref(), tile_w) {
                        Ok(jpeg) => {
                            let env = build_lbr_frame_envelope(
                                wire_camera_id,
                                now_unix_ms(),
                                B64.encode(&jpeg),
                            );
                            if let Err(e) = outbox.send(env).await {
                                debug!(camera_id, error = %e, "lbr_frame dropped (tunnel down)");
                            }
                            last_emit = now;
                        }
                        Err(e) => warn!(camera_id, error = %e, "LBR encode failed; skipping tick"),
                    }
                }
                last_frame_id = Some(frame_id);
                last_sig = sig;
            }

            // Report the stall rather than falling silent. `should_emit`
            // bails on `!new_frame`, and the arm above does nothing at all
            // when the cache has no entry, so a dead source produces no
            // frames, no keepalive and no further log — the cloud goes on
            // rendering the last tile it received under a LIVE badge. This
            // is the only place that notices (BUG-057).
            let quiet_for = last_new_frame.elapsed();
            if (quiet_for >= STALL_AFTER) != stalled {
                stalled = !stalled;
                if stalled {
                    warn!(
                        camera_id,
                        quiet_for_s = quiet_for.as_secs(),
                        "live view: source has stopped producing frames; this camera's \
                         wall cell is stale"
                    );
                } else {
                    warn!(camera_id, "live view: source is producing frames again");
                }
            }
            {
                let mut s = state.lock();
                s.stalled = stalled;
                s.suppressed = loop_logged;
            }

            tokio::time::sleep(sample_interval).await;
        }
    })
}

/// Resize (down, never up) the clean supervisor frame to the tile and
/// JPEG-encode it.
fn encode_lbr(frame: &Frame, tile_w: Option<u32>) -> Result<Vec<u8>, String> {
    let rgb = match frame.format {
        PixelFormat::Rgb24 => frame.data.as_ref().clone(),
        PixelFormat::Bgr24 => bgr_to_rgb(frame.data.as_ref()),
        other => return Err(format!("unsupported pixel format {other:?}")),
    };
    let (tw, th) = target_size(frame.width, frame.height, tile_w);
    let mut out = Vec::new();
    if tw == frame.width && th == frame.height {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, LBR_JPEG_QUALITY)
            .write_image(
                &rgb,
                frame.width,
                frame.height,
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|e| e.to_string())?;
    } else {
        let img = image::RgbImage::from_raw(frame.width, frame.height, rgb)
            .ok_or_else(|| "frame buffer size mismatch".to_string())?;
        let resized = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, LBR_JPEG_QUALITY)
            .write_image(resized.as_raw(), tw, th, image::ExtendedColorType::Rgb8)
            .map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Target encode dims: clamp the requested tile width to the native frame
/// (LBR never upscales past the supervisor frame) and derive the height from
/// the native aspect ratio so a slightly-off `tile_w`/`tile_h` from the
/// browser never distorts the picture.
fn target_size(src_w: u32, src_h: u32, tile_w: Option<u32>) -> (u32, u32) {
    match tile_w {
        Some(tw) if tw >= 1 && tw < src_w && src_w > 0 => {
            let th = (u64::from(src_h) * u64::from(tw) / u64::from(src_w)).max(1);
            (tw, u32::try_from(th).unwrap_or(src_h))
        }
        _ => (src_w, src_h),
    }
}

/// Cheap FNV-1a signature of the tracked-object set for the adaptive-fps
/// motion gate. Reuses the detector's already-computed tracks (no new
/// analysis): a change in the object count, a track id, or any object's
/// coarse (8-px-bucketed) box centre reads as "scene changing" and bursts
/// the pump to its fps ceiling; an unchanged signature settles it to the
/// keepalive rate. Sub-pixel tracker jitter on a truly static object is
/// absorbed by the coarse bucketing.
fn objects_signature(objects: &[TrackedObject]) -> u64 {
    #[inline]
    fn mix(hash: u64, v: u64) -> u64 {
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        (hash ^ v).wrapping_mul(FNV_PRIME)
    }
    let mut hash = mix(0xcbf2_9ce4_8422_2325, objects.len() as u64);
    for o in objects {
        let cx = ((o.bbox.x1 + o.bbox.x2) * 0.5 / 8.0) as i64;
        let cy = ((o.bbox.y1 + o.bbox.y2) * 0.5 / 8.0) as i64;
        hash = mix(hash, o.track_id);
        hash = mix(hash, cx as u64);
        hash = mix(hash, cy as u64);
    }
    hash
}

/// Swap B and R channels in a packed 24-bit buffer (BGR → RGB). Mirrors the
/// snapshot helper in `api.rs`.
fn bgr_to_rgb(buf: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; buf.len()];
    for (i, chunk) in buf.chunks_exact(3).enumerate() {
        let off = i * 3;
        out[off] = chunk[2];
        out[off + 1] = chunk[1];
        out[off + 2] = chunk[0];
    }
    out
}

/// Edge capture wall-clock in unix ms (diagnostics / staleness only).
fn now_unix_ms() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_types::BBox;

    fn obj(track_id: u64, cx: f32, cy: f32) -> TrackedObject {
        TrackedObject {
            track_id,
            label: "person".to_string(),
            confidence: 0.9,
            bbox: BBox {
                x1: cx - 5.0,
                y1: cy - 5.0,
                x2: cx + 5.0,
                y2: cy + 5.0,
            },
            detection_bbox: None,
            age_frames: 3,
            age_ms: 100,
            attributes: serde_json::Map::new(),
        }
    }

    #[test]
    fn signature_stable_under_subpixel_jitter() {
        let a = vec![obj(1, 100.0, 100.0), obj(2, 200.0, 50.0)];
        // Jitter under the 8-px bucket must not read as motion.
        let b = vec![obj(1, 101.0, 100.5), obj(2, 200.5, 51.0)];
        assert_eq!(objects_signature(&a), objects_signature(&b));
    }

    #[test]
    fn signature_changes_on_motion() {
        let a = vec![obj(1, 100.0, 100.0)];
        let b = vec![obj(1, 140.0, 100.0)]; // moved > 1 bucket
        assert_ne!(objects_signature(&a), objects_signature(&b));
    }

    #[test]
    fn signature_changes_on_count() {
        let a = vec![obj(1, 100.0, 100.0)];
        let b = vec![obj(1, 100.0, 100.0), obj(2, 300.0, 300.0)];
        assert_ne!(objects_signature(&a), objects_signature(&b));
    }

    #[test]
    fn signature_empty_is_stable() {
        assert_eq!(objects_signature(&[]), objects_signature(&[]));
    }

    #[test]
    fn target_size_no_hint_is_native() {
        assert_eq!(target_size(960, 540, None), (960, 540));
    }

    #[test]
    fn target_size_never_upscales() {
        assert_eq!(target_size(640, 360, Some(1280)), (640, 360));
    }

    #[test]
    fn target_size_downscales_preserving_aspect() {
        // 960×540 (16:9) → width 480 → height derived 270.
        assert_eq!(target_size(960, 540, Some(480)), (480, 270));
    }

    #[test]
    fn budget_caps_encodes_per_window() {
        let b = LbrBudget::new(3);
        assert!(b.try_acquire());
        assert!(b.try_acquire());
        assert!(b.try_acquire());
        assert!(!b.try_acquire()); // 4th in the same window is denied
    }

    /// The live-wall starvation this bypass exists for: 53 engaged cameras
    /// against the default budget of 24. Every cell must still get its 1 fps
    /// keepalive, or ~29 of them sit frozen on whatever they last sent.
    #[test]
    fn keepalive_survives_a_wall_wider_than_the_budget() {
        let b = LbrBudget::new(DEFAULT_BUDGET_PER_SEC);
        let emitted = (0..53)
            .filter(|_| should_emit(true, false, true, false, &b))
            .count();
        assert_eq!(
            emitted, 53,
            "every engaged camera keeps its cell alive regardless of budget"
        );
    }

    /// The budget must still bite on motion, which is what protects
    /// inference — the bypass is for the keepalive floor only.
    #[test]
    fn motion_bursts_still_yield_to_the_budget() {
        let b = LbrBudget::new(24);
        let emitted = (0..53)
            .filter(|_| should_emit(true, true, false, false, &b))
            .count();
        assert_eq!(emitted, 24, "motion-driven encodes stay capped");
    }

    #[test]
    fn a_stale_duplicate_is_never_emitted() {
        let b = LbrBudget::new(24);
        assert!(!should_emit(true, true, true, true, &b));
        assert!(b.try_acquire(), "and it spent no token doing so");
    }

    #[test]
    fn no_new_frame_means_no_send() {
        let b = LbrBudget::new(24);
        assert!(!should_emit(false, true, true, false, &b));
    }

    fn rgb_frame(w: u32, h: u32) -> Frame {
        Frame {
            camera_id: 1,
            frame_id: 1,
            captured_at: chrono::Utc::now(),
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![120u8; w as usize * h as usize * 3]),
            trace_id: "t".to_string(),
        }
    }

    #[test]
    fn encode_native_produces_jpeg() {
        let jpeg = encode_lbr(&rgb_frame(64, 36), None).expect("encode");
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]); // JPEG SOI marker
    }

    #[test]
    fn encode_resize_path_produces_jpeg() {
        let jpeg = encode_lbr(&rgb_frame(96, 54), Some(48)).expect("encode");
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn bgr_to_rgb_swaps_channels() {
        // One pixel BGR (10, 20, 30) → RGB (30, 20, 10).
        assert_eq!(bgr_to_rgb(&[10, 20, 30]), vec![30, 20, 10]);
    }

    #[tokio::test]
    async fn manager_subscribe_is_idempotent_and_unsubscribe_stops() {
        let cache = Arc::new(LatestFrameCache::new());
        let outbox = Arc::new(TunnelOutbox::new());
        let mgr = LiveViewManager::new(cache, outbox);
        assert_eq!(mgr.active_pump_count(), 0);

        mgr.on_subscribe(&LbrSubscribePayload {
            camera_id: 1,
            tile_w: Some(320),
            tile_h: Some(180),
            fps_tier: Some("grid".to_string()),
        });
        assert_eq!(mgr.active_pump_count(), 1);

        // Re-subscribe (retarget to focus) must NOT spawn a second pump.
        mgr.on_subscribe(&LbrSubscribePayload {
            camera_id: 1,
            tile_w: Some(640),
            tile_h: Some(360),
            fps_tier: Some("focus".to_string()),
        });
        assert_eq!(mgr.active_pump_count(), 1);

        mgr.on_unsubscribe(&LbrUnsubscribePayload { camera_id: 1 });
        assert_eq!(mgr.active_pump_count(), 0);
    }

    fn test_frame(camera_id: CameraId, frame_id: u64) -> Arc<Frame> {
        frame_with_fill(camera_id, frame_id, 0)
    }

    fn frame_with_fill(camera_id: CameraId, frame_id: u64, fill: u8) -> Arc<Frame> {
        Arc::new(Frame {
            camera_id,
            frame_id,
            captured_at: chrono::Utc::now(),
            width: 16,
            height: 16,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![fill; 16 * 16 * 3]),
            trace_id: "t".into(),
        })
    }

    fn subscribe(mgr: &LiveViewManager, camera_id: u64) {
        mgr.on_subscribe(&LbrSubscribePayload {
            camera_id,
            tile_w: Some(320),
            tile_h: Some(180),
            fps_tier: Some("grid".to_string()),
        });
    }

    /// The BUG-057 invariant: a source that stops producing frames must be
    /// *reported*, not silently ignored. `should_emit` bails on `!new_frame`,
    /// so without this the pump emits nothing and nothing says why.
    #[tokio::test(start_paused = true)]
    async fn a_source_that_stops_producing_frames_is_reported_as_stalled() {
        let cache = Arc::new(LatestFrameCache::new());
        let mgr = LiveViewManager::new(cache.clone(), Arc::new(TunnelOutbox::new()));
        let epoch = cache.begin_session(7);
        cache.put(7, epoch, test_frame(7, 1), Arc::new(vec![]));
        subscribe(&mgr, 7);

        tokio::time::sleep(STALL_AFTER / 2).await;
        assert!(!mgr.status()[0].stalled, "not stalled before STALL_AFTER");
        assert!(mgr.stalled_cameras().is_empty());

        // frame_id never advances again.
        tokio::time::sleep(STALL_AFTER).await;
        assert!(
            mgr.status()[0].stalled,
            "stalled once the source goes quiet"
        );
        assert_eq!(mgr.stalled_cameras(), vec![7]);
    }

    /// A camera with no cache entry at all takes the same path — the pump's
    /// `if let Some(entry)` arm never runs, which used to mean the tick did
    /// nothing and left no trace.
    #[tokio::test(start_paused = true)]
    async fn a_camera_that_never_produced_a_frame_is_reported_as_stalled() {
        let cache = Arc::new(LatestFrameCache::new());
        let mgr = LiveViewManager::new(cache, Arc::new(TunnelOutbox::new()));
        subscribe(&mgr, 9);

        tokio::time::sleep(STALL_AFTER * 2).await;
        assert_eq!(mgr.stalled_cameras(), vec![9]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_recovered_source_clears_the_stall() {
        let cache = Arc::new(LatestFrameCache::new());
        let mgr = LiveViewManager::new(cache.clone(), Arc::new(TunnelOutbox::new()));
        let epoch = cache.begin_session(3);
        cache.put(3, epoch, test_frame(3, 1), Arc::new(vec![]));
        subscribe(&mgr, 3);

        tokio::time::sleep(STALL_AFTER * 2).await;
        assert_eq!(mgr.stalled_cameras(), vec![3]);

        cache.put(3, epoch, test_frame(3, 2), Arc::new(vec![]));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            mgr.stalled_cameras().is_empty(),
            "a fresh frame_id clears the stall"
        );
    }

    /// The lifecycle gap: stopping or deleting a camera must reap its pump.
    /// Previously only `lbr_unsubscribe` or a tunnel drop could, so a stopped
    /// camera left a task polling a cache entry that would never be refilled.
    #[tokio::test]
    async fn stopping_a_camera_reaps_its_pump() {
        let mgr = LiveViewManager::new(
            Arc::new(LatestFrameCache::new()),
            Arc::new(TunnelOutbox::new()),
        );
        subscribe(&mgr, 4);
        assert_eq!(mgr.active_pump_count(), 1);

        mgr.stop(4);
        assert_eq!(mgr.active_pump_count(), 0);
        assert!(mgr.status().is_empty());
    }

    /// A decoder re-serving already-delivered surfaces produces a rotating
    /// cycle of pixel-identical frames while `frame_id` keeps advancing. The
    /// pump suppresses those sends; without a read surface, "suppressing"
    /// and "healthy but static" looked identical from outside the process.
    #[tokio::test(start_paused = true)]
    async fn a_looping_decoder_is_reported_as_suppressed() {
        let cache = Arc::new(LatestFrameCache::new());
        let mgr = LiveViewManager::new(cache.clone(), Arc::new(TunnelOutbox::new()));
        let epoch = cache.begin_session(6);
        cache.put(6, epoch, frame_with_fill(6, 1, 10), Arc::new(vec![]));
        subscribe(&mgr, 6);

        // Period-3 content cycle: distinct fills repeating at distance 3, so
        // every hit lands at the >= 2 distance the guard treats as impossible
        // for live video. Drive past LOOP_LOG_TRIP.
        let fills = [10u8, 20, 30];
        for i in 0..(u64::from(LOOP_LOG_TRIP) + 6) {
            let fill = fills[(i as usize) % fills.len()];
            cache.put(6, epoch, frame_with_fill(6, i + 2, fill), Arc::new(vec![]));
            tokio::time::sleep(Duration::from_millis(300)).await;
        }

        let s = mgr.status();
        assert!(s[0].suppressed, "a fixed content cycle is reported");
        assert!(
            !s[0].stalled,
            "frame_id is advancing, so this is not a stall"
        );
    }

    #[tokio::test]
    async fn status_is_sorted_and_carries_the_current_tier() {
        let mgr = LiveViewManager::new(
            Arc::new(LatestFrameCache::new()),
            Arc::new(TunnelOutbox::new()),
        );
        subscribe(&mgr, 5);
        subscribe(&mgr, 2);
        mgr.on_subscribe(&LbrSubscribePayload {
            camera_id: 2,
            tile_w: Some(640),
            tile_h: Some(360),
            fps_tier: Some("focus".to_string()),
        });

        let s = mgr.status();
        assert_eq!(
            s.iter().map(|p| p.camera_id).collect::<Vec<_>>(),
            vec![2, 5]
        );
        assert_eq!(s[0].ceiling_fps, FOCUS_FPS);
        assert_eq!(s[0].tile_w, Some(640));
        assert_eq!(s[1].ceiling_fps, GRID_FPS);
    }
}

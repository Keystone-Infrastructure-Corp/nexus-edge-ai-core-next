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

use std::collections::HashMap;
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
    task: JoinHandle<()>,
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
        let task = spawn_pump(
            camera_id,
            payload.camera_id,
            self.cache.clone(),
            self.outbox.clone(),
            self.budget.clone(),
            shared.clone(),
        );
        pumps.insert(
            camera_id,
            PumpEntry {
                params: shared,
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

    /// Number of cameras currently being pumped. Test-only for now; a
    /// diagnostics surface (e.g. an admin `live/status` endpoint) can
    /// un-gate it when Phase C/D wires one.
    #[cfg(test)]
    #[must_use]
    pub fn active_pump_count(&self) -> usize {
        self.pumps.lock().len()
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
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_frame_id: Option<u64> = None;
        let mut last_sig: u64 = 0;
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
                let scene_changed = new_frame && sig != last_sig;
                let keepalive_due = now.duration_since(last_emit) >= KEEPALIVE_INTERVAL;

                // Encode only when there is a NEW frame to send and either
                // the scene changed or a keepalive is due — and the shared
                // budget has a token to spare (else skip this tick).
                if new_frame && (scene_changed || keepalive_due) && budget.try_acquire() {
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
}

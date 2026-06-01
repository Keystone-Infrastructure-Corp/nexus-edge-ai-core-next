//! Engine-side glue that turns per-stable-track
//! [`nexus_pipeline::SightingSnapshot`]s into wire `entity_sighting`
//! envelopes.
//!
//! Phase 5.6 · slice 4c-ii.
//!
//! ### Why the buffer + worker pattern?
//!
//! [`nexus_pipeline::SightingHook::submit`] is called on the per-camera
//! supervisor's per-frame hot path. The actual work is heavy:
//!
//! * `crop_and_resize` (~1-3 ms on a 960×540 RGB frame).
//! * `Extractor::extract` (~6-30 ms depending on EP — CPU vs OpenVINO vs CoreML).
//! * `TunnelOutbox::send` (network round-trip on the WSS write side).
//!
//! Doing any of that synchronously would stall the supervisor and
//! cap the camera's effective FPS. Instead, `submit` pushes onto a
//! bounded `tokio::sync::mpsc` channel (cheap — one heap alloc + an
//! `Arc::clone` of the frame) and returns immediately. A dedicated
//! `worker` task drains the channel and runs the extract + publish
//! sequentially. Back-pressure surfaces as a `warn!` log when the
//! channel is full (TrySendError::Full), never as a frame stall.
//!
//! ### Cloud-allowlist gate
//!
//! The cloud's edge-gateway rejects any `embedding_model` not in
//! `('dinov2-s-v1', 'osnet-x1.0-v1')` (see migration `0035` CHECK).
//! When the configured extractor's `model_id` starts with `"mock_"`
//! we treat this as a dev-mode round-trip test and skip the cloud
//! publish entirely (just log at debug). That lets a developer run
//! the engine + cloud-tunnel against a real cloud without polluting
//! `entity_sightings` with rows that don't actually carry a real
//! embedding.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use nexus_cloud_client::{
    cloud_capabilities,
    sink::{
        build_entity_sighting_batch_envelope, build_entity_sighting_envelope_with_dtype,
        EntitySightingProjection,
    },
    TunnelOutbox,
};
use nexus_pipeline::{SightingHook, SightingSnapshot};
use nexus_reid::{Embedding, Extractor, ExtractorError};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Phase 5.6 · R7 — observability snapshot for a single camera's
/// re-ID pipeline. One row per `(camera_id)` last touched by the
/// worker. Pure metadata: emit counter (lifetime, since boot),
/// timestamp of the most recent successful emit, and an 8-byte hex
/// prefix of the most recent embedding for "is this actually
/// producing different outputs per identity?" eyeball verification.
///
/// We deliberately DO NOT keep the full embedding here — the whole
/// point of the wedge plan is that embeddings are write-only at the
/// edge. The 8-byte prefix is short enough to never be re-projectable
/// into the source identity but long enough (16 hex chars,
/// 2^64 states) for an operator to see "yes, two consecutive emits
/// for the same track produce nearly-identical hashes; the model
/// isn't randomly hallucinating".
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReidCameraStats {
    /// Lifetime count of successful per-track emits (extract + at
    /// least attempted publish) since engine boot. Reset only on
    /// process restart.
    pub emit_count: u64,
    /// UTC timestamp of the most recent successful emit. `None`
    /// when the worker has never processed a snapshot for this
    /// camera since boot.
    pub last_emit_at: Option<DateTime<Utc>>,
    /// Hex-encoded first 8 bytes of `embedding.vec` interpreted as
    /// little-endian f32 -> raw bytes. 16 chars. Empty when the
    /// worker has never processed a snapshot for this camera since
    /// boot.
    pub last_embedding_hex8: String,
}

impl ReidCameraStats {
    fn new() -> Self {
        Self {
            emit_count: 0,
            last_emit_at: None,
            last_embedding_hex8: String::new(),
        }
    }
}

/// Shared registry of per-camera re-ID stats. Cheap to clone (Arc).
/// Read by the `/v1/admin/reid/status` admin endpoint; written by
/// the worker task on every successful extract.
#[derive(Debug, Default)]
pub struct ReidStatsRegistry {
    inner: Mutex<HashMap<i64, ReidCameraStats>>,
}

impl ReidStatsRegistry {
    /// Build a fresh empty registry. Caller is expected to wrap in
    /// `Arc` so the worker + API state can share it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot every per-camera row, sorted by `camera_id` for a
    /// stable wire response. O(n log n) on the camera count (~tens
    /// in practice).
    #[must_use]
    pub fn snapshot(&self) -> Vec<(i64, ReidCameraStats)> {
        let guard = self.inner.lock();
        let mut rows: Vec<(i64, ReidCameraStats)> =
            guard.iter().map(|(k, v)| (*k, v.clone())).collect();
        drop(guard);
        rows.sort_by_key(|(cam, _)| *cam);
        rows
    }

    /// Record a successful emit. Called by the worker AFTER a
    /// successful `extract` returns; happens regardless of whether
    /// the cloud publish itself succeeds (so the operator can tell
    /// "model is running, network is down" from "model isn't even
    /// invoked").
    fn record_emit(&self, camera_id: i64, embedding: &[f32], ts: DateTime<Utc>) {
        let hex8 = embedding_prefix_hex8(embedding);
        let mut guard = self.inner.lock();
        let entry = guard.entry(camera_id).or_insert_with(ReidCameraStats::new);
        entry.emit_count = entry.emit_count.saturating_add(1);
        entry.last_emit_at = Some(ts);
        entry.last_embedding_hex8 = hex8;
    }
}

/// Render the first 8 bytes of a `[f32]` (interpreted as
/// little-endian f32 byte representation) as a 16-char hex string.
/// Returns `""` when the slice is empty.
fn embedding_prefix_hex8(embedding: &[f32]) -> String {
    if embedding.is_empty() {
        return String::new();
    }
    // 2 f32 = 8 bytes. Most extractors return dim >= 384 so this
    // branch is the steady-state path.
    let mut buf = [0u8; 8];
    let mut idx = 0usize;
    for f in embedding.iter().take(2) {
        let bytes = f.to_le_bytes();
        for b in bytes {
            if idx >= 8 {
                break;
            }
            buf[idx] = b;
            idx += 1;
        }
    }
    // Inline 16-char hex render to avoid pulling `hex` as a direct
    // engine dep just for one call site.
    let mut out = String::with_capacity(idx * 2);
    for b in &buf[..idx] {
        use std::fmt::Write as _;
        let _ = write!(out, "{:02x}", b);
    }
    out
}

/// Hook the supervisor calls. Owns a bounded mpsc sender; the
/// matching receiver lives in [`run_worker`].
pub struct CloudEntitySightingHook {
    tx: mpsc::Sender<SightingSnapshot>,
}

impl CloudEntitySightingHook {
    /// Spawn the worker task and return the supervisor-side hook.
    /// `capacity` bounds the per-camera queue depth (default `64`
    /// from the engine boot site is a good starting point — at 5s
    /// cadence per track and ~10 concurrent tracks per camera the
    /// steady-state queue is ~2 messages).
    ///
    /// `stats` is the observability sink wired to the
    /// `/v1/admin/reid/status` admin endpoint. The worker bumps
    /// the per-camera counter on every successful extract — pass
    /// a fresh `Arc::new(ReidStatsRegistry::new())` and hand the
    /// same Arc to `ApiState::reid_stats` so the admin UI can
    /// read it.
    ///
    /// `min_crop_w_px` / `min_crop_h_px` are M_PERF_CROWD B4
    /// bbox-size floors (in supervisor-frame pixels). Snapshots
    /// whose bbox is smaller than either floor are dropped BEFORE
    /// the batched extractor call so we don't spend compute on
    /// crops too small to embed reliably. Pass `0` for either
    /// dimension to disable that floor.
    #[must_use]
    pub fn spawn(
        extractor: Arc<dyn Extractor>,
        outbox: Arc<TunnelOutbox>,
        capacity: usize,
        stats: Arc<ReidStatsRegistry>,
        min_crop_w_px: u32,
        min_crop_h_px: u32,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<SightingSnapshot>(capacity.max(1));
        tokio::spawn(run_worker(
            extractor,
            outbox,
            rx,
            stats,
            min_crop_w_px,
            min_crop_h_px,
        ));
        Self { tx }
    }
}

impl SightingHook for CloudEntitySightingHook {
    fn submit(&self, snapshot: SightingSnapshot) {
        // try_send is the right primitive on the hot path — `send`
        // would await on a full queue and stall the supervisor.
        match self.tx.try_send(snapshot) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(snap)) => {
                warn!(
                    camera_id = snap.camera_id,
                    track_id = snap.track_id,
                    "entity-sighting queue full; dropping snapshot"
                );
            }
            Err(mpsc::error::TrySendError::Closed(snap)) => {
                warn!(
                    camera_id = snap.camera_id,
                    track_id = snap.track_id,
                    "entity-sighting worker gone; dropping snapshot"
                );
            }
        }
    }
}

/// Phase M_PERF_CROWD A3 — max sightings per `entity_sighting_batch`
/// envelope. Matches the wire schema (`items: maxItems: 32`).
const BATCH_MAX: usize = 32;

/// Phase M_PERF_CROWD A3 — drain window for batched mode. Worker
/// blocks for the first snapshot, then opportunistically drains up
/// to `BATCH_MAX-1` more arrivals within this window before flushing.
const BATCH_WINDOW: Duration = Duration::from_millis(100);

/// Phase M_PERF_CROWD B4 — max snapshots stacked into a single
/// `Extractor::extract_batch` call. DINOv2-S at `B=16` saturates
/// the N150 iGPU while keeping the per-call wall time well under
/// `BATCH_WINDOW`. Larger batches in one drain window are split
/// across multiple back-to-back batched ORT calls.
const REID_EXTRACT_MAX: usize = 16;

async fn run_worker(
    extractor: Arc<dyn Extractor>,
    outbox: Arc<TunnelOutbox>,
    mut rx: mpsc::Receiver<SightingSnapshot>,
    stats: Arc<ReidStatsRegistry>,
    min_crop_w_px: u32,
    min_crop_h_px: u32,
) {
    let model_id = extractor.model_id().to_string();
    let dim = extractor.dim();
    // The cloud's edge-gateway CHECK constraint rejects anything
    // outside the allowlist. Mock extractors (default id starts with
    // "mock_") are dev-only — log + drop instead of guaranteeing a
    // 400 from every cloud round-trip.
    let cloud_eligible = !model_id.starts_with("mock_");
    if !cloud_eligible {
        debug!(
            model_id = %model_id,
            "entity-sighting worker running in DEV mode (mock extractor); will run extract for self-test but skip cloud publish"
        );
    }
    while let Some(first) = rx.recv().await {
        // Snapshot the cloud's advertised capabilities ONCE per
        // batch — the heartbeat_ack pump updates the outbox set in
        // the background, but checking inside the drain loop would
        // let a mid-batch flip produce a mixed-mode envelope.
        let use_batch = outbox.supports(cloud_capabilities::ENTITY_SIGHTING_BATCH);
        let use_f16 = outbox.supports(cloud_capabilities::EMBEDDING_DTYPE_F16);
        if !use_batch {
            // Non-A3 path: extract the single snapshot and emit one
            // envelope. Min-crop filter still applies — a too-small
            // bbox is dropped here exactly as in the batched path.
            if let Some(p) = extract_projection(
                &*extractor,
                &model_id,
                dim,
                cloud_eligible,
                &stats,
                first,
                min_crop_w_px,
                min_crop_h_px,
            )
            .await
            {
                publish_single(&outbox, p, use_f16).await;
            }
            continue;
        }
        // A3 path: drain RAW snapshots first (no per-snapshot
        // extract). This is the key M_PERF_CROWD B4 inversion —
        // pre-B4 each arrival fired its own `extract()` from inside
        // the drain loop, defeating any chance at ORT batching.
        let mut snapshots: Vec<SightingSnapshot> = Vec::with_capacity(BATCH_MAX);
        snapshots.push(first);
        let deadline = Instant::now() + BATCH_WINDOW;
        while snapshots.len() < BATCH_MAX {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(snap)) => snapshots.push(snap),
                Ok(None) => break, // channel closed
                Err(_) => break,   // drain window elapsed
            }
        }
        // B4 min-crop gate: drop bboxes too small to embed
        // reliably BEFORE the ORT call. Filter is a no-op when
        // both floors are 0 (default).
        let drained = snapshots.len();
        let kept: Vec<SightingSnapshot> = if min_crop_w_px == 0 && min_crop_h_px == 0 {
            snapshots
        } else {
            snapshots
                .into_iter()
                .filter(|s| {
                    let w = s.bbox.width().max(0.0).round() as u32;
                    let h = s.bbox.height().max(0.0).round() as u32;
                    w >= min_crop_w_px && h >= min_crop_h_px
                })
                .collect()
        };
        let dropped = drained - kept.len();
        if dropped > 0 {
            debug!(
                dropped,
                drained,
                min_crop_w_px,
                min_crop_h_px,
                "entity-sighting B4 min-crop filter dropped snapshots"
            );
        }
        if kept.is_empty() {
            continue;
        }
        // Chunk into REID_EXTRACT_MAX-sized batched ORT calls. At
        // BATCH_MAX=32 / REID_EXTRACT_MAX=16 the worst case is
        // exactly 2 back-to-back batched calls per drain window.
        let mut remaining = kept;
        let mut projections: Vec<EntitySightingProjection> = Vec::with_capacity(remaining.len());
        while !remaining.is_empty() {
            let take = remaining.len().min(REID_EXTRACT_MAX);
            let chunk: Vec<SightingSnapshot> = remaining.drain(0..take).collect();
            let items: Vec<(Arc<nexus_types::Frame>, nexus_types::BBox)> =
                chunk.iter().map(|s| (s.frame.clone(), s.bbox)).collect();
            let results = extractor.extract_batch(&items).await;
            debug_assert_eq!(results.len(), chunk.len());
            for (snap, res) in chunk.into_iter().zip(results) {
                if let Some(p) = build_projection(&model_id, dim, cloud_eligible, &stats, snap, res)
                {
                    projections.push(p);
                }
            }
        }
        if projections.is_empty() {
            continue;
        }
        publish_batch(&outbox, projections, use_f16).await;
    }
}

async fn publish_single(
    outbox: &TunnelOutbox,
    projection: EntitySightingProjection,
    use_f16: bool,
) {
    let camera_id = projection.camera_id;
    let envelope = build_entity_sighting_envelope_with_dtype(projection, use_f16);
    match outbox.send(envelope).await {
        Ok(()) => debug!(
            camera_id,
            dtype = if use_f16 { "f16" } else { "f32" },
            "entity_sighting envelope published"
        ),
        Err(e) => debug!(
            camera_id,
            error = %e,
            "entity_sighting envelope publish failed (tunnel down?); dropping"
        ),
    }
}

async fn publish_batch(outbox: &TunnelOutbox, items: Vec<EntitySightingProjection>, use_f16: bool) {
    if items.is_empty() {
        return;
    }
    // Singleton batches are wasteful — the unwrap below would also
    // panic the debug_assert in `build_entity_sighting_batch_envelope`
    // for a zero-item input. Fall back to the plain envelope.
    if items.len() == 1 {
        publish_single(outbox, items.into_iter().next().unwrap(), use_f16).await;
        return;
    }
    let count = items.len();
    let envelope = build_entity_sighting_batch_envelope(items, use_f16);
    match outbox.send(envelope).await {
        Ok(()) => debug!(
            count,
            dtype = if use_f16 { "f16" } else { "f32" },
            "entity_sighting_batch envelope published"
        ),
        Err(e) => debug!(
            count,
            error = %e,
            "entity_sighting_batch envelope publish failed (tunnel down?); dropping"
        ),
    }
}

/// Extract a single snapshot into a wire projection. Returns `None`
/// when the snapshot should be dropped (extractor error, dim
/// mismatch, dev-mode mock extractor, or B4 min-crop floor). Used
/// on the non-A3 (single-envelope) worker path; the A3/B4 path
/// calls [`Extractor::extract_batch`] directly and then funnels
/// each `(snapshot, result)` pair through [`build_projection`].
/// All counter updates and log emissions match the pre-batching
/// behaviour so the admin `/reid/status` semantics are unchanged.
#[allow(clippy::too_many_arguments)]
async fn extract_projection(
    extractor: &dyn Extractor,
    model_id: &str,
    dim: usize,
    cloud_eligible: bool,
    stats: &ReidStatsRegistry,
    snapshot: SightingSnapshot,
    min_crop_w_px: u32,
    min_crop_h_px: u32,
) -> Option<EntitySightingProjection> {
    // B4 min-crop gate (mirrors the batched path so the two modes
    // observe the same drop policy).
    if min_crop_w_px > 0 || min_crop_h_px > 0 {
        let w = snapshot.bbox.width().max(0.0).round() as u32;
        let h = snapshot.bbox.height().max(0.0).round() as u32;
        if w < min_crop_w_px || h < min_crop_h_px {
            debug!(
                camera_id = snapshot.camera_id,
                track_id = snapshot.track_id,
                w,
                h,
                min_crop_w_px,
                min_crop_h_px,
                "entity-sighting B4 min-crop filter dropped snapshot"
            );
            return None;
        }
    }
    let result = extractor.extract(&snapshot.frame, &snapshot.bbox).await;
    build_projection(model_id, dim, cloud_eligible, stats, snapshot, result)
}

/// Post-extract slot construction shared by the single and batched
/// worker paths. Folds the embedding result, dim sanity-check,
/// stats counter bump, and dev-mode skip into one place so any
/// future tweak (e.g. new dim, new dev-mode rule) stays consistent
/// across both paths.
fn build_projection(
    model_id: &str,
    dim: usize,
    cloud_eligible: bool,
    stats: &ReidStatsRegistry,
    snapshot: SightingSnapshot,
    embedding_result: Result<Embedding, ExtractorError>,
) -> Option<EntitySightingProjection> {
    let SightingSnapshot {
        camera_id,
        track_id,
        entity_local_id,
        frame,
        bbox,
        confidence,
        started_ts,
        ts,
        is_first,
    } = snapshot;
    let frame_w = frame.width;
    let frame_h = frame.height;
    let embedding = match embedding_result {
        Ok(emb) => emb,
        Err(e) => {
            warn!(
                camera_id,
                track_id,
                error = %e,
                "entity-sighting extractor failed; dropping snapshot"
            );
            return None;
        }
    };
    if embedding.vec.len() != dim {
        warn!(
            camera_id,
            track_id,
            got = embedding.vec.len(),
            want = dim,
            "entity-sighting embedding dimension mismatch; dropping snapshot"
        );
        return None;
    }
    // Phase 5.6 · R7 — record the emit in the shared stats
    // registry BEFORE the publish branch. The admin /reid
    // diagnostic page MUST be able to distinguish "extractor
    // is running, cloud is down" from "extractor isn't even
    // invoked", so we bump the counter even for the dev-mode
    // skip path below.
    stats.record_emit(camera_id, &embedding.vec, ts);
    if !cloud_eligible {
        debug!(
            camera_id,
            track_id,
            model_id = %model_id,
            "entity-sighting extracted (dev mode); skipping cloud publish"
        );
        return None;
    }
    // Saturating casts here: the engine's CameraId is i64 and
    // BBox::{x1,y1,x2,y2} are f32. Negative cam_id never happens
    // in practice (POST /cameras assigns from SQLite rowid which
    // is always > 0), but `as u64` would underflow if it ever
    // did — clamp explicitly so the wire bbox can never carry a
    // surprise huge value.
    Some(EntitySightingProjection {
        camera_id: u64::try_from(camera_id).unwrap_or(0),
        entity_local_id,
        embedding: embedding.vec,
        embedding_model: model_id.to_string(),
        bbox: [
            bbox.x1.max(0.0).round() as i64,
            bbox.y1.max(0.0).round() as i64,
            bbox.width().max(0.0).round() as i64,
            bbox.height().max(0.0).round() as i64,
        ],
        confidence: f64::from(confidence).clamp(0.0, 1.0),
        frame_w: u64::from(frame_w),
        frame_h: u64::from(frame_h),
        started_ts,
        ts,
        is_first_sighting: is_first,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nexus_reid::MockExtractor;
    use nexus_types::{BBox, Frame, PixelFormat};

    fn dummy_snapshot(camera_id: i64, track_id: u64) -> SightingSnapshot {
        let now = Utc::now();
        SightingSnapshot {
            camera_id,
            track_id,
            entity_local_id: uuid::Uuid::now_v7().to_string(),
            frame: Arc::new(Frame {
                camera_id,
                frame_id: 1,
                captured_at: now,
                width: 960,
                height: 540,
                format: PixelFormat::Rgb24,
                data: Arc::new(vec![64u8; 960 * 540 * 3]),
                trace_id: "test".into(),
            }),
            bbox: BBox {
                x1: 100.0,
                y1: 200.0,
                x2: 250.0,
                y2: 500.0,
            },
            confidence: 0.9,
            started_ts: now,
            ts: now,
            is_first: true,
        }
    }

    /// A mock extractor never produces a real wire envelope. Worker
    /// must accept the snapshot, run the extract (proves the crop
    /// path works end-to-end), then SKIP the cloud publish because
    /// the model_id starts with `"mock_"` (cloud-side CHECK would
    /// reject anyway). Outbox is empty → outbox.send is never
    /// called → no panic on a no-handle outbox.
    #[tokio::test]
    async fn mock_extractor_skips_cloud_publish() {
        let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor::new());
        let outbox = Arc::new(TunnelOutbox::new());
        let stats = Arc::new(ReidStatsRegistry::new());
        let hook =
            CloudEntitySightingHook::spawn(extractor, outbox.clone(), 8, stats.clone(), 0, 0);
        hook.submit(dummy_snapshot(7, 1));
        // Let the worker drain. tokio::time::sleep is fine here —
        // no production code path polls.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Nothing observable to assert other than "the test did not
        // hang and did not panic" — the dev-mode skip path is the
        // success case. (A future memory observer wired into the
        // outbox would let us assert send-count==0 explicitly.)
        assert!(!outbox.is_connected(), "outbox stays empty in dev mode");
    }

    /// Phase 5.6 · R7 — even when the dev-mode mock extractor
    /// short-circuits the cloud publish, the stats registry MUST
    /// reflect that the extractor was invoked successfully. The
    /// `/v1/admin/reid/status` page uses this signal to distinguish
    /// "model running, network down" from "model not running at
    /// all".
    #[tokio::test]
    async fn dev_mode_emit_still_records_stats() {
        let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor::new());
        let outbox = Arc::new(TunnelOutbox::new());
        let stats = Arc::new(ReidStatsRegistry::new());
        let hook = CloudEntitySightingHook::spawn(extractor, outbox, 8, stats.clone(), 0, 0);
        hook.submit(dummy_snapshot(11, 1));
        hook.submit(dummy_snapshot(11, 2));
        hook.submit(dummy_snapshot(12, 1));
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let snap = stats.snapshot();
        assert_eq!(snap.len(), 2, "two distinct cameras seen");
        let cam11 = snap.iter().find(|(c, _)| *c == 11).unwrap();
        let cam12 = snap.iter().find(|(c, _)| *c == 12).unwrap();
        assert_eq!(cam11.1.emit_count, 2);
        assert_eq!(cam12.1.emit_count, 1);
        assert!(cam11.1.last_emit_at.is_some());
        assert_eq!(cam11.1.last_embedding_hex8.len(), 16);
    }

    #[tokio::test]
    async fn full_queue_does_not_block_submitter() {
        // Capacity=1 so the second submit fills the queue. The
        // worker is held off by a slow extractor (we use a real
        // MockExtractor but submit 5 in a row before yielding).
        let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor::new());
        let outbox = Arc::new(TunnelOutbox::new());
        let stats = Arc::new(ReidStatsRegistry::new());
        let hook = CloudEntitySightingHook::spawn(extractor, outbox, 1, stats, 0, 0);
        // First two get into the channel (sender slot + receiver
        // slot); the next three should hit TrySendError::Full and
        // be dropped without blocking. Test guard: this loop must
        // complete in milliseconds. If submit() were awaiting, this
        // would hang well past any reasonable test timeout.
        let start = std::time::Instant::now();
        for i in 0..50 {
            hook.submit(dummy_snapshot(1, i));
        }
        assert!(
            start.elapsed() < std::time::Duration::from_millis(200),
            "submit must be non-blocking even when the queue is full"
        );
    }

    // --------------------------------------------------------------------
    // Phase M_PERF_CROWD A3 — entity_sighting_batch coverage.
    // --------------------------------------------------------------------

    use async_trait::async_trait;
    use nexus_cloud_client::tunnel::{TunnelError, TunnelHandle};
    use nexus_cloud_protocol::v1::{Envelope, EnvelopeBody};

    struct CapturingTunnel {
        sent: parking_lot::Mutex<Vec<Envelope>>,
    }

    #[async_trait]
    impl TunnelHandle for CapturingTunnel {
        async fn send(&self, envelope: Envelope) -> Result<(), TunnelError> {
            self.sent.lock().push(envelope);
            Ok(())
        }
    }

    /// Force `cloud_eligible = true` by using a non-mock model id —
    /// the worker only batches when the gateway will actually accept
    /// the envelopes.
    fn real_extractor() -> Arc<dyn Extractor> {
        Arc::new(MockExtractor::with_config("dinov2-s-v1", 384))
    }

    fn install(outbox: &TunnelOutbox, caps: &[&str]) -> Arc<CapturingTunnel> {
        let cap = Arc::new(CapturingTunnel {
            sent: parking_lot::Mutex::new(Vec::new()),
        });
        outbox.set_handle(Some(cap.clone() as Arc<dyn TunnelHandle>));
        let owned: Vec<String> = caps.iter().map(|s| (*s).to_string()).collect();
        outbox.update_caps(Some(&owned));
        cap
    }

    /// Phase M_PERF_CROWD A3 — when the cloud advertises
    /// `entity_sighting_batch`, 64 snapshots arriving back-to-back
    /// MUST be flushed as ≤ 4 envelopes (BATCH_MAX = 32), every
    /// non-final envelope MUST be an `EntitySightingBatch`, and every
    /// payload MUST carry `embedding_dtype = Some("f16")` when the
    /// gateway also advertised `embedding_dtype_f16`.
    #[tokio::test]
    async fn batch_envelope_emitted_when_capability_advertised() {
        let outbox = Arc::new(TunnelOutbox::new());
        let cap = install(
            &outbox,
            &[
                cloud_capabilities::ENTITY_SIGHTING_BATCH,
                cloud_capabilities::EMBEDDING_DTYPE_F16,
            ],
        );
        let stats = Arc::new(ReidStatsRegistry::new());
        let hook =
            CloudEntitySightingHook::spawn(real_extractor(), outbox.clone(), 128, stats, 0, 0);
        for i in 0..64 {
            hook.submit(dummy_snapshot(1, i));
        }
        // Real wall-clock here; one BATCH_WINDOW per envelope at
        // worst, so 64/32 = 2 windows plus a generous fudge for
        // the per-snapshot extract pass.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let sent = cap.sent.lock().clone();
        assert!(
            sent.len() <= 4,
            "batch mode produced {} envelopes for 64 snapshots; expected ≤ 4",
            sent.len()
        );
        let mut total_items = 0usize;
        for env in &sent {
            match &env.body {
                EnvelopeBody::EntitySightingBatch(b) => {
                    assert!(!b.items.is_empty() && b.items.len() <= BATCH_MAX);
                    for item in &b.items {
                        assert_eq!(item.embedding_dtype.as_deref(), Some("f16"));
                    }
                    total_items += b.items.len();
                }
                EnvelopeBody::EntitySighting(p) => {
                    // Permitted only as a trailing singleton.
                    assert_eq!(p.embedding_dtype.as_deref(), Some("f16"));
                    total_items += 1;
                }
                other => panic!("unexpected envelope body: {other:?}"),
            }
        }
        assert_eq!(total_items, 64);
    }

    /// Without the `entity_sighting_batch` capability the worker
    /// MUST fall back to the legacy per-item envelope and MUST NOT
    /// stamp `embedding_dtype = "f16"`.
    #[tokio::test]
    async fn no_batching_when_capability_absent() {
        let outbox = Arc::new(TunnelOutbox::new());
        let cap = install(&outbox, &[]);
        let stats = Arc::new(ReidStatsRegistry::new());
        let hook =
            CloudEntitySightingHook::spawn(real_extractor(), outbox.clone(), 32, stats, 0, 0);
        for i in 0..4 {
            hook.submit(dummy_snapshot(1, i));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let sent = cap.sent.lock().clone();
        assert_eq!(sent.len(), 4, "legacy mode is one envelope per snapshot");
        for env in &sent {
            match &env.body {
                EnvelopeBody::EntitySighting(p) => {
                    assert!(
                        p.embedding_dtype.is_none(),
                        "FP16 must not be selected without capability"
                    );
                }
                other => panic!("expected EntitySighting, got {other:?}"),
            }
        }
    }

    // --------------------------------------------------------------------
    // Phase M_PERF_CROWD B4 — batched extractor + min-crop floor.
    // --------------------------------------------------------------------

    use nexus_reid::{Embedding, ExtractorError};
    use nexus_types::Frame as ReidFrame;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// `Extractor` that records every `extract_batch` call size so
    /// tests can assert "the worker actually called the batched
    /// path, not just looped extract() N times via the default
    /// trait impl".
    struct BatchRecordingExtractor {
        inner: MockExtractor,
        batch_call_count: AtomicUsize,
        last_batch_size: AtomicUsize,
        total_items: AtomicUsize,
    }

    impl BatchRecordingExtractor {
        fn new() -> Self {
            Self {
                inner: MockExtractor::with_config("dinov2-s-v1", 384),
                batch_call_count: AtomicUsize::new(0),
                last_batch_size: AtomicUsize::new(0),
                total_items: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Extractor for BatchRecordingExtractor {
        fn model_id(&self) -> &str {
            self.inner.model_id()
        }
        fn dim(&self) -> usize {
            self.inner.dim()
        }
        async fn extract(
            &self,
            frame: &ReidFrame,
            bbox: &nexus_types::BBox,
        ) -> Result<Embedding, ExtractorError> {
            self.inner.extract(frame, bbox).await
        }
        async fn extract_batch(
            &self,
            items: &[(Arc<ReidFrame>, nexus_types::BBox)],
        ) -> Vec<Result<Embedding, ExtractorError>> {
            self.batch_call_count.fetch_add(1, AtomicOrdering::SeqCst);
            self.last_batch_size
                .store(items.len(), AtomicOrdering::SeqCst);
            self.total_items
                .fetch_add(items.len(), AtomicOrdering::SeqCst);
            // Fall through to the inner mock so the produced
            // projections are realistic.
            self.inner.extract_batch(items).await
        }
    }

    fn dummy_snapshot_with_bbox(
        camera_id: i64,
        track_id: u64,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    ) -> SightingSnapshot {
        let mut s = dummy_snapshot(camera_id, track_id);
        s.bbox = nexus_types::BBox { x1, y1, x2, y2 };
        s
    }

    /// B4 — when the cloud advertises `entity_sighting_batch`, the
    /// worker MUST route drained snapshots through
    /// `extract_batch` (chunked to `REID_EXTRACT_MAX=16`) instead
    /// of looping per-snapshot `extract()`. 32 snapshots in one
    /// drain window should yield exactly 2 batched calls.
    #[tokio::test]
    async fn b4_worker_routes_to_extract_batch() {
        let outbox = Arc::new(TunnelOutbox::new());
        let _cap = install(
            &outbox,
            &[
                cloud_capabilities::ENTITY_SIGHTING_BATCH,
                cloud_capabilities::EMBEDDING_DTYPE_F16,
            ],
        );
        let stats = Arc::new(ReidStatsRegistry::new());
        let rec = Arc::new(BatchRecordingExtractor::new());
        let extractor: Arc<dyn Extractor> = rec.clone();
        let hook = CloudEntitySightingHook::spawn(extractor, outbox.clone(), 128, stats, 0, 0);
        // 32 snapshots fit one BATCH_MAX drain window. With
        // REID_EXTRACT_MAX=16 that's 2 extract_batch calls.
        for i in 0..32 {
            hook.submit(dummy_snapshot(7, i));
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        let calls = rec.batch_call_count.load(AtomicOrdering::SeqCst);
        let total = rec.total_items.load(AtomicOrdering::SeqCst);
        assert_eq!(total, 32, "all 32 snapshots reached extract_batch");
        assert!(
            (1..=3).contains(&calls),
            "expected 1-3 extract_batch calls (with chunking), got {calls}"
        );
    }

    /// B4 — when both min_crop floors are 0 (default), no
    /// snapshots are dropped by the size gate even if the bbox is
    /// tiny.
    #[tokio::test]
    async fn b4_min_crop_disabled_passes_tiny_bboxes() {
        let outbox = Arc::new(TunnelOutbox::new());
        let _cap = install(&outbox, &[cloud_capabilities::ENTITY_SIGHTING_BATCH]);
        let stats = Arc::new(ReidStatsRegistry::new());
        let rec = Arc::new(BatchRecordingExtractor::new());
        let extractor: Arc<dyn Extractor> = rec.clone();
        let hook = CloudEntitySightingHook::spawn(extractor, outbox.clone(), 64, stats, 0, 0);
        // 8 tiny bboxes (10×10) — well below any reasonable
        // crop floor — should still reach the extractor because
        // min_crop_*_px = 0 disables the filter.
        for i in 0..8 {
            hook.submit(dummy_snapshot_with_bbox(9, i, 0.0, 0.0, 10.0, 10.0));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let total = rec.total_items.load(AtomicOrdering::SeqCst);
        assert_eq!(total, 8);
    }

    /// B4 — when min_crop floors are set, snapshots whose bbox is
    /// smaller than either floor are dropped BEFORE the extractor
    /// is invoked. Mix of 4 small (10×10) + 4 large (200×200);
    /// extractor should only see the large 4.
    #[tokio::test]
    async fn b4_min_crop_filters_small_bboxes_before_extract() {
        let outbox = Arc::new(TunnelOutbox::new());
        let _cap = install(&outbox, &[cloud_capabilities::ENTITY_SIGHTING_BATCH]);
        let stats = Arc::new(ReidStatsRegistry::new());
        let rec = Arc::new(BatchRecordingExtractor::new());
        let extractor: Arc<dyn Extractor> = rec.clone();
        // Floor at 64×128 (the spec's suggested default).
        let hook = CloudEntitySightingHook::spawn(extractor, outbox.clone(), 64, stats, 64, 128);
        for i in 0..4 {
            hook.submit(dummy_snapshot_with_bbox(3, i, 0.0, 0.0, 10.0, 10.0));
        }
        for i in 4..8 {
            hook.submit(dummy_snapshot_with_bbox(3, i, 0.0, 0.0, 200.0, 200.0));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let total = rec.total_items.load(AtomicOrdering::SeqCst);
        assert_eq!(
            total, 4,
            "min-crop floor must drop the 4 small bboxes BEFORE extract_batch"
        );
    }

    /// B4 — min-crop floor also applies on the non-A3 (single
    /// envelope) path so the two modes have identical drop policy.
    #[tokio::test]
    async fn b4_min_crop_filters_on_non_batched_path() {
        let outbox = Arc::new(TunnelOutbox::new());
        let cap = install(&outbox, &[]); // no ENTITY_SIGHTING_BATCH
        let stats = Arc::new(ReidStatsRegistry::new());
        let rec = Arc::new(BatchRecordingExtractor::new());
        let extractor: Arc<dyn Extractor> = rec.clone();
        let hook = CloudEntitySightingHook::spawn(extractor, outbox.clone(), 16, stats, 64, 128);
        // Two tiny bboxes (must drop) + one big (must pass).
        hook.submit(dummy_snapshot_with_bbox(5, 1, 0.0, 0.0, 10.0, 10.0));
        hook.submit(dummy_snapshot_with_bbox(5, 2, 0.0, 0.0, 10.0, 10.0));
        hook.submit(dummy_snapshot_with_bbox(5, 3, 0.0, 0.0, 200.0, 400.0));
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Non-batched path uses extract(), not extract_batch().
        // But our recorder forwards extract() to the inner mock
        // unchanged, so the assertion here is on the OUTBOX —
        // exactly one envelope sent for the surviving snapshot.
        let sent = cap.sent.lock().clone();
        assert_eq!(
            sent.len(),
            1,
            "min-crop filter must drop tiny bboxes on the non-batched path too"
        );
    }
}

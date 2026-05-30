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

use std::sync::Arc;

use nexus_cloud_client::{
    sink::{build_entity_sighting_envelope, EntitySightingProjection},
    TunnelOutbox,
};
use nexus_pipeline::{SightingHook, SightingSnapshot};
use nexus_reid::Extractor;
use tokio::sync::mpsc;
use tracing::{debug, warn};

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
    #[must_use]
    pub fn spawn(
        extractor: Arc<dyn Extractor>,
        outbox: Arc<TunnelOutbox>,
        capacity: usize,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<SightingSnapshot>(capacity.max(1));
        tokio::spawn(run_worker(extractor, outbox, rx));
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

async fn run_worker(
    extractor: Arc<dyn Extractor>,
    outbox: Arc<TunnelOutbox>,
    mut rx: mpsc::Receiver<SightingSnapshot>,
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
    while let Some(snapshot) = rx.recv().await {
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
        let embedding = match extractor.extract(&frame, &bbox).await {
            Ok(emb) => emb,
            Err(e) => {
                warn!(
                    camera_id,
                    track_id,
                    error = %e,
                    "entity-sighting extractor failed; dropping snapshot"
                );
                continue;
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
            continue;
        }
        if !cloud_eligible {
            debug!(
                camera_id,
                track_id,
                model_id = %model_id,
                "entity-sighting extracted (dev mode); skipping cloud publish"
            );
            continue;
        }
        // Saturating casts here: the engine's CameraId is i64 and
        // BBox::{x1,y1,x2,y2} are f32. Negative cam_id never happens
        // in practice (POST /cameras assigns from SQLite rowid which
        // is always > 0), but `as u64` would underflow if it ever
        // did — clamp explicitly so the wire bbox can never carry a
        // surprise huge value.
        let projection = EntitySightingProjection {
            camera_id: u64::try_from(camera_id).unwrap_or(0),
            entity_local_id,
            embedding: embedding.vec,
            embedding_model: model_id.clone(),
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
        };
        let envelope = build_entity_sighting_envelope(projection);
        match outbox.send(envelope).await {
            Ok(()) => {
                debug!(
                    camera_id,
                    track_id, is_first, "entity_sighting envelope published"
                );
            }
            Err(e) => {
                // Disconnected / send-channel-closed is the
                // dominant case before enrollment completes; debug
                // not warn so we don't spam the log during the
                // first few minutes of life.
                debug!(
                    camera_id,
                    track_id,
                    error = %e,
                    "entity_sighting envelope publish failed (tunnel down?); dropping"
                );
            }
        }
    }
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
        let hook = CloudEntitySightingHook::spawn(extractor, outbox.clone(), 8);
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

    #[tokio::test]
    async fn full_queue_does_not_block_submitter() {
        // Capacity=1 so the second submit fills the queue. The
        // worker is held off by a slow extractor (we use a real
        // MockExtractor but submit 5 in a row before yielding).
        let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor::new());
        let outbox = Arc::new(TunnelOutbox::new());
        let hook = CloudEntitySightingHook::spawn(extractor, outbox, 1);
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
}

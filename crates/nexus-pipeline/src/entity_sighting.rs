//! Per-camera entity-sighting scheduler.
//!
//! Phase 5.6 · slice 4c-ii. Decides, per stable track on each new
//! frame, whether to fire a [`SightingSnapshot`] into the engine's
//! [`SightingHook`]. The hook is engine-owned and concretely wraps a
//! [`nexus_reid::Extractor`] + [`nexus_cloud_client::CloudConsoleSink`]
//! to turn the snapshot into a wire `entity_sighting` envelope (see
//! `WIRE_PROTOCOL.md §4` / `WEDGE_PLAN.md §4.1`). The scheduler itself
//! is hook-agnostic so this crate can stay free of `nexus-reid` and
//! `nexus-cloud-client` deps — only the engine glues them together.
//!
//! ### Per-track lifecycle
//!
//! * **First emit** on the first frame where the track's `age_frames`
//!   ≥ `min_track_age_frames`. The scheduler mints a UUIDv7 as the
//!   wire `entity_local_id` and stamps `is_first = true`.
//! * **Periodic re-emit** every `emit_interval` of wall-clock after
//!   the first emit, while the track is still seen. `is_first = false`
//!   on every subsequent emit.
//! * **Track GC**: once a track is absent for `track_gc_after` of
//!   wall-clock (default = `2 * emit_interval`), its entry is dropped;
//!   if the same `track_id` appears later, it gets a brand-new
//!   `entity_local_id`. The cloud-side cross-camera linker re-stitches
//!   the global identity via pgvector, so a slightly chatty
//!   `entity_local_id` namespace on the wire is acceptable.
//!
//! ### Hook contract
//!
//! [`SightingHook::submit`] is **synchronous and non-blocking** —
//! the supervisor calls it on the per-frame hot path. The engine's
//! concrete implementation buffers into a bounded channel that a
//! dedicated tokio task drains; back-pressure surfaces as a
//! tracing warn (and a future M_OPS gauge), never as a frame stall.
//! [`NoopSightingHook`] is the default when re-id is disabled in
//! `nexus.toml`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use nexus_types::{BBox, CameraId, Frame, TrackId, TrackedObject};

/// What the engine hook receives per stable-track emit window. Holds
/// an `Arc<Frame>` so the supervisor's per-frame cache clone (already
/// `Arc<Frame>`) is shared cheaply; the engine's extractor reads the
/// supervisor-resolution RGB pixels and crops to `bbox` for embedding.
#[derive(Debug, Clone)]
pub struct SightingSnapshot {
    pub camera_id: CameraId,
    pub track_id: TrackId,
    /// Stable per-track UUIDv7 minted by the scheduler. The cloud
    /// uses `(core_id, entity_local_id)` as the dedup key for a
    /// track and to follow it across re-sends. Capped at 64 bytes
    /// (UUIDv7 string is 36 — well under).
    pub entity_local_id: String,
    pub frame: Arc<Frame>,
    pub bbox: BBox,
    pub confidence: f32,
    /// Wall-clock of the FIRST frame the track was observed on.
    pub started_ts: DateTime<Utc>,
    /// Wall-clock of THIS sighting. Equals `started_ts` for the first
    /// snapshot; > `started_ts` for every periodic re-send.
    pub ts: DateTime<Utc>,
    /// `true` only for the first snapshot a `(camera_id, track_id)`
    /// pair produces in its current lifecycle.
    pub is_first: bool,
}

/// Engine-side sink for [`SightingSnapshot`]s. Implementations MUST
/// be non-blocking — the supervisor calls `submit` synchronously on
/// the per-frame hot path. Hand the snapshot off to an unbounded /
/// bounded channel and return immediately.
pub trait SightingHook: Send + Sync {
    fn submit(&self, snapshot: SightingSnapshot);
}

/// Default no-op hook. Wired when `[reid].enabled = false` in
/// `nexus.toml` or when the engine boots without cloud connectivity.
pub struct NoopSightingHook;

impl SightingHook for NoopSightingHook {
    fn submit(&self, _snapshot: SightingSnapshot) {
        // intentionally empty
    }
}

/// Per-camera scheduler. Owned by the supervisor task; not `Send`
/// because the supervisor is single-threaded per camera. One
/// `SightingScheduler` services one camera's stream of tracked
/// objects.
pub struct SightingScheduler {
    camera_id: CameraId,
    min_track_age_frames: u32,
    emit_interval: Duration,
    track_gc_after: Duration,
    tracks: HashMap<TrackId, TrackState>,
}

#[derive(Debug, Clone)]
struct TrackState {
    entity_local_id: String,
    started_ts: DateTime<Utc>,
    /// `None` until the track first crosses the `min_track_age_frames`
    /// threshold and the first emit fires.
    last_emit_at: Option<DateTime<Utc>>,
    /// `last_seen_at` is updated every frame the track is present.
    last_seen_at: DateTime<Utc>,
}

impl SightingScheduler {
    /// Construct a new scheduler. `min_track_age_frames` is the
    /// minimum tracker age before the first sighting fires (matches
    /// the WEDGE_PLAN's "stable track" definition — filters out
    /// 1-frame false positives). `emit_interval` is the cadence for
    /// periodic re-sends after the first sighting.
    #[must_use]
    pub fn new(camera_id: CameraId, min_track_age_frames: u32, emit_interval: Duration) -> Self {
        let track_gc_after = emit_interval.saturating_mul(2).max(Duration::from_secs(10));
        Self {
            camera_id,
            min_track_age_frames,
            emit_interval,
            track_gc_after,
            tracks: HashMap::new(),
        }
    }

    /// Drive the scheduler with one frame's worth of tracked objects.
    /// Synchronously emits zero or more [`SightingSnapshot`]s via
    /// `hook.submit()` and returns the count of snapshots emitted
    /// (for the supervisor's frame-stats counter).
    pub fn tick(
        &mut self,
        frame: &Arc<Frame>,
        tracked: &[TrackedObject],
        now: DateTime<Utc>,
        hook: &dyn SightingHook,
    ) -> usize {
        // Update / insert per current frame.
        let mut emitted = 0usize;
        for obj in tracked {
            let due = {
                let entry = self
                    .tracks
                    .entry(obj.track_id)
                    .or_insert_with(|| TrackState {
                        entity_local_id: new_local_id(),
                        started_ts: now,
                        last_emit_at: None,
                        last_seen_at: now,
                    });
                entry.last_seen_at = now;
                let stable = obj.age_frames >= self.min_track_age_frames;
                match entry.last_emit_at {
                    None if stable => Some(EmitPlan {
                        entity_local_id: entry.entity_local_id.clone(),
                        started_ts: entry.started_ts,
                        is_first: true,
                    }),
                    Some(prev)
                        if now.signed_duration_since(prev).to_std().unwrap_or_default()
                            >= self.emit_interval =>
                    {
                        Some(EmitPlan {
                            entity_local_id: entry.entity_local_id.clone(),
                            started_ts: entry.started_ts,
                            is_first: false,
                        })
                    }
                    _ => None,
                }
            };
            if let Some(plan) = due {
                hook.submit(SightingSnapshot {
                    camera_id: self.camera_id,
                    track_id: obj.track_id,
                    entity_local_id: plan.entity_local_id,
                    frame: Arc::clone(frame),
                    bbox: obj.bbox,
                    confidence: obj.confidence,
                    started_ts: plan.started_ts,
                    ts: now,
                    is_first: plan.is_first,
                });
                emitted += 1;
                // Re-borrow to stamp last_emit_at now that submit returned.
                if let Some(entry) = self.tracks.get_mut(&obj.track_id) {
                    entry.last_emit_at = Some(now);
                }
            }
        }
        // GC absent tracks.
        let gc_horizon = self.track_gc_after;
        self.tracks.retain(|_, state| {
            now.signed_duration_since(state.last_seen_at)
                .to_std()
                .map(|d| d < gc_horizon)
                .unwrap_or(true)
        });
        emitted
    }
}

struct EmitPlan {
    entity_local_id: String,
    started_ts: DateTime<Utc>,
    is_first: bool,
}

fn new_local_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use nexus_types::{Frame, PixelFormat};
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[derive(Default)]
    struct CaptureHook {
        seen: Mutex<Vec<SightingSnapshot>>,
    }

    impl SightingHook for CaptureHook {
        fn submit(&self, snapshot: SightingSnapshot) {
            self.seen.lock().push(snapshot);
        }
    }

    fn dummy_frame(camera_id: CameraId, captured_at: DateTime<Utc>) -> Arc<Frame> {
        Arc::new(Frame {
            camera_id,
            frame_id: 1,
            captured_at,
            width: 960,
            height: 540,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![0u8; 960 * 540 * 3]),
            trace_id: "test".into(),
        })
    }

    fn tracked(id: TrackId, age: u32) -> TrackedObject {
        TrackedObject {
            track_id: id,
            label: "person".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: 100.0,
                y1: 200.0,
                x2: 250.0,
                y2: 500.0,
            },
            age_frames: age,
            age_ms: u64::from(age) * 33,
            attributes: serde_json::Map::new(),
        }
    }

    #[test]
    fn first_emit_waits_for_min_track_age() {
        let hook = CaptureHook::default();
        let mut sched = SightingScheduler::new(7, 5, Duration::from_secs(5));
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let frame = dummy_frame(7, t0);
        // age=1: too young
        let n = sched.tick(&frame, &[tracked(1, 1)], t0, &hook);
        assert_eq!(n, 0);
        // age=4: still too young
        let n = sched.tick(&frame, &[tracked(1, 4)], t0, &hook);
        assert_eq!(n, 0);
        // age=5: emits, is_first=true
        let n = sched.tick(&frame, &[tracked(1, 5)], t0, &hook);
        assert_eq!(n, 1);
        let seen = hook.seen.lock();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].is_first);
        assert_eq!(seen[0].camera_id, 7);
        assert_eq!(seen[0].track_id, 1);
        assert_eq!(seen[0].started_ts, seen[0].ts);
        assert!(!seen[0].entity_local_id.is_empty());
    }

    #[test]
    fn periodic_emit_respects_interval() {
        let hook = CaptureHook::default();
        let mut sched = SightingScheduler::new(7, 1, Duration::from_secs(5));
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let frame = dummy_frame(7, t0);
        // First emit.
        assert_eq!(sched.tick(&frame, &[tracked(1, 2)], t0, &hook), 1);
        // 1s later — too soon, no emit.
        assert_eq!(
            sched.tick(
                &frame,
                &[tracked(1, 3)],
                t0 + chrono::Duration::seconds(1),
                &hook
            ),
            0
        );
        // 4s after first — still inside interval.
        assert_eq!(
            sched.tick(
                &frame,
                &[tracked(1, 4)],
                t0 + chrono::Duration::seconds(4),
                &hook
            ),
            0
        );
        // 5s after first — fires periodic.
        assert_eq!(
            sched.tick(
                &frame,
                &[tracked(1, 5)],
                t0 + chrono::Duration::seconds(5),
                &hook
            ),
            1
        );
        let seen = hook.seen.lock();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].is_first);
        assert!(!seen[1].is_first);
        // entity_local_id persists across the lifecycle.
        assert_eq!(seen[0].entity_local_id, seen[1].entity_local_id);
        assert_eq!(seen[0].started_ts, seen[1].started_ts);
        assert!(seen[1].ts > seen[1].started_ts);
    }

    #[test]
    fn each_camera_emit_is_independent() {
        let hook = CaptureHook::default();
        let mut sched = SightingScheduler::new(11, 1, Duration::from_secs(5));
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let frame = dummy_frame(11, t0);
        // Three tracks at age=2 → all fire first-emit on this tick.
        let n = sched.tick(
            &frame,
            &[tracked(1, 2), tracked(2, 2), tracked(3, 2)],
            t0,
            &hook,
        );
        assert_eq!(n, 3);
        let seen = hook.seen.lock();
        assert!(seen.iter().all(|s| s.is_first));
        let ids: std::collections::HashSet<_> =
            seen.iter().map(|s| s.entity_local_id.clone()).collect();
        assert_eq!(ids.len(), 3, "each track gets a unique entity_local_id");
    }

    #[test]
    fn absent_track_is_gc_then_new_local_id_on_return() {
        let hook = CaptureHook::default();
        let mut sched = SightingScheduler::new(7, 1, Duration::from_secs(5)); // gc_after = 10s
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let frame = dummy_frame(7, t0);
        sched.tick(&frame, &[tracked(1, 2)], t0, &hook);
        let id_a = hook.seen.lock()[0].entity_local_id.clone();
        // Skip the track for 20s — well past gc_after=10s. Tick with
        // no objects so the scheduler's GC sweep can run.
        sched.tick(&frame, &[], t0 + chrono::Duration::seconds(20), &hook);
        // Same track_id reappears: it's a new lifecycle, new id.
        sched.tick(
            &frame,
            &[tracked(1, 2)],
            t0 + chrono::Duration::seconds(21),
            &hook,
        );
        let seen = hook.seen.lock();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].is_first);
        assert_ne!(
            seen[1].entity_local_id, id_a,
            "post-GC reappearance gets a fresh entity_local_id"
        );
    }

    #[test]
    fn noop_hook_does_not_panic() {
        let mut sched = SightingScheduler::new(1, 1, Duration::from_secs(5));
        let frame = dummy_frame(1, Utc::now());
        let n = sched.tick(&frame, &[tracked(1, 2)], Utc::now(), &NoopSightingHook);
        assert_eq!(n, 1, "noop hook still counts as 'emitted'");
    }

    #[test]
    fn snapshot_shares_frame_arc_without_copy() {
        let hook = CaptureHook::default();
        let mut sched = SightingScheduler::new(1, 1, Duration::from_secs(5));
        let now = Utc::now();
        let frame = dummy_frame(1, now);
        let before_count = Arc::strong_count(&frame);
        sched.tick(&frame, &[tracked(1, 2)], now, &hook);
        let seen = hook.seen.lock();
        assert_eq!(seen.len(), 1);
        // Snapshot holds a strong ref; refcount went up by one (the
        // scheduler doesn't retain a copy).
        assert_eq!(Arc::strong_count(&frame), before_count + 1);
        assert_eq!(seen[0].frame.width, frame.width);
    }
}

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
use rand::{rngs::SmallRng, Rng, SeedableRng};

/// One persisted `(camera_id, track_id) -> entity_local_id` record
/// loaded from `nexus-store` at supervisor boot. Passed to
/// [`SightingScheduler::new_with_persistence`] so the scheduler can
/// re-use the prior `entity_local_id` when the tracker re-issues the
/// same `(camera_id, track_id)` within the GC window after a crash +
/// systemd restart. Phase 5.6 · R4.
#[derive(Debug, Clone)]
pub struct EntityLocalSeed {
    pub camera_id: CameraId,
    pub track_id: TrackId,
    pub entity_local_id: String,
    pub started_ts: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// Update payload pushed to the persistence sink on every emit.
#[derive(Debug, Clone)]
pub struct EntityLocalUpdate {
    pub camera_id: CameraId,
    pub track_id: TrackId,
    pub entity_local_id: String,
    pub started_ts: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// Engine-side sink for [`EntityLocalUpdate`]s. Implementations MUST
/// be non-blocking — the supervisor calls `upsert`/`delete`
/// synchronously on the per-frame hot path. The engine's concrete
/// impl buffers into a bounded mpsc whose worker batches writes
/// against `nexus-store`.
pub trait EntityLocalPersist: Send + Sync {
    fn upsert(&self, update: EntityLocalUpdate);
    fn delete(&self, camera_id: CameraId, track_id: TrackId);
}

/// Default no-op persistence sink. Wired when `[reid].enabled = false`
/// in `nexus.toml`, when the engine boots without a usable store, or
/// when callers explicitly opt out via [`SightingScheduler::new`].
pub struct NoopEntityLocalPersist;

impl EntityLocalPersist for NoopEntityLocalPersist {
    fn upsert(&self, _update: EntityLocalUpdate) {}
    fn delete(&self, _camera_id: CameraId, _track_id: TrackId) {}
}

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
    /// M_PERF_CROWD B2 — above this concurrent-track count the
    /// scheduler swaps the periodic re-emit cadence to
    /// [`crowded_emit_interval`]. `0` (the default) disables the
    /// crowded-mode adaptive cadence — callers must opt in via
    /// [`Self::with_crowded_cadence`].
    crowded_track_threshold: u32,
    crowded_emit_interval: Duration,
    track_gc_after: Duration,
    tracks: HashMap<TrackId, TrackState>,
    /// Pre-loaded `(track_id -> seed)` records from `nexus-store`.
    /// Consumed on first touch of a matching `(camera_id, track_id)`
    /// so the scheduler reuses the prior `entity_local_id` instead
    /// of minting a fresh one. Stale entries (not touched before the
    /// next GC sweep) are dropped along with the in-memory tracks
    /// they would have hydrated.
    seed: HashMap<TrackId, EntityLocalSeed>,
    persist: Arc<dyn EntityLocalPersist>,
    /// M_PERF_CROWD B3 — per-scheduler PRNG used to jitter the
    /// first-emit `last_emit_at` stamp so newly-stable tracks don't
    /// lock-step their periodic re-emit with their siblings on the
    /// same camera. Only consulted when `jitter_first_emit` is true;
    /// callers opt in via [`Self::with_first_emit_jitter`] (prod) or
    /// [`Self::with_rng_seed`] (deterministic tests).
    rng: SmallRng,
    jitter_first_emit: bool,
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
    ///
    /// This constructor opts out of persistence — every restart
    /// mints fresh `entity_local_id`s. Use
    /// [`Self::new_with_persistence`] to wire the
    /// `nexus-store`-backed persistence + seed for crash-resilient
    /// id reuse (Phase 5.6 · R4).
    #[must_use]
    pub fn new(camera_id: CameraId, min_track_age_frames: u32, emit_interval: Duration) -> Self {
        Self::new_with_persistence(
            camera_id,
            min_track_age_frames,
            emit_interval,
            Vec::new(),
            Arc::new(NoopEntityLocalPersist),
        )
    }

    /// Construct a scheduler that hydrates from `seed` on first touch
    /// of each `(camera_id, track_id)` and persists every emit + GC
    /// via `persist`. `seed` is filtered to entries matching this
    /// scheduler's `camera_id`; entries for other cameras are
    /// silently discarded (the engine boot site is expected to
    /// pre-filter, but defending against caller bugs is cheap).
    #[must_use]
    pub fn new_with_persistence(
        camera_id: CameraId,
        min_track_age_frames: u32,
        emit_interval: Duration,
        seed: Vec<EntityLocalSeed>,
        persist: Arc<dyn EntityLocalPersist>,
    ) -> Self {
        let track_gc_after = emit_interval.saturating_mul(2).max(Duration::from_secs(10));
        let seed = seed
            .into_iter()
            .filter(|s| s.camera_id == camera_id)
            .map(|s| (s.track_id, s))
            .collect();
        Self {
            camera_id,
            min_track_age_frames,
            emit_interval,
            crowded_track_threshold: 0,
            crowded_emit_interval: Duration::ZERO,
            track_gc_after,
            tracks: HashMap::new(),
            seed,
            persist,
            rng: SmallRng::seed_from_u64(0),
            jitter_first_emit: false,
        }
    }

    /// M_PERF_CROWD B3 — enable the first-emit jitter using a PRNG
    /// seeded from system entropy. Production callers (engine
    /// supervisor) should chain this on; the constructors leave
    /// jitter disabled so unit tests stay deterministic by default.
    #[must_use]
    pub fn with_first_emit_jitter(mut self) -> Self {
        self.rng = SmallRng::from_entropy();
        self.jitter_first_emit = true;
        self
    }

    /// M_PERF_CROWD B3 — enable the first-emit jitter with a
    /// deterministic PRNG seed. Tests use this to make the jitter
    /// stream reproducible; production callers should prefer
    /// [`Self::with_first_emit_jitter`].
    #[must_use]
    pub fn with_rng_seed(mut self, seed: u64) -> Self {
        self.rng = SmallRng::seed_from_u64(seed);
        self.jitter_first_emit = true;
        self
    }

    /// M_PERF_CROWD B2 — enable the adaptive re-emit cadence. When
    /// the current tick sees more than `threshold` concurrent tracked
    /// objects, the scheduler uses `crowded_interval` for the periodic
    /// re-emit branch instead of `emit_interval`. First-emit is
    /// unaffected so freshly-stable entities still get linked
    /// promptly. `threshold = 0` disables crowded mode entirely.
    #[must_use]
    pub fn with_crowded_cadence(mut self, threshold: u32, crowded_interval: Duration) -> Self {
        self.crowded_track_threshold = threshold;
        self.crowded_emit_interval = crowded_interval;
        self
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
        // M_PERF_CROWD B2 — pick the periodic re-emit cadence based
        // on the current per-camera tracked-object count. Threshold
        // 0 disables crowded mode (always use the regular interval).
        let periodic_interval = if self.crowded_track_threshold > 0
            && tracked.len() > self.crowded_track_threshold as usize
        {
            self.crowded_emit_interval
        } else {
            self.emit_interval
        };
        // Update / insert per current frame.
        let mut emitted = 0usize;
        for obj in tracked {
            let due = {
                let seed = self.seed.remove(&obj.track_id);
                let entry = self.tracks.entry(obj.track_id).or_insert_with(|| {
                    if let Some(s) = seed {
                        TrackState {
                            entity_local_id: s.entity_local_id,
                            started_ts: s.started_ts,
                            last_emit_at: None,
                            last_seen_at: s.last_seen_at,
                        }
                    } else {
                        TrackState {
                            entity_local_id: new_local_id(),
                            started_ts: now,
                            last_emit_at: None,
                            last_seen_at: now,
                        }
                    }
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
                            >= periodic_interval =>
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
                    entity_local_id: plan.entity_local_id.clone(),
                    frame: Arc::clone(frame),
                    bbox: obj.bbox,
                    confidence: obj.confidence,
                    started_ts: plan.started_ts,
                    ts: now,
                    is_first: plan.is_first,
                });
                emitted += 1;
                // M_PERF_CROWD B3 — back-date the first-emit stamp
                // by a random offset in [0, emit_interval) so this
                // track's next periodic re-emit phase-shifts away
                // from its siblings'. Periodic re-emits use `now`
                // unchanged so the jitter doesn't compound. Jitter
                // is opt-in via `with_first_emit_jitter` / seed.
                let stamp = if plan.is_first && self.jitter_first_emit {
                    let interval_ms = self.emit_interval.as_millis().min(i64::MAX as u128) as i64;
                    if interval_ms > 0 {
                        let jitter_ms = self.rng.gen_range(0..interval_ms);
                        now - chrono::Duration::milliseconds(jitter_ms)
                    } else {
                        now
                    }
                } else {
                    now
                };
                // Re-borrow to stamp last_emit_at now that submit returned.
                if let Some(entry) = self.tracks.get_mut(&obj.track_id) {
                    entry.last_emit_at = Some(stamp);
                }
                self.persist.upsert(EntityLocalUpdate {
                    camera_id: self.camera_id,
                    track_id: obj.track_id,
                    entity_local_id: plan.entity_local_id,
                    started_ts: plan.started_ts,
                    last_seen_at: now,
                });
            }
        }
        // GC absent tracks. Collect first so we can notify the
        // persistence sink for every dropped track.
        let gc_horizon = self.track_gc_after;
        let mut gc_drops: Vec<TrackId> = Vec::new();
        self.tracks.retain(|track_id, state| {
            let keep = now
                .signed_duration_since(state.last_seen_at)
                .to_std()
                .map(|d| d < gc_horizon)
                .unwrap_or(true);
            if !keep {
                gc_drops.push(*track_id);
            }
            keep
        });
        // Also drop any seed entries that were never touched and are
        // now past the GC horizon — keeps the seed map from growing
        // unbounded when the tracker never re-issues a stale id.
        self.seed.retain(|_, s| {
            now.signed_duration_since(s.last_seen_at)
                < chrono::Duration::from_std(gc_horizon)
                    .unwrap_or_else(|_| chrono::Duration::seconds(0))
        });
        for track_id in gc_drops {
            self.persist.delete(self.camera_id, track_id);
        }
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

    // ------------------------------------------------------------------
    // Phase 5.6 · R4 — seed + persistence sink coverage.
    // ------------------------------------------------------------------

    #[derive(Default, Debug)]
    struct CapturePersist {
        upserts: Mutex<Vec<EntityLocalUpdate>>,
        deletes: Mutex<Vec<(CameraId, TrackId)>>,
    }

    impl EntityLocalPersist for CapturePersist {
        fn upsert(&self, update: EntityLocalUpdate) {
            self.upserts.lock().push(update);
        }
        fn delete(&self, camera_id: CameraId, track_id: TrackId) {
            self.deletes.lock().push((camera_id, track_id));
        }
    }

    #[test]
    fn seeded_track_reuses_prior_entity_local_id() {
        let hook = CaptureHook::default();
        let persist = Arc::new(CapturePersist::default());
        let t_pre = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let seed = vec![EntityLocalSeed {
            camera_id: 7,
            track_id: 42,
            entity_local_id: "eid-from-disk".into(),
            started_ts: t_pre,
            last_seen_at: t_pre + chrono::Duration::seconds(2),
        }];
        let mut sched = SightingScheduler::new_with_persistence(
            7,
            1,
            Duration::from_secs(5),
            seed,
            persist.clone(),
        );
        // Restart "now" is 8s after the seed's last_seen_at — still
        // inside the GC window (2 * emit_interval = 10s).
        let t_now = t_pre + chrono::Duration::seconds(10);
        let frame = dummy_frame(7, t_now);
        let n = sched.tick(&frame, &[tracked(42, 2)], t_now, &hook);
        assert_eq!(n, 1);
        let seen = hook.seen.lock();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].is_first);
        assert_eq!(
            seen[0].entity_local_id, "eid-from-disk",
            "seeded entity_local_id is reused after restart"
        );
        assert_eq!(seen[0].started_ts, t_pre, "seeded started_ts is preserved");
        // Persistence sink saw the upsert with the reused id.
        let upserts = persist.upserts.lock();
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].entity_local_id, "eid-from-disk");
        assert_eq!(upserts[0].started_ts, t_pre);
        assert_eq!(upserts[0].last_seen_at, t_now);
    }

    #[test]
    fn seed_for_other_camera_is_ignored() {
        let hook = CaptureHook::default();
        let persist = Arc::new(CapturePersist::default());
        let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let seed = vec![EntityLocalSeed {
            camera_id: 99, // not us
            track_id: 42,
            entity_local_id: "eid-other-cam".into(),
            started_ts: t,
            last_seen_at: t,
        }];
        let mut sched = SightingScheduler::new_with_persistence(
            7,
            1,
            Duration::from_secs(5),
            seed,
            persist.clone(),
        );
        let frame = dummy_frame(7, t);
        sched.tick(&frame, &[tracked(42, 2)], t, &hook);
        let seen = hook.seen.lock();
        assert_eq!(seen.len(), 1);
        assert_ne!(seen[0].entity_local_id, "eid-other-cam");
    }

    #[test]
    fn persist_delete_fires_on_gc() {
        let hook = CaptureHook::default();
        let persist = Arc::new(CapturePersist::default());
        let mut sched = SightingScheduler::new_with_persistence(
            7,
            1,
            Duration::from_secs(5),
            Vec::new(),
            persist.clone(),
        );
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let frame = dummy_frame(7, t0);
        sched.tick(&frame, &[tracked(42, 2)], t0, &hook);
        // 20s with no objects — well past gc_after=10s.
        sched.tick(&frame, &[], t0 + chrono::Duration::seconds(20), &hook);
        let deletes = persist.deletes.lock();
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0], (7, 42));
    }

    // ---- M_PERF_CROWD B2 — adaptive re-id cadence ----

    /// Build a slice of `count` distinct tracks, all aged past
    /// `min_track_age_frames`, so first-emit fires on the first tick
    /// for every one of them.
    fn many_tracked(count: u64) -> Vec<TrackedObject> {
        (1..=count).map(|id| tracked(id, 10)).collect()
    }

    #[test]
    fn b2_crowded_threshold_zero_keeps_regular_cadence() {
        // crowded_threshold = 0 disables adaptive mode entirely:
        // even with 50 tracks, the scheduler must keep using the
        // regular `emit_interval`.
        let hook = CaptureHook::default();
        let mut sched = SightingScheduler::new(7, 1, Duration::from_secs(5))
            .with_crowded_cadence(0, Duration::from_secs(15));
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let frame = dummy_frame(7, t0);
        let many = many_tracked(50);
        // First-emit tick fires once per track regardless of cadence.
        assert_eq!(sched.tick(&frame, &many, t0, &hook), 50);
        hook.seen.lock().clear();
        // At t+5s (= regular emit_interval), periodic re-emit must fire
        // for all 50 because crowded mode is disabled.
        let t5 = t0 + chrono::Duration::seconds(5);
        assert_eq!(sched.tick(&frame, &many, t5, &hook), 50);
    }

    #[test]
    fn b2_crowded_cadence_throttles_periodic_emit() {
        // 50 tracks > threshold 15 → periodic re-emit must wait for
        // crowded_emit_interval (15s) instead of emit_interval (5s).
        let hook = CaptureHook::default();
        let mut sched = SightingScheduler::new(7, 1, Duration::from_secs(5))
            .with_crowded_cadence(15, Duration::from_secs(15));
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let frame = dummy_frame(7, t0);
        let many = many_tracked(50);
        // First-emit at t0 — adaptive cadence does NOT gate first-emit.
        assert_eq!(sched.tick(&frame, &many, t0, &hook), 50);
        hook.seen.lock().clear();
        // t0+5s: regular emit_interval has elapsed but crowded cadence
        // has not — no periodic emits.
        let t5 = t0 + chrono::Duration::seconds(5);
        assert_eq!(sched.tick(&frame, &many, t5, &hook), 0);
        // t0+14s: still below crowded 15s threshold.
        let t14 = t0 + chrono::Duration::seconds(14);
        assert_eq!(sched.tick(&frame, &many, t14, &hook), 0);
        // t0+15s: crowded interval elapsed → re-emit fires for all 50.
        let t15 = t0 + chrono::Duration::seconds(15);
        assert_eq!(sched.tick(&frame, &many, t15, &hook), 50);
    }

    #[test]
    fn b2_below_threshold_uses_regular_cadence() {
        // 10 tracks <= threshold 15 → regular emit_interval (5s) wins.
        let hook = CaptureHook::default();
        let mut sched = SightingScheduler::new(7, 1, Duration::from_secs(5))
            .with_crowded_cadence(15, Duration::from_secs(15));
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let frame = dummy_frame(7, t0);
        let few = many_tracked(10);
        assert_eq!(sched.tick(&frame, &few, t0, &hook), 10);
        hook.seen.lock().clear();
        // t0+5s — regular cadence fires.
        let t5 = t0 + chrono::Duration::seconds(5);
        assert_eq!(sched.tick(&frame, &few, t5, &hook), 10);
    }

    #[test]
    fn b2_crowded_first_emit_still_prompt() {
        // First-emit for freshly-stable tracks must NOT be throttled
        // by crowded mode — the cloud linker needs them promptly.
        // A 50-track scene + a brand-new 51st stable track at t+1s
        // must see the 51st fire first-emit at t+1s.
        let hook = CaptureHook::default();
        let mut sched = SightingScheduler::new(7, 1, Duration::from_secs(5))
            .with_crowded_cadence(15, Duration::from_secs(15));
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let frame = dummy_frame(7, t0);
        let many = many_tracked(50);
        assert_eq!(sched.tick(&frame, &many, t0, &hook), 50);
        hook.seen.lock().clear();
        // t+1s: same 50 tracks (no periodic — crowded cadence) plus a
        // brand-new track 99 at age=2 (just crossed min_track_age 1).
        let mut next = many.clone();
        next.push(tracked(99, 2));
        let t1 = t0 + chrono::Duration::seconds(1);
        assert_eq!(sched.tick(&frame, &next, t1, &hook), 1);
        let seen = hook.seen.lock();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].is_first);
        assert_eq!(seen[0].track_id, 99);
    }

    // ------------------------------------------------------------------
    // M_PERF_CROWD B3 — first-emit timestamp jitter.
    // ------------------------------------------------------------------

    /// Drive a single track to its first emit at `t0`, then tick at
    /// `t0 + step` for step in 1..emit_interval_ms and return the
    /// offset (ms after t0) at which the second emit fires. Returns
    /// `None` if no second emit fires within `emit_interval_ms`.
    fn second_emit_offset_ms(sched: &mut SightingScheduler) -> Option<i64> {
        let hook = CaptureHook::default();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let frame = dummy_frame(7, t0);
        // First emit at t0 (age=2 > min_track_age=1).
        assert_eq!(sched.tick(&frame, &[tracked(1, 2)], t0, &hook), 1);
        hook.seen.lock().clear();
        for ms in 1..=5_000 {
            let t = t0 + chrono::Duration::milliseconds(ms);
            if sched.tick(&frame, &[tracked(1, 3)], t, &hook) == 1 {
                return Some(ms);
            }
        }
        None
    }

    #[test]
    fn b3_first_emit_back_dates_so_second_emit_fires_early() {
        // With a fixed seed, the first emit's `last_emit_at` is
        // back-dated by some 0..5000ms offset, so the next periodic
        // emit fires strictly before 5000ms after `t0` (assuming the
        // RNG didn't draw exactly 0 — seed 0xB3 below draws > 0).
        let mut sched = SightingScheduler::new(7, 1, Duration::from_secs(5)).with_rng_seed(0xB3);
        let offset = second_emit_offset_ms(&mut sched).expect("second emit must fire");
        assert!(
            offset < 5_000,
            "back-dated first-emit should make second emit fire before full interval, got {offset}ms",
        );
        assert!(
            offset > 0,
            "second emit can't fire at the same tick as first"
        );
    }

    #[test]
    fn b3_seeded_jitter_is_deterministic() {
        // Two schedulers with the same seed must produce identical
        // second-emit offsets — that's the contract the supervisor
        // relies on for reproducible test scenarios.
        let mut a = SightingScheduler::new(7, 1, Duration::from_secs(5)).with_rng_seed(0xB3);
        let mut b = SightingScheduler::new(7, 1, Duration::from_secs(5)).with_rng_seed(0xB3);
        assert_eq!(second_emit_offset_ms(&mut a), second_emit_offset_ms(&mut b));
    }

    #[test]
    fn b3_different_seeds_produce_different_phase_offsets() {
        // The whole point of B3 is that sibling tracks land on
        // different jitter offsets. Two distinct seeds must produce
        // distinct second-emit offsets for the same input.
        let mut a = SightingScheduler::new(7, 1, Duration::from_secs(5)).with_rng_seed(0xB3);
        let mut b = SightingScheduler::new(7, 1, Duration::from_secs(5)).with_rng_seed(0xC4);
        assert_ne!(second_emit_offset_ms(&mut a), second_emit_offset_ms(&mut b));
    }

    #[test]
    fn b3_zero_emit_interval_does_not_panic() {
        // Defensive: if a config bug ever lands `emit_interval = 0`,
        // the jitter call must not panic on `gen_range(0..0)`.
        let hook = CaptureHook::default();
        let mut sched = SightingScheduler::new(7, 1, Duration::ZERO).with_rng_seed(0xB3);
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let frame = dummy_frame(7, t0);
        assert_eq!(sched.tick(&frame, &[tracked(1, 2)], t0, &hook), 1);
    }
}

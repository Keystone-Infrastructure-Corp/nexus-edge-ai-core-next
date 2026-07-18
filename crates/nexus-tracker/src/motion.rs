//! Motion-event emitter — M2.1 Stage A.
//!
//! Sits between the tracker output and the supervisor's per-frame
//! flush. The tracker exposes a snapshot per frame
//! (`Vec<TrackedObject>`) and offers no lifecycle stream of its own,
//! so this module diffs the per-camera live track-id set across
//! frames to produce three kinds of events:
//!
//! * `Born`    — first frame a track id appears for a camera.
//! * `Updated` — track persists; gated to `motion_events_sample_hz`
//!   so we don't emit one row per detection at 30 fps.
//! * `Died`    — track id disappears (the supervisor sees only
//!   Confirmed-or-better tracks, so the moment a Confirmed track
//!   stops being returned by `tracker.update` is when ByteTrack has
//!   moved it to Lost / dropped it past `max_lost_frames`).
//!
//! The emitter is **pure** — it owns no I/O, no DB handles, no
//! recorder. The supervisor (PR4 of M2.1 Stage A) is what turns
//! these decisions into `store.insert_motion_event(...)` rows + the
//! matching `recorder.open / close` calls. Keeping the decision
//! logic free of I/O makes the corner cases (sample-rate jitter,
//! camera FPS changes, large skips between frames) trivially
//! testable.
//!
//! `clip_id` is intentionally NOT carried by [`MotionDecision`]:
//! the supervisor opens/owns the clip and stamps `clip_id` on the
//! row at write-time. This is what the schema-level
//! `motion_events.clip_id NOT NULL` invariant requires.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use nexus_types::{BBox, CameraId, TrackId, TrackedObject};

/// Lifecycle kind for a [`MotionDecision`]. Mirrors
/// `nexus_store::MotionEventKind` deliberately so the supervisor's
/// translation is a 1:1 enum map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionKind {
    Born,
    Updated,
    Died,
}

/// What the emitter decided about a single track for a single frame.
/// The supervisor turns each decision into an `insert_motion_event`
/// call (and, for `Born`, asks the recorder to open a clip if none
/// is open for the camera).
#[derive(Debug, Clone)]
pub struct MotionDecision {
    pub camera_id: CameraId,
    pub track_id: TrackId,
    pub kind: MotionKind,
    pub captured_at: DateTime<Utc>,
    pub bbox: BBox,
    pub label: String,
    pub confidence: f32,
    /// Snapshot of the tracker / annotator attribute map at the
    /// moment of the lifecycle event. Cloned so the supervisor's
    /// downstream mutation of the live `TrackedObject` doesn't
    /// rewrite history.
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

/// Per-track bookkeeping kept alive between frames so Died can carry
/// the last-known bbox/label/confidence/attributes (the snapshot
/// disappeared from `tracker.update`, but operators care WHERE the
/// track was when it was lost — for example to draw a fade-out arrow
/// on the live view).
#[derive(Debug, Clone)]
struct TrackSnapshot {
    /// Last time we emitted Updated, or None if only Born has been
    /// emitted so far. Used to gate sampling.
    last_emitted_at: Option<DateTime<Utc>>,
    /// Last bbox/label/confidence/attributes seen for the track.
    /// These are what Died will publish.
    last_bbox: BBox,
    last_label: String,
    last_confidence: f32,
    last_attributes: serde_json::Map<String, serde_json::Value>,
}

/// Shared state for one camera's open tracks.
#[derive(Debug, Default)]
struct CameraState {
    tracks: HashMap<TrackId, TrackSnapshot>,
}

/// Pure decision engine. One instance owns the per-camera bookkeeping
/// for ALL cameras the supervisor handles, but in the typical wiring
/// the supervisor will create one emitter PER camera task and never
/// share it (so the locking is trivial). Either pattern is fine —
/// the API is `&mut self`.
pub struct MotionEventEmitter {
    /// `1.0` = at most one Updated per track per second. `0.0` =
    /// no Updated events at all (only Born + Died). Negative or NaN
    /// values are treated as `0.0`.
    sample_hz: f32,
    cameras: HashMap<CameraId, CameraState>,
}

impl MotionEventEmitter {
    pub fn new(sample_hz: f32) -> Self {
        Self {
            sample_hz: if sample_hz.is_finite() && sample_hz > 0.0 {
                sample_hz
            } else {
                0.0
            },
            cameras: HashMap::new(),
        }
    }

    /// Effective minimum gap between Updated emits for a single
    /// track. `None` means "no Updated events at all".
    fn update_interval(&self) -> Option<Duration> {
        if self.sample_hz <= 0.0 {
            None
        } else {
            Some(Duration::from_micros((1_000_000.0 / self.sample_hz) as u64))
        }
    }

    /// Process a per-camera frame. Returns the lifecycle decisions
    /// produced by this frame in stable order: Born events first
    /// (sorted by track_id), then Updated, then Died. The supervisor
    /// can reuse this ordering when batching writes.
    ///
    /// `now` is the frame's `captured_at`; passing wall-clock would
    /// also work but `captured_at` is what the rest of the pipeline
    /// uses so the rows line up time-wise with `events.captured_at`.
    pub fn tick(
        &mut self,
        camera_id: CameraId,
        tracked: &[TrackedObject],
        now: DateTime<Utc>,
    ) -> Vec<MotionDecision> {
        let interval = self.update_interval();
        let state = self.cameras.entry(camera_id).or_default();

        let live: HashSet<TrackId> = tracked.iter().map(|t| t.track_id).collect();
        let mut born = Vec::new();
        let mut updated = Vec::new();

        for t in tracked {
            match state.tracks.get(&t.track_id) {
                None => {
                    state.tracks.insert(
                        t.track_id,
                        TrackSnapshot {
                            last_emitted_at: None,
                            last_bbox: t.bbox,
                            last_label: t.label.clone(),
                            last_confidence: t.confidence,
                            last_attributes: t.attributes.clone(),
                        },
                    );
                    born.push(decision(camera_id, MotionKind::Born, now, t));
                }
                Some(snap) => {
                    let last_emit = snap.last_emitted_at;
                    let due = match (interval, last_emit) {
                        (None, _) => false,
                        (Some(_), None) => false,
                        (Some(int), Some(prev)) => {
                            // chrono Duration -> std::time::Duration
                            // is fallible only for negative spans;
                            // use total_microseconds to stay precise.
                            let gap = (now - prev).num_microseconds().unwrap_or(i64::MAX);
                            gap >= int.as_micros() as i64
                        }
                    };
                    // Refresh the last-known snapshot every frame so
                    // Died always carries the most-recent bbox /
                    // label / attributes, regardless of whether we
                    // emitted an Updated this frame.
                    let snap = state.tracks.get_mut(&t.track_id).unwrap();
                    snap.last_bbox = t.bbox;
                    snap.last_label = t.label.clone();
                    snap.last_confidence = t.confidence;
                    snap.last_attributes = t.attributes.clone();
                    if due {
                        snap.last_emitted_at = Some(now);
                        updated.push(decision(camera_id, MotionKind::Updated, now, t));
                    }
                }
            }
        }

        // Promote Born tracks to "ready for Updated after one full
        // interval has elapsed" by stamping them at `now` AFTER the
        // born event went out. Doing this in a second pass keeps the
        // first-sample-interval-after-born grace period.
        for d in &born {
            if let Some(snap) = state.tracks.get_mut(&d.track_id) {
                snap.last_emitted_at = Some(now);
            }
        }

        // Died: anything we knew about that's not in `live` this
        // frame. Use the last-known snapshot so the decision carries
        // the operator-useful bbox / label / attributes from the
        // last frame the track was visible. The supervisor stamps
        // the previously-known clip_id at write time.
        let dead: Vec<TrackId> = state
            .tracks
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        let mut died = Vec::with_capacity(dead.len());
        for track_id in dead {
            let snap = state.tracks.remove(&track_id).expect("just enumerated");
            died.push(MotionDecision {
                camera_id,
                track_id,
                kind: MotionKind::Died,
                captured_at: now,
                bbox: snap.last_bbox,
                label: snap.last_label,
                confidence: snap.last_confidence,
                attributes: snap.last_attributes,
            });
        }

        let mut out = Vec::with_capacity(born.len() + updated.len() + died.len());
        born.sort_by_key(|d| d.track_id);
        updated.sort_by_key(|d| d.track_id);
        died.sort_by_key(|d| d.track_id);
        out.extend(born);
        out.extend(updated);
        out.extend(died);
        out
    }

    /// Drop all per-camera state. Used when the supervisor restarts
    /// a camera after a stream loss, since track ids only mean
    /// anything within a single ByteTrack instance.
    pub fn forget_camera(&mut self, camera_id: CameraId) {
        self.cameras.remove(&camera_id);
    }

    /// Number of tracks currently considered live for the camera.
    /// Exposed for tests + future health endpoints.
    #[doc(hidden)]
    pub fn live_track_count(&self, camera_id: CameraId) -> usize {
        self.cameras
            .get(&camera_id)
            .map(|s| s.tracks.len())
            .unwrap_or(0)
    }
}

fn decision(
    camera_id: CameraId,
    kind: MotionKind,
    captured_at: DateTime<Utc>,
    t: &TrackedObject,
) -> MotionDecision {
    MotionDecision {
        camera_id,
        track_id: t.track_id,
        kind,
        captured_at,
        bbox: t.bbox,
        label: t.label.clone(),
        confidence: t.confidence,
        attributes: t.attributes.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 13, 22, 0, 0).unwrap()
    }

    fn tobj(id: TrackId, label: &str) -> TrackedObject {
        let mut attrs = serde_json::Map::new();
        attrs.insert("tracking.hit_streak".into(), json!(3));
        TrackedObject {
            track_id: id,
            label: label.into(),
            confidence: 0.9,
            bbox: BBox {
                x1: 10.0,
                y1: 20.0,
                x2: 100.0,
                y2: 200.0,
            },
            detection_bbox: None,
            age_frames: 1,
            age_ms: 33,
            attributes: attrs,
        }
    }

    #[test]
    fn first_frame_for_track_emits_born() {
        let mut em = MotionEventEmitter::new(1.0);
        let out = em.tick(7, &[tobj(1, "person")], t0());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].camera_id, 7);
        assert_eq!(out[0].track_id, 1);
        assert_eq!(out[0].kind, MotionKind::Born);
        assert_eq!(out[0].label, "person");
        assert_eq!(em.live_track_count(7), 1);
    }

    #[test]
    fn updated_is_gated_by_sample_interval() {
        let mut em = MotionEventEmitter::new(1.0); // 1 Hz -> 1s gap
        let now = t0();
        let _ = em.tick(7, &[tobj(1, "person")], now);

        // 100 ms later: too soon, no Updated.
        let snap2 = em.tick(
            7,
            &[tobj(1, "person")],
            now + chrono::Duration::milliseconds(100),
        );
        assert!(snap2.is_empty(), "100 ms < 1 s should not emit Updated");

        // 1.5 s later: due.
        let snap3 = em.tick(
            7,
            &[tobj(1, "person")],
            now + chrono::Duration::milliseconds(1_500),
        );
        assert_eq!(snap3.len(), 1);
        assert_eq!(snap3[0].kind, MotionKind::Updated);

        // 200 ms after that Updated: too soon again.
        let snap4 = em.tick(
            7,
            &[tobj(1, "person")],
            now + chrono::Duration::milliseconds(1_700),
        );
        assert!(snap4.is_empty());
    }

    #[test]
    fn track_disappearing_emits_died_and_clears_state() {
        let mut em = MotionEventEmitter::new(1.0);
        let _ = em.tick(7, &[tobj(1, "person")], t0());
        let out = em.tick(7, &[], t0() + chrono::Duration::milliseconds(50));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, MotionKind::Died);
        assert_eq!(out[0].track_id, 1);
        assert_eq!(em.live_track_count(7), 0);

        // Same id reappearing later is a NEW Born, not a re-Updated.
        let out2 = em.tick(7, &[tobj(1, "person")], t0() + chrono::Duration::seconds(5));
        assert_eq!(out2.len(), 1);
        assert_eq!(out2[0].kind, MotionKind::Born);
    }

    #[test]
    fn died_carries_last_known_bbox_and_label() {
        // Operator-useful regression: when a track disappears, the
        // Died decision should carry where + what the track was on
        // its LAST visible frame — not zeros.
        let mut em = MotionEventEmitter::new(0.0); // no Updateds, focus the test
        let now = t0();
        let _ = em.tick(7, &[tobj(1, "person")], now);

        // Frame 2: same id, MOVED bbox.
        let mut moved = tobj(1, "person");
        moved.bbox = BBox {
            x1: 500.0,
            y1: 600.0,
            x2: 540.0,
            y2: 700.0,
        };
        moved.label = "person".into();
        moved.confidence = 0.42;
        let _ = em.tick(
            7,
            &[moved.clone()],
            now + chrono::Duration::milliseconds(33),
        );

        // Frame 3: track gone -> Died should carry the moved bbox /
        // confidence, not the original tobj() bbox.
        let died = em.tick(7, &[], now + chrono::Duration::milliseconds(66));
        assert_eq!(died.len(), 1);
        assert_eq!(died[0].kind, MotionKind::Died);
        assert_eq!(died[0].bbox.x1, 500.0);
        assert_eq!(died[0].bbox.y2, 700.0);
        assert_eq!(died[0].label, "person");
        assert!((died[0].confidence - 0.42).abs() < 1e-6);
    }

    #[test]
    fn cameras_are_isolated() {
        let mut em = MotionEventEmitter::new(1.0);
        let _ = em.tick(7, &[tobj(1, "person")], t0());
        let _ = em.tick(8, &[tobj(1, "person")], t0());
        assert_eq!(em.live_track_count(7), 1);
        assert_eq!(em.live_track_count(8), 1);

        let out = em.tick(7, &[], t0() + chrono::Duration::seconds(1));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].camera_id, 7);
        assert_eq!(em.live_track_count(8), 1, "camera 8 should be untouched");
    }

    #[test]
    fn sample_hz_zero_disables_updated_but_keeps_born_and_died() {
        let mut em = MotionEventEmitter::new(0.0);
        let now = t0();
        let born = em.tick(7, &[tobj(1, "person")], now);
        assert_eq!(born[0].kind, MotionKind::Born);
        // Many frames later — still no Updated.
        for i in 1..30 {
            let frame_t = now + chrono::Duration::milliseconds(i * 33);
            let out = em.tick(7, &[tobj(1, "person")], frame_t);
            assert!(out.is_empty(), "sample_hz=0 must not emit Updated");
        }
        let died = em.tick(7, &[], now + chrono::Duration::seconds(2));
        assert_eq!(died.len(), 1);
        assert_eq!(died[0].kind, MotionKind::Died);
    }

    #[test]
    fn born_does_not_immediately_emit_updated_in_same_frame() {
        // Even with very high sample_hz, the first Updated should
        // wait until at least one full interval elapses past Born.
        let mut em = MotionEventEmitter::new(1000.0); // 1 ms interval
        let now = t0();
        let out = em.tick(7, &[tobj(1, "person")], now);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].kind,
            MotionKind::Born,
            "no double-fire on born frame"
        );

        // 5 ms later (> 1 ms interval): Updated due.
        let out2 = em.tick(
            7,
            &[tobj(1, "person")],
            now + chrono::Duration::milliseconds(5),
        );
        assert_eq!(out2.len(), 1);
        assert_eq!(out2[0].kind, MotionKind::Updated);
    }

    #[test]
    fn forget_camera_drops_state() {
        let mut em = MotionEventEmitter::new(1.0);
        let _ = em.tick(7, &[tobj(1, "person"), tobj(2, "car")], t0());
        assert_eq!(em.live_track_count(7), 2);
        em.forget_camera(7);
        assert_eq!(em.live_track_count(7), 0);

        // Reappearing tracks register as Born again.
        let out = em.tick(7, &[tobj(1, "person")], t0() + chrono::Duration::seconds(1));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, MotionKind::Born);
    }

    #[test]
    fn mixed_born_updated_died_ordering() {
        // Frame 1: t1, t2 born.
        // Frame 2 (1.5 s later): t1 still alive (Updated due), t2 gone (Died), t3 new (Born).
        let mut em = MotionEventEmitter::new(1.0);
        let now = t0();
        let _ = em.tick(7, &[tobj(1, "person"), tobj(2, "car")], now);

        let later = now + chrono::Duration::milliseconds(1_500);
        let out = em.tick(7, &[tobj(1, "person"), tobj(3, "dog")], later);

        // Order: Born first (track 3), then Updated (1), then Died (2).
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].kind, MotionKind::Born);
        assert_eq!(out[0].track_id, 3);
        assert_eq!(out[1].kind, MotionKind::Updated);
        assert_eq!(out[1].track_id, 1);
        assert_eq!(out[2].kind, MotionKind::Died);
        assert_eq!(out[2].track_id, 2);
    }
}

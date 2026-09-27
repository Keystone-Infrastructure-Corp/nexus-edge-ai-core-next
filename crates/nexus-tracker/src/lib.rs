//! Object trackers behind a single trait, plus a per-camera
//! [`TrackAnnotator`] that stamps motion / dwell / zone / group
//! attributes onto each track post-update.
//!
//! * [`IouNaiveTracker`] — nearest-IoU + TTL. Cheapest, used as a
//!   fallback / smoke-test path.
//! * [`ByteTrackTracker`] — full ByteTrack (real impl in
//!   [`bytetrack`]). Default backend at v1 parity from M4 onward.
//! * [`TrackAnnotator`] — real annotator (real impl in [`annotator`]).
//!
//! Trackers are per-camera (one instance per pipeline). All state is owned
//! inside the implementation; the pipeline only calls `update`. The
//! annotator is also per-camera and owned by the supervisor task; it
//! mutates the post-tracker [`TrackedObject`] list in place.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use nexus_config::TrackerConfig;
use nexus_types::{BBox, Detection, TrackId, TrackedObject};
use parking_lot::Mutex;

pub mod annotator;
pub mod bytetrack;
pub mod motion;
mod rate;
pub mod static_object;
pub mod zone_filter;
pub use annotator::TrackAnnotator;
pub use bytetrack::ByteTrackTracker;
pub use motion::{MotionDecision, MotionEventEmitter, MotionKind};
pub use static_object::{
    is_object_static, StaticObjectFilter, ALERT_EPOCH_ATTRIBUTE_KEY, EMA_ATTRIBUTE_KEY,
    MOVING_FRAMES_ATTRIBUTE_KEY, STATIC_ATTRIBUTE_KEY, STATIC_FRAMES_ATTRIBUTE_KEY,
};
pub use zone_filter::{filter_excluded_zones, filter_zone_min_area};

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

pub trait Tracker: Send + Sync {
    /// Associate one frame's detections. `captured_at` is that frame's
    /// capture time; a track's `age_ms` is measured in it.
    fn update(&self, detections: Vec<Detection>, captured_at: DateTime<Utc>) -> Vec<TrackedObject>;
    fn name(&self) -> &'static str;
}

pub fn build_tracker(cfg: &TrackerConfig) -> Box<dyn Tracker> {
    match cfg.backend {
        nexus_config::TrackerBackendKind::IouNaive => Box::new(IouNaiveTracker::new(cfg)),
        nexus_config::TrackerBackendKind::Bytetrack => {
            Box::new(ByteTrackTracker::new(cfg.bytetrack.clone()))
        }
    }
}

// ---------------------------------------------------------------------------
// Naive IoU tracker (production-grade enough for M0)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ActiveTrack {
    id: TrackId,
    label: String,
    bbox: BBox,
    confidence: f32,
    born_at: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    age_frames: u32,
}

pub struct IouNaiveTracker {
    inner: Mutex<IouState>,
    iou_threshold: f32,
    ttl: Duration,
}

struct IouState {
    next_id: TrackId,
    active: HashMap<TrackId, ActiveTrack>,
}

impl IouNaiveTracker {
    /// Build a tracker with the supplied IoU threshold and
    /// track-TTL. Cheap to construct — the only allocation is
    /// the empty `HashMap` behind the inner `Mutex`.
    pub fn new(cfg: &TrackerConfig) -> Self {
        Self {
            inner: Mutex::new(IouState {
                next_id: 1,
                active: HashMap::new(),
            }),
            iou_threshold: cfg.iou_threshold,
            ttl: Duration::from_millis(cfg.track_ttl_ms),
        }
    }
}

impl Tracker for IouNaiveTracker {
    fn update(&self, detections: Vec<Detection>, captured_at: DateTime<Utc>) -> Vec<TrackedObject> {
        let now = captured_at;
        let mut state = self.inner.lock();

        // Drop tracks unseen for `ttl` of frame time. A clock stepped back
        // past `last_seen` keeps the track.
        state.active.retain(|_, t| {
            now.signed_duration_since(t.last_seen)
                .to_std()
                .map_or(true, |unseen| unseen < self.ttl)
        });

        let mut matched_track_ids: Vec<TrackId> = Vec::with_capacity(detections.len());
        let mut consumed: std::collections::HashSet<TrackId> = Default::default();

        for d in &detections {
            let mut best: Option<(TrackId, f32)> = None;
            for (id, t) in state.active.iter() {
                if consumed.contains(id) {
                    continue;
                }
                if t.label != d.label {
                    continue;
                }
                let iou = t.bbox.iou(&d.bbox);
                if iou >= self.iou_threshold && best.is_none_or(|(_, b)| iou > b) {
                    best = Some((*id, iou));
                }
            }
            match best {
                Some((id, _)) => {
                    consumed.insert(id);
                    matched_track_ids.push(id);
                }
                None => {
                    let id = state.next_id;
                    state.next_id += 1;
                    state.active.insert(
                        id,
                        ActiveTrack {
                            id,
                            label: d.label.clone(),
                            bbox: d.bbox,
                            confidence: d.confidence,
                            born_at: now,
                            last_seen: now,
                            age_frames: 0,
                        },
                    );
                    matched_track_ids.push(id);
                }
            }
        }

        let mut out = Vec::with_capacity(detections.len());
        for (d, tid) in detections.iter().zip(matched_track_ids.iter()) {
            if let Some(t) = state.active.get_mut(tid) {
                t.bbox = d.bbox;
                t.confidence = d.confidence;
                t.last_seen = now;
                t.age_frames = t.age_frames.saturating_add(1);
                out.push(TrackedObject {
                    track_id: t.id,
                    label: t.label.clone(),
                    confidence: t.confidence,
                    bbox: t.bbox,
                    // iou-naive only emits matched detections, so the emitted
                    // box IS the raw detection box for this frame.
                    detection_bbox: Some(t.bbox),
                    age_frames: t.age_frames,
                    age_ms: now
                        .signed_duration_since(t.born_at)
                        .num_milliseconds()
                        .max(0) as u64,
                    attributes: d.attributes.clone(),
                });
            }
        }
        out
    }

    fn name(&self) -> &'static str {
        "iou-naive"
    }
}

// ---------------------------------------------------------------------------
// The annotator surface lives in [`annotator`]. It's a struct (per-camera
// state) rather than a free function so dwell, EMA-smoothed speed, and
// the per-zone entering/exiting FSM can persist across frames.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const T: DateTime<Utc> = DateTime::UNIX_EPOCH;

    fn det(label: &str, x: f32) -> Detection {
        Detection {
            label: label.into(),
            confidence: 0.9,
            bbox: BBox {
                x1: x,
                y1: 0.0,
                x2: x + 10.0,
                y2: 10.0,
            },
            attributes: Default::default(),
        }
    }

    #[test]
    fn iou_assigns_stable_id_across_frames() {
        let cfg = TrackerConfig::default();
        let t = IouNaiveTracker::new(&cfg);
        let f1 = t.update(vec![det("person", 0.0)], T);
        let f2 = t.update(vec![det("person", 1.0)], T);
        assert_eq!(f1[0].track_id, f2[0].track_id);
    }

    #[test]
    fn iou_ttl_is_measured_in_frame_time() {
        let cfg = TrackerConfig::default();
        let t = IouNaiveTracker::new(&cfg);
        let ttl = chrono::Duration::milliseconds(cfg.track_ttl_ms as i64);
        let f1 = t.update(vec![det("person", 0.0)], T);
        // Unseen for less than the TTL of video: the same track.
        let f2 = t.update(vec![det("person", 1.0)], T + ttl / 2);
        assert_eq!(f1[0].track_id, f2[0].track_id);
        // Unseen for longer than the TTL of video, however fast the frames
        // were processed: the track expired and this is a new one.
        let f3 = t.update(vec![det("person", 2.0)], T + ttl / 2 + ttl * 2);
        assert_ne!(f2[0].track_id, f3[0].track_id);
    }

    #[test]
    fn iou_assigns_new_id_on_label_change() {
        let cfg = TrackerConfig::default();
        let t = IouNaiveTracker::new(&cfg);
        let f1 = t.update(vec![det("person", 0.0)], T);
        let f2 = t.update(vec![det("dog", 0.0)], T);
        assert_ne!(f1[0].track_id, f2[0].track_id);
    }
}

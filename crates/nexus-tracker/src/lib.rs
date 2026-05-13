//! Object trackers behind a single trait.
//!
//! * [`IouNaiveTracker`] — nearest-IoU + TTL. Cheapest, used as a
//!   fallback / smoke-test path.
//! * [`ByteTrackTracker`] — full ByteTrack (real impl in
//!   [`bytetrack`]). Default backend at v1 parity from M4 onward.
//!
//! Trackers are per-camera (one instance per pipeline). All state is owned
//! inside the implementation; the pipeline only calls `update`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use nexus_config::TrackerConfig;
use nexus_types::{BBox, Detection, TrackId, TrackedObject};
use parking_lot::Mutex;

pub mod bytetrack;
pub use bytetrack::ByteTrackTracker;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

pub trait Tracker: Send + Sync {
    fn update(&self, detections: Vec<Detection>) -> Vec<TrackedObject>;
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
    born_at: Instant,
    last_seen: Instant,
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
    fn update(&self, detections: Vec<Detection>) -> Vec<TrackedObject> {
        let now = Instant::now();
        let mut state = self.inner.lock();

        // Drop stale tracks.
        state
            .active
            .retain(|_, t| now.duration_since(t.last_seen) < self.ttl);

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
                    age_frames: t.age_frames,
                    age_ms: now.duration_since(t.born_at).as_millis() as u64,
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
// Annotator — stamps motion / dwell attrs onto tracked objects post-update.
// Hooked here (vs. inside the tracker) so it works for every Tracker impl.
// ---------------------------------------------------------------------------

pub fn annotate_motion_attributes(objects: &mut [TrackedObject]) {
    for o in objects.iter_mut() {
        let class = if o.age_ms < 500 {
            "stationary"
        } else if o.bbox.area() > 10_000.0 {
            "near"
        } else {
            "far"
        };
        o.attributes.insert(
            "motion.speed_class".into(),
            serde_json::Value::String(class.into()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let f1 = t.update(vec![det("person", 0.0)]);
        let f2 = t.update(vec![det("person", 1.0)]);
        assert_eq!(f1[0].track_id, f2[0].track_id);
    }

    #[test]
    fn iou_assigns_new_id_on_label_change() {
        let cfg = TrackerConfig::default();
        let t = IouNaiveTracker::new(&cfg);
        let f1 = t.update(vec![det("person", 0.0)]);
        let f2 = t.update(vec![det("dog", 0.0)]);
        assert_ne!(f1[0].track_id, f2[0].track_id);
    }
}

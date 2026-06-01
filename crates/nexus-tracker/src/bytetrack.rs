//! Real ByteTrack implementation.
//!
//! Mirrors v1's `src/tracking/byte_track_tracker.cpp` so the M4
//! predicate-equivalence test can hold. Algorithm:
//!
//! 1. **Predict.** Each track ages by one frame. Its bbox is shifted by
//!    its EMA velocity so the IoU comparison below is against a
//!    one-frame-ahead prior.
//! 2. **Bucket.** Detections split into *high* (`>= high_confidence`)
//!    and *low* (`>= low_confidence` but `< high_confidence`).
//! 3. **First pass.** For every track, pick the best-IoU same-label high
//!    detection. Match if IoU `>= match_iou_threshold`.
//! 4. **Second pass.** Unmatched tracks try the same trick on the low
//!    bucket — that's the "BYTE" of ByteTrack: rescue tracks during
//!    occlusion using detections you'd otherwise discard.
//! 5. **Age unmatched tracks.** Confirmed tracks demote to lost.
//!    Tentative ones just bump `missed_frames`.
//! 6. **Spawn.** Every still-unmatched detection above
//!    `low_confidence` becomes a new track (Tentative unless
//!    `confirm_frames <= 1`, in which case Confirmed immediately —
//!    the v1 default).
//! 7. **Emit.** Confirmed and Lost tracks get returned. Tentative ones
//!    are held back so the rule layer doesn't see flicker.
//! 8. **Retire.** Tentative tracks past `tentative_max_missed_frames`,
//!    and Confirmed/Lost tracks past `max_lost_frames`, are dropped.
//!
//! The Tracker trait is stateless from the caller's perspective and
//! one instance is owned per camera, so no `cameraId` map is needed
//! — state lives behind a single `Mutex<ByteTrackState>` here.

use std::time::Instant;

use nexus_config::ByteTrackConfig;
use nexus_types::{BBox, Detection, TrackId, TrackedObject};
use parking_lot::Mutex;
use serde_json::json;

use crate::Tracker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Tentative,
    Confirmed,
    Lost,
}

#[derive(Debug, Clone)]
struct TrackState {
    id: TrackId,
    label: String,
    /// Current predicted/observed bbox (the one used for IoU matching).
    bbox: BBox,
    /// EMA-smoothed bbox emitted to downstream consumers.
    display_bbox: BBox,
    confidence: f32,
    velocity_x: f32,
    velocity_y: f32,
    age_frames: u32,
    hit_streak: u32,
    missed_frames: u32,
    born_at: Instant,
    lifecycle: Lifecycle,
}

struct ByteTrackState {
    next_id: TrackId,
    tracks: Vec<TrackState>,
}

pub struct ByteTrackTracker {
    cfg: ByteTrackConfig,
    inner: Mutex<ByteTrackState>,
}

impl ByteTrackTracker {
    pub fn new(cfg: ByteTrackConfig) -> Self {
        Self {
            cfg,
            inner: Mutex::new(ByteTrackState {
                next_id: 1,
                tracks: Vec::new(),
            }),
        }
    }
}

impl Tracker for ByteTrackTracker {
    fn update(&self, detections: Vec<Detection>) -> Vec<TrackedObject> {
        let cfg = &self.cfg;
        let now = Instant::now();
        let mut state = self.inner.lock();

        // ---- 1. Predict + age. ----
        for t in state.tracks.iter_mut() {
            t.bbox = predict(t.bbox, t.velocity_x, t.velocity_y);
            t.age_frames = t.age_frames.saturating_add(1);
        }

        // ---- 2. Bucket detections. ----
        let mut high_idx: Vec<usize> = Vec::with_capacity(detections.len());
        let mut low_idx: Vec<usize> = Vec::with_capacity(detections.len());
        for (i, d) in detections.iter().enumerate() {
            if d.confidence >= cfg.high_confidence {
                high_idx.push(i);
            } else if d.confidence >= cfg.low_confidence {
                low_idx.push(i);
            }
        }

        let mut det_used = vec![false; detections.len()];
        let mut track_matched = vec![false; state.tracks.len()];

        // ---- 3. First pass: high-conf detections vs. all tracks. ----
        associate_pass(
            &mut state.tracks,
            &detections,
            &high_idx,
            cfg.match_iou_threshold,
            &mut det_used,
            &mut track_matched,
            cfg.confirm_frames,
            cfg.display_smoothing_alpha,
            cfg.spatial_bucket_size_px,
        );

        // ---- 4. Second pass: low-conf detections recover unmatched tracks. ----
        associate_pass(
            &mut state.tracks,
            &detections,
            &low_idx,
            cfg.match_iou_threshold,
            &mut det_used,
            &mut track_matched,
            cfg.confirm_frames,
            cfg.display_smoothing_alpha,
            cfg.spatial_bucket_size_px,
        );

        // ---- 5. Age unmatched tracks. ----
        for (idx, t) in state.tracks.iter_mut().enumerate() {
            if track_matched[idx] {
                continue;
            }
            t.missed_frames = t.missed_frames.saturating_add(1);
            t.hit_streak = 0;
            if t.lifecycle == Lifecycle::Confirmed {
                t.lifecycle = Lifecycle::Lost;
            }
        }

        // ---- 6. Spawn tracks for still-unmatched detections >= low_conf. ----
        for (i, d) in detections.iter().enumerate() {
            if det_used[i] || d.confidence < cfg.low_confidence {
                continue;
            }
            let id = state.next_id;
            state.next_id += 1;
            let lifecycle = if cfg.confirm_frames <= 1 {
                Lifecycle::Confirmed
            } else {
                Lifecycle::Tentative
            };
            state.tracks.push(TrackState {
                id,
                label: d.label.clone(),
                bbox: d.bbox,
                display_bbox: d.bbox,
                confidence: d.confidence,
                velocity_x: 0.0,
                velocity_y: 0.0,
                age_frames: 1,
                hit_streak: 1,
                missed_frames: 0,
                born_at: now,
                lifecycle,
            });
        }

        // ---- 7. Retire stale tracks BEFORE emit so an over-aged track
        // doesn't get one last emission. (Order chosen so the test
        // contract holds: max_lost_frames=N means a confirmed track that
        // just demoted to lost can still emit for N more frames.)
        let max_lost = cfg.max_lost_frames;
        let max_tent_miss = cfg.tentative_max_missed_frames;
        state.tracks.retain(|t| match t.lifecycle {
            Lifecycle::Tentative => t.missed_frames <= max_tent_miss,
            Lifecycle::Confirmed | Lifecycle::Lost => t.missed_frames <= max_lost,
        });

        // ---- 8. Emit confirmed + lost. ----
        let out: Vec<TrackedObject> = state
            .tracks
            .iter()
            .filter(|t| matches!(t.lifecycle, Lifecycle::Confirmed | Lifecycle::Lost))
            .map(|t| {
                let mut attrs = serde_json::Map::new();
                let lifecycle = match t.lifecycle {
                    Lifecycle::Confirmed => "confirmed",
                    Lifecycle::Lost => "lost",
                    Lifecycle::Tentative => "tentative", // unreachable per filter
                };
                attrs.insert("tracking.lifecycle".into(), json!(lifecycle));
                attrs.insert(
                    "tracking.predicted_only".into(),
                    json!(t.lifecycle == Lifecycle::Lost),
                );
                attrs.insert("tracking.missed_frames".into(), json!(t.missed_frames));
                attrs.insert("tracking.hit_streak".into(), json!(t.hit_streak));
                TrackedObject {
                    track_id: t.id,
                    label: t.label.clone(),
                    confidence: t.confidence,
                    bbox: t.display_bbox,
                    age_frames: t.age_frames,
                    age_ms: now.duration_since(t.born_at).as_millis() as u64,
                    attributes: attrs,
                }
            })
            .collect();

        out
    }

    fn name(&self) -> &'static str {
        "bytetrack"
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn predict(b: BBox, vx: f32, vy: f32) -> BBox {
    BBox {
        x1: b.x1 + vx,
        y1: b.y1 + vy,
        x2: b.x2 + vx,
        y2: b.y2 + vy,
    }
}

/// EMA blend of `new` weighted `alpha`, `prior` weighted `1 - alpha`.
fn blend(new: BBox, prior: BBox, alpha: f32) -> BBox {
    let inv = 1.0 - alpha;
    BBox {
        x1: alpha * new.x1 + inv * prior.x1,
        y1: alpha * new.y1 + inv * prior.y1,
        x2: alpha * new.x2 + inv * prior.x2,
        y2: alpha * new.y2 + inv * prior.y2,
    }
}

/// One association pass over `det_indices`. Mutates the tracks (velocity,
/// bbox, lifecycle, hit streak) and the `det_used` / `track_matched`
/// vectors. Greedy best-IoU per track — same as v1.
///
/// `spatial_bucket_size_px` (Phase M_PERF_CROWD C1):
/// - `None` or `Some(0)` → original O(N²) sweep (every track scans every
///   candidate detection).
/// - `Some(n)` → builds a `HashMap<(i32, i32), Vec<usize>>` grid keyed
///   by `floor(det_centre / n)` over `det_indices`, then each track only
///   scans the 3×3 cell neighbourhood of its own predicted-centre cell.
///   Safe when `n ≥ max_velocity_per_frame + half_max_bbox_dim` (any
///   det with positive IoU against the track's bbox is centred within
///   one cell of the track centre, so it lies in the 3×3 neighbourhood).
#[allow(clippy::too_many_arguments)]
fn associate_pass(
    tracks: &mut [TrackState],
    detections: &[Detection],
    det_indices: &[usize],
    match_iou_threshold: f32,
    det_used: &mut [bool],
    track_matched: &mut [bool],
    confirm_frames: u32,
    display_smoothing_alpha: f32,
    spatial_bucket_size_px: Option<u32>,
) {
    // Build the spatial grid once per pass when bucketing is enabled.
    // Maps cell -> indices into `detections` (already filtered to this
    // pass's confidence band via `det_indices`).
    type CellMap = std::collections::HashMap<(i32, i32), Vec<usize>>;
    let grid: Option<(f32, CellMap)> = match spatial_bucket_size_px {
        Some(px) if px > 0 => {
            let cell = px as f32;
            let mut g: CellMap = std::collections::HashMap::with_capacity(det_indices.len());
            for &i in det_indices {
                let b = &detections[i].bbox;
                let cx = (b.x1 + b.x2) * 0.5;
                let cy = (b.y1 + b.y2) * 0.5;
                let key = ((cx / cell).floor() as i32, (cy / cell).floor() as i32);
                g.entry(key).or_default().push(i);
            }
            Some((cell, g))
        }
        _ => None,
    };

    for (t_idx, t) in tracks.iter_mut().enumerate() {
        if track_matched[t_idx] {
            continue;
        }
        let mut best: Option<(usize, f32)> = None;
        match &grid {
            None => {
                // Original O(N²) sweep — preserves v1 behaviour exactly.
                for &i in det_indices {
                    if det_used[i] {
                        continue;
                    }
                    let d = &detections[i];
                    if d.label != t.label {
                        continue;
                    }
                    let iou = t.bbox.iou(&d.bbox);
                    if iou > best.map_or(0.0, |(_, b)| b) {
                        best = Some((i, iou));
                    }
                }
            }
            Some((cell, g)) => {
                let tcx = (t.bbox.x1 + t.bbox.x2) * 0.5;
                let tcy = (t.bbox.y1 + t.bbox.y2) * 0.5;
                let gx = (tcx / cell).floor() as i32;
                let gy = (tcy / cell).floor() as i32;
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let Some(cell_dets) = g.get(&(gx + dx, gy + dy)) else {
                            continue;
                        };
                        for &i in cell_dets {
                            if det_used[i] {
                                continue;
                            }
                            let d = &detections[i];
                            if d.label != t.label {
                                continue;
                            }
                            let iou = t.bbox.iou(&d.bbox);
                            if iou > best.map_or(0.0, |(_, b)| b) {
                                best = Some((i, iou));
                            }
                        }
                    }
                }
            }
        }
        let Some((i, iou)) = best else { continue };
        if iou < match_iou_threshold {
            continue;
        }

        det_used[i] = true;
        track_matched[t_idx] = true;

        let d = &detections[i];
        let dx = d.bbox.x1 - t.bbox.x1;
        let dy = d.bbox.y1 - t.bbox.y1;
        // Same EMA constants as v1: 0.6 weight on prior velocity, 0.4 on
        // newly observed dx/dy.
        t.velocity_x = 0.6 * t.velocity_x + 0.4 * dx;
        t.velocity_y = 0.6 * t.velocity_y + 0.4 * dy;
        t.bbox = d.bbox;
        t.display_bbox = blend(d.bbox, t.display_bbox, display_smoothing_alpha);
        t.confidence = d.confidence;
        t.missed_frames = 0;
        t.hit_streak = t.hit_streak.saturating_add(1);
        // Promote tentative tracks once they've hit enough frames; recover
        // lost tracks immediately on any new match.
        match t.lifecycle {
            Lifecycle::Tentative if t.hit_streak >= confirm_frames => {
                t.lifecycle = Lifecycle::Confirmed;
            }
            Lifecycle::Lost => {
                t.lifecycle = Lifecycle::Confirmed;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn det(label: &str, x: f32, conf: f32) -> Detection {
        Detection {
            label: label.into(),
            confidence: conf,
            bbox: BBox {
                x1: x,
                y1: 0.0,
                x2: x + 10.0,
                y2: 10.0,
            },
            attributes: Default::default(),
        }
    }

    fn cfg_default() -> ByteTrackConfig {
        ByteTrackConfig::default()
    }

    #[test]
    fn high_conf_detection_creates_track_and_keeps_id() {
        let t = ByteTrackTracker::new(cfg_default());
        let f1 = t.update(vec![det("person", 0.0, 0.9)]);
        let f2 = t.update(vec![det("person", 1.0, 0.9)]);
        assert_eq!(f1.len(), 1);
        assert_eq!(f2.len(), 1);
        assert_eq!(f1[0].track_id, f2[0].track_id);
        assert_eq!(f1[0].attributes["tracking.lifecycle"], "confirmed");
    }

    #[test]
    fn label_change_starts_new_track() {
        let t = ByteTrackTracker::new(cfg_default());
        let f1 = t.update(vec![det("person", 0.0, 0.9)]);
        // Frame 2 has only a `dog` detection at the same coords. The
        // existing person track stays alive (now lost) and a new dog
        // track spawns. The contract is: distinct ids per label.
        let f2 = t.update(vec![det("dog", 0.0, 0.9)]);
        let person_id = f1
            .iter()
            .find(|o| o.label == "person")
            .map(|o| o.track_id)
            .expect("person track in f1");
        let dog_id = f2
            .iter()
            .find(|o| o.label == "dog")
            .map(|o| o.track_id)
            .expect("dog track in f2");
        assert_ne!(person_id, dog_id);
    }

    #[test]
    fn low_conf_detection_recovers_lost_track() {
        let mut cfg = cfg_default();
        cfg.high_confidence = 0.5;
        cfg.low_confidence = 0.1;
        let t = ByteTrackTracker::new(cfg);

        let f1 = t.update(vec![det("person", 0.0, 0.9)]);
        // Simulate occlusion: only a low-confidence detection survives,
        // and slightly to the right. ByteTrack's second pass should keep
        // the track alive with the same id.
        let f2 = t.update(vec![det("person", 1.0, 0.2)]);
        assert_eq!(f1.len(), 1);
        assert_eq!(f2.len(), 1);
        assert_eq!(f1[0].track_id, f2[0].track_id);
    }

    #[test]
    fn unmatched_track_demotes_to_lost_then_retires() {
        let mut cfg = cfg_default();
        cfg.max_lost_frames = 2;
        let t = ByteTrackTracker::new(cfg);

        let f1 = t.update(vec![det("person", 0.0, 0.9)]);
        assert_eq!(f1[0].attributes["tracking.lifecycle"], "confirmed");

        // Frame with no detections — track ages and demotes to lost.
        let f2 = t.update(vec![]);
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0].attributes["tracking.lifecycle"], "lost");
        assert_eq!(f2[0].attributes["tracking.predicted_only"], true);

        // Two more empty frames push past max_lost_frames=2 → retired.
        let _ = t.update(vec![]);
        let f4 = t.update(vec![]);
        assert!(
            f4.is_empty(),
            "track should retire after max_lost_frames empty frames"
        );
    }

    #[test]
    fn tentative_track_holds_back_until_confirm_frames() {
        let mut cfg = cfg_default();
        cfg.confirm_frames = 3;
        let t = ByteTrackTracker::new(cfg);

        // First two hits — track exists internally but is Tentative,
        // so it's filtered out of the emit list.
        let f1 = t.update(vec![det("person", 0.0, 0.9)]);
        assert!(f1.is_empty(), "tentative track must not emit");
        let f2 = t.update(vec![det("person", 1.0, 0.9)]);
        assert!(f2.is_empty(), "still tentative");

        // Third hit promotes to confirmed.
        let f3 = t.update(vec![det("person", 2.0, 0.9)]);
        assert_eq!(f3.len(), 1);
        assert_eq!(f3[0].attributes["tracking.lifecycle"], "confirmed");
    }

    #[test]
    fn velocity_ema_predicts_motion() {
        let t = ByteTrackTracker::new(cfg_default());
        // Three frames of consistent rightward drift establish velocity.
        let _ = t.update(vec![det("person", 0.0, 0.9)]);
        let _ = t.update(vec![det("person", 5.0, 0.9)]);
        let _ = t.update(vec![det("person", 10.0, 0.9)]);
        // Now skip a frame (no detection). Internally the bbox should be
        // predicted forward so a detection at x=20 still matches via IoU.
        let _ = t.update(vec![]);
        let f5 = t.update(vec![det("person", 20.0, 0.9)]);
        assert_eq!(f5.len(), 1, "velocity prediction should keep the match");
    }

    #[test]
    fn detection_below_low_confidence_is_ignored() {
        let mut cfg = cfg_default();
        cfg.low_confidence = 0.3;
        let t = ByteTrackTracker::new(cfg);
        let out = t.update(vec![det("person", 0.0, 0.05)]);
        assert!(out.is_empty(), "below low_confidence → no track");
    }

    #[test]
    fn display_bbox_smooths_jitter() {
        let mut cfg = cfg_default();
        cfg.display_smoothing_alpha = 0.5;
        let t = ByteTrackTracker::new(cfg);
        let _ = t.update(vec![det("person", 0.0, 0.9)]);
        // Detection drifts a bit — small enough to keep the IoU match
        // (default match_iou_threshold = 0.3) but big enough that the
        // smoothed display bbox lands strictly between the prior and
        // the new bbox.
        let f2 = t.update(vec![det("person", 3.0, 0.9)]);
        let person = f2
            .iter()
            .find(|o| o.label == "person")
            .expect("person track in f2");
        let display_x = person.bbox.x1;
        assert!(
            display_x > 0.0 && display_x < 3.0,
            "display bbox x ({display_x}) should be between prior (0.0) and new (3.0)"
        );
    }

    // Phase M_PERF_CROWD C1: bucketed associate_pass must produce
    // identical TrackedObject output to the naive O(N²) sweep when
    // bucket_size_px ≥ max bbox dim + max per-frame motion.
    fn det_at(label: &str, x: f32, y: f32, w: f32, h: f32, conf: f32) -> Detection {
        Detection {
            label: label.into(),
            confidence: conf,
            bbox: BBox {
                x1: x,
                y1: y,
                x2: x + w,
                y2: y + h,
            },
            attributes: Default::default(),
        }
    }

    fn lcg_next(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state
    }

    fn frames_random_cluster(seed: u64) -> Vec<Vec<Detection>> {
        // 8 frames, 40 detections per frame, three label classes, motion
        // ≤ 4 px/frame, bbox up to 30 px. Bucket size 64 px must yield
        // identical results to naive.
        let mut s = seed;
        let labels = ["person", "car", "dog"];
        (0..8)
            .map(|_| {
                (0..40)
                    .map(|_| {
                        let lab = labels[(lcg_next(&mut s) % 3) as usize];
                        let x = (lcg_next(&mut s) % 1200) as f32;
                        let y = (lcg_next(&mut s) % 680) as f32;
                        let w = 15.0 + ((lcg_next(&mut s) % 16) as f32);
                        let h = 15.0 + ((lcg_next(&mut s) % 16) as f32);
                        let conf = 0.10 + (lcg_next(&mut s) % 90) as f32 / 100.0;
                        det_at(lab, x, y, w, h, conf)
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn bucketed_associate_pass_matches_naive_on_random_cluster() {
        let frames = frames_random_cluster(0xDEAD_BEEF);
        let mut cfg_naive = cfg_default();
        cfg_naive.spatial_bucket_size_px = None;
        let mut cfg_bucketed = cfg_default();
        cfg_bucketed.spatial_bucket_size_px = Some(64);

        let t_n = ByteTrackTracker::new(cfg_naive);
        let t_b = ByteTrackTracker::new(cfg_bucketed);

        for (i, frame) in frames.iter().enumerate() {
            let out_n = t_n.update(frame.clone());
            let out_b = t_b.update(frame.clone());
            assert_eq!(
                out_n.len(),
                out_b.len(),
                "frame {i}: naive emitted {} tracks, bucketed emitted {}",
                out_n.len(),
                out_b.len()
            );
            // Track ids may diverge if the greedy best-IoU pick differs,
            // but at this bucket size every IoU > 0 candidate is in the
            // 3×3 neighbourhood, so picks are identical. Compare on
            // (label, bbox, confidence) — track ids should also match
            // since both trackers see the same input order.
            for (a, b) in out_n.iter().zip(out_b.iter()) {
                assert_eq!(a.track_id, b.track_id, "frame {i}: track_id mismatch");
                assert_eq!(a.label, b.label, "frame {i}: label mismatch");
                assert_eq!(
                    a.bbox.x1, b.bbox.x1,
                    "frame {i}: bbox.x1 mismatch on track {}",
                    a.track_id
                );
                assert_eq!(a.bbox.y1, b.bbox.y1);
                assert_eq!(a.bbox.x2, b.bbox.x2);
                assert_eq!(a.bbox.y2, b.bbox.y2);
                assert_eq!(a.confidence, b.confidence);
            }
        }
    }

    #[test]
    fn bucketed_zero_size_falls_back_to_naive() {
        // Some(0) is treated as None — defensive against config typos.
        let mut cfg = cfg_default();
        cfg.spatial_bucket_size_px = Some(0);
        let t = ByteTrackTracker::new(cfg);
        let f1 = t.update(vec![det("person", 0.0, 0.9)]);
        let f2 = t.update(vec![det("person", 1.0, 0.9)]);
        assert_eq!(f1.len(), 1);
        assert_eq!(f2.len(), 1);
        assert_eq!(f1[0].track_id, f2[0].track_id);
    }
}

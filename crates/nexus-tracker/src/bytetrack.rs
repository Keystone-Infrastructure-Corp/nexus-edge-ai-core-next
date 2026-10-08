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
//!    occlusion using detections you'd otherwise discard. A **motion
//!    pass** then lets a track still unmatched take the nearest
//!    same-label detection within `motion_match_box_lengths_per_sec` of
//!    it, scaled by the time since its last match. IoU alone cannot link
//!    a vehicle that moves more than ~half its box between analysed
//!    frames, which at low analysis rates is most traffic (#362).
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

use std::time::{Duration, Instant};

use nexus_config::ByteTrackConfig;
use nexus_types::{Attributes, BBox, Detection, TrackId, TrackedObject};
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
    /// Capture stamp of the frame this track last matched a detection on.
    last_matched_at: Instant,
    /// Detections matched over the track's life, including the one it
    /// was born from.
    hits: u32,
    lifecycle: Lifecycle,
    /// The attributes of the detection this track matched on the current
    /// frame, moved into the emitted object. Empty on a predicted-only
    /// frame, like `detection_bbox`: there is no detection to describe.
    attributes: Attributes,
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
    fn update(&self, mut detections: Vec<Detection>, captured_mono: Instant) -> Vec<TrackedObject> {
        let cfg = &self.cfg;
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
            &mut detections,
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
            &mut detections,
            &low_idx,
            cfg.match_iou_threshold,
            &mut det_used,
            &mut track_matched,
            cfg.confirm_frames,
            cfg.display_smoothing_alpha,
            cfg.spatial_bucket_size_px,
        );

        // ---- 4b. Motion pass: what overlap could not link. ----
        if cfg.motion_match_box_lengths_per_sec > 0.0 {
            associate_by_motion(
                &mut state.tracks,
                &mut detections,
                cfg,
                captured_mono,
                &mut det_used,
                &mut track_matched,
            );
        }

        // ---- 5. Age unmatched tracks. ----
        for (idx, t) in state.tracks.iter_mut().enumerate() {
            if track_matched[idx] {
                t.last_matched_at = captured_mono;
                continue;
            }
            t.missed_frames = t.missed_frames.saturating_add(1);
            t.hit_streak = 0;
            if t.lifecycle == Lifecycle::Confirmed {
                t.lifecycle = Lifecycle::Lost;
            }
        }

        // ---- 6. Spawn tracks for still-unmatched detections >= low_conf. ----
        for (i, d) in detections.iter_mut().enumerate() {
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
                born_at: captured_mono,
                last_matched_at: captured_mono,
                hits: 1,
                lifecycle,
                attributes: std::mem::take(&mut d.attributes),
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
            .iter_mut()
            .filter(|t| matches!(t.lifecycle, Lifecycle::Confirmed | Lifecycle::Lost))
            .map(|t| {
                let mut attrs = std::mem::take(&mut t.attributes);
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
                    // Frame-aligned raw detection box when this track matched
                    // a detection on the current frame (missed_frames == 0 iff
                    // matched, so t.bbox == d.bbox); None when predicted-only.
                    detection_bbox: (t.missed_frames == 0).then_some(t.bbox),
                    age_frames: t.age_frames,
                    age_ms: captured_mono
                        .saturating_duration_since(t.born_at)
                        .as_millis() as u64,
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
/// vectors, and moves each matched detection's attributes onto its track.
/// Greedy best-IoU per track — same as v1.
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
    detections: &mut [Detection],
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
        apply_match(
            t,
            &mut detections[i],
            1.0,
            confirm_frames,
            display_smoothing_alpha,
        );
    }
}

/// Longest gap since a track's last match that the motion pass bridges.
/// The reach grows with the gap, so without a bound a lost track would
/// eventually reach across the whole frame. The longest gap between
/// successive detections of the #362 vehicle was 2.39 s.
const MOTION_MATCH_MAX_GAP: Duration = Duration::from_millis(2_500);

/// Speed, in box lengths per analysed frame, below which a track with a
/// velocity is treated as stationary and left out of the motion pass. A
/// parked car's track must not take the detection of a vehicle passing in
/// front of it; while it is visible the IoU pass already gave it its own.
const MOTION_MATCH_MIN_SPEED: f32 = 0.1;

/// Link unmatched tracks to unused same-label detections by distance,
/// closest pairs first. A track is eligible if it has no velocity yet (one
/// match) or is measurably moving; a detection must lie within
/// `motion_match_box_lengths_per_sec` box lengths per second of the time
/// since the track's last match, and be between half and twice its size.
fn associate_by_motion(
    tracks: &mut [TrackState],
    detections: &mut [Detection],
    cfg: &ByteTrackConfig,
    now: Instant,
    det_used: &mut [bool],
    track_matched: &mut [bool],
) {
    let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
    for (t_idx, t) in tracks.iter().enumerate() {
        if track_matched[t_idx] {
            continue;
        }
        let gap = now.saturating_duration_since(t.last_matched_at);
        if gap.is_zero() || gap > MOTION_MATCH_MAX_GAP {
            continue;
        }
        let (tw, th) = (t.bbox.x2 - t.bbox.x1, t.bbox.y2 - t.bbox.y1);
        let length = tw.max(th);
        if length <= 0.0 {
            continue;
        }
        if t.hits > 1 && t.velocity_x.hypot(t.velocity_y) < MOTION_MATCH_MIN_SPEED * length {
            continue;
        }
        let reach = cfg.motion_match_box_lengths_per_sec * length * gap.as_secs_f32();
        let (tcx, tcy) = t.bbox.center();
        for (i, d) in detections.iter().enumerate() {
            if det_used[i] || d.confidence < cfg.low_confidence || d.label != t.label {
                continue;
            }
            let (dw, dh) = (d.bbox.x2 - d.bbox.x1, d.bbox.y2 - d.bbox.y1);
            if !(0.5..=2.0).contains(&(dw / tw)) || !(0.5..=2.0).contains(&(dh / th)) {
                continue;
            }
            let (dcx, dcy) = d.bbox.center();
            let dist = (dcx - tcx).hypot(dcy - tcy);
            if dist <= reach {
                pairs.push((dist / reach, t_idx, i));
            }
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (_, t_idx, i) in pairs {
        if track_matched[t_idx] || det_used[i] {
            continue;
        }
        det_used[i] = true;
        track_matched[t_idx] = true;
        let t = &mut tracks[t_idx];
        // The displacement spans every frame since the last match; record
        // it per frame, as the predictor applies it.
        let frames = (t.missed_frames + 1) as f32;
        apply_match(
            t,
            &mut detections[i],
            frames,
            cfg.confirm_frames,
            cfg.display_smoothing_alpha,
        );
    }
}

/// Fold detection `d` into track `t`. `frames` is how many analysed frames
/// the observed displacement spans.
fn apply_match(
    t: &mut TrackState,
    d: &mut Detection,
    frames: f32,
    confirm_frames: u32,
    display_smoothing_alpha: f32,
) {
    let dx = (d.bbox.x1 - t.bbox.x1) / frames;
    let dy = (d.bbox.y1 - t.bbox.y1) / frames;
    // Same EMA constants as v1: 0.6 weight on prior velocity, 0.4 on
    // newly observed dx/dy.
    t.velocity_x = 0.6 * t.velocity_x + 0.4 * dx;
    t.velocity_y = 0.6 * t.velocity_y + 0.4 * dy;
    t.bbox = d.bbox;
    t.display_bbox = blend(d.bbox, t.display_bbox, display_smoothing_alpha);
    t.confidence = d.confidence;
    t.attributes = std::mem::take(&mut d.attributes);
    t.missed_frames = 0;
    t.hit_streak = t.hit_streak.saturating_add(1);
    t.hits = t.hits.saturating_add(1);
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    static T: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

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
        let f1 = t.update(vec![det("person", 0.0, 0.9)], *T);
        let f2 = t.update(vec![det("person", 1.0, 0.9)], *T);
        assert_eq!(f1.len(), 1);
        assert_eq!(f2.len(), 1);
        assert_eq!(f1[0].track_id, f2[0].track_id);
        assert_eq!(f1[0].attributes["tracking.lifecycle"], "confirmed");
    }

    #[test]
    fn label_change_starts_new_track() {
        let t = ByteTrackTracker::new(cfg_default());
        let f1 = t.update(vec![det("person", 0.0, 0.9)], *T);
        // Frame 2 has only a `dog` detection at the same coords. The
        // existing person track stays alive (now lost) and a new dog
        // track spawns. The contract is: distinct ids per label.
        let f2 = t.update(vec![det("dog", 0.0, 0.9)], *T);
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

        let f1 = t.update(vec![det("person", 0.0, 0.9)], *T);
        // Simulate occlusion: only a low-confidence detection survives,
        // and slightly to the right. ByteTrack's second pass should keep
        // the track alive with the same id.
        let f2 = t.update(vec![det("person", 1.0, 0.2)], *T);
        assert_eq!(f1.len(), 1);
        assert_eq!(f2.len(), 1);
        assert_eq!(f1[0].track_id, f2[0].track_id);
    }

    #[test]
    fn unmatched_track_demotes_to_lost_then_retires() {
        let mut cfg = cfg_default();
        cfg.max_lost_frames = 2;
        let t = ByteTrackTracker::new(cfg);

        let f1 = t.update(vec![det("person", 0.0, 0.9)], *T);
        assert_eq!(f1[0].attributes["tracking.lifecycle"], "confirmed");

        // Frame with no detections — track ages and demotes to lost.
        let f2 = t.update(vec![], *T);
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0].attributes["tracking.lifecycle"], "lost");
        assert_eq!(f2[0].attributes["tracking.predicted_only"], true);

        // Two more empty frames push past max_lost_frames=2 → retired.
        let _ = t.update(vec![], *T);
        let f4 = t.update(vec![], *T);
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
        let f1 = t.update(vec![det("person", 0.0, 0.9)], *T);
        assert!(f1.is_empty(), "tentative track must not emit");
        let f2 = t.update(vec![det("person", 1.0, 0.9)], *T);
        assert!(f2.is_empty(), "still tentative");

        // Third hit promotes to confirmed.
        let f3 = t.update(vec![det("person", 2.0, 0.9)], *T);
        assert_eq!(f3.len(), 1);
        assert_eq!(f3[0].attributes["tracking.lifecycle"], "confirmed");
    }

    #[test]
    fn velocity_ema_predicts_motion() {
        let t = ByteTrackTracker::new(cfg_default());
        // Three frames of consistent rightward drift establish velocity.
        let _ = t.update(vec![det("person", 0.0, 0.9)], *T);
        let _ = t.update(vec![det("person", 5.0, 0.9)], *T);
        let _ = t.update(vec![det("person", 10.0, 0.9)], *T);
        // Now skip a frame (no detection). Internally the bbox should be
        // predicted forward so a detection at x=20 still matches via IoU.
        let _ = t.update(vec![], *T);
        let f5 = t.update(vec![det("person", 20.0, 0.9)], *T);
        assert_eq!(f5.len(), 1, "velocity prediction should keep the match");
    }

    #[test]
    fn detection_below_low_confidence_is_ignored() {
        let mut cfg = cfg_default();
        cfg.low_confidence = 0.3;
        let t = ByteTrackTracker::new(cfg);
        let out = t.update(vec![det("person", 0.0, 0.05)], *T);
        assert!(out.is_empty(), "below low_confidence → no track");
    }

    #[test]
    fn detection_bbox_is_raw_when_matched_none_when_predicted() {
        // display smoothing lags the emitted `bbox`, but `detection_bbox`
        // must carry the frame-aligned RAW detection box so alert snapshots
        // and burned-in alert clips draw the object where it actually is.
        let mut cfg = cfg_default();
        cfg.display_smoothing_alpha = 0.5;
        cfg.max_lost_frames = 2;
        let t = ByteTrackTracker::new(cfg);
        let _ = t.update(vec![det("person", 0.0, 0.9)], *T);
        // Object moved to x=3. The emitted (smoothed) bbox lags between 0
        // and 3; detection_bbox must equal the raw detection (x1 == 3.0).
        let f2 = t.update(vec![det("person", 3.0, 0.9)], *T);
        let p = f2
            .iter()
            .find(|o| o.label == "person")
            .expect("person track in f2");
        let raw = p
            .detection_bbox
            .expect("matched track carries a detection_bbox");
        assert_eq!(raw.x1, 3.0, "detection_bbox must be the raw detection box");
        assert!(
            p.bbox.x1 > 0.0 && p.bbox.x1 < 3.0,
            "sanity: the emitted bbox is still smoothed (lags the raw box)"
        );

        // Predicted-only frame (no detection): detection_bbox is None while
        // the track is still emitted with a predicted `bbox`.
        let f3 = t.update(vec![], *T);
        let p = f3
            .iter()
            .find(|o| o.label == "person")
            .expect("lost track still emitted");
        assert_eq!(p.attributes["tracking.predicted_only"], true);
        assert!(
            p.detection_bbox.is_none(),
            "predicted-only track must not carry a detection_bbox"
        );
    }

    #[test]
    fn detection_attributes_are_this_frames_and_absent_when_predicted() {
        let t = ByteTrackTracker::new(cfg_default());
        let mut d = det("person", 0.0, 0.9);
        d.attributes.insert("ppe.hardhat".into(), json!(true));
        let f1 = t.update(vec![d], *T);
        assert_eq!(f1[0].attributes["ppe.hardhat"], true);
        assert_eq!(f1[0].attributes["tracking.lifecycle"], "confirmed");

        // Predicted-only: no detection this frame, so nothing describes it.
        let f2 = t.update(vec![], *T);
        assert_eq!(f2[0].attributes["tracking.predicted_only"], true);
        assert!(!f2[0].attributes.contains_key("ppe.hardhat"));

        // Re-matched by a detection without the attribute: none carried over.
        let f3 = t.update(vec![det("person", 1.0, 0.9)], *T);
        assert_eq!(f3[0].track_id, f1[0].track_id);
        assert!(!f3[0].attributes.contains_key("ppe.hardhat"));
    }

    #[test]
    fn display_bbox_smooths_jitter() {
        let mut cfg = cfg_default();
        cfg.display_smoothing_alpha = 0.5;
        let t = ByteTrackTracker::new(cfg);
        let _ = t.update(vec![det("person", 0.0, 0.9)], *T);
        // Detection drifts a bit — small enough to keep the IoU match
        // (default match_iou_threshold = 0.3) but big enough that the
        // smoothed display bbox lands strictly between the prior and
        // the new bbox.
        let f2 = t.update(vec![det("person", 3.0, 0.9)], *T);
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

    // #362: the field core analysed each camera every ~690 ms. A vehicle
    // moving 0.626 of its box per analysed frame overlaps its previous box
    // at IoU 0.23, below `match_iou_threshold`.
    const FIELD_INTERVAL_MS: u64 = 690;

    fn at_ms(ms: u64) -> Instant {
        *T + std::time::Duration::from_millis(ms)
    }

    fn car(cx: f32, cy: f32) -> Detection {
        det_at("vehicle.car", cx - 50.0, cy - 30.0, 100.0, 60.0, 0.8)
    }

    /// Distinct track ids among objects carrying a detection this frame,
    /// and how many of those objects the rule layer would evaluate
    /// (`detection_bbox` present and at least 500 ms old).
    fn drive(t: &ByteTrackTracker, frames: &[(u64, Vec<Detection>)]) -> (Vec<TrackId>, usize) {
        let mut ids = Vec::new();
        let mut evaluable = 0;
        for (ms, dets) in frames {
            for o in t.update(dets.clone(), at_ms(*ms)) {
                if o.detection_bbox.is_some() {
                    if !ids.contains(&o.track_id) {
                        ids.push(o.track_id);
                    }
                    if o.age_ms >= 500 {
                        evaluable += 1;
                    }
                }
            }
        }
        (ids, evaluable)
    }

    fn crossing(step_px: f32, every: u64, frames: u64) -> Vec<(u64, Vec<Detection>)> {
        (0..frames)
            .map(|i| {
                let dets = if i % every == 0 {
                    vec![car(50.0 + i as f32 * step_px, 100.0)]
                } else {
                    Vec::new()
                };
                (i * FIELD_INTERVAL_MS, dets)
            })
            .collect()
    }

    #[test]
    fn a_vehicle_past_the_iou_cliff_keeps_one_track_at_a_low_analysis_rate() {
        let t = ByteTrackTracker::new(cfg_default());
        let (ids, evaluable) = drive(&t, &crossing(62.6, 1, 11));
        assert_eq!(ids.len(), 1, "one vehicle, one track: {ids:?}");
        assert_eq!(
            evaluable, 10,
            "every frame after the first reaches the rules"
        );
    }

    #[test]
    fn a_vehicle_detected_on_every_third_frame_keeps_one_track() {
        let t = ByteTrackTracker::new(cfg_default());
        let (ids, _) = drive(&t, &crossing(62.6, 3, 13));
        assert_eq!(ids.len(), 1, "{ids:?}");
    }

    #[test]
    fn zero_motion_speed_keeps_iou_only_association() {
        let mut cfg = cfg_default();
        cfg.motion_match_box_lengths_per_sec = 0.0;
        let t = ByteTrackTracker::new(cfg);
        let (ids, evaluable) = drive(&t, &crossing(62.6, 1, 11));
        assert_eq!(ids.len(), 11);
        assert_eq!(evaluable, 0);
    }

    #[test]
    fn the_motion_reach_grows_with_the_time_since_the_last_match() {
        for (gap_ms, want) in [(100, 2), (FIELD_INTERVAL_MS, 1), (3_000, 2)] {
            let t = ByteTrackTracker::new(cfg_default());
            let (ids, _) = drive(
                &t,
                &[
                    (0, vec![car(50.0, 100.0)]),
                    (gap_ms, vec![car(112.6, 100.0)]),
                ],
            );
            assert_eq!(ids.len(), want, "gap {gap_ms} ms: {ids:?}");
        }
    }

    #[test]
    fn a_parked_car_never_takes_a_passing_vehicles_detection() {
        let t = ByteTrackTracker::new(cfg_default());
        let parked: Vec<_> = (0..5)
            .map(|i| (i * FIELD_INTERVAL_MS, vec![car(50.0, 100.0)]))
            .collect();
        let (parked_ids, _) = drive(&t, &parked);
        // The passer occludes the parked car, so only the passer is
        // detected, 70 px from the parked track and past IoU.
        let (ids, _) = drive(&t, &[(5 * FIELD_INTERVAL_MS, vec![car(120.0, 100.0)])]);
        assert_eq!(ids.len(), 1);
        assert_ne!(ids[0], parked_ids[0], "the parked track took the passer");
    }

    #[test]
    fn the_motion_pass_assigns_the_closest_pairs_first() {
        // Track a would reach b's detection first in track order; b's own
        // detection is nearer to b, so b must keep it and a take its own.
        let small = |cx: f32| det_at("vehicle.car", cx - 20.0, 70.0, 40.0, 60.0, 0.8);
        let t = ByteTrackTracker::new(cfg_default());
        let (first, _) = drive(&t, &[(0, vec![small(100.0), small(200.0)])]);
        let out = t.update(vec![small(170.0), small(10.0)], at_ms(1_380));
        let id_at = |cx: f32| {
            out.iter()
                .find(|o| {
                    o.detection_bbox
                        .is_some_and(|b| (b.center().0 - cx).abs() < 0.5)
                })
                .map(|o| o.track_id)
        };
        assert_eq!(id_at(10.0), Some(first[0]));
        assert_eq!(id_at(170.0), Some(first[1]));
    }

    #[test]
    fn a_vehicle_reacquired_after_a_detector_gap_keeps_one_track_at_full_rate() {
        // 15 fps, 10 px per frame, seen once and then missed for 20 frames.
        // The link across the gap must record 10 px per frame, not the
        // 210 px the gap spans, or the next prediction overshoots.
        let t = ByteTrackTracker::new(cfg_default());
        let frames: Vec<_> = (0..25u64)
            .map(|i| {
                let dets = if i == 0 || i > 20 {
                    vec![car(50.0 + i as f32 * 10.0, 100.0)]
                } else {
                    Vec::new()
                };
                (i * 66, dets)
            })
            .collect();
        let (ids, _) = drive(&t, &frames);
        assert_eq!(ids.len(), 1, "{ids:?}");
    }

    #[test]
    fn a_box_of_a_different_size_is_not_motion_linked() {
        let t = ByteTrackTracker::new(cfg_default());
        let big = det_at("vehicle.car", 60.0, 40.0, 250.0, 150.0, 0.8);
        let (ids, _) = drive(
            &t,
            &[(0, vec![car(50.0, 100.0)]), (FIELD_INTERVAL_MS, vec![big])],
        );
        assert_eq!(ids.len(), 2);
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
            let out_n = t_n.update(frame.clone(), *T);
            let out_b = t_b.update(frame.clone(), *T);
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
        let f1 = t.update(vec![det("person", 0.0, 0.9)], *T);
        let f2 = t.update(vec![det("person", 1.0, 0.9)], *T);
        assert_eq!(f1.len(), 1);
        assert_eq!(f2.len(), 1);
        assert_eq!(f1[0].track_id, f2[0].track_id);
    }
}

//! Per-camera track annotator. Stamps motion / dwell / zone / group
//! attributes onto every [`TrackedObject`] that comes out of the
//! tracker so the rule layer can filter on them.
//!
//! Mirrors v1's `src/tracking/track_annotator.cpp` so the M4
//! predicate-equivalence test can hold. Attributes populated:
//!
//! * `motion.speed_class`   — `stationary | walking | running | vehicle_speed`
//! * `motion.direction`     — 8-way compass (`n / ne / e / se / s / sw / w / nw`) or `none`
//! * `motion.parked_vehicle`— `yes | no` (only on `vehicle.*` labels)
//! * `motion.dwell_seconds` — integer seconds since the track was first seen
//! * `motion.zone_state`    — `outside | entering | inside | exiting`
//! * `motion.zone_ids`      — `Vec<String>` of inclusion/dwell zone IDs
//!   the object is currently inside (post-transition). Lets rules
//!   target a specific zone, e.g.
//!   `'parking' in object.attributes['motion.zone_ids']`.
//! * `group.size`           — count of *other* same-label tracks within
//!   `group_radius_box_multiplier × bbox half-perimeter`
//!
//! State surface: one `TrackAnnotator` per camera, owned by the
//! supervisor task. State is keyed by `track_id` only (no camera key
//! needed at this scope). Stale tracks are GC'd after
//! `stale_state_frames` empty observations.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use nexus_config::{AnnotatorConfig, ZoneConfig, ZoneKind};
use nexus_types::{Frame, TrackId, TrackedObject};
use serde_json::json;

const FIRST_FRAME_DT_SECONDS: f64 = 1.0 / 30.0;

#[derive(Debug, Default, Clone)]
struct PerTrackState {
    first_seen_at: Option<DateTime<Utc>>,
    last_seen_at: Option<DateTime<Utc>>,
    last_seen_tick: u64,
    last_center: Option<(f32, f32)>,
    /// EMA of px/frame movement magnitude. Drives speed_class and parked.
    movement_ema: f64,
    /// EMA of (dx, dy) — drives direction.
    direction_dx_ema: f64,
    direction_dy_ema: f64,
    /// Frames spent below the parked threshold (vehicles only).
    parked_frames_accum: u32,
    /// Per-zone "was inside last frame" flag for the entering/exiting FSM.
    inside_by_zone: HashMap<String, bool>,
}

pub struct TrackAnnotator {
    cfg: AnnotatorConfig,
    state_by_track: HashMap<TrackId, PerTrackState>,
    frame_tick: u64,
}

impl TrackAnnotator {
    pub fn new(cfg: AnnotatorConfig) -> Self {
        Self {
            cfg,
            state_by_track: HashMap::new(),
            frame_tick: 0,
        }
    }

    /// Stamp attributes onto every track in `objects`. Mutates internal
    /// per-track state so subsequent calls have access to deltas.
    pub fn annotate(&mut self, frame: &Frame, zones: &[ZoneConfig], objects: &mut [TrackedObject]) {
        self.frame_tick = self.frame_tick.saturating_add(1);
        if objects.is_empty() {
            self.gc_stale();
            return;
        }

        let frame_w = frame.width as f32;
        let frame_h = frame.height as f32;

        // Group-size pre-pass: bucket centers + radii by label so the
        // per-track loop is O(k) instead of O(n²) — same shape as v1.
        let mut centers_by_label: HashMap<String, Vec<(f32, f32)>> = HashMap::new();
        for o in objects.iter() {
            centers_by_label
                .entry(o.label.clone())
                .or_default()
                .push(o.bbox.center());
        }

        for o in objects.iter_mut() {
            let state = self.state_by_track.entry(o.track_id).or_default();
            let now = frame.captured_at;
            let center = o.bbox.center();

            // ---- dt seconds since last observation ----
            let dt_seconds = match state.last_seen_at {
                Some(prev) => {
                    let delta_us = now
                        .signed_duration_since(prev)
                        .num_microseconds()
                        .unwrap_or(0);
                    if delta_us > 0 {
                        delta_us as f64 / 1_000_000.0
                    } else {
                        FIRST_FRAME_DT_SECONDS
                    }
                }
                None => FIRST_FRAME_DT_SECONDS,
            };
            if state.first_seen_at.is_none() {
                state.first_seen_at = Some(now);
            }
            state.last_seen_at = Some(now);
            state.last_seen_tick = self.frame_tick;

            // ---- movement + direction EMAs ----
            let (dx, dy, px_per_frame) = match state.last_center {
                Some((lx, ly)) => {
                    let dx = (center.0 - lx) as f64;
                    let dy = (center.1 - ly) as f64;
                    (dx, dy, (dx * dx + dy * dy).sqrt())
                }
                None => (0.0, 0.0, 0.0),
            };
            let m_alpha = self.cfg.movement_ema_alpha as f64;
            let d_alpha = self.cfg.direction_ema_alpha as f64;
            if state.last_center.is_some() {
                state.movement_ema = m_alpha * px_per_frame + (1.0 - m_alpha) * state.movement_ema;
                state.direction_dx_ema = d_alpha * dx + (1.0 - d_alpha) * state.direction_dx_ema;
                state.direction_dy_ema = d_alpha * dy + (1.0 - d_alpha) * state.direction_dy_ema;
            } else {
                state.movement_ema = px_per_frame;
                state.direction_dx_ema = dx;
                state.direction_dy_ema = dy;
            }
            state.last_center = Some(center);

            // ---- speed_class ----
            let dt_safe = dt_seconds.max(1e-6);
            let px_per_sec = state.movement_ema / dt_safe;
            let is_vehicle = is_vehicle_label(&o.label);
            let speed_class =
                if is_vehicle && px_per_sec >= self.cfg.speed_vehicle_px_per_sec as f64 {
                    "vehicle_speed"
                } else if px_per_sec >= self.cfg.speed_running_px_per_sec as f64 {
                    "running"
                } else if px_per_sec >= self.cfg.speed_walking_px_per_sec as f64 {
                    "walking"
                } else {
                    "stationary"
                };
            o.attributes
                .insert("motion.speed_class".into(), json!(speed_class));

            // ---- direction (8-way compass) ----
            let dir_mag_px_per_sec = (state.direction_dx_ema * state.direction_dx_ema
                + state.direction_dy_ema * state.direction_dy_ema)
                .sqrt()
                / dt_safe;
            let direction = if dir_mag_px_per_sec >= self.cfg.direction_min_px_per_sec as f64 {
                compass8(state.direction_dx_ema, state.direction_dy_ema)
            } else {
                "none"
            };
            o.attributes
                .insert("motion.direction".into(), json!(direction));

            // ---- parked_vehicle (vehicle.* only) ----
            if is_vehicle {
                if state.movement_ema <= self.cfg.parked_ema_threshold_px as f64 {
                    state.parked_frames_accum = state.parked_frames_accum.saturating_add(1);
                } else {
                    state.parked_frames_accum = 0;
                }
                let parked = state.parked_frames_accum >= self.cfg.parked_min_frames_to_flag;
                o.attributes.insert(
                    "motion.parked_vehicle".into(),
                    json!(if parked { "yes" } else { "no" }),
                );
            }

            // ---- dwell_seconds (integer seconds since first_seen_at) ----
            let dwell_ms = state
                .first_seen_at
                .map(|first| now.signed_duration_since(first).num_milliseconds().max(0))
                .unwrap_or(0);
            o.attributes
                .insert("motion.dwell_seconds".into(), json!(dwell_ms / 1000));

            // ---- zone_state + zone_ids ----
            // Per-zone FSM: outside -> entering -> inside -> exiting -> outside.
            // Aggregate via priority: entering > exiting > inside > outside,
            // so a single rule that matches "entering" still fires on the
            // first frame the track crosses *any* zone boundary, even if
            // the track is already "inside" a different zone.
            //
            // `motion.zone_ids` is the list of inclusion/dwell zone IDs the
            // object is currently INSIDE (post-transition). Lets rules
            // target a specific zone by id:
            //   `'parking' in object.attributes['motion.zone_ids']`
            // Excluded zones never appear (they're filtered upstream by
            // `filter_excluded_zones`; even if they reached here, an
            // exclusion zone the object is "inside" would have already
            // dropped the detection — they're not meaningful as labels).
            let mut aggregate_state = "outside";
            let mut state_priority: u8 = 1;
            let mut inside_zone_ids: Vec<String> = Vec::new();
            for zone in zones.iter().filter(|z| z.kind != ZoneKind::Exclusion) {
                let inside_now = point_in_normalized_polygon(
                    center.0 / frame_w.max(1.0),
                    center.1 / frame_h.max(1.0),
                    &zone.polygon,
                );
                let inside_prev = state.inside_by_zone.get(&zone.id).copied().unwrap_or(false);
                state.inside_by_zone.insert(zone.id.clone(), inside_now);

                if inside_now {
                    inside_zone_ids.push(zone.id.clone());
                }

                let (zone_state, prio) = match (inside_now, inside_prev) {
                    (true, false) => ("entering", 4u8),
                    (false, true) => ("exiting", 3),
                    (true, true) => ("inside", 2),
                    (false, false) => ("outside", 1),
                };
                if prio > state_priority {
                    state_priority = prio;
                    aggregate_state = zone_state;
                }
            }
            o.attributes
                .insert("motion.zone_state".into(), json!(aggregate_state));
            o.attributes
                .insert("motion.zone_ids".into(), json!(inside_zone_ids));

            // ---- group.size ----
            // Same-label tracks whose centers fall within
            // group_radius_box_multiplier × half-perimeter of *this* bbox.
            // Subtract self.
            let half_perim = 0.5 * (o.bbox.width() + o.bbox.height());
            let radius = self.cfg.group_radius_box_multiplier * half_perim;
            let r2 = (radius * radius) as f64;
            let mut group: u32 = 0;
            if let Some(centers) = centers_by_label.get(&o.label) {
                for (cx, cy) in centers {
                    let ddx = (*cx - center.0) as f64;
                    let ddy = (*cy - center.1) as f64;
                    if ddx * ddx + ddy * ddy <= r2 {
                        group = group.saturating_add(1);
                    }
                }
            }
            let group = group.saturating_sub(1); // remove self
            o.attributes.insert("group.size".into(), json!(group));
        }

        self.gc_stale();
    }

    fn gc_stale(&mut self) {
        let max_age = self.cfg.stale_state_frames as u64;
        let tick = self.frame_tick;
        self.state_by_track
            .retain(|_, s| tick.saturating_sub(s.last_seen_tick) <= max_age);
    }

    pub fn name(&self) -> &'static str {
        "track-annotator"
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn is_vehicle_label(label: &str) -> bool {
    // Mirrors v1: `label.startsWith("vehicle")`. Domain labels use the
    // `vehicle.*` taxonomy after `mapCocoToDomainLabel`.
    label.starts_with("vehicle")
}

/// Image-y is down; "n" should mean "up the screen", so flip dy. Returns
/// one of `e | ne | n | nw | w | sw | s | se` matching v1.
fn compass8(dx: f64, dy: f64) -> &'static str {
    use std::f64::consts::TAU;
    let mut a = (-dy).atan2(dx);
    if a < 0.0 {
        a += TAU;
    }
    let bucket = (a / (TAU / 8.0) + 0.5).floor() as i32;
    let idx = (bucket.rem_euclid(8)) as usize;
    const COMPASS: [&str; 8] = ["e", "ne", "n", "nw", "w", "sw", "s", "se"];
    COMPASS[idx]
}

/// Even-odd winding on a normalized (0..1) polygon, evaluated at a
/// normalized point. Returns false if the polygon has fewer than 3
/// vertices.
pub(crate) fn point_in_normalized_polygon(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    if poly.len() < 3 {
        return false;
    }
    let mut inside = false;
    let n = poly.len();
    let xd = x as f64;
    let yd = y as f64;
    for i in 0..n {
        let (p1x, p1y) = poly[i];
        let (p2x, p2y) = poly[(i + 1) % n];
        let p1y = p1y as f64;
        let p2y = p2y as f64;
        let p1x = p1x as f64;
        let p2x = p2x as f64;
        let intersects = ((p1y > yd) != (p2y > yd))
            && (xd < ((p2x - p1x) * (yd - p1y) / ((p2y - p1y) + 1e-9) + p1x));
        if intersects {
            inside = !inside;
        }
    }
    inside
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use nexus_types::{BBox, Frame, PixelFormat};
    use std::sync::Arc;

    fn frame_at(secs: i64, w: u32, h: u32) -> Frame {
        Frame {
            camera_id: 1,
            frame_id: secs as u64,
            captured_at: Utc.timestamp_opt(secs, 0).unwrap(),
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            data: Arc::new(Vec::new()),
            trace_id: "test".into(),
        }
    }

    fn frame_at_ms(ms: i64, w: u32, h: u32) -> Frame {
        Frame {
            camera_id: 1,
            frame_id: ms as u64,
            captured_at: Utc.timestamp_millis_opt(ms).unwrap(),
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            data: Arc::new(Vec::new()),
            trace_id: "test".into(),
        }
    }

    fn obj(track_id: TrackId, label: &str, cx: f32, cy: f32) -> TrackedObject {
        TrackedObject {
            track_id,
            label: label.into(),
            confidence: 0.9,
            bbox: BBox {
                x1: cx - 5.0,
                y1: cy - 5.0,
                x2: cx + 5.0,
                y2: cy + 5.0,
            },
            age_frames: 1,
            age_ms: 0,
            attributes: Default::default(),
        }
    }

    #[test]
    fn stationary_object_classifies_stationary() {
        let mut a = TrackAnnotator::new(AnnotatorConfig::default());
        let mut o1 = vec![obj(1, "person", 100.0, 100.0)];
        a.annotate(&frame_at(0, 1920, 1080), &[], &mut o1);
        assert_eq!(o1[0].attributes["motion.speed_class"], "stationary");
        // Second frame, same position → still stationary.
        let mut o2 = vec![obj(1, "person", 100.0, 100.0)];
        a.annotate(&frame_at(1, 1920, 1080), &[], &mut o2);
        assert_eq!(o2[0].attributes["motion.speed_class"], "stationary");
        assert_eq!(o2[0].attributes["motion.direction"], "none");
    }

    #[test]
    fn fast_horizontal_motion_classifies_running() {
        let mut a = TrackAnnotator::new(AnnotatorConfig::default());
        let mut o1 = vec![obj(1, "person", 100.0, 500.0)];
        a.annotate(&frame_at_ms(0, 1920, 1080), &[], &mut o1);
        // After ~250 px in 1 s → 250 px/s → walking (>30) but not
        // running yet because the EMA is still ramping. Push two more
        // identical-velocity frames to let the EMA settle past 120.
        for i in 1..=4 {
            let mut o = vec![obj(1, "person", 100.0 + 250.0 * i as f32, 500.0)];
            a.annotate(&frame_at_ms(i as i64 * 1000, 1920, 1080), &[], &mut o);
            if i == 4 {
                assert_eq!(o[0].attributes["motion.speed_class"], "running");
                assert_eq!(o[0].attributes["motion.direction"], "e");
            }
        }
    }

    #[test]
    fn vehicle_label_uses_vehicle_speed_threshold() {
        let mut a = TrackAnnotator::new(AnnotatorConfig::default());
        // Same 250 px/s motion, but on a vehicle.* label, doesn't reach
        // the vehicle_speed threshold (250 px/s) — should be running
        // (vehicle uses the running bucket as the next-down rung).
        let mut o1 = vec![obj(1, "vehicle.car", 100.0, 500.0)];
        a.annotate(&frame_at_ms(0, 1920, 1080), &[], &mut o1);
        for i in 1..=4 {
            let mut o = vec![obj(1, "vehicle.car", 100.0 + 250.0 * i as f32, 500.0)];
            a.annotate(&frame_at_ms(i as i64 * 1000, 1920, 1080), &[], &mut o);
            if i == 4 {
                // EMA has settled at ~250 px/sec → running, NOT vehicle_speed.
                assert_eq!(o[0].attributes["motion.speed_class"], "running");
            }
        }
        // Now bump to ~500 px/s — should clear the vehicle_speed bar.
        for i in 5..=10 {
            let mut o = vec![obj(1, "vehicle.car", 100.0 + 500.0 * i as f32, 500.0)];
            a.annotate(&frame_at_ms(i as i64 * 1000, 1920, 1080), &[], &mut o);
            if i == 10 {
                assert_eq!(o[0].attributes["motion.speed_class"], "vehicle_speed");
            }
        }
    }

    #[test]
    fn parked_vehicle_flips_yes_after_min_frames() {
        let cfg = AnnotatorConfig {
            parked_min_frames_to_flag: 3,
            ..Default::default()
        };
        let mut a = TrackAnnotator::new(cfg);
        for i in 0..5 {
            let mut o = vec![obj(1, "vehicle.car", 100.0, 100.0)];
            a.annotate(&frame_at_ms(i * 1000, 1920, 1080), &[], &mut o);
            // i=0,1 → accum 1,2 (still "no"); i=2 → accum 3 (>=3 → "yes").
            let expected = if i >= 2 { "yes" } else { "no" };
            assert_eq!(
                o[0].attributes["motion.parked_vehicle"], expected,
                "frame {i}"
            );
        }
    }

    #[test]
    fn dwell_seconds_counts_from_first_observation() {
        let mut a = TrackAnnotator::new(AnnotatorConfig::default());
        let mut o = vec![obj(1, "person", 100.0, 100.0)];
        a.annotate(&frame_at(0, 1920, 1080), &[], &mut o);
        assert_eq!(o[0].attributes["motion.dwell_seconds"], 0);
        let mut o2 = vec![obj(1, "person", 100.0, 100.0)];
        a.annotate(&frame_at(7, 1920, 1080), &[], &mut o2);
        assert_eq!(o2[0].attributes["motion.dwell_seconds"], 7);
    }

    #[test]
    fn zone_state_fsm_transitions_through_entering_inside_exiting() {
        let mut a = TrackAnnotator::new(AnnotatorConfig::default());
        // Box covering the upper-left quadrant in normalized coords.
        let zone = ZoneConfig {
            id: "z1".into(),
            name: "z1".into(),
            polygon: vec![(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)],
            kind: ZoneKind::Inclusion,
            min_bbox_area_px_override: None,
        };
        let zones = vec![zone];

        // Frame 1: outside (center at 90% across).
        let mut o = vec![obj(1, "person", 1700.0, 100.0)];
        a.annotate(&frame_at(0, 1920, 1080), &zones, &mut o);
        assert_eq!(o[0].attributes["motion.zone_state"], "outside");

        // Frame 2: cross into the zone → entering.
        let mut o = vec![obj(1, "person", 200.0, 200.0)];
        a.annotate(&frame_at(1, 1920, 1080), &zones, &mut o);
        assert_eq!(o[0].attributes["motion.zone_state"], "entering");

        // Frame 3: still inside → inside.
        let mut o = vec![obj(1, "person", 300.0, 300.0)];
        a.annotate(&frame_at(2, 1920, 1080), &zones, &mut o);
        assert_eq!(o[0].attributes["motion.zone_state"], "inside");

        // Frame 4: leave → exiting.
        let mut o = vec![obj(1, "person", 1700.0, 100.0)];
        a.annotate(&frame_at(3, 1920, 1080), &zones, &mut o);
        assert_eq!(o[0].attributes["motion.zone_state"], "exiting");
    }

    #[test]
    fn exclusion_zones_do_not_drive_zone_state() {
        let mut a = TrackAnnotator::new(AnnotatorConfig::default());
        let zone = ZoneConfig {
            id: "x".into(),
            name: "x".into(),
            polygon: vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
            kind: ZoneKind::Exclusion,
            min_bbox_area_px_override: None,
        };
        let mut o = vec![obj(1, "person", 100.0, 100.0)];
        a.annotate(&frame_at(0, 1920, 1080), &[zone], &mut o);
        // Despite being inside the exclusion polygon, zone_state stays
        // outside — exclusion zones are a gate concern, not an annotator one.
        assert_eq!(o[0].attributes["motion.zone_state"], "outside");
    }

    #[test]
    fn zone_ids_lists_inclusion_zones_currently_inside() {
        let mut a = TrackAnnotator::new(AnnotatorConfig::default());
        // Two overlapping inclusion zones + one exclusion zone that
        // covers the whole frame. The exclusion zone must NEVER show
        // up in motion.zone_ids regardless of object position.
        let zones = vec![
            ZoneConfig {
                id: "parking".into(),
                name: "parking".into(),
                polygon: vec![(0.0, 0.0), (0.5, 0.0), (0.5, 1.0), (0.0, 1.0)],
                kind: ZoneKind::Inclusion,
                min_bbox_area_px_override: None,
            },
            ZoneConfig {
                id: "loading_dock".into(),
                name: "loading_dock".into(),
                polygon: vec![(0.2, 0.2), (0.6, 0.2), (0.6, 0.6), (0.2, 0.6)],
                kind: ZoneKind::Inclusion,
                min_bbox_area_px_override: None,
            },
            ZoneConfig {
                id: "should_never_appear".into(),
                name: "x".into(),
                polygon: vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
                kind: ZoneKind::Exclusion,
                min_bbox_area_px_override: None,
            },
        ];

        // Center at (0.15, 0.15) — inside `parking` only.
        let mut o = vec![obj(1, "person", 1920.0 * 0.15, 1080.0 * 0.15)];
        a.annotate(&frame_at(0, 1920, 1080), &zones, &mut o);
        let ids = o[0].attributes["motion.zone_ids"].as_array().unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], "parking");

        // Center at (0.4, 0.4) — inside BOTH parking and loading_dock.
        let mut o = vec![obj(1, "person", 1920.0 * 0.4, 1080.0 * 0.4)];
        a.annotate(&frame_at(1, 1920, 1080), &zones, &mut o);
        let ids = o[0].attributes["motion.zone_ids"].as_array().unwrap();
        let id_strs: Vec<&str> = ids.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(id_strs.contains(&"parking"));
        assert!(id_strs.contains(&"loading_dock"));
        assert!(!id_strs.contains(&"should_never_appear"));

        // Center at (0.9, 0.9) — outside both inclusion zones.
        let mut o = vec![obj(1, "person", 1920.0 * 0.9, 1080.0 * 0.9)];
        a.annotate(&frame_at(2, 1920, 1080), &zones, &mut o);
        let ids = o[0].attributes["motion.zone_ids"].as_array().unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn group_size_counts_same_label_neighbors_minus_self() {
        let mut a = TrackAnnotator::new(AnnotatorConfig::default());
        let mut o = vec![
            obj(1, "person", 100.0, 100.0),
            obj(2, "person", 110.0, 100.0),
            obj(3, "person", 120.0, 100.0),
            obj(4, "dog", 105.0, 100.0),      // different label, ignored
            obj(5, "person", 1000.0, 1000.0), // far away, ignored
        ];
        a.annotate(&frame_at(0, 1920, 1080), &[], &mut o);
        assert_eq!(o[0].attributes["group.size"], 2);
        assert_eq!(o[1].attributes["group.size"], 2);
        assert_eq!(o[2].attributes["group.size"], 2);
        assert_eq!(o[4].attributes["group.size"], 0);
    }

    #[test]
    fn stale_track_state_is_gc_after_threshold() {
        let cfg = AnnotatorConfig {
            stale_state_frames: 2,
            ..Default::default()
        };
        let mut a = TrackAnnotator::new(cfg);
        let mut o = vec![obj(1, "person", 100.0, 100.0)];
        a.annotate(&frame_at(0, 1920, 1080), &[], &mut o);
        assert_eq!(a.state_by_track.len(), 1);

        // Three more frames with NO objects — track 1 is now stale and
        // should be evicted by the 4th frame (3 ticks > 2 stale_state_frames).
        a.annotate(&frame_at(1, 1920, 1080), &[], &mut []);
        a.annotate(&frame_at(2, 1920, 1080), &[], &mut []);
        a.annotate(&frame_at(3, 1920, 1080), &[], &mut []);
        assert_eq!(
            a.state_by_track.len(),
            0,
            "stale track state should be GC'd"
        );
    }

    #[test]
    fn point_in_polygon_basic_square() {
        let sq = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        assert!(point_in_normalized_polygon(0.5, 0.5, &sq));
        assert!(!point_in_normalized_polygon(1.5, 0.5, &sq));
    }

    #[test]
    fn compass8_cardinals() {
        // dx > 0, dy = 0 → east
        assert_eq!(compass8(10.0, 0.0), "e");
        // dx = 0, dy < 0 → image-up → north
        assert_eq!(compass8(0.0, -10.0), "n");
        // dx < 0, dy = 0 → west
        assert_eq!(compass8(-10.0, 0.0), "w");
        // dx = 0, dy > 0 → image-down → south
        assert_eq!(compass8(0.0, 10.0), "s");
    }
}

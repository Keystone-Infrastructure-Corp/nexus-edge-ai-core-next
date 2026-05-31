//! Drop tracked objects whose bbox centre falls inside any
//! [`ZoneKind::Exclusion`] polygon for the camera.
//!
//! Slot: the supervisor calls [`filter_excluded_zones`] *between*
//! [`Tracker::update`](super::Tracker::update) and
//! [`TrackAnnotator::annotate`](super::TrackAnnotator::annotate), so
//! excluded objects:
//!
//!   * never enter the L7 cache or the FRAME_METADATA bus message,
//!   * never accumulate per-track annotator state, and
//!   * never reach the rule evaluator → never fire alerts.
//!
//! The intent of an exclusion zone is "I don't want any record of
//! activity in this region" — typical use is "neighbour's driveway"
//! or "the busy street outside the parking lot". Dropping early
//! (rather than gating only at rule eval) is the simplest mental
//! model: an excluded detection is treated as if it never existed.
//!
//! Inclusion / Dwell zones are intentionally *not* enforced here —
//! they're observational, used by [`TrackAnnotator`] to compute
//! `motion.zone_state`. Only Exclusion is a hard gate.

use nexus_config::{ZoneConfig, ZoneKind};
use nexus_types::{Frame, TrackedObject};

use crate::annotator::point_in_normalized_polygon;

/// Retain only objects whose bbox centre is **not** inside any
/// Exclusion polygon. Mutates `objects` in place. A no-op if there
/// are no Exclusion zones (the common case before any operator
/// has opted in via the admin UI).
///
/// Returns the count of objects dropped — primarily for tracing /
/// metrics; supervisor callers can ignore the return value.
pub fn filter_excluded_zones(
    frame: &Frame,
    zones: &[ZoneConfig],
    objects: &mut Vec<TrackedObject>,
) -> usize {
    // Fast path: no exclusion zones configured.
    if !zones.iter().any(|z| z.kind == ZoneKind::Exclusion) {
        return 0;
    }
    let frame_w = (frame.width as f32).max(1.0);
    let frame_h = (frame.height as f32).max(1.0);
    let before = objects.len();
    objects.retain(|o| {
        let (cx, cy) = o.bbox.center();
        let nx = cx / frame_w;
        let ny = cy / frame_h;
        !zones
            .iter()
            .filter(|z| z.kind == ZoneKind::Exclusion)
            .any(|z| point_in_normalized_polygon(nx, ny, &z.polygon))
    });
    before - objects.len()
}

/// M_PERF_CROWD Phase B1 — per-zone minimum bbox area filter.
///
/// Drops any tracked object whose bbox **centre** falls inside a zone
/// (of any [`ZoneKind`]) that declares
/// [`ZoneConfig::min_bbox_area_px_override`] and whose bbox area
/// (`(x2 − x1) × (y2 − y1)`, in supervisor analysis-frame pixels)
/// is below that override.
///
/// Layered on top of the global
/// [`nexus_config::ModelConfig::min_bbox_area_px`] (applied at the
/// inference layer's [`crate::MinBBoxAreaDetector`] wrapper, so it
/// catches detections before they ever reach the tracker). Per-zone
/// overrides operate on already-tracked objects so a doorway zone
/// with the override unset inherits the lower global threshold and
/// keeps tiny boxes, while a wide-field zone can tighten the
/// threshold to suppress distant noise that survived the global
/// filter only because some other zone needs it.
///
/// Zones without `min_bbox_area_px_override` set are completely
/// ignored — the fast path. Returns the count of objects dropped.
pub fn filter_zone_min_area(
    frame: &Frame,
    zones: &[ZoneConfig],
    objects: &mut Vec<TrackedObject>,
) -> usize {
    // Fast path: no zone declares an override.
    if !zones.iter().any(|z| z.min_bbox_area_px_override.is_some()) {
        return 0;
    }
    let frame_w = (frame.width as f32).max(1.0);
    let frame_h = (frame.height as f32).max(1.0);
    let before = objects.len();
    objects.retain(|o| {
        let area = ((o.bbox.x2 - o.bbox.x1).max(0.0)) * ((o.bbox.y2 - o.bbox.y1).max(0.0));
        let (cx, cy) = o.bbox.center();
        let nx = cx / frame_w;
        let ny = cy / frame_h;
        // Drop iff at least one zone with an override covers the
        // centre AND the bbox area is below that override. We use
        // `any` so the tightest matching zone wins — a single
        // restrictive zone is enough to drop. The fast path above
        // means this loop body never runs on the common no-override
        // configuration.
        !zones.iter().any(|z| {
            let Some(min_area) = z.min_bbox_area_px_override else {
                return false;
            };
            if !point_in_normalized_polygon(nx, ny, &z.polygon) {
                return false;
            }
            area < min_area as f32
        })
    });
    before - objects.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use nexus_config::ZoneKind;
    use nexus_types::{BBox, PixelFormat};

    fn frame() -> Frame {
        Frame {
            camera_id: 1,
            frame_id: 1,
            captured_at: chrono::Utc.timestamp_opt(0, 0).unwrap(),
            width: 1920,
            height: 1080,
            format: PixelFormat::Rgb24,
            data: std::sync::Arc::new(Vec::new()),
            trace_id: "t".into(),
        }
    }

    fn obj(track_id: u64, cx: f32, cy: f32) -> TrackedObject {
        let half_w = 20.0;
        let half_h = 20.0;
        TrackedObject {
            track_id,
            label: "person".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: cx - half_w,
                y1: cy - half_h,
                x2: cx + half_w,
                y2: cy + half_h,
            },
            age_frames: 1,
            age_ms: 33,
            attributes: Default::default(),
        }
    }

    fn exclusion_zone(id: &str, poly: Vec<(f32, f32)>) -> ZoneConfig {
        ZoneConfig {
            id: id.into(),
            name: format!("Excl {id}"),
            polygon: poly,
            kind: ZoneKind::Exclusion,
            min_bbox_area_px_override: None,
        }
    }

    fn inclusion_zone(id: &str, poly: Vec<(f32, f32)>) -> ZoneConfig {
        ZoneConfig {
            id: id.into(),
            name: format!("Incl {id}"),
            polygon: poly,
            kind: ZoneKind::Inclusion,
            min_bbox_area_px_override: None,
        }
    }

    #[test]
    fn no_exclusion_zones_is_noop() {
        let f = frame();
        let zones = vec![inclusion_zone(
            "z1",
            vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
        )];
        let mut objects = vec![obj(1, 100.0, 100.0), obj(2, 800.0, 600.0)];
        let dropped = filter_excluded_zones(&f, &zones, &mut objects);
        assert_eq!(dropped, 0);
        assert_eq!(objects.len(), 2);
    }

    #[test]
    fn drops_object_inside_exclusion_polygon() {
        let f = frame();
        // Exclusion square covering the top-left quarter of the
        // frame (0..0.5 normalized).
        let zones = vec![exclusion_zone(
            "tl",
            vec![(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)],
        )];
        // (200, 200) center is inside the TL quadrant.
        // (1500, 800) is far outside.
        let mut objects = vec![obj(1, 200.0, 200.0), obj(2, 1500.0, 800.0)];
        let dropped = filter_excluded_zones(&f, &zones, &mut objects);
        assert_eq!(dropped, 1);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].track_id, 2);
    }

    #[test]
    fn inclusion_zone_does_not_drop() {
        let f = frame();
        let zones = vec![inclusion_zone(
            "tl",
            vec![(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)],
        )];
        let mut objects = vec![obj(1, 200.0, 200.0)];
        let dropped = filter_excluded_zones(&f, &zones, &mut objects);
        assert_eq!(dropped, 0);
        assert_eq!(objects.len(), 1);
    }

    #[test]
    fn multiple_exclusion_zones_union() {
        let f = frame();
        // Two disjoint exclusion zones — TL quadrant + BR quadrant.
        let zones = vec![
            exclusion_zone("tl", vec![(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)]),
            exclusion_zone("br", vec![(0.5, 0.5), (1.0, 0.5), (1.0, 1.0), (0.5, 1.0)]),
        ];
        let mut objects = vec![
            obj(1, 200.0, 200.0),  // TL → dropped
            obj(2, 1500.0, 800.0), // BR → dropped
            obj(3, 1500.0, 200.0), // TR → kept
            obj(4, 200.0, 800.0),  // BL → kept
        ];
        let dropped = filter_excluded_zones(&f, &zones, &mut objects);
        assert_eq!(dropped, 2);
        let ids: Vec<u64> = objects.iter().map(|o| o.track_id).collect();
        assert_eq!(ids, vec![3, 4]);
    }

    #[test]
    fn mixed_inclusion_and_exclusion_only_exclusion_filters() {
        let f = frame();
        // Inclusion zone covering everything — would be a no-op
        // for filtering. Exclusion zone covering top-left quadrant.
        let zones = vec![
            inclusion_zone("all", vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]),
            exclusion_zone("tl", vec![(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)]),
        ];
        let mut objects = vec![obj(1, 200.0, 200.0), obj(2, 1500.0, 800.0)];
        let dropped = filter_excluded_zones(&f, &zones, &mut objects);
        assert_eq!(dropped, 1);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].track_id, 2);
    }

    #[test]
    fn degenerate_polygon_is_ignored() {
        let f = frame();
        // Polygon with fewer than 3 vertices — point_in_polygon
        // returns false, so nothing is dropped.
        let zones = vec![exclusion_zone("bad", vec![(0.0, 0.0), (0.5, 0.5)])];
        let mut objects = vec![obj(1, 200.0, 200.0)];
        let dropped = filter_excluded_zones(&f, &zones, &mut objects);
        assert_eq!(dropped, 0);
        assert_eq!(objects.len(), 1);
    }

    // ---- filter_zone_min_area (Phase B1 per-zone override) ----

    fn override_zone(id: &str, poly: Vec<(f32, f32)>, min_area: u32) -> ZoneConfig {
        ZoneConfig {
            id: id.into(),
            name: format!("Ov {id}"),
            polygon: poly,
            kind: ZoneKind::Inclusion,
            min_bbox_area_px_override: Some(min_area),
        }
    }

    /// 40-px bbox centred at (cx, cy) → area 1600.
    fn small_obj(track_id: u64, cx: f32, cy: f32) -> TrackedObject {
        let half = 20.0;
        TrackedObject {
            track_id,
            label: "person".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: cx - half,
                y1: cy - half,
                x2: cx + half,
                y2: cy + half,
            },
            age_frames: 1,
            age_ms: 33,
            attributes: Default::default(),
        }
    }

    /// 200-px bbox centred at (cx, cy) → area 40_000.
    fn big_obj(track_id: u64, cx: f32, cy: f32) -> TrackedObject {
        let half = 100.0;
        TrackedObject {
            track_id,
            label: "person".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: cx - half,
                y1: cy - half,
                x2: cx + half,
                y2: cy + half,
            },
            age_frames: 1,
            age_ms: 33,
            attributes: Default::default(),
        }
    }

    #[test]
    fn min_area_no_overrides_is_noop() {
        let f = frame();
        // Zones without overrides → fast path, even Exclusion zones
        // are ignored by this filter (that's filter_excluded_zones'
        // job).
        let zones = vec![
            inclusion_zone("z1", vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]),
            exclusion_zone("z2", vec![(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)]),
        ];
        let mut objects = vec![small_obj(1, 200.0, 200.0), big_obj(2, 1500.0, 800.0)];
        let dropped = filter_zone_min_area(&f, &zones, &mut objects);
        assert_eq!(dropped, 0);
        assert_eq!(objects.len(), 2);
    }

    #[test]
    fn min_area_drops_small_inside_override_zone() {
        let f = frame();
        // Override of 5000 px² covers the top-left quadrant. A 1600
        // bbox in TL is dropped; a 40_000 bbox in TL is kept; a 1600
        // bbox outside is kept (no covering override).
        let zones = vec![override_zone(
            "tl",
            vec![(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)],
            5000,
        )];
        let mut objects = vec![
            small_obj(1, 200.0, 200.0),  // TL, area 1600 → drop
            big_obj(2, 200.0, 300.0),    // TL, area 40_000 → keep
            small_obj(3, 1500.0, 800.0), // BR, no override → keep
        ];
        let dropped = filter_zone_min_area(&f, &zones, &mut objects);
        assert_eq!(dropped, 1);
        let ids: Vec<u64> = objects.iter().map(|o| o.track_id).collect();
        assert_eq!(ids, vec![2, 3]);
    }

    #[test]
    fn min_area_tightest_zone_wins() {
        let f = frame();
        // Two overlapping override zones — one loose (1000), one
        // tight (10_000). An object whose area is 1600 lies between
        // the two thresholds; it falls under the tight override and
        // is dropped.
        let zones = vec![
            override_zone(
                "loose",
                vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
                1000,
            ),
            override_zone(
                "tight",
                vec![(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)],
                10_000,
            ),
        ];
        let mut objects = vec![
            small_obj(1, 200.0, 200.0),  // covered by both, area 1600 < 10_000 → drop
            small_obj(2, 1500.0, 800.0), // covered by loose only, area 1600 > 1000 → keep
        ];
        let dropped = filter_zone_min_area(&f, &zones, &mut objects);
        assert_eq!(dropped, 1);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].track_id, 2);
    }

    #[test]
    fn min_area_exclusion_zone_with_override_still_filters() {
        let f = frame();
        // ZoneKind::Exclusion with a min_bbox_area_px_override — the
        // exclusion semantic is filter_excluded_zones' job; here we
        // just verify that the per-zone area filter does not skip
        // the override based on kind.
        let zones = vec![ZoneConfig {
            id: "x".into(),
            name: "x".into(),
            polygon: vec![(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)],
            kind: ZoneKind::Exclusion,
            min_bbox_area_px_override: Some(5000),
        }];
        let mut objects = vec![small_obj(1, 200.0, 200.0)];
        let dropped = filter_zone_min_area(&f, &zones, &mut objects);
        assert_eq!(dropped, 1);
        assert!(objects.is_empty());
    }
}

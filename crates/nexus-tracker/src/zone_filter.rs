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
        }
    }

    fn inclusion_zone(id: &str, poly: Vec<(f32, f32)>) -> ZoneConfig {
        ZoneConfig {
            id: id.into(),
            name: format!("Incl {id}"),
            polygon: poly,
            kind: ZoneKind::Inclusion,
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
}

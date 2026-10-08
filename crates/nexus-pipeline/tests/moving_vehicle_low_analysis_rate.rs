//! #362: a vehicle crossing a parking-lot camera that is analysed every
//! ~690 ms must reach the rules. At that rate a moving vehicle's
//! consecutive boxes do not overlap, so with IoU-only association every
//! detection became a one-detection track that no rule ever evaluated.
//!
//! Drives the real ByteTrack tracker, the parking-lot static filter and the
//! rule evaluator in the supervisor's order, with synthetic capture times.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use nexus_config::{
    ByteTrackConfig, RuleConfig, RuleDebounce, RuleGates, RulePredicate, RulesConfig,
    StaticObjectConfig,
};
use nexus_rules::RuleEvaluator;
use nexus_tracker::{is_object_static, ByteTrackTracker, StaticObjectFilter, Tracker};
use nexus_types::{BBox, Detection, Frame, PixelFormat};

const W: u32 = 512;
const H: u32 = 288;
/// The field core's measured analysis interval per camera (~1.45 fps).
const INTERVAL_MS: u64 = 690;

/// "All People and Vehicles", the rule on the #362 camera.
fn field_rule() -> RuleConfig {
    RuleConfig {
        id: "all_people_and_vehicles".into(),
        name: "All People and Vehicles".into(),
        predicate: RulePredicate {
            when: "(object.label == 'person' || (object.label.startsWith('vehicle.'))) \
                   && object.confidence >= 0.5"
                .into(),
            severity: "high".into(),
        },
        gates: RuleGates::default(),
        debounce: RuleDebounce {
            min_track_age_ms: 500,
            consecutive_frames: 2,
            cooldown_ms: 30_000,
        },
        enabled: true,
        sinks: Vec::new(),
        verify: false,
    }
}

/// A vehicle `length` px long crossing the frame at 0.625 of its length per
/// analysed frame: (length, height, frames in view). The near one steps
/// 75 px, past the static filter's 60 px ID-reuse guard.
const CROSSINGS: [(f32, f32, u64); 2] = [(80.0, 50.0, 8), (120.0, 70.0, 5)];

/// Alerts fired while one vehicle crosses the frame.
fn alerts_for_a_crossing(
    tracker_cfg: ByteTrackConfig,
    (length, height, frames): (f32, f32, u64),
) -> usize {
    let tracker = ByteTrackTracker::new(tracker_cfg);
    // MORGAN's ID-reuse guard is the default 60 px.
    let mut filter = StaticObjectFilter::new(StaticObjectConfig::default(), 14, None);
    let eval = RuleEvaluator::new(&RulesConfig::default(), &[field_rule()]).unwrap();
    let t0 = Instant::now();
    let mut fired = 0;
    for i in 0..frames {
        let x = 10.0 + i as f32 * length * 0.625;
        let frame = Frame {
            camera_id: 14,
            frame_id: i,
            captured_at: Utc.timestamp_millis_opt((i * INTERVAL_MS) as i64).unwrap(),
            captured_mono: t0 + Duration::from_millis(i * INTERVAL_MS),
            width: W,
            height: H,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![]),
            trace_id: format!("trace-{i}"),
        };
        let car = Detection {
            label: "vehicle.car".into(),
            confidence: 0.6,
            bbox: BBox {
                x1: x,
                y1: 150.0,
                x2: x + length,
                y2: 150.0 + height,
            },
            attributes: Default::default(),
        };
        let mut tracked = tracker.update(vec![car], frame.captured_mono);
        filter.classify(&frame, &mut tracked);
        let dynamic = tracked.iter().filter(|t| !is_object_static(t));
        fired += eval
            .evaluate(
                14,
                i,
                frame.captured_at,
                frame.captured_mono,
                &frame.trace_id,
                W,
                H,
                &[],
                dynamic,
            )
            .len();
    }
    fired
}

#[test]
fn a_vehicle_crossing_a_parking_lot_camera_at_the_field_rate_alerts_once() {
    for crossing in CROSSINGS {
        assert_eq!(
            alerts_for_a_crossing(ByteTrackConfig::default(), crossing),
            1,
            "{crossing:?}"
        );
    }
}

#[test]
fn without_the_motion_pass_the_same_crossing_never_alerts() {
    let cfg = ByteTrackConfig {
        motion_match_box_lengths_per_sec: 0.0,
        ..Default::default()
    };
    for crossing in CROSSINGS {
        assert_eq!(
            alerts_for_a_crossing(cfg.clone(), crossing),
            0,
            "{crossing:?}"
        );
    }
}

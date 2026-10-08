//! #362: a vehicle crossing a parking-lot camera that is analysed every
//! ~690 ms must reach the rules. At that rate a moving vehicle's
//! consecutive boxes do not overlap, so with IoU-only association every
//! detection became a one-detection track that no rule ever evaluated.
//!
//! Drives the real ByteTrack tracker, the parking-lot static filter and the
//! rule evaluator in the supervisor's order, with synthetic capture times:
//! every tracked object goes to the rules, which skip the static ones.

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

/// "All People and Vehicles", the rule on the #362 camera, with its
/// debounce as given.
fn field_rule(consecutive_frames: u32, cooldown_ms: u64) -> RuleConfig {
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
            consecutive_frames,
            cooldown_ms,
        },
        enabled: true,
        sinks: Vec::new(),
        verify: false,
    }
}

/// One vehicle's pass: its box size, and its box's left edge on each analysed
/// frame (`None`: not detected on that frame).
type Pass = ((f32, f32), Vec<Option<f32>>);

/// Constant-speed crossings at 0.625 of the box length per analysed frame.
/// The near one steps 75 px, past the static filter's 60 px ID-reuse guard.
fn crossings() -> Vec<Pass> {
    vec![
        (
            (80.0, 50.0),
            (0..8).map(|i| Some(10.0 + i as f32 * 50.0)).collect(),
        ),
        (
            (120.0, 70.0),
            (0..5).map(|i| Some(10.0 + i as f32 * 75.0)).collect(),
        ),
    ]
}

/// The crossings, plus vehicles that stand still for a frame and then
/// leave: one accelerating away, one driving off and then missed for two
/// frames.
fn every_pass() -> Vec<Pass> {
    let mut passes = crossings();
    let xs = |v: &[Option<f32>]| v.to_vec();
    passes.push((
        (120.0, 70.0),
        xs(&[10.0, 15.0, 45.0, 90.0, 150.0, 225.0, 300.0].map(Some)),
    ));
    passes.push((
        (100.0, 60.0),
        xs(&[
            Some(0.0),
            Some(5.0),
            Some(45.0),
            Some(95.0),
            Some(160.0),
            Some(225.0),
            None,
            None,
            Some(390.0),
            Some(445.0),
        ]),
    ));
    passes
}

fn car(x: f32, y1: f32, (length, height): (f32, f32)) -> Detection {
    Detection {
        label: "vehicle.car".into(),
        confidence: 0.6,
        bbox: BBox {
            x1: x,
            y1,
            x2: x + length,
            y2: y1 + height,
        },
        attributes: Default::default(),
    }
}

/// A row of parked cars just above the lane, whose centres the near
/// crossing passes within the static filter's 40 px anchor radius.
const PARKED_ROW: [f32; 3] = [10.0, 160.0, 310.0];
/// Frames the row stands before a pass starts: long enough at this rate for
/// the default 5 s dwell to anchor it, and for the 30 s cooldown its own
/// arrival alerts start to run out.
const PARKED_LEAD_FRAMES: u64 = 50;

/// Alerts fired during one pass.
fn alerts_for(tracker_cfg: ByteTrackConfig, rule: RuleConfig, pass: &Pass) -> usize {
    alerts_beside(tracker_cfg, rule, pass, &[])
}

/// Alerts fired during one pass, beside cars parked at `parked` that stand
/// from the start and are anchored before the pass begins. Alerts the parked
/// cars raise on arrival are not counted.
fn alerts_beside(
    tracker_cfg: ByteTrackConfig,
    rule: RuleConfig,
    (size, xs): &Pass,
    parked: &[f32],
) -> usize {
    let tracker = ByteTrackTracker::new(tracker_cfg);
    // MORGAN's ID-reuse guard is the default 60 px.
    let mut filter = StaticObjectFilter::new(StaticObjectConfig::default(), 14, None);
    let eval = RuleEvaluator::new(&RulesConfig::default(), &[rule]).unwrap();
    let lead = if parked.is_empty() {
        0
    } else {
        PARKED_LEAD_FRAMES
    };
    let t0 = Instant::now();
    let mut fired = 0;
    for i in 0..lead + xs.len() as u64 {
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
        let mut detections: Vec<Detection> = parked
            .iter()
            .map(|&x| car(x, 130.0, (120.0, 70.0)))
            .collect();
        if let Some(Some(x)) = i.checked_sub(lead).map(|k| xs[k as usize]) {
            detections.push(car(x, 150.0, *size));
        }
        let mut tracked = tracker.update(detections, frame.captured_mono);
        filter.classify(&frame, &mut tracked);
        if i + 1 == lead {
            assert!(
                tracked.iter().all(is_object_static),
                "the parked row must be anchored before the pass"
            );
        }
        let alerts = eval
            .evaluate(
                14,
                i,
                frame.captured_at,
                frame.captured_mono,
                &frame.trace_id,
                W,
                H,
                &[],
                &tracked,
            )
            .len();
        if i >= lead {
            fired += alerts;
        }
    }
    fired
}

#[test]
fn a_vehicle_crossing_a_parking_lot_camera_at_the_field_rate_alerts_once() {
    for pass in every_pass() {
        assert_eq!(
            alerts_for(ByteTrackConfig::default(), field_rule(2, 30_000), &pass),
            1,
            "{pass:?}"
        );
    }
}

#[test]
fn without_the_motion_pass_the_same_crossing_never_alerts() {
    let cfg = ByteTrackConfig {
        motion_match_box_lengths_per_sec: 0.0,
        ..Default::default()
    };
    for pass in crossings() {
        assert_eq!(
            alerts_for(cfg.clone(), field_rule(2, 30_000), &pass),
            0,
            "{pass:?}"
        );
    }
}

/// With no cooldown, only the parking-lot alert episode stops a vehicle
/// alerting again during its visit: a vehicle that never parks has one.
#[test]
fn every_pass_alerts_once_with_no_cooldown() {
    for (consecutive_frames, pass) in [1, 2]
        .into_iter()
        .flat_map(|c| every_pass().into_iter().map(move |p| (c, p)))
    {
        assert_eq!(
            alerts_for(
                ByteTrackConfig::default(),
                field_rule(consecutive_frames, 0),
                &pass
            ),
            1,
            "consecutive_frames {consecutive_frames}: {pass:?}"
        );
    }
}

/// Review of #369 (P2): a vehicle at a steady speed while the analysis rate
/// rises from one frame per 690 ms to one per 133 ms is one vehicle, one
/// alert, even with no cooldown.
#[test]
fn a_vehicle_alerts_once_when_the_analysis_rate_rises() {
    let mut frames: Vec<(u64, f32)> = [
        0.0f32, 20.0, 50.0, 90.0, 140.0, 200.0, 270.0, 350.0, 430.0, 510.0, 590.0,
    ]
    .iter()
    .enumerate()
    .map(|(i, &x)| (i as u64 * INTERVAL_MS, x))
    .collect();
    let t_last = 10 * INTERVAL_MS;
    frames.extend((1..=6u64).map(|k| (t_last + k * 133, 590.0 + 80.0 * (k * 133) as f32 / 690.0)));
    let tracker = ByteTrackTracker::new(ByteTrackConfig::default());
    let mut filter = StaticObjectFilter::new(StaticObjectConfig::default(), 14, None);
    let eval = RuleEvaluator::new(&RulesConfig::default(), &[field_rule(2, 0)]).unwrap();
    let t0 = Instant::now();
    let mut fired = 0;
    for (i, (ms, x)) in frames.into_iter().enumerate() {
        let frame = Frame {
            camera_id: 14,
            frame_id: i as u64,
            captured_at: Utc.timestamp_millis_opt(ms as i64).unwrap(),
            captured_mono: t0 + Duration::from_millis(ms),
            width: 1024,
            height: 576,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![]),
            trace_id: format!("trace-{i}"),
        };
        let mut tracked = tracker.update(vec![car(x, 150.0, (100.0, 60.0))], frame.captured_mono);
        filter.classify(&frame, &mut tracked);
        fired += eval
            .evaluate(
                14,
                i as u64,
                frame.captured_at,
                frame.captured_mono,
                &frame.trace_id,
                1024,
                576,
                &[],
                &tracked,
            )
            .len();
    }
    assert_eq!(fired, 1);
}

/// Review of #369: a leader seen three times and then lost, and a follower
/// behind it. Two vehicles, two alerts, as with IoU-only association.
#[test]
fn a_follower_seen_after_its_leader_is_lost_alerts_on_its_own() {
    let pass: Pass = (
        (100.0, 60.0),
        [350.0, 380.0, 410.0, 270.0, 300.0, 330.0]
            .map(Some)
            .to_vec(),
    );
    assert_eq!(
        alerts_for(ByteTrackConfig::default(), field_rule(2, 0), &pass),
        2
    );
}

/// A lane in front of a row of anchored parked cars, the usual parking-lot
/// view: the pass comes within a parked car's anchor radius on some frames.
/// The field rule alerts exactly once and every rule alerts at least once.
/// Exactly once does not hold in general for a rule whose cooldown is
/// shorter than the pass: once the static filter counts the vehicle as
/// moving, each anchor it crosses reads as that car departing and starts a
/// new episode (BUG-260, SPEC-077).
#[test]
fn every_pass_alerts_beside_a_row_of_parked_cars() {
    for pass in every_pass() {
        let alerts = |c, cd| {
            alerts_beside(
                ByteTrackConfig::default(),
                field_rule(c, cd),
                &pass,
                &PARKED_ROW,
            )
        };
        assert_eq!(alerts(2, 30_000), 1, "field rule: {pass:?}");
        assert!(alerts(2, 0) >= 1, "no cooldown: {pass:?}");
        assert!(alerts(1, 0) >= 1, "per-frame rule: {pass:?}");
    }
}

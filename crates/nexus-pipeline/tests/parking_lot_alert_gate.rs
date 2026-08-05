//! Cross-crate regression suite for the parking-lot alert gate (BUG-020).
//!
//! The defect: in `parking_lot_mode`, a vehicle that had been sitting in
//! the lot for hours kept re-alerting every `cooldown_ms` window, because
//! the static-object FSM exposed only a final `tracker.is_static` verdict
//! and nothing that represented *an alert episode* during the dwell
//! learning window. The contract the fix establishes is:
//!
//!   **One alert on first sight, silence for as long as the object stays
//!   put, and exactly one more alert after it breaks the static gate.**
//!
//! That contract spans two crates that do NOT depend on each other:
//! `nexus-tracker` stamps `ALERT_EPOCH_ATTRIBUTE_KEY` and `nexus-rules`
//! reads the same key back as a private string literal. Nothing in the
//! type system links them, so these tests are the only thing standing
//! between a rename and a silently-reverted fix. They live in
//! `nexus-pipeline` because it is the lowest crate that depends on both.
//!
//! `Sim::step` mirrors the supervisor's real wiring — see
//! `classify()` + the `!is_object_static(t)` partition in
//! `nexus-pipeline/src/supervisor.rs`. Keep the two in sync.

use chrono::{TimeZone, Utc};
use nexus_config::{
    RuleConfig, RuleDebounce, RuleGates, RulePredicate, RulesBackendKind, RulesConfig,
    StaticObjectConfig,
};
use nexus_rules::RuleEvaluator;
use nexus_tracker::{is_object_static, StaticObjectFilter, ALERT_EPOCH_ATTRIBUTE_KEY};
use nexus_types::{AlertEvent, BBox, Frame, PixelFormat, TrackedObject};
use std::sync::Arc;

const FRAME_W: u32 = 1920;
const FRAME_H: u32 = 1080;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Tight thresholds so a full park → break cycle fits in a handful of
/// frames. `track_id_reuse_reset_pixels: 0` disables the ID-reuse guard,
/// which would otherwise wipe per-track state when the gate-break test
/// slides the vehicle 50px in one frame.
///
/// `persistence_enabled: true` matches the production default and keeps
/// the in-memory anchor registry live; with a `None` registry path
/// nothing touches the disk.
fn tight_static_cfg() -> StaticObjectConfig {
    StaticObjectConfig {
        dwell_frames: 3,
        significant_movement_pixels: 10,
        significant_movement_frames: 2,
        movement_ema_alpha: 1.0,
        match_distance_pixels: 5,
        track_id_reuse_reset_pixels: 0,
        anchor_ttl_secs: 0,
        persistence_enabled: true,
    }
}

fn rules_cfg() -> RulesConfig {
    RulesConfig {
        backend: RulesBackendKind::Cel,
        inline: vec![],
    }
}

/// A rule that fires on every object carrying `label`, with no zone gate
/// and no debounce, so any alert suppression the tests observe comes
/// from the static-alert gate rather than from the debounce machinery.
fn rule_for(label: &str, cooldown_ms: u64) -> RuleConfig {
    RuleConfig {
        id: format!("any_{label}"),
        name: format!("any {label}"),
        predicate: RulePredicate {
            when: format!("object.label == '{label}'"),
            severity: "high".into(),
        },
        gates: RuleGates {
            camera_filter: None,
            zones: None,
        },
        debounce: RuleDebounce {
            min_track_age_ms: 0,
            consecutive_frames: 1,
            cooldown_ms,
        },
        enabled: true,
        sinks: Vec::new(),
        verify: false,
    }
}

fn frame(frame_id: u64) -> Frame {
    Frame {
        camera_id: 1,
        frame_id,
        captured_at: Utc.timestamp_millis_opt(frame_id as i64 * 33).unwrap(),
        width: FRAME_W,
        height: FRAME_H,
        format: PixelFormat::Rgb24,
        data: Arc::new(vec![]),
        trace_id: format!("trace-{frame_id}"),
    }
}

fn object(track_id: u64, label: &str, cx: f32, cy: f32) -> TrackedObject {
    let bbox = BBox {
        x1: cx - 25.0,
        y1: cy - 15.0,
        x2: cx + 25.0,
        y2: cy + 15.0,
    };
    TrackedObject {
        track_id,
        label: label.into(),
        confidence: 0.95,
        bbox,
        // The rules engine skips objects without a frame-aligned box
        // (the coasting-track guard), so every fixture needs one.
        detection_bbox: Some(bbox),
        age_frames: 10,
        age_ms: 1000,
        attributes: serde_json::Map::new(),
    }
}

/// One camera's worth of the supervisor's per-frame path: static
/// classification → partition → rule evaluation.
struct Sim {
    filter: Option<StaticObjectFilter>,
    evaluator: RuleEvaluator,
    frame_id: u64,
}

impl Sim {
    /// `filter: None` models a camera with `parking_lot_mode = false`.
    fn new(filter: Option<StaticObjectFilter>, rules: &[RuleConfig]) -> Self {
        Self {
            filter,
            evaluator: RuleEvaluator::new(&rules_cfg(), rules).expect("build evaluator"),
            frame_id: 0,
        }
    }

    fn step(&mut self, mut objects: Vec<TrackedObject>) -> Vec<AlertEvent> {
        let f = frame(self.frame_id);
        self.frame_id += 1;

        let dynamic: Vec<TrackedObject> = match self.filter.as_mut() {
            Some(filter) => {
                filter.classify(&f, &mut objects);
                objects
                    .iter()
                    .filter(|t| !is_object_static(t))
                    .cloned()
                    .collect()
            }
            None => objects,
        };

        self.evaluator
            .evaluate(1, f.frame_id, &f.trace_id, FRAME_W, FRAME_H, &[], &dynamic)
    }

    /// Run `frames` steps with the object parked at a fixed point and
    /// return the frame indices on which an alert fired.
    fn run_parked(&mut self, frames: u64, track_id: u64, label: &str) -> Vec<u64> {
        let mut fired = Vec::new();
        for _ in 0..frames {
            let idx = self.frame_id;
            if !self
                .step(vec![object(track_id, label, 500.0, 500.0)])
                .is_empty()
            {
                fired.push(idx);
            }
        }
        fired
    }
}

// ---------------------------------------------------------------------------
// The cross-crate contract
// ---------------------------------------------------------------------------

#[test]
fn alert_epoch_attribute_key_is_the_agreed_wire_string() {
    // `nexus-rules` cannot import this constant (it does not depend on
    // `nexus-tracker`), so it re-declares the same string. If either
    // side is renamed without the other, the parking gate silently
    // stops working and every parked vehicle re-alerts again.
    assert_eq!(
        ALERT_EPOCH_ATTRIBUTE_KEY, "tracker.static_alert_epoch",
        "attribute key is a cross-crate contract with nexus-rules; \
         update both sides together"
    );
}

#[test]
fn rules_engine_honours_the_epoch_key_produced_by_the_tracker() {
    // Behavioural half of the contract test above: stamp an object
    // using the *tracker's* constant and prove the *rules* crate
    // recognises it. A drift on either side turns this into 2 alerts.
    let mut ev = Sim::new(None, &[rule_for("vehicle.car", 0)]);
    let mut o = object(7, "vehicle.car", 500.0, 500.0);
    o.attributes
        .insert(ALERT_EPOCH_ATTRIBUTE_KEY.to_string(), serde_json::json!(0));

    let first = ev.step(vec![o.clone()]);
    let second = ev.step(vec![o]);

    assert_eq!(first.len(), 1, "first sight alerts");
    assert!(
        second.is_empty(),
        "same epoch must not re-alert even with cooldown_ms = 0"
    );
}

// ---------------------------------------------------------------------------
// BUG-020: the defect itself
// ---------------------------------------------------------------------------

#[test]
fn parked_vehicle_alerts_once_across_dwell_and_the_whole_parked_window() {
    // The regression. `cooldown_ms = 0` is the worst case: before the
    // fix this produced one alert per frame for the entire dwell window
    // (and then again on every cooldown expiry for as long as the car
    // sat there).
    let filter = StaticObjectFilter::new(tight_static_cfg(), 1, None);
    let mut sim = Sim::new(Some(filter), &[rule_for("vehicle.car", 0)]);

    let fired = sim.run_parked(40, 7, "vehicle.car");

    assert_eq!(
        fired,
        vec![0],
        "a parked vehicle must alert exactly once, on first sight"
    );
}

#[test]
fn vehicle_alerts_again_only_after_it_breaks_the_static_gate() {
    let filter = StaticObjectFilter::new(tight_static_cfg(), 1, None);
    let mut sim = Sim::new(Some(filter), &[rule_for("vehicle.car", 0)]);

    // Park it long enough to be promoted and suppressed (frames 0..9).
    let parked = sim.run_parked(10, 7, "vehicle.car");
    assert_eq!(parked, vec![0], "one alert on arrival");

    // Now drive away: 50px per frame, far above the 10px threshold.
    // The gate needs `significant_movement_frames = 2` consecutive
    // moving frames, so the new epoch opens on frame 11, not 10.
    let mut fired_after_break = Vec::new();
    for i in 1..=5u64 {
        let idx = sim.frame_id;
        let alerts = sim.step(vec![object(
            7,
            "vehicle.car",
            500.0 + i as f32 * 50.0,
            500.0,
        )]);
        if !alerts.is_empty() {
            fired_after_break.push(idx);
        }
    }

    assert_eq!(
        fired_after_break,
        vec![11],
        "breaking the static gate must open exactly one new alert epoch on \
         the frame the gate breaks, not one alert per moving frame"
    );
}

#[test]
fn parked_car_reacquired_under_a_new_track_id_does_not_re_alert() {
    // The real-world shape of BUG-020: a passing pedestrian occludes a
    // parked car, the tracker retires the track and re-acquires it under
    // a fresh id. The per-track epoch is useless here (new track = new
    // state), so suppression has to come from the anchor registry
    // matching the new track against the parked car's stored position.
    let filter = StaticObjectFilter::new(tight_static_cfg(), 1, None);
    let mut sim = Sim::new(Some(filter), &[rule_for("vehicle.car", 0)]);

    assert_eq!(
        sim.run_parked(10, 7, "vehicle.car"),
        vec![0],
        "one alert on arrival"
    );

    // Same spot, brand-new track id, for a good long while.
    let fired = sim.run_parked(20, 8, "vehicle.car");

    assert!(
        fired.is_empty(),
        "tracker id churn on a parked car must not re-alert (fired on {fired:?})"
    );
}

#[test]
fn id_churn_re_alerts_once_when_the_anchor_registry_is_disabled() {
    // Documented consequence of `persistence_enabled = false`: the
    // anchor registry is inert, so a re-acquired parked car has nothing
    // to match against and pays for one alert before its fresh dwell
    // window suppresses it again. Operators who turn the registry off
    // are opting into this; the test exists so nobody "fixes" the
    // registry-enabled path by weakening it to match.
    let mut cfg = tight_static_cfg();
    cfg.persistence_enabled = false;
    let mut sim = Sim::new(
        Some(StaticObjectFilter::new(cfg, 1, None)),
        &[rule_for("vehicle.car", 0)],
    );

    assert_eq!(sim.run_parked(10, 7, "vehicle.car"), vec![0]);
    let fired = sim.run_parked(20, 8, "vehicle.car");

    assert_eq!(
        fired.len(),
        1,
        "without an anchor registry a re-acquired car alerts once, then \
         re-earns suppression (fired on {fired:?})"
    );
}

#[test]
fn a_different_car_parking_in_a_vacated_spot_alerts_once() {
    // The flip side of the anchor match: once the original car breaks
    // the gate its anchor is removed, so the spot must not stay
    // permanently deaf to new arrivals.
    let filter = StaticObjectFilter::new(tight_static_cfg(), 1, None);
    let mut sim = Sim::new(Some(filter), &[rule_for("vehicle.car", 0)]);

    sim.run_parked(10, 7, "vehicle.car");
    // Drive off, clearing the anchor.
    for i in 1..=4u64 {
        sim.step(vec![object(
            7,
            "vehicle.car",
            500.0 + i as f32 * 50.0,
            500.0,
        )]);
    }

    // A different car takes the vacated spot.
    let fired = sim.run_parked(20, 42, "vehicle.car");

    assert_eq!(
        fired.len(),
        1,
        "a new arrival in a vacated spot must alert exactly once \
         (fired on {fired:?})"
    );
}

#[test]
fn coasting_frame_does_not_resurrect_a_spent_alert_epoch() {
    // The tracker keeps emitting a lost track for a few frames with
    // `detection_bbox: None`. The rules engine skips those objects but
    // MUST still count them as "present" when pruning its per-track
    // alert state — otherwise a single coasting frame wipes the
    // "already alerted" marker and the next real detection re-alerts.
    let filter = StaticObjectFilter::new(tight_static_cfg(), 1, None);
    let mut sim = Sim::new(Some(filter), &[rule_for("vehicle.car", 0)]);

    assert_eq!(
        sim.step(vec![object(7, "vehicle.car", 500.0, 500.0)]).len(),
        1
    );

    let mut coasting = object(7, "vehicle.car", 500.0, 500.0);
    coasting.detection_bbox = None;
    assert!(
        sim.step(vec![coasting]).is_empty(),
        "coasting frame must not alert"
    );

    assert!(
        sim.step(vec![object(7, "vehicle.car", 500.0, 500.0)])
            .is_empty(),
        "re-acquiring the track must not re-open the spent epoch"
    );
}

#[test]
fn equipment_anchor_class_alerts_once_like_a_vehicle() {
    // Phase 8.1 `static_anchor_classes` (a ladder left in the yard)
    // participates in the same FSM and must get the same one-shot
    // treatment; it regressed to per-cooldown re-alerts when the epoch
    // stamp was gated on vehicle labels only.
    let filter = StaticObjectFilter::with_anchor_classes(
        tight_static_cfg(),
        1,
        None,
        vec!["ladder".to_string()],
    );
    let mut sim = Sim::new(Some(filter), &[rule_for("ladder", 0)]);

    let fired = sim.run_parked(40, 9, "ladder");

    assert_eq!(
        fired,
        vec![0],
        "a stationary ladder must alert exactly once"
    );
}

// ---------------------------------------------------------------------------
// Non-regression: everything that is NOT a parked anchor is untouched
// ---------------------------------------------------------------------------

#[test]
fn person_on_a_parking_lot_camera_keeps_legacy_repeat_alerts() {
    // People are never anchor-eligible, so they carry no epoch and must
    // keep the pre-existing camera-scoped debounce/cooldown semantics —
    // a loiterer standing still is exactly what the operator wants to
    // keep hearing about.
    let filter = StaticObjectFilter::new(tight_static_cfg(), 1, None);
    let mut sim = Sim::new(Some(filter), &[rule_for("person", 0)]);

    let fired = sim.run_parked(5, 3, "person");

    assert_eq!(
        fired,
        vec![0, 1, 2, 3, 4],
        "person alerts must not be gated by the static-alert epoch"
    );
}

#[test]
fn parking_lot_mode_disabled_leaves_vehicle_alerts_unchanged() {
    // No static filter → no epoch attribute → legacy path. Guards
    // against the gate leaking onto ordinary driveway/doorway cameras.
    let mut sim = Sim::new(None, &[rule_for("vehicle.car", 0)]);

    let fired = sim.run_parked(5, 7, "vehicle.car");

    assert_eq!(
        fired,
        vec![0, 1, 2, 3, 4],
        "cameras without parking_lot_mode keep the old behaviour"
    );
}

#[test]
fn camera_cooldown_still_paces_alerts_across_distinct_vehicles() {
    // The epoch gate makes alerting per-track, but `cooldown_ms` stays
    // deliberately camera-scoped so tracker ID churn cannot be used to
    // bypass it. Two fresh vehicles in one frame → one alert.
    let filter = StaticObjectFilter::new(tight_static_cfg(), 1, None);
    let mut sim = Sim::new(Some(filter), &[rule_for("vehicle.car", 60_000)]);

    let alerts = sim.step(vec![
        object(10, "vehicle.car", 300.0, 300.0),
        object(20, "vehicle.car", 900.0, 700.0),
    ]);

    assert_eq!(
        alerts.len(),
        1,
        "camera-scoped cooldown must still throttle simultaneous tracks"
    );
}

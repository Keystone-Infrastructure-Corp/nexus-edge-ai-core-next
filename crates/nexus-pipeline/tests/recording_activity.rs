//! SPEC-089 Task 1: the pure fresh-evidence recording activity policy, the
//! per-backend observation predicate on real tracker output, and the dormant
//! `recording_policy` camera config.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use nexus_config::{
    ByteTrackConfig, CameraConfig, RecordingPolicyConfig, RecordingPolicyMode, TrackerBackendKind,
    TrackerConfig,
};
use nexus_pipeline::recording_activity::{
    is_observed, AnalysisOutcome, Observation, RecordingActivityPolicy, RecordingReason,
};
use nexus_tracker::{ByteTrackTracker, IouNaiveTracker, Tracker};
use nexus_types::{BBox, Detection, TrackedObject};

#[test]
fn predicted_frames_do_not_renew_recording() {
    let mut p = RecordingActivityPolicy::new(10_000, 10_000, 0);
    assert!(p.tick(0).open);
    for t in [0, 2_000, 4_000] {
        p.observe(Observation {
            captured_ms: t,
            received_ms: t,
            qualifying_observed: 0,
            predicted_only: 0,
            outcome: AnalysisOutcome::Success,
        });
    }
    p.observe(Observation {
        captured_ms: 6_000,
        received_ms: 6_000,
        qualifying_observed: 1,
        predicted_only: 0,
        outcome: AnalysisOutcome::Success,
    });
    assert!(p.tick(6_000).open);
    for t in [8_000, 10_000, 12_000, 14_000, 16_000] {
        p.observe(Observation {
            captured_ms: t,
            received_ms: t,
            qualifying_observed: 0,
            predicted_only: 1,
            outcome: AnalysisOutcome::Success,
        });
    }
    assert!(!p.tick(16_000).open);
}

const POST_ROLL: u64 = 10_000;
const STALE: u64 = 10_000;

fn obs(t: u64, observed: u32, predicted: u32, outcome: AnalysisOutcome) -> Observation {
    Observation {
        captured_ms: t,
        received_ms: t,
        qualifying_observed: observed,
        predicted_only: predicted,
        outcome,
    }
}

fn success(t: u64, observed: u32) -> Observation {
    obs(t, observed, 0, AnalysisOutcome::Success)
}

/// Empty successful passes every second over `[from, to]`.
fn quiet(p: &mut RecordingActivityPolicy, from: u64, to: u64) {
    for t in (from..=to).step_by(1_000) {
        p.observe(success(t, 0));
    }
}

/// Healthy and closed at 30 s: recovered at 2 s, its one window long expired.
fn healthy_quiet_at_30s() -> RecordingActivityPolicy {
    let mut p = RecordingActivityPolicy::new(POST_ROLL, STALE, 0);
    quiet(&mut p, 0, 30_000);
    let i = p.tick(30_000);
    assert_eq!((i.open, i.reason), (false, RecordingReason::Quiet));
    p
}

fn state(p: &mut RecordingActivityPolicy, now: u64) -> (bool, RecordingReason) {
    let i = p.tick(now);
    (i.open, i.reason)
}

const OPEN_ACTIVITY: (bool, RecordingReason) = (true, RecordingReason::Activity);
const OPEN_POST_ROLL: (bool, RecordingReason) = (true, RecordingReason::PostRoll);
const OPEN_DEGRADED: (bool, RecordingReason) = (true, RecordingReason::DegradedCoverage);
const CLOSED: (bool, RecordingReason) = (false, RecordingReason::Quiet);

#[test]
fn stationary_observed_person_keeps_renewing() {
    let mut p = healthy_quiet_at_30s();
    // Ten minutes of a person standing still, detected on every pass at 2 FPS.
    for t in (31_000..=631_000).step_by(500) {
        p.observe(success(t, 1));
        assert_eq!(state(&mut p, t), OPEN_ACTIVITY, "at {t}");
    }
    quiet(&mut p, 632_000, 650_000);
    assert_eq!(state(&mut p, 631_000 + POST_ROLL - 1), OPEN_POST_ROLL);
    assert_eq!(state(&mut p, 631_000 + POST_ROLL), CLOSED);
}

#[test]
fn explicit_alert_opens_and_holds_post_roll() {
    let mut p = healthy_quiet_at_30s();
    p.alert(30_500);
    assert_eq!(state(&mut p, 30_500), OPEN_ACTIVITY);
    quiet(&mut p, 31_000, 45_000);
    assert_eq!(state(&mut p, 30_500 + POST_ROLL - 1), OPEN_POST_ROLL);
    assert_eq!(state(&mut p, 30_500 + POST_ROLL), CLOSED);
}

#[test]
fn empty_success_refreshes_health_not_activity() {
    let mut p = healthy_quiet_at_30s();
    // Far past both the post-roll and the stale deadline: quiet, not degraded.
    quiet(&mut p, 31_000, 120_000);
    assert_eq!(state(&mut p, 120_000), CLOSED);
}

#[test]
fn skipped_refreshes_neither_health_nor_activity() {
    // Never credited toward recovery from the initial degraded state.
    let mut p = RecordingActivityPolicy::new(POST_ROLL, STALE, 0);
    for t in (0..=5_000).step_by(500) {
        p.observe(obs(t, 1, 0, AnalysisOutcome::Skipped));
    }
    assert_eq!(state(&mut p, 5_000), OPEN_DEGRADED);

    // Healthy and quiet, last success at 30 s: skipped frames (even ones
    // carrying a count) neither open recording nor keep analysis fresh.
    let mut p = healthy_quiet_at_30s();
    for t in (30_500..=41_000).step_by(500) {
        p.observe(obs(t, 1, 0, AnalysisOutcome::Skipped));
    }
    assert_eq!(state(&mut p, 30_000 + STALE), CLOSED);
    assert_eq!(state(&mut p, 30_000 + STALE + 1), OPEN_DEGRADED);
}

#[test]
fn failed_analysis_enters_degraded_coverage_immediately() {
    let mut p = healthy_quiet_at_30s();
    p.observe(obs(30_500, 0, 0, AnalysisOutcome::Failed));
    assert_eq!(state(&mut p, 30_500), OPEN_DEGRADED);
}

#[test]
fn first_analysis_hang_records_degraded_from_startup() {
    let mut p = RecordingActivityPolicy::new(POST_ROLL, STALE, 5_000);
    assert_eq!(state(&mut p, 5_000), OPEN_DEGRADED);
    // The first analysis never returns: still recording, never "quiet".
    assert_eq!(state(&mut p, 5_000 + 3_600_000), OPEN_DEGRADED);
}

#[test]
fn stale_analysis_enters_degraded_coverage() {
    let mut p = healthy_quiet_at_30s();
    assert_eq!(state(&mut p, 30_000 + STALE), CLOSED);
    assert_eq!(state(&mut p, 30_000 + STALE + 1), OPEN_DEGRADED);
    // A late success does not end it on its own: recovery still needs the gate.
    p.observe(success(40_500, 0));
    assert_eq!(state(&mut p, 40_500), OPEN_DEGRADED);
}

#[test]
fn recovery_needs_three_passes_spanning_two_seconds() {
    let mut p = healthy_quiet_at_30s();
    p.observe(obs(30_000, 0, 0, AnalysisOutcome::Failed));
    for t in [30_100, 30_200, 30_300] {
        p.observe(success(t, 0));
    }
    assert_eq!(
        state(&mut p, 30_300),
        OPEN_DEGRADED,
        "three passes in 200 ms"
    );
    p.observe(success(32_099, 0));
    assert_eq!(state(&mut p, 32_099), OPEN_DEGRADED, "span 1999 ms");
    p.observe(success(32_100, 0));
    assert_eq!(state(&mut p, 32_100), OPEN_ACTIVITY, "recovered");

    // Two passes spanning two seconds are not three.
    let mut p = healthy_quiet_at_30s();
    p.observe(obs(30_000, 0, 0, AnalysisOutcome::Failed));
    for t in [30_100, 32_100] {
        p.observe(success(t, 0));
    }
    assert_eq!(state(&mut p, 32_100), OPEN_DEGRADED);
}

#[test]
fn recovery_grants_exactly_one_post_roll_window() {
    let mut p = healthy_quiet_at_30s();
    p.observe(obs(30_000, 0, 0, AnalysisOutcome::Failed));
    for t in [31_000, 32_000, 33_000] {
        p.observe(success(t, 0));
    }
    assert_eq!(state(&mut p, 33_000), OPEN_ACTIVITY);
    quiet(&mut p, 34_000, 80_000);
    assert_eq!(state(&mut p, 33_000 + POST_ROLL - 1), OPEN_POST_ROLL);
    assert_eq!(state(&mut p, 33_000 + POST_ROLL), CLOSED);
    assert_eq!(state(&mut p, 80_000), CLOSED, "no second window");
}

#[test]
fn recovery_restarts_after_a_failure_or_a_stale_gap() {
    let mut p = healthy_quiet_at_30s();
    p.observe(obs(30_000, 0, 0, AnalysisOutcome::Failed));
    p.observe(success(31_000, 0));
    p.observe(success(32_000, 0));
    p.observe(obs(32_500, 0, 0, AnalysisOutcome::Failed));
    p.observe(success(33_000, 0));
    assert_eq!(
        state(&mut p, 33_000),
        OPEN_DEGRADED,
        "failure reset the count"
    );

    // Passes separated by more than the stale deadline do not show sustained health.
    p.observe(success(33_000 + STALE + 1, 0));
    p.observe(success(33_000 + STALE + 2_001, 0));
    assert_eq!(state(&mut p, 33_000 + STALE + 2_001), OPEN_DEGRADED);
    p.observe(success(33_000 + STALE + 3_001, 0));
    assert_eq!(state(&mut p, 33_000 + STALE + 3_001), OPEN_ACTIVITY);
}

#[test]
fn reappearance_after_closure_reopens_from_fresh_evidence() {
    let mut p = healthy_quiet_at_30s();
    p.observe(success(31_000, 1));
    // The same track coasts on predictions: closes at the evidence deadline.
    for t in (32_000..=45_000).step_by(1_000) {
        p.observe(obs(t, 0, 1, AnalysisOutcome::Success));
    }
    assert_eq!(state(&mut p, 31_000 + POST_ROLL), CLOSED);
    p.observe(success(46_000, 1));
    assert_eq!(state(&mut p, 46_000), OPEN_ACTIVITY);
}

#[test]
fn reordered_capture_times_never_move_the_activity_clock_backwards() {
    let mut p = healthy_quiet_at_30s();
    p.observe(success(40_000, 1));
    // An older pass with a person arrives late.
    p.observe(Observation {
        received_ms: 40_500,
        ..success(35_000, 1)
    });
    quiet(&mut p, 41_000, 60_000);
    assert_eq!(state(&mut p, 40_000 + POST_ROLL - 1), OPEN_POST_ROLL);
    assert_eq!(state(&mut p, 40_000 + POST_ROLL), CLOSED);

    // Late successes older than the degradation do not count toward recovery.
    p.observe(obs(60_500, 0, 0, AnalysisOutcome::Failed));
    for t in [60_100, 60_200, 60_300, 60_400] {
        p.observe(Observation {
            received_ms: 63_000,
            ..success(t, 0)
        });
    }
    assert_eq!(state(&mut p, 63_000), OPEN_DEGRADED);
}

#[test]
fn zero_post_roll_opens_only_at_the_activity_instant() {
    let mut p = RecordingActivityPolicy::new(0, STALE, 0);
    quiet(&mut p, 0, 30_000);
    assert_eq!(state(&mut p, 30_000), CLOSED);
    p.observe(success(31_000, 1));
    assert_eq!(state(&mut p, 31_000), OPEN_ACTIVITY);
    assert_eq!(state(&mut p, 31_001), CLOSED);
    p.alert(32_000);
    assert_eq!(state(&mut p, 32_000), OPEN_ACTIVITY);
    assert_eq!(state(&mut p, 32_001), CLOSED);
}

#[test]
fn elapsed_clock_jumps_fail_safe() {
    let mut p = healthy_quiet_at_30s();
    p.observe(success(31_000, 1));
    // A tick before the evidence it holds (clock stepped back): open, no underflow.
    assert_eq!(state(&mut p, 20_000), OPEN_ACTIVITY);
    // An hour passes with no analysis: degraded coverage, never "quiet".
    let later = 31_000 + 3_600_000;
    assert_eq!(state(&mut p, later), OPEN_DEGRADED);
    for t in [later, later + 1_000, later + 2_000] {
        p.observe(success(t, 0));
    }
    assert_eq!(state(&mut p, later + 2_000), OPEN_ACTIVITY);
    quiet(&mut p, later + 3_000, later + 20_000);
    assert_eq!(state(&mut p, later + 2_000 + POST_ROLL), CLOSED);

    // A capture stamp ahead of its own arrival is clamped to the arrival.
    let t = later + 20_500;
    p.observe(Observation {
        captured_ms: t + 1_000_000,
        ..success(t, 1)
    });
    quiet(&mut p, later + 21_000, later + 40_000);
    assert_eq!(state(&mut p, t + POST_ROLL - 1), OPEN_POST_ROLL);
    assert_eq!(state(&mut p, t + POST_ROLL), CLOSED);

    // Clocks near the end of u64 do not overflow.
    let mut p = RecordingActivityPolicy::new(POST_ROLL, STALE, u64::MAX - 10_000);
    for t in [u64::MAX - 5_000, u64::MAX - 3_000, u64::MAX - 1_000] {
        p.observe(success(t, 0));
    }
    assert_eq!(state(&mut p, u64::MAX), OPEN_ACTIVITY);
}

/// First and last open millisecond, ignoring startup, for a person present
/// over capture time `[20 s, 40 s)`, analysed every `interval_ms` with
/// alternating quarter-interval jitter, a 15 FPS camera whose other frames
/// are detector-skipped and a 5 s post-roll. Asserts the window is unbroken.
fn activity_window(interval_ms: f64) -> (u64, u64) {
    let mut p = RecordingActivityPolicy::new(5_000, STALE, 0);
    let mut passes = (0u64..)
        .map(|k| {
            let jitter = if k % 2 == 1 { interval_ms / 4.0 } else { 0.0 };
            (k as f64 * interval_ms + jitter).round() as u64
        })
        .peekable();
    let (mut first, mut last, mut open_ms) = (None, 0, 0);
    for now in 0..60_000u64 {
        while let Some(t) = passes.next_if(|&t| t <= now) {
            let present = (20_000..40_000).contains(&t);
            p.observe(success(t, u32::from(present)));
        }
        if now % 66 == 0 {
            p.observe(obs(now, 0, 1, AnalysisOutcome::Skipped));
        }
        if p.tick(now).open && now >= 12_000 {
            first.get_or_insert(now);
            last = now;
            open_ms += 1;
        }
    }
    let first = first.expect("recording opened");
    assert_eq!(open_ms, last - first + 1, "one unbroken window");
    (first, last)
}

#[test]
fn analysis_rate_does_not_change_the_activity_window() {
    // The true window is [20 s, 40 s + post-roll).
    for fps in [0.5_f64, 2.0, 8.0, 15.0] {
        let interval = 1_000.0 / fps;
        // Jitter stretches every other gap to 1.25 intervals.
        let tolerance = (interval * 1.25).ceil() as u64;
        let (first, last) = activity_window(interval);
        assert!(
            first.abs_diff(20_000) <= tolerance,
            "{fps} FPS opened at {first}"
        );
        assert!(
            last.abs_diff(44_999) <= tolerance,
            "{fps} FPS last open {last}"
        );
    }
}

// ---------------------------------------------------------------------------
// Real tracker output through the observation predicate.
// ---------------------------------------------------------------------------

fn person() -> Detection {
    Detection {
        label: "person".into(),
        confidence: 0.9,
        bbox: BBox {
            x1: 100.0,
            y1: 100.0,
            x2: 150.0,
            y2: 220.0,
        },
        attributes: Default::default(),
    }
}

fn observation(backend: TrackerBackendKind, t: u64, objects: &[TrackedObject]) -> Observation {
    let observed = objects.iter().filter(|o| is_observed(backend, o)).count() as u32;
    obs(
        t,
        observed,
        objects.len() as u32 - observed,
        AnalysisOutcome::Success,
    )
}

/// Drives ByteTrack at 2 FPS: a person seen for 10 s from 30 s, then gone.
/// Returns the first closed tick after the person leaves, and asserts the
/// tracker kept emitting (coasting) the track while nothing qualified.
fn bytetrack_close_time(max_lost_frames: u32) -> u64 {
    let tracker = ByteTrackTracker::new(ByteTrackConfig {
        max_lost_frames,
        ..Default::default()
    });
    let base = Instant::now();
    let mut p = healthy_quiet_at_30s();
    let mut coasting = 0;
    for t in (30_500..=80_000).step_by(500) {
        let detections = if t <= 40_000 { vec![person()] } else { vec![] };
        let out = tracker.update(detections, base + Duration::from_millis(t));
        let o = observation(TrackerBackendKind::Bytetrack, t, &out);
        if t > 40_000 && !out.is_empty() {
            assert_eq!(o.qualifying_observed, 0, "predicted-only output at {t}");
            assert_eq!(o.predicted_only, 1);
            coasting += 1;
        }
        p.observe(o);
        if t > 40_000 && !p.tick(t).open {
            assert!(coasting >= 1, "the tracker never coasted");
            return t;
        }
    }
    panic!("recording never closed");
}

#[test]
fn bytetrack_coasting_never_qualifies_and_retention_does_not_move_the_deadline() {
    let default_close = bytetrack_close_time(30);
    assert_eq!(default_close, 40_000 + POST_ROLL);
    assert_eq!(bytetrack_close_time(300), default_close);
}

#[test]
fn bytetrack_same_id_reappearing_after_closure_reopens() {
    let tracker = ByteTrackTracker::new(ByteTrackConfig {
        max_lost_frames: 300,
        ..Default::default()
    });
    let base = Instant::now();
    let at = |t: u64| base + Duration::from_millis(t);
    let mut p = healthy_quiet_at_30s();
    let seen = tracker.update(vec![person()], at(31_000));
    p.observe(observation(TrackerBackendKind::Bytetrack, 31_000, &seen));
    for t in (31_500..=45_000).step_by(500) {
        let out = tracker.update(vec![], at(t));
        assert_eq!(out.len(), 1, "the track is still retained at {t}");
        p.observe(observation(TrackerBackendKind::Bytetrack, t, &out));
    }
    assert_eq!(state(&mut p, 45_000), CLOSED);
    let back = tracker.update(vec![person()], at(45_500));
    assert_eq!(back[0].track_id, seen[0].track_id, "same tracker id");
    p.observe(observation(TrackerBackendKind::Bytetrack, 45_500, &back));
    assert_eq!(state(&mut p, 45_500), OPEN_ACTIVITY);
}

#[test]
fn iou_naive_output_is_always_an_observation() {
    let tracker = IouNaiveTracker::new(&TrackerConfig::default());
    let base = Instant::now();
    let seen = tracker.update(vec![person(), person()], base);
    assert_eq!(seen.len(), 2);
    assert!(seen
        .iter()
        .all(|o| is_observed(TrackerBackendKind::IouNaive, o)));
    // It never emits a prediction: a frame without detections emits nothing.
    let gone = tracker.update(vec![], base + Duration::from_millis(100));
    assert!(gone.is_empty());
}

// ---------------------------------------------------------------------------
// Dormant `recording_policy` camera config.
// ---------------------------------------------------------------------------

#[test]
fn recording_policy_parses_each_mode_and_defaults_to_legacy() {
    let parse = |json: &str| serde_json::from_str::<RecordingPolicyConfig>(json).unwrap();
    let default = parse("{}");
    assert_eq!(default.mode, RecordingPolicyMode::Legacy);
    assert_eq!(default.analysis_stale_after_ms, 10_000);
    assert_eq!(
        parse(r#"{"mode":"legacy"}"#).mode,
        RecordingPolicyMode::Legacy
    );
    assert_eq!(
        parse(r#"{"mode":"shadow"}"#).mode,
        RecordingPolicyMode::Shadow
    );
    let observed = parse(r#"{"mode":"observed","analysis_stale_after_ms":4000}"#);
    assert_eq!(observed.mode, RecordingPolicyMode::Observed);
    assert_eq!(observed.analysis_stale_after_ms, 4_000);
    assert_eq!(
        serde_json::to_string(&observed).unwrap(),
        r#"{"mode":"observed","analysis_stale_after_ms":4000}"#
    );
}

#[test]
fn recording_policy_rejects_invalid_values() {
    for json in [
        r#"{"analysis_stale_after_ms":0}"#,
        r#"{"analysis_stale_after_ms":60001}"#,
        r#"{"analysis_stale_after_ms":-1}"#,
        r#"{"analysis_stale_after_ms":2.5}"#,
        r#"{"analysis_stale_after_ms":"10000"}"#,
        r#"{"mode":"auto"}"#,
        r#"{"mode":"Observed"}"#,
    ] {
        assert!(
            serde_json::from_str::<RecordingPolicyConfig>(json).is_err(),
            "{json} must be rejected"
        );
    }
    for ok in [1, 60_000] {
        let json = format!(r#"{{"analysis_stale_after_ms":{ok}}}"#);
        assert!(serde_json::from_str::<RecordingPolicyConfig>(&json).is_ok());
    }
}

#[test]
fn old_camera_json_round_trips_without_a_recording_policy() {
    let old = r#"{"id":7,"name":"Lot","url":"rtsp://10.0.0.5/stream","enabled":true,"max_fps":0,"prompts":[],"visual_prompts":[],"model_override":null,"parking_lot_mode":true,"zones":[]}"#;
    let camera: CameraConfig = serde_json::from_str(old).unwrap();
    assert_eq!(serde_json::to_string(&camera).unwrap(), old);
}

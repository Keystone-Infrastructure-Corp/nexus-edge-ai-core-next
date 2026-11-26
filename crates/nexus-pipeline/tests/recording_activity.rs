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
    is_observed, AnalysisOutcome, EvidenceSummary, Observation, RecordingActivityPolicy,
    RecordingReason,
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

/// SPEC-089 Task 3: the controller sleeps until the policy's answer can
/// change on its own: the post-roll end or the stale deadline, never while
/// degraded (only evidence ends that). Each instant is the first the answer
/// differs.
#[test]
fn the_next_change_is_the_post_roll_end_or_the_stale_deadline() {
    let mut p = RecordingActivityPolicy::new(POST_ROLL, STALE, 0);
    assert_eq!(
        p.next_change_ms(0),
        None,
        "degraded until evidence recovers it"
    );
    let mut p2 = healthy_quiet_at_30s();
    // The last success was at 30 s: stale after 40 s.
    assert_eq!(p2.next_change_ms(30_000), Some(40_001));
    p2.observe(success(31_000, 1));
    assert_eq!(
        p2.next_change_ms(31_000),
        Some(41_000),
        "post-roll end first"
    );
    assert_eq!(state(&mut p2, 40_999), OPEN_ACTIVITY);
    assert_eq!(state(&mut p2, 41_000), CLOSED);
    assert_eq!(p2.next_change_ms(41_000), Some(41_001), "then staleness");
    assert_eq!(state(&mut p2, 41_001), OPEN_DEGRADED);
    p.observe(success(0, 0));
    assert_eq!(p.next_change_ms(0), None);
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
// Coalesced evidence: a slow consumer reads only the latest summary.
// ---------------------------------------------------------------------------

/// A producer for generation 1 that numbers its observations from 1.
struct Producer {
    summary: EvidenceSummary,
    sequence: u64,
}

impl Producer {
    fn new() -> Self {
        Self {
            summary: EvidenceSummary::new(1, STALE),
            sequence: 0,
        }
    }

    fn push(&mut self, o: Observation) -> &EvidenceSummary {
        self.sequence += 1;
        assert!(self.summary.reduce(1, self.sequence, o));
        &self.summary
    }

    fn quiet(&mut self, from: u64, to: u64, step: usize) {
        for t in (from..=to).step_by(step) {
            self.push(success(t, 0));
        }
    }
}

/// Healthy (recovered at 2 s) and quiet at 30 s, fed only by summaries.
fn summary_fed_healthy_at_30s() -> (Producer, RecordingActivityPolicy) {
    let mut producer = Producer::new();
    let mut p = RecordingActivityPolicy::new(POST_ROLL, STALE, 0);
    for t in (0..=30_000).step_by(1_000) {
        p.apply_summary(producer.push(success(t, 0)), t);
    }
    assert_eq!(state(&mut p, 30_000), CLOSED);
    (producer, p)
}

#[test]
fn a_stalled_consumer_recovers_exactly_once_from_coalesced_successes() {
    let mut producer = Producer::new();
    let mut p = RecordingActivityPolicy::new(POST_ROLL, STALE, 0);
    assert_eq!(state(&mut p, 0), OPEN_DEGRADED);
    // The consumer is stalled while successes at 0, 2 and 4 s coalesce.
    for t in [0, 2_000, 4_000] {
        producer.push(success(t, 0));
    }
    let read = producer.summary;
    p.apply_summary(&read, 4_000);
    assert_eq!(state(&mut p, 4_000), OPEN_ACTIVITY, "one read recovers");
    // Reading the same snapshot again grants no second window.
    p.apply_summary(&read, 4_000);
    for t in (6_000..=20_000).step_by(2_000) {
        p.apply_summary(producer.push(success(t, 0)), t);
        p.apply_summary(&read, t);
    }
    assert_eq!(state(&mut p, 4_000 + POST_ROLL - 1), OPEN_POST_ROLL);
    assert_eq!(state(&mut p, 4_000 + POST_ROLL), CLOSED);
    assert_eq!(state(&mut p, 20_000), CLOSED, "post-roll expired once");
}

#[test]
fn a_run_broken_by_a_failure_needs_a_new_gate() {
    let (mut producer, mut p) = summary_fed_healthy_at_30s();
    // Missed by the consumer: a run, a failure, then one success.
    producer.quiet(31_000, 33_000, 1_000);
    producer.push(obs(33_500, 0, 0, AnalysisOutcome::Failed));
    p.apply_summary(producer.push(success(34_000, 0)), 34_000);
    assert_eq!(state(&mut p, 34_000), OPEN_DEGRADED, "the failure was seen");
    p.apply_summary(producer.push(success(35_000, 0)), 35_000);
    assert_eq!(state(&mut p, 35_000), OPEN_DEGRADED, "two passes in 1 s");
    p.apply_summary(producer.push(success(36_000, 0)), 36_000);
    assert_eq!(
        state(&mut p, 36_000),
        OPEN_ACTIVITY,
        "three passes over 2 s"
    );
}

#[test]
fn a_failure_and_its_recovery_survive_missed_publications() {
    let (mut producer, mut p) = summary_fed_healthy_at_30s();
    producer.push(obs(30_500, 0, 0, AnalysisOutcome::Failed));
    producer.quiet(31_000, 32_000, 1_000);
    // One read sees the failure and the run that recovered from it.
    p.apply_summary(producer.push(success(33_000, 0)), 33_000);
    // Quiet before the failure; open now only because recovery is activity.
    assert_eq!(state(&mut p, 33_000), OPEN_ACTIVITY);
    for t in (34_000..=50_000).step_by(1_000) {
        p.apply_summary(producer.push(success(t, 0)), t);
    }
    assert_eq!(state(&mut p, 33_000 + POST_ROLL - 1), OPEN_POST_ROLL);
    assert_eq!(state(&mut p, 33_000 + POST_ROLL), CLOSED);
}

#[test]
fn a_stale_gap_resets_the_success_run() {
    let mut producer = Producer::new();
    let mut p = RecordingActivityPolicy::new(POST_ROLL, STALE, 0);
    producer.push(success(0, 0));
    producer.push(success(1_000, 0));
    // Two passes, a gap past the stale deadline, then two more spanning 2 s.
    let resumed = 1_000 + STALE + 1;
    producer.push(success(resumed, 0));
    p.apply_summary(producer.push(success(resumed + 2_000, 0)), resumed + 2_000);
    let run = producer.summary.run.expect("a run");
    assert_eq!((run.first_ms, run.passes), (resumed, 2));
    assert_eq!(state(&mut p, resumed + 2_000), OPEN_DEGRADED);
    p.apply_summary(producer.push(success(resumed + 2_500, 0)), resumed + 2_500);
    assert_eq!(state(&mut p, resumed + 2_500), OPEN_ACTIVITY);
}

#[test]
fn a_recovered_run_read_after_it_went_stale_does_not_recover() {
    let mut producer = Producer::new();
    let mut p = RecordingActivityPolicy::new(POST_ROLL, STALE, 0);
    producer.quiet(0, 2_000, 1_000);
    // Read long after the producer stopped: freshness is rechecked now.
    let now = 2_000 + STALE + 1;
    p.apply_summary(&producer.summary, now);
    assert_eq!(state(&mut p, now), OPEN_DEGRADED);
}

#[test]
fn skipped_analyses_neither_count_toward_nor_refresh_a_run() {
    let mut producer = Producer::new();
    let mut p = RecordingActivityPolicy::new(POST_ROLL, STALE, 0);
    producer.push(success(0, 0));
    for t in (100..=5_000).step_by(100) {
        producer.push(obs(t, 1, 0, AnalysisOutcome::Skipped));
    }
    let s = producer.summary;
    assert_eq!(s.through_sequence, 51);
    assert_eq!(s.last_success_ms, Some(0));
    assert_eq!(s.last_activity_ms, None, "a skipped count is not activity");
    assert_eq!(s.run.map(|r| r.passes), Some(1));
    p.apply_summary(&s, 5_000);
    assert_eq!(state(&mut p, 5_000), OPEN_DEGRADED);
}

#[test]
fn duplicate_and_older_summary_reads_change_nothing() {
    let (mut producer, mut p) = summary_fed_healthy_at_30s();
    let older = *producer.push(success(31_000, 1));
    let newer = *producer.push(obs(31_500, 0, 0, AnalysisOutcome::Failed));
    p.apply_summary(&newer, 31_500);
    p.apply_summary(&older, 31_500);
    p.apply_summary(&newer, 31_500);
    assert_eq!(state(&mut p, 31_500), OPEN_DEGRADED);
    // Recover; a re-read of the failure summary does not degrade again.
    producer.quiet(32_000, 34_000, 1_000);
    p.apply_summary(&producer.summary, 34_000);
    assert_eq!(state(&mut p, 34_000), OPEN_ACTIVITY);
    p.apply_summary(&newer, 34_000);
    assert_eq!(state(&mut p, 34_000), OPEN_ACTIVITY);
}

#[test]
fn old_generations_and_sequences_are_rejected_before_reduction() {
    let mut s = EvidenceSummary::new(2, STALE);
    assert!(s.reduce(2, 5, success(1_000, 1)));
    let before = s;
    assert!(!s.reduce(1, 6, success(2_000, 1)), "an older generation");
    assert!(!s.reduce(2, 5, success(2_000, 1)), "a repeated sequence");
    assert!(!s.reduce(2, 4, obs(2_000, 0, 0, AnalysisOutcome::Failed)));
    assert_eq!(s, before);
    // A newer generation starts over: the old activity is not its evidence.
    assert!(s.reduce(3, 1, success(500, 0)));
    assert_eq!((s.generation, s.last_activity_ms), (3, None));

    // A consumer never applies an older generation after a newer one.
    let mut p = RecordingActivityPolicy::new(POST_ROLL, STALE, 0);
    let mut newer = EvidenceSummary::new(3, STALE);
    for (seq, t) in [(1, 0), (2, 1_000), (3, 2_000)] {
        newer.reduce(3, seq, success(t, 0));
    }
    p.apply_summary(&newer, 2_000);
    let mut stale_generation = EvidenceSummary::new(2, STALE);
    stale_generation.reduce(2, 9, obs(2_100, 0, 0, AnalysisOutcome::Failed));
    p.apply_summary(&stale_generation, 2_100);
    assert_eq!(state(&mut p, 2_100), OPEN_ACTIVITY);
}

#[test]
fn the_latest_qualifying_time_survives_a_newer_empty_frame() {
    let (mut producer, mut p) = summary_fed_healthy_at_30s();
    producer.push(success(31_000, 1));
    let read = *producer.push(success(31_500, 0));
    assert_eq!(read.last_activity_ms, Some(31_000));
    p.apply_summary(&read, 31_500);
    assert_eq!(state(&mut p, 31_500), OPEN_POST_ROLL);
    assert_eq!(state(&mut p, 31_000 + POST_ROLL - 1), OPEN_POST_ROLL);
}

#[test]
fn a_high_rate_producer_stays_bounded_for_a_slow_consumer() {
    // Fixed size, no heap: the summary cannot grow with the backlog.
    assert!(std::mem::size_of::<EvidenceSummary>() <= 128);
    let (mut producer, mut p) = summary_fed_healthy_at_30s();
    // An hour at 15 FPS, a person in one frame of every 4 s, read once a minute.
    let mut t = 30_000;
    let mut last_person = 0;
    for k in 1..=54_000u64 {
        t = 30_000 + k * 1_000 / 15;
        let person = u32::from(k % 60 == 30);
        if person == 1 {
            last_person = t;
        }
        producer.push(success(t, person));
        if k % 900 == 0 {
            p.apply_summary(&producer.summary, t);
        }
    }
    assert_eq!(producer.summary.through_sequence, 31 + 54_000);
    assert_eq!(producer.summary.last_activity_ms, Some(last_person));
    assert_eq!(state(&mut p, t), OPEN_POST_ROLL);
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

// ---------------------------------------------------------------------------
// The real supervisor in shadow: legacy keeps every recorder call.
// ---------------------------------------------------------------------------

mod supervisor_shadow {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use chrono::{DateTime, TimeZone, Utc};
    use futures::StreamExt;
    use nexus_bus::{topic, BroadcastBus, Bus, BusExt};
    use nexus_config::{
        CameraBehavior, CameraConfig, ClipsConfig, RecordingPolicyConfig, RecordingPolicyMode,
        RulesConfig, StoreConfig, TrackerBackendKind, TrackerConfig,
    };
    use nexus_inference::{Detector, InferenceError};
    use nexus_pipeline::cache::LatestFrameCache;
    use nexus_pipeline::recording_diagnostics::RecordingActivityRuntime;
    use nexus_pipeline::supervisor::{spawn_camera, SightingSchedulerConfig};
    use nexus_pipeline::{
        ClipFinal, ClipHandle, ClipMeta, ClipRecorder, FrameSource, FrameSourceError,
        FrameStatsRegistry, OpenClip, RecorderError, StubClipRecorder,
    };
    use nexus_rules::RuleEvaluator;
    use nexus_store::{RecordingActivityEpisode, RecordingActivityMinute, Store};
    use nexus_tracker::build_tracker;
    use nexus_types::{BBox, CameraId, Detection, Frame, FrameMetadata, PixelFormat};
    use parking_lot::Mutex;
    use tokio::sync::mpsc;

    const W: u32 = 512;
    const H: u32 = 288;
    const GAP_MS: u64 = 500;

    fn wall(i: usize) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(1_700_000_000_000 + (GAP_MS * i as u64) as i64)
            .unwrap()
    }

    /// Sends its frames one per `go`, then stays open.
    struct Scripted {
        frames: Vec<Frame>,
        go: mpsc::UnboundedReceiver<()>,
    }

    #[async_trait]
    impl FrameSource for Scripted {
        async fn run(mut self: Box<Self>, tx: mpsc::Sender<Frame>) -> Result<(), FrameSourceError> {
            for f in std::mem::take(&mut self.frames) {
                if self.go.recv().await.is_none() || tx.send(f).await.is_err() {
                    return Ok(());
                }
            }
            std::future::pending().await
        }
    }

    /// The stub recorder, logging every call the supervisor makes of it.
    struct Logged {
        stub: StubClipRecorder,
        source: Mutex<Option<Box<dyn FrameSource + Send>>>,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ClipRecorder for Logged {
        async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
            let at = args.started_at;
            let opened = self.stub.open(args).await;
            let id = opened
                .as_ref()
                .map(|h| h.clip_id)
                .map_err(|e| e.to_string());
            self.calls.lock().push(format!("open {at} -> {id:?}"));
            opened
        }

        async fn close(
            &self,
            handle: ClipHandle,
            args: ClipFinal,
        ) -> Result<ClipMeta, RecorderError> {
            let at = args.ended_at;
            let closed = self.stub.close(handle, args).await;
            let discarded = closed
                .as_ref()
                .map(|m| m.discarded)
                .map_err(|e| e.to_string());
            self.calls
                .lock()
                .push(format!("close {} at {at} -> {discarded:?}", handle.clip_id));
            closed
        }

        /// Over the scenario's 1-byte cap: every size check rotates.
        async fn inflight_size_bytes(&self, handle: ClipHandle) -> Option<u64> {
            self.calls.lock().push(format!("size {}", handle.clip_id));
            Some(10)
        }

        fn set_panic(&self, panic: bool) {
            self.stub.set_panic(panic);
        }

        fn is_panic(&self) -> bool {
            self.stub.is_panic()
        }

        fn kind(&self) -> &'static str {
            "logged"
        }

        fn shared_frame_source(&self, _id: CameraId) -> Option<Box<dyn FrameSource + Send>> {
            self.source.lock().take()
        }
    }

    /// `script[i]`: people on frame `i`, or `None` for a detector error.
    struct People {
        script: Vec<Option<usize>>,
        seen: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl Detector for People {
        async fn detect(&self, f: &Frame, _p: &[String]) -> Result<Vec<Detection>, InferenceError> {
            let i = f.frame_id as usize - 1;
            self.seen.lock().push(i);
            let n = self.script[i].ok_or_else(|| InferenceError::Failed("scripted".into()))?;
            Ok((0..n)
                .map(|k| {
                    let x = 8.0 + 24.0 * k as f32;
                    Detection {
                        label: "person".into(),
                        confidence: 0.9,
                        bbox: BBox {
                            x1: x,
                            y1: 40.0,
                            x2: x + 20.0,
                            y2: 120.0,
                        },
                        attributes: Default::default(),
                    }
                })
                .collect())
        }

        fn name(&self) -> &'static str {
            "people"
        }
    }

    struct Outcome {
        calls: Vec<String>,
        clips: Vec<serde_json::Value>,
        motion: Vec<serde_json::Value>,
        /// The runtime snapshot once the last frame was tracked.
        runtime: Option<RecordingActivityRuntime>,
        minutes: Vec<RecordingActivityMinute>,
        episodes: Vec<RecordingActivityEpisode>,
    }

    /// A person for 33 s (the byte cap rotates the clip once), a detector
    /// error, then ByteTrack coasting the lost track until legacy closes.
    fn scenario() -> Vec<Option<usize>> {
        let mut script = vec![Some(0); 4];
        script.extend([Some(1); 66]);
        script.push(None);
        // 120 frames: one minute of capture time.
        script.extend([Some(0); 49]);
        script
    }

    fn policy(mode: RecordingPolicyMode) -> Option<RecordingPolicyConfig> {
        Some(RecordingPolicyConfig {
            mode,
            analysis_stale_after_ms: 10_000,
        })
    }

    /// Run `script` through the real supervisor; close the store's pool
    /// before frame `close_store_at` to make the collector fail.
    async fn run(
        recording_policy: Option<RecordingPolicyConfig>,
        script: Vec<Option<usize>>,
        close_store_at: Option<usize>,
    ) -> Outcome {
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(256));
        let mut metadata = bus
            .subscribe::<FrameMetadata>(topic::FRAME_METADATA)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Store::open(&StoreConfig {
                url: format!("sqlite:{}?mode=rwc", dir.path().join("nexus.db").display()),
                seed_from_config: false,
                duckdb_attach: false,
                duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
            })
            .await
            .unwrap(),
        );
        let cam = CameraConfig {
            id: 1,
            name: "shadow".into(),
            ingest: nexus_config::CameraIngest {
                url: url::Url::parse("virtual://local").unwrap(),
                analysis_url: None,
                enabled: true,
                max_fps: 20,
                codec: None,
            },
            detector: nexus_config::CameraDetector {
                prompts: vec!["person".into()],
                visual_prompts: vec![],
                model_override: None,
            },
            behavior: CameraBehavior {
                recording_policy,
                recording_policy_revision: "rev-saved".into(),
                ..Default::default()
            },
            onvif: Default::default(),
            talk_down: Default::default(),
            zones: vec![],
        };
        store.upsert_camera(&cam).await.unwrap();

        let mono0 = Instant::now();
        let pixels = Arc::new(vec![0u8; (W * H * 3) as usize]);
        let frames = (0..script.len())
            .map(|i| Frame {
                camera_id: 1,
                frame_id: i as u64 + 1,
                captured_at: wall(i),
                captured_mono: mono0 + Duration::from_millis(GAP_MS * i as u64),
                width: W,
                height: H,
                format: PixelFormat::Rgb24,
                data: pixels.clone(),
                trace_id: format!("frame-{i}"),
            })
            .collect();
        let (go, go_rx) = mpsc::unbounded_channel();
        let recorder = Arc::new(Logged {
            stub: StubClipRecorder::new(store.clone(), dir.path().join("clips")),
            source: Mutex::new(Some(Box::new(Scripted { frames, go: go_rx }))),
            calls: Mutex::new(Vec::new()),
        });
        let detector = Arc::new(People {
            script: script.clone(),
            seen: Mutex::new(Vec::new()),
        });
        let tracker_cfg = TrackerConfig {
            backend: TrackerBackendKind::Bytetrack,
            ..Default::default()
        };
        let stats = Arc::new(FrameStatsRegistry::new());
        let handle = spawn_camera(
            cam,
            detector.clone(),
            None,
            Arc::from(build_tracker(&tracker_cfg)),
            tracker_cfg.annotator.clone(),
            tracker_cfg.static_object.clone(),
            ClipsConfig {
                post_roll_secs: 2,
                max_clip_bytes: 1,
                ..Default::default()
            },
            dir.path().to_path_buf(),
            Arc::new(RuleEvaluator::new(&RulesConfig::default(), &[]).unwrap()),
            store.clone(),
            recorder.clone(),
            bus.clone(),
            Arc::new(LatestFrameCache::new()),
            stats.clone(),
            nexus_pipeline::StaticAnchorClearRegistry::new(),
            W,
            H,
            Arc::new(nexus_pipeline::NoopSightingHook),
            SightingSchedulerConfig::default(),
            Vec::new(),
            Arc::new(nexus_pipeline::NoopEntityLocalPersist),
            None,
            Arc::new(nexus_pipeline::NoopSinkRouter),
            Arc::new(nexus_pipeline::NoopAlertClipScheduleGate),
        );

        for (i, people) in script.iter().enumerate() {
            if close_store_at == Some(i) {
                store.pool().close().await;
            }
            go.send(()).unwrap();
            // A failed frame publishes no metadata: wait for its detect call.
            tokio::time::timeout(Duration::from_secs(10), async {
                if people.is_some() {
                    loop {
                        let m = metadata.next().await.unwrap().unwrap();
                        if m.frame_id == i as u64 + 1 {
                            break;
                        }
                    }
                } else {
                    while !detector.seen.lock().contains(&i) {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("frame {i} analysed within 10 s"));
        }
        let runtime = stats.recording_activity().snapshot(1, Instant::now());
        handle.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), handle.task)
            .await
            .expect("supervisor stops")
            .expect("supervisor did not panic");
        assert!(
            stats
                .recording_activity()
                .snapshot(1, Instant::now())
                .is_none(),
            "an ended supervisor reports no runtime"
        );

        let calls = recorder.calls.lock().clone();
        let mut clips = Vec::new();
        for id in 1..=calls.iter().filter(|c| c.starts_with("open")).count() as i64 {
            if let Ok(clip) = store.get_clip(id).await {
                clips.push(serde_json::to_value(clip).unwrap());
            }
        }
        let day = chrono::Duration::days(1);
        let motion = store
            .list_motion_events_for_camera(1, wall(0) - day, wall(0) + day, 10_000)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| serde_json::to_value(row).unwrap())
            .collect();
        Outcome {
            calls,
            clips,
            motion,
            runtime,
            minutes: store
                .list_recording_activity_minutes(1)
                .await
                .unwrap_or_default(),
            episodes: store
                .list_recording_activity_episodes(1)
                .await
                .unwrap_or_default(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shadow_leaves_every_recorder_call_and_file_row_unchanged() {
        let legacy = run(None, scenario(), None).await;
        let shadow = run(policy(RecordingPolicyMode::Shadow), scenario(), None).await;

        // The scenario exercises an open, a byte-cap rotation and a post-roll close.
        let opens = legacy
            .calls
            .iter()
            .filter(|c| c.starts_with("open"))
            .count();
        let closes = legacy
            .calls
            .iter()
            .filter(|c| c.starts_with("close"))
            .count();
        assert_eq!((opens, closes), (2, 2), "{:#?}", legacy.calls);
        assert!(legacy.calls.iter().any(|c| c.starts_with("size")));
        assert_eq!(shadow.calls, legacy.calls, "identical recorder calls");
        assert_eq!(shadow.clips.len(), 2);
        assert_eq!(shadow.clips, legacy.clips, "identical clip rows");
        assert!(!legacy.motion.is_empty());
        assert_eq!(shadow.motion, legacy.motion, "identical motion rows");

        // Diagnostics differ: only shadow measures and persists anything.
        assert!(legacy.runtime.is_none());
        assert!(legacy.minutes.is_empty() && legacy.episodes.is_empty());
        let runtime = shadow.runtime.expect("shadow reports its runtime");
        assert_eq!(runtime.mode, RecordingPolicyMode::Shadow);
        assert_eq!(
            runtime.policy_revision, "rev-saved",
            "the runtime names the saved revision it runs"
        );
        assert!(runtime.diagnostics.last_failure_age_ms.is_some());
        assert!(!runtime.diagnostics.trace_incomplete);
        assert_eq!(runtime.diagnostics.collector_error, None);
        assert!(
            runtime.diagnostics.elapsed.legacy_open.quiet > 0,
            "legacy kept recording on ByteTrack's coasting track; shadow did not"
        );

        let clip_ids: Vec<i64> = shadow.episodes.iter().map(|e| e.clip_id).collect();
        assert_eq!(clip_ids, [1, 2], "one episode per finalized clip");
        let rotated = &shadow.episodes[0];
        assert_eq!(
            (rotated.open_reason, rotated.close_reason),
            ("activity", "activity")
        );
        let last = &shadow.episodes[1];
        assert_eq!(last.close_reason, "quiet");
        let elapsed: serde_json::Value = serde_json::from_str(&last.elapsed_json).unwrap();
        assert!(elapsed["quiet"].as_u64().unwrap() > 0, "{elapsed}");
        assert!(
            elapsed["degraded_coverage"].as_u64().unwrap() > 0,
            "{elapsed}"
        );
        assert!(
            shadow
                .episodes
                .iter()
                .all(|e| e.generation == runtime.generation as i64
                    && e.policy_revision == "rev-saved")
        );

        assert_eq!(
            shadow.minutes.len(),
            1,
            "one minute of frames, flushed at stop"
        );
        let counters: serde_json::Value =
            serde_json::from_str(&shadow.minutes[0].counters_json).unwrap();
        assert_eq!(counters["failed"], 1);
        assert_eq!(counters["succeeded"], scenario().len() - 1);
        assert_eq!(counters["trace_incomplete"], false);
    }

    /// `observed` is not implemented yet: it measures like shadow and
    /// legacy still writes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observed_runs_as_shadow_until_it_is_implemented() {
        let legacy = run(None, scenario(), None).await;
        let observed = run(policy(RecordingPolicyMode::Observed), scenario(), None).await;
        assert_eq!(observed.calls, legacy.calls);
        assert_eq!(
            observed.runtime.map(|r| r.mode),
            Some(RecordingPolicyMode::Shadow)
        );
    }

    /// A summary the store refuses is reported on the runtime snapshot as an
    /// incomplete trace, and recording carries on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_collector_failure_is_reported_and_recording_continues() {
        let script = scenario();
        let o = run(policy(RecordingPolicyMode::Shadow), script, Some(80)).await;
        let runtime = o.runtime.expect("shadow reports its runtime");
        assert!(runtime.diagnostics.trace_incomplete);
        let error = runtime.diagnostics.collector_error.expect("reported");
        assert!(error.contains("not saved"), "{error}");
        assert!(
            o.calls.iter().filter(|c| c.starts_with("close")).count() >= 2,
            "legacy still closed its clip: {:#?}",
            o.calls
        );
    }
}

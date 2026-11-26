//! SPEC-089: whether a camera should be recording, decided from fresh
//! analysis evidence and explicit alerts, never from tracker memory.
//!
//! Pure: no I/O, no async, no clock. Every time is elapsed monotonic
//! milliseconds from the caller (capture stamps for evidence, the
//! controller's clock for [`RecordingActivityPolicy::tick`]); nothing here
//! reads wall or UTC time. SPEC-089 Task 2 runs it in shadow beside legacy
//! recording ([`crate::recording_diagnostics`]); nothing here writes footage.

use nexus_config::TrackerBackendKind;
use nexus_types::TrackedObject;

/// What one analysis pass over one frame produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalysisOutcome {
    /// The detector ran. An empty result is a real "nothing seen".
    Success,
    /// Deliberately not analysed (detector skip): neither evidence nor failure.
    Skipped,
    /// The detector errored: analysis is unhealthy from this frame.
    Failed,
}

#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub captured_ms: u64,
    pub received_ms: u64,
    /// Objects [`is_observed`] on this frame, after exclusion and static suppression.
    pub qualifying_observed: u32,
    /// Objects the tracker only predicted on this frame: diagnostics, never activity.
    pub predicted_only: u32,
    pub outcome: AnalysisOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingReason {
    Activity,
    PostRoll,
    Quiet,
    DegradedCoverage,
    /// Activity has ended but the writer holds a new clip until it has the
    /// recorder's minimum media duration. Set by the controller, never by the policy.
    MinimumDuration,
}

#[derive(Clone, Copy, Debug)]
pub struct RecordingIntent {
    pub open: bool,
    pub reason: RecordingReason,
}

/// Successful passes, spanning at least [`RECOVERY_SPAN_MS`], that end degraded coverage.
const RECOVERY_PASSES: u32 = 3;
const RECOVERY_SPAN_MS: u64 = 2_000;

/// Whether `object` is a fresh observation on its frame. Exhaustive so a new
/// tracker backend must state its observed/predicted contract here.
pub fn is_observed(backend: TrackerBackendKind, object: &TrackedObject) -> bool {
    match backend {
        // Sets `detection_bbox` only when the track matched a detection this frame.
        TrackerBackendKind::Bytetrack => object.detection_bbox.is_some(),
        // Emits only this frame's detections, each with `detection_bbox` set.
        TrackerBackendKind::IouNaive => object.detection_bbox.is_some(),
    }
}

/// Per-camera recording decision. Starts in degraded coverage (open) until
/// analysis proves healthy; then open from qualifying evidence or an alert
/// until `last activity + post_roll` (end exclusive).
pub struct RecordingActivityPolicy {
    post_roll_ms: u64,
    stale_after_ms: u64,
    last_success_ms: Option<u64>,
    last_activity_ms: Option<u64>,
    degraded: bool,
    degraded_since_ms: u64,
    recovery_passes: u32,
    recovery_started_ms: Option<u64>,
    /// Generation and through-sequence of the last [`EvidenceSummary`] applied.
    applied: Option<(u64, u64)>,
}

impl RecordingActivityPolicy {
    pub fn new(post_roll_ms: u64, stale_after_ms: u64, started_ms: u64) -> Self {
        Self {
            post_roll_ms,
            stale_after_ms,
            last_success_ms: None,
            last_activity_ms: None,
            degraded: true,
            degraded_since_ms: started_ms,
            recovery_passes: 0,
            recovery_started_ms: None,
            applied: None,
        }
    }

    /// Apply a coalesced [`EvidenceSummary`]. Idempotent: a summary at or
    /// below the generation/through-sequence already applied changes
    /// nothing, so a repeated or older read never counts a pass twice.
    /// `now_ms` is the consumer's clock: a success run that is no longer
    /// fresh proves nothing about analysis now and cannot end degradation.
    pub fn apply_summary(&mut self, summary: &EvidenceSummary, now_ms: u64) {
        let mark = (summary.generation, summary.through_sequence);
        let applied_through = match self.applied {
            Some(applied) if mark <= applied => return,
            Some((generation, through)) if generation == summary.generation => through,
            // A new generation: every failure in its summary is new.
            _ => 0,
        };
        self.applied = Some(mark);
        if let Some(at) = summary.last_activity_ms {
            self.activity(at);
        }
        if let Some((at, _)) = summary
            .last_failure
            .filter(|&(_, sequence)| sequence > applied_through)
        {
            self.degrade(at);
        }
        // A run reduced under another stale deadline (a live change the
        // producer had not yet applied) proves nothing under this one.
        let run = summary
            .run
            .filter(|_| summary.stale_after_ms == self.stale_after_ms);
        // A run that began after the last success seen here was broken
        // since: by a failure, or by a stale gap no tick saw.
        if let (Some(run), Some(seen)) = (run, self.last_success_ms) {
            if run.first_ms > seen {
                self.degrade(seen.saturating_add(self.stale_after_ms));
            }
        }
        self.last_success_ms = self.last_success_ms.max(summary.last_success_ms);
        let fresh_run = run.filter(|run| now_ms.saturating_sub(run.last_ms) <= self.stale_after_ms);
        // The producer resets a run on every failure and stale gap, so a
        // fresh run that met the gate is sustained health after them.
        if let Some(at) = fresh_run.and_then(|run| run.recovered_ms) {
            if self.degraded {
                self.recover(at);
            }
        }
    }

    pub fn observe(&mut self, observation: Observation) {
        // Evidence cannot postdate its own arrival.
        let at = observation.captured_ms.min(observation.received_ms);
        match observation.outcome {
            AnalysisOutcome::Skipped => {}
            AnalysisOutcome::Failed => self.degrade(at),
            AnalysisOutcome::Success => {
                if observation.qualifying_observed > 0 {
                    self.activity(at);
                }
                // A reordered pass proves nothing newer about health.
                if self.last_success_ms.is_some_and(|s| at <= s) {
                    return;
                }
                // A gap past the stale deadline no tick saw: health was not sustained.
                if let Some(s) = self
                    .last_success_ms
                    .filter(|&s| at - s > self.stale_after_ms)
                {
                    self.degrade(s + self.stale_after_ms);
                }
                if self.degraded {
                    self.count_recovery(at);
                }
                self.last_success_ms = Some(at);
            }
        }
    }

    /// Explicit alert recording: activity independent of the detector's evidence.
    pub fn alert(&mut self, captured_ms: u64) {
        self.activity(captured_ms);
    }

    /// Degraded coverage once the newest successful analysis is older than
    /// the stale deadline at `now_ms`, the consumer's clock: evidence used
    /// late is as old as it is when used, whenever its frame was captured.
    pub fn expire(&mut self, now_ms: u64) {
        let stale = self
            .last_success_ms
            .is_none_or(|s| now_ms.saturating_sub(s) > self.stale_after_ms);
        if !self.degraded && stale {
            self.degrade(now_ms);
        }
    }

    pub fn tick(&mut self, now_ms: u64) -> RecordingIntent {
        self.expire(now_ms);
        let reason = match self.last_activity_ms {
            _ if self.degraded => RecordingReason::DegradedCoverage,
            // The activity instant itself is open even with zero post-roll.
            Some(a) if now_ms <= a || now_ms - a < self.post_roll_ms => {
                // Activity while the newest analysed frame carried it.
                if self.last_success_ms.is_none_or(|s| a >= s) {
                    RecordingReason::Activity
                } else {
                    RecordingReason::PostRoll
                }
            }
            _ => RecordingReason::Quiet,
        };
        RecordingIntent {
            open: reason != RecordingReason::Quiet,
            reason,
        }
    }

    /// The first instant after `now_ms` at which [`Self::tick`] can answer
    /// differently with no new evidence: activity running out or analysis
    /// going stale. `None` while degraded, which only evidence ends.
    pub fn next_change_ms(&self, now_ms: u64) -> Option<u64> {
        if self.degraded {
            return None;
        }
        let quiet = self.last_activity_ms.map(|a| a + self.post_roll_ms.max(1));
        let stale = self.last_success_ms.map(|s| s + self.stale_after_ms + 1);
        [quiet, stale]
            .into_iter()
            .flatten()
            .filter(|&t| t > now_ms)
            .min()
    }

    fn activity(&mut self, at: u64) {
        self.last_activity_ms = self.last_activity_ms.max(Some(at));
    }

    fn degrade(&mut self, at: u64) {
        if !self.degraded {
            self.degraded = true;
            self.degraded_since_ms = at;
        }
        self.recovery_passes = 0;
        self.recovery_started_ms = None;
    }

    fn count_recovery(&mut self, at: u64) {
        // A pass captured before the degradation is not evidence of recovery.
        if at < self.degraded_since_ms {
            return;
        }
        // A gap past the stale deadline restarts the count: health was not sustained.
        let gap = self
            .last_success_ms
            .is_some_and(|s| at - s > self.stale_after_ms);
        let started = match self.recovery_started_ms {
            Some(started) if !gap => started,
            _ => {
                self.recovery_passes = 0;
                at
            }
        };
        self.recovery_started_ms = Some(started);
        self.recovery_passes += 1;
        if self.recovery_passes >= RECOVERY_PASSES && at - started >= RECOVERY_SPAN_MS {
            self.recover(at);
        }
    }

    fn recover(&mut self, at: u64) {
        self.degraded = false;
        self.recovery_passes = 0;
        self.recovery_started_ms = None;
        // The recovery instant counts as activity once: one post-roll window.
        self.activity(at);
    }
}

/// The current run of consecutive successful passes, as far as recovery
/// needs it: bounded however many passes it spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SuccessRun {
    pub first_ms: u64,
    pub last_ms: u64,
    /// Saturates at the recovery gate's pass count.
    pub passes: u32,
    /// The pass at which this run first met the recovery gate.
    pub recovered_ms: Option<u64>,
}

/// SPEC-089 coalesced evidence: every generation/sequence-validated
/// [`Observation`] of one camera generation reduced into a fixed-size
/// summary, so a consumer that reads only the latest one still sees the
/// newest qualifying activity, a failure and the recovery after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvidenceSummary {
    pub generation: u64,
    /// Sequence of the last observation reduced; `0` before the first.
    pub through_sequence: u64,
    /// Latest capture time with a qualifying observation.
    pub last_activity_ms: Option<u64>,
    pub last_success_ms: Option<u64>,
    /// Capture time and sequence of the latest failed analysis.
    pub last_failure: Option<(u64, u64)>,
    /// Reset by a failure and by a gap past the stale deadline.
    pub run: Option<SuccessRun>,
    stale_after_ms: u64,
}

impl EvidenceSummary {
    pub fn new(generation: u64, stale_after_ms: u64) -> Self {
        Self {
            generation,
            through_sequence: 0,
            last_activity_ms: None,
            last_success_ms: None,
            last_failure: None,
            run: None,
            stale_after_ms,
        }
    }

    /// Reduce from now on under `stale_after_ms`. A changed deadline drops
    /// the success run, gathered under the old one, so recovery takes the
    /// full gate again.
    pub fn retune(&mut self, stale_after_ms: u64) {
        if stale_after_ms != self.stale_after_ms {
            self.stale_after_ms = stale_after_ms;
            self.run = None;
        }
    }

    /// Reduce one observation numbered `sequence` (from 1) in `generation`.
    /// Returns `false`, changing nothing, for an older generation or a
    /// sequence already reduced; a newer generation starts a fresh summary.
    pub fn reduce(&mut self, generation: u64, sequence: u64, observation: Observation) -> bool {
        if generation < self.generation
            || (generation == self.generation && sequence <= self.through_sequence)
        {
            return false;
        }
        if generation > self.generation {
            *self = Self::new(generation, self.stale_after_ms);
        }
        self.through_sequence = sequence;
        let at = observation.captured_ms.min(observation.received_ms);
        match observation.outcome {
            AnalysisOutcome::Skipped => {}
            AnalysisOutcome::Failed => {
                self.last_failure = Some((at, sequence));
                self.run = None;
            }
            AnalysisOutcome::Success => {
                if observation.qualifying_observed > 0 {
                    self.last_activity_ms = self.last_activity_ms.max(Some(at));
                }
                // A reordered pass, or one captured before the latest
                // failure, proves nothing newer about health.
                if self.last_success_ms.is_some_and(|s| at <= s)
                    || self.last_failure.is_some_and(|(f, _)| at < f)
                {
                    return true;
                }
                self.last_success_ms = Some(at);
                let mut run = match self.run {
                    Some(run) if at - run.last_ms <= self.stale_after_ms => run,
                    _ => SuccessRun {
                        first_ms: at,
                        last_ms: at,
                        passes: 0,
                        recovered_ms: None,
                    },
                };
                run.passes = (run.passes + 1).min(RECOVERY_PASSES);
                run.last_ms = at;
                if run.recovered_ms.is_none()
                    && run.passes >= RECOVERY_PASSES
                    && at - run.first_ms >= RECOVERY_SPAN_MS
                {
                    run.recovered_ms = Some(at);
                }
                self.run = Some(run);
            }
        }
        true
    }
}

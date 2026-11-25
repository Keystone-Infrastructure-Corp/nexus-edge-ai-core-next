//! SPEC-089: whether a camera should be recording, decided from fresh
//! analysis evidence and explicit alerts, never from tracker memory.
//!
//! Pure: no I/O, no async, no clock. Every time is elapsed monotonic
//! milliseconds from the caller (capture stamps for evidence, the
//! controller's clock for [`RecordingActivityPolicy::tick`]); nothing here
//! reads wall or UTC time. Dormant until a controller wires it in.

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingReason {
    Activity,
    PostRoll,
    Quiet,
    DegradedCoverage,
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

    pub fn tick(&mut self, now_ms: u64) -> RecordingIntent {
        let stale = self
            .last_success_ms
            .is_none_or(|s| now_ms.saturating_sub(s) > self.stale_after_ms);
        if !self.degraded && stale {
            self.degrade(now_ms);
        }
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
            self.degraded = false;
            self.recovery_passes = 0;
            self.recovery_started_ms = None;
            // The recovery instant counts as activity once: one post-roll window.
            self.activity(at);
        }
    }
}

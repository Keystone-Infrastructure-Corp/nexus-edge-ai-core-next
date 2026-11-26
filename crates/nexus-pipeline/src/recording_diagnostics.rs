//! SPEC-089: the fresh-evidence recording policy and its bounded
//! diagnostics. Nothing here opens, closes or rotates a file: the camera's
//! [`crate::recording_controller`] does, from legacy's decisions in shadow
//! mode (the policy is only measured) or from the policy's in observed mode.
//!
//! Memory per camera is fixed: a [`TRANSITION_RING`]-entry ring, a
//! fixed-size [`EvidenceSummary`] and counters. What is persisted is one row
//! per camera-minute and one per finalized legacy clip, never a frame, an
//! image or a track id.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::Instant;

use chrono::{DateTime, Utc};
use nexus_config::{RecordingPolicyConfig, RecordingPolicyMode, TrackerBackendKind};
use nexus_store::{ClipId, RecordingActivityEpisode, RecordingActivityMinute};
use nexus_types::{CameraId, TrackedObject};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tracing::warn;

use crate::recording_activity::{
    is_observed, AnalysisOutcome, DegradedCause, EvidenceSummary, Observation,
    RecordingActivityPolicy, RecordingIntent, RecordingReason,
};

/// Transition summaries kept per camera. Dropping an older one marks the
/// trace incomplete; the aggregate counters stay exact regardless.
pub const TRANSITION_RING: usize = 256;

/// Width of a persisted counter bucket.
const MINUTE_MS: u64 = 60_000;

/// One change of the legacy recorder's state or of the shadow intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Transition {
    /// The policy clock: an evidence-driven change at its frame's capture,
    /// a change the stale deadline drove at the controller's now; never
    /// before the previous transition.
    pub at_ms: u64,
    /// The wall clock at `at_ms`, from the latest frame's own capture wall
    /// clock: a correction reaches every transition after it and moves no
    /// earlier one.
    pub wall_at: DateTime<Utc>,
    pub legacy_open: bool,
    pub shadow_open: bool,
    pub reason: RecordingReason,
}

/// Milliseconds spent under each shadow reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ReasonMs {
    pub activity: u64,
    pub post_roll: u64,
    pub degraded_coverage: u64,
    pub quiet: u64,
    /// Only observed mode holds a writer for the minimum duration; absent at zero.
    #[serde(skip_serializing_if = "is_zero")]
    pub minimum_duration: u64,
}

fn is_zero(ms: &u64) -> bool {
    *ms == 0
}

impl ReasonMs {
    fn add(&mut self, reason: RecordingReason, ms: u64) {
        *match reason {
            RecordingReason::Activity => &mut self.activity,
            RecordingReason::PostRoll => &mut self.post_roll,
            RecordingReason::DegradedCoverage => &mut self.degraded_coverage,
            RecordingReason::Quiet => &mut self.quiet,
            RecordingReason::MinimumDuration => &mut self.minimum_duration,
        } += ms;
    }

    fn since(self, earlier: Self) -> Self {
        Self {
            activity: self.activity - earlier.activity,
            post_roll: self.post_roll - earlier.post_roll,
            degraded_coverage: self.degraded_coverage - earlier.degraded_coverage,
            quiet: self.quiet - earlier.quiet,
            minimum_duration: self.minimum_duration - earlier.minimum_duration,
        }
    }
}

/// Traced time by legacy state and shadow reason. The eight cells never
/// overlap, so any union of them (legacy or shadow open, say) is exact.
/// `legacy_open.quiet` is footage the fresh-evidence policy would not have
/// kept; `legacy_closed` cells other than `quiet` are footage it would add.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Elapsed {
    pub legacy_open: ReasonMs,
    pub legacy_closed: ReasonMs,
}

impl Elapsed {
    fn add(&mut self, state: Transition, ms: u64) {
        let cell = if state.legacy_open {
            &mut self.legacy_open
        } else {
            &mut self.legacy_closed
        };
        cell.add(state.reason, ms);
    }
}

/// One persisted camera-minute.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MinuteCounters {
    pub elapsed: Elapsed,
    pub succeeded: u64,
    pub skipped: u64,
    pub failed: u64,
    /// Successful passes with at least one qualifying observation.
    pub qualifying_frames: u64,
    /// Successful passes whose only objects were tracker predictions.
    pub predicted_only_frames: u64,
    pub transitions: u64,
    /// The trace had a gap by the end of this minute.
    pub trace_incomplete: bool,
}

/// One analysed frame's objects after exclusion zones and static suppression.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EvidenceCounts {
    /// Qualifying: a current detection, outside exclusion zones, not static.
    pub observed: u32,
    /// Tracker predictions without a current detection: never activity.
    pub predicted: u32,
    /// Suppressed by the static-object policy.
    #[serde(rename = "static")]
    pub stationary: u32,
    /// Dropped by exclusion zones or a zone's minimum area.
    pub excluded: u32,
}

/// Total length of half-open intervals added in start order, each
/// overlap counted once: `[0,10)` then `[5,15)` is 15, not 20.
#[derive(Clone, Copy, Debug, Default)]
struct IntervalUnion {
    end: u64,
    total: u64,
}

impl IntervalUnion {
    fn add(&mut self, start: u64, end: u64) {
        let start = start.max(self.end);
        if end > start {
            self.total += end - start;
            self.end = end;
        }
    }
}

/// The files one side would have written, each `[open - pre_roll, close)`.
/// Kept shifted by `+pre_roll`, which leaves a union's length unchanged and
/// keeps the pre-roll before the trace's first frame.
#[derive(Clone, Copy, Debug, Default)]
struct Files {
    union: IntervalUnion,
    open_since: Option<u64>,
}

impl Files {
    fn edge(&mut self, open: bool, at: u64, pre_roll_ms: u64) {
        match (self.open_since, open) {
            (None, true) => self.open_since = Some(at),
            (Some(start), false) => {
                self.union.add(start, at + pre_roll_ms);
                self.open_since = None;
            }
            _ => {}
        }
    }

    fn total_at(self, now: u64, pre_roll_ms: u64) -> u64 {
        let mut union = self.union;
        if let Some(start) = self.open_since {
            union.add(start, now + pre_roll_ms);
        }
        union.total
    }
}

/// Read-only view of a camera's diagnostics at one instant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiagnosticsSnapshot {
    pub legacy_open: bool,
    pub shadow_open: bool,
    /// `None` before the first analysed frame.
    pub reason: Option<RecordingReason>,
    pub last_success_age_ms: Option<u64>,
    pub last_qualifying_age_ms: Option<u64>,
    pub last_failure_age_ms: Option<u64>,
    /// The latest analysed frame's objects.
    pub counts: EvidenceCounts,
    pub transitions: u64,
    /// Transitions were dropped, the clock stepped back, or the collector failed.
    pub trace_incomplete: bool,
    pub collector_error: Option<String>,
    pub elapsed: Elapsed,
    /// File time with pre-roll, overlapping windows counted once.
    pub legacy_recorded_ms: u64,
    pub shadow_recorded_ms: u64,
}

/// Bounded diagnostics of one camera's shadow policy. Times are the
/// policy's monotonic milliseconds.
#[derive(Debug)]
pub struct RecordingDiagnostics {
    pre_roll_ms: u64,
    ring: VecDeque<Transition>,
    state: Option<Transition>,
    /// Elapsed time is credited up to here.
    credited_ms: u64,
    elapsed: Elapsed,
    legacy_files: Files,
    shadow_files: Files,
    transitions: u64,
    trace_incomplete: bool,
    clock_steps_back: u64,
    collector_error: Option<String>,
    minute: MinuteCounters,
    counts: EvidenceCounts,
    last_success_ms: Option<u64>,
    last_qualifying_ms: Option<u64>,
    last_failure_ms: Option<u64>,
}

impl RecordingDiagnostics {
    pub fn new(pre_roll_ms: u64) -> Self {
        Self {
            pre_roll_ms,
            ring: VecDeque::with_capacity(TRANSITION_RING),
            state: None,
            credited_ms: 0,
            elapsed: Elapsed::default(),
            legacy_files: Files::default(),
            shadow_files: Files::default(),
            transitions: 0,
            trace_incomplete: false,
            clock_steps_back: 0,
            collector_error: None,
            minute: MinuteCounters::default(),
            counts: EvidenceCounts::default(),
            last_success_ms: None,
            last_qualifying_ms: None,
            last_failure_ms: None,
        }
    }

    /// The state at `at_ms`, read with the wall clock `wall_at`. Time since
    /// the previous call is credited to the previous state; an unchanged
    /// state records no transition.
    pub fn transition(
        &mut self,
        at_ms: u64,
        wall_at: DateTime<Utc>,
        legacy_open: bool,
        shadow_open: bool,
        reason: RecordingReason,
    ) {
        if at_ms < self.credited_ms {
            // Never credit time twice: hold the clock and say the trace has a gap.
            self.step_back();
        }
        let at_ms = at_ms.max(self.credited_ms);
        self.credit(at_ms);
        let next = Transition {
            at_ms,
            wall_at,
            legacy_open,
            shadow_open,
            reason,
        };
        if self.state.is_some_and(|s| {
            Transition {
                at_ms,
                wall_at,
                ..s
            } == next
        }) {
            return;
        }
        self.legacy_files.edge(legacy_open, at_ms, self.pre_roll_ms);
        self.shadow_files.edge(shadow_open, at_ms, self.pre_roll_ms);
        if self.ring.len() == TRANSITION_RING {
            self.ring.pop_front();
            self.trace_incomplete = true;
        }
        self.ring.push_back(next);
        self.state = Some(next);
        self.transitions += 1;
        self.minute.transitions += 1;
    }

    /// Count one analysis pass and take the evidence times from `summary`.
    pub fn observe(
        &mut self,
        outcome: AnalysisOutcome,
        counts: EvidenceCounts,
        summary: &EvidenceSummary,
    ) {
        match outcome {
            AnalysisOutcome::Skipped => self.minute.skipped += 1,
            AnalysisOutcome::Failed => self.minute.failed += 1,
            AnalysisOutcome::Success => {
                self.minute.succeeded += 1;
                if counts.observed > 0 {
                    self.minute.qualifying_frames += 1;
                } else if counts.predicted > 0 {
                    self.minute.predicted_only_frames += 1;
                }
            }
        }
        if outcome != AnalysisOutcome::Failed {
            self.counts = counts;
        }
        self.last_success_ms = summary.last_success_ms;
        self.last_qualifying_ms = summary.last_activity_ms;
        self.last_failure_ms = summary.last_failure.map(|(at, _)| at);
    }

    /// A state was held at the latest traced time, its own clock behind
    /// what is already credited: the trace has a gap.
    fn step_back(&mut self) {
        self.trace_incomplete = true;
        self.clock_steps_back += 1;
    }

    /// The collector lost data (a summary was not saved): the trace has a gap.
    pub fn collector_failed(&mut self, error: String) {
        self.trace_incomplete = true;
        self.collector_error = Some(error);
    }

    /// Close the current minute at `at_ms` and start the next.
    pub fn take_minute(&mut self, at_ms: u64) -> MinuteCounters {
        self.credit(at_ms.max(self.credited_ms));
        self.minute.trace_incomplete = self.trace_incomplete;
        std::mem::take(&mut self.minute)
    }

    /// Time credited so far, up to the latest [`Self::transition`].
    pub fn elapsed(&self) -> Elapsed {
        self.elapsed
    }

    /// The retained transitions, oldest first.
    pub fn transitions(&self) -> impl Iterator<Item = &Transition> {
        self.ring.iter()
    }

    pub fn snapshot(&self, now_ms: u64) -> DiagnosticsSnapshot {
        let now = now_ms.max(self.credited_ms);
        let mut elapsed = self.elapsed;
        if let Some(state) = self.state {
            elapsed.add(state, now - self.credited_ms);
        }
        let age = |t: Option<u64>| t.map(|t| now.saturating_sub(t));
        DiagnosticsSnapshot {
            legacy_open: self.state.is_some_and(|s| s.legacy_open),
            shadow_open: self.state.is_some_and(|s| s.shadow_open),
            reason: self.state.map(|s| s.reason),
            last_success_age_ms: age(self.last_success_ms),
            last_qualifying_age_ms: age(self.last_qualifying_ms),
            last_failure_age_ms: age(self.last_failure_ms),
            counts: self.counts,
            transitions: self.transitions,
            trace_incomplete: self.trace_incomplete,
            collector_error: self.collector_error.clone(),
            elapsed,
            legacy_recorded_ms: self.legacy_files.total_at(now, self.pre_roll_ms),
            shadow_recorded_ms: self.shadow_files.total_at(now, self.pre_roll_ms),
        }
    }

    fn credit(&mut self, to: u64) {
        if let Some(state) = self.state {
            let ms = to - self.credited_ms;
            self.elapsed.add(state, ms);
            self.minute.elapsed.add(state, ms);
        }
        self.credited_ms = to;
    }
}

/// A camera's shadow runtime, shared by its supervisor and stats readers.
#[derive(Debug)]
pub struct ShadowState {
    mode: RecordingPolicyMode,
    generation: u64,
    policy_revision: String,
    /// Origin of the policy's millisecond clock: the generation's first frame.
    origin: Option<Instant>,
    active_clip: Option<ClipId>,
    diagnostics: RecordingDiagnostics,
}

/// The read-only runtime snapshot on a camera's stats.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecordingActivityRuntime {
    pub schema_version: u32,
    /// The mode running.
    pub mode: RecordingPolicyMode,
    /// The supervisor generation running it.
    pub generation: u64,
    /// The camera's saved `recording_policy_revision` the controller applied.
    pub policy_revision: String,
    /// The open motion clip of whichever side writes: legacy, or observed's policy.
    pub active_clip_id: Option<ClipId>,
    /// `None` for legacy, which measures nothing.
    #[serde(flatten)]
    pub diagnostics: Option<DiagnosticsSnapshot>,
}

/// A non-legacy camera's retained transitions, for the explicit diagnostics
/// bundle only: shadow intervals placed on capture time can be matched to
/// independently labeled events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TransitionTrace {
    pub mode: RecordingPolicyMode,
    pub generation: u64,
    pub policy_revision: String,
    /// `None` until the generation's first frame starts the policy clock.
    pub clock: Option<TraceClock>,
    /// Oldest first, at most [`TRANSITION_RING`]; each state holds until
    /// the next, the last one through `clock.policy_ms`.
    pub transitions: Vec<Transition>,
    pub trace_incomplete: bool,
    /// Why it is incomplete: older transitions the full ring dropped, clock
    /// steps back (a transition, or evidence that would have changed the
    /// trace earlier, was held at the latest traced time), and the
    /// collector's last failure.
    pub dropped_transitions: u64,
    pub clock_steps_back: u64,
    pub collector_error: Option<String>,
}

/// The policy clock and the wall clock read together at export, where the
/// last state ends. Each transition carries its own capture-time wall clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TraceClock {
    pub policy_ms: u64,
    pub wall_at: DateTime<Utc>,
}

/// Each camera whose supervisor runs, whatever its mode, so no entry means
/// nothing runs it. Entries are weak: a supervisor that ends, however it
/// ends, takes its entry with it.
#[derive(Debug, Default)]
pub struct RecordingActivityRegistry {
    cameras: RwLock<HashMap<CameraId, Weak<Mutex<ShadowState>>>>,
}

impl RecordingActivityRegistry {
    fn start(&self, camera_id: CameraId, state: ShadowState) -> Arc<Mutex<ShadowState>> {
        let shared = Arc::new(Mutex::new(state));
        self.cameras
            .write()
            .insert(camera_id, Arc::downgrade(&shared));
        shared
    }

    /// A camera whose supervisor runs legacy, reported while the handle lives.
    pub(crate) fn start_legacy(
        &self,
        camera_id: CameraId,
        generation: u64,
        policy_revision: &str,
    ) -> LegacyRuntime {
        LegacyRuntime(self.start(
            camera_id,
            ShadowState {
                mode: RecordingPolicyMode::Legacy,
                generation,
                policy_revision: policy_revision.to_owned(),
                origin: None,
                active_clip: None,
                // Never read: legacy reports no diagnostics.
                diagnostics: RecordingDiagnostics::new(0),
            },
        ))
    }

    pub fn snapshot(&self, camera_id: CameraId, now: Instant) -> Option<RecordingActivityRuntime> {
        let shared = self.cameras.read().get(&camera_id)?.upgrade()?;
        let s = shared.lock();
        let now_ms = s.origin.map_or(0, |origin| millis(now, origin));
        Some(RecordingActivityRuntime {
            schema_version: 1,
            mode: s.mode,
            generation: s.generation,
            policy_revision: s.policy_revision.clone(),
            active_clip_id: s.active_clip,
            diagnostics: (s.mode != RecordingPolicyMode::Legacy)
                .then(|| s.diagnostics.snapshot(now_ms)),
        })
    }

    /// The camera's [`TransitionTrace`], `None` for legacy; `now` and
    /// `wall_at` must be read together.
    pub fn trace(
        &self,
        camera_id: CameraId,
        now: Instant,
        wall_at: DateTime<Utc>,
    ) -> Option<TransitionTrace> {
        let shared = self.cameras.read().get(&camera_id)?.upgrade()?;
        let s = shared.lock();
        if s.mode == RecordingPolicyMode::Legacy {
            return None;
        }
        let d = &s.diagnostics;
        Some(TransitionTrace {
            mode: s.mode,
            generation: s.generation,
            policy_revision: s.policy_revision.clone(),
            clock: s.origin.map(|origin| TraceClock {
                policy_ms: millis(now, origin),
                wall_at,
            }),
            transitions: d.ring.iter().copied().collect(),
            trace_incomplete: d.trace_incomplete,
            // Every transition entered the ring; only a full ring drops one.
            dropped_transitions: d.transitions - d.ring.len() as u64,
            clock_steps_back: d.clock_steps_back,
            collector_error: d.collector_error.clone(),
        })
    }
}

/// A legacy camera's entry in the [`RecordingActivityRegistry`].
pub(crate) struct LegacyRuntime(Arc<Mutex<ShadowState>>);

impl LegacyRuntime {
    /// Legacy's open motion clip, which the camera's stats report.
    pub(crate) fn set_active_clip(&self, clip: Option<ClipId>) {
        self.0.lock().active_clip = clip;
    }
}

pub(crate) fn millis(at: Instant, origin: Instant) -> u64 {
    u64::try_from(at.saturating_duration_since(origin).as_millis()).unwrap_or(u64::MAX)
}

#[expect(
    clippy::expect_used,
    reason = "plain structs of numbers and strings always serialize"
)]
fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("diagnostics serialize to JSON")
}

fn reason_str(reason: RecordingReason) -> &'static str {
    match reason {
        RecordingReason::Activity => "activity",
        RecordingReason::PostRoll => "post_roll",
        RecordingReason::DegradedCoverage => "degraded_coverage",
        RecordingReason::Quiet => "quiet",
        RecordingReason::MinimumDuration => "minimum_duration",
    }
}

/// The legacy clip open now, as the shadow policy has seen it so far.
#[derive(Debug)]
struct Episode {
    clip_id: ClipId,
    opened_at: DateTime<Utc>,
    open_reason: RecordingReason,
    elapsed_at_open: ReasonMs,
}

/// Summaries a frame finished, for the supervisor to persist.
#[derive(Debug, Default)]
pub struct ShadowWrites {
    pub minute: Option<RecordingActivityMinute>,
    pub episode: Option<RecordingActivityEpisode>,
}

/// The tracker backend `nexus_tracker` names `name`, if it has an
/// observed/predicted contract the policy can rely on.
pub fn tracker_backend(name: &str) -> Option<TrackerBackendKind> {
    match name {
        "bytetrack" => Some(TrackerBackendKind::Bytetrack),
        "iou-naive" => Some(TrackerBackendKind::IouNaive),
        _ => None,
    }
}

/// Classify a frame's tracks: `dynamic` are those left after static
/// suppression, of `total` tracked after the zone filters dropped `excluded`.
pub fn evidence_counts<'a>(
    backend: TrackerBackendKind,
    dynamic: impl Iterator<Item = &'a TrackedObject>,
    total: usize,
    excluded: usize,
) -> EvidenceCounts {
    let (mut observed, mut seen) = (0u32, 0u32);
    for t in dynamic {
        seen += 1;
        observed += u32::from(is_observed(backend, t));
    }
    EvidenceCounts {
        observed,
        predicted: seen - observed,
        stationary: u32::try_from(total)
            .unwrap_or(u32::MAX)
            .saturating_sub(seen),
        excluded: u32::try_from(excluded).unwrap_or(u32::MAX),
    }
}

/// The fresh-evidence policy for one supervisor generation, with its
/// diagnostics: fed the frames legacy decides on, after the same zone and
/// static filters. In shadow mode it is only measured; in observed mode the
/// recording controller writes what it intends.
pub struct ShadowRecording {
    camera_id: CameraId,
    generation: u64,
    backend: TrackerBackendKind,
    post_roll_ms: u64,
    stale_after_ms: u64,
    /// What [`Self::frame`] reduces itself; the controller passes the producer's.
    sequence: u64,
    summary: EvidenceSummary,
    /// Built at the first frame: degraded coverage counts from healthy ingest.
    policy: Option<RecordingActivityPolicy>,
    /// The policy's latest answer, which [`Self::record`] traces.
    intent: Option<RecordingIntent>,
    /// A pass [`Self::evidence`] applied, counted once [`Self::record`] has
    /// closed any minute it ended.
    pass: Option<(AnalysisOutcome, EvidenceCounts, EvidenceSummary)>,
    /// Index and first frame's wall clock of the minute being counted.
    minute: Option<(u64, DateTime<Utc>)>,
    episode: Option<Episode>,
    /// The latest traced clock, wall clock and shadow reason.
    last: Option<(u64, DateTime<Utc>, RecordingReason)>,
    /// Capture of the newest evidence used: none captured before it is used
    /// after it, so tracing up to it can never hold later evidence.
    used_ms: Option<u64>,
    /// The current state was traced later than evidence showed it, and
    /// that was counted: evidence agreeing with it is no new gap.
    late: bool,
    shared: Arc<Mutex<ShadowState>>,
}

impl ShadowRecording {
    /// `None` for legacy, and for a tracker with no observed/predicted
    /// contract. Runs as shadow whatever the policy's mode: legacy writes.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        registry: &RecordingActivityRegistry,
        camera_id: CameraId,
        generation: u64,
        policy: Option<RecordingPolicyConfig>,
        policy_revision: &str,
        tracker: &str,
        post_roll_secs: u32,
        pre_roll_secs: u32,
    ) -> Option<Self> {
        let policy = policy.filter(|p| p.mode != RecordingPolicyMode::Legacy)?;
        Self::start_as(
            registry,
            camera_id,
            generation,
            policy,
            policy_revision,
            RecordingPolicyMode::Shadow,
            tracker,
            post_roll_secs,
            pre_roll_secs,
        )
    }

    /// [`Self::start`] reporting `mode` as the one running.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_as(
        registry: &RecordingActivityRegistry,
        camera_id: CameraId,
        generation: u64,
        policy: RecordingPolicyConfig,
        policy_revision: &str,
        mode: RecordingPolicyMode,
        tracker: &str,
        post_roll_secs: u32,
        pre_roll_secs: u32,
    ) -> Option<Self> {
        let Some(backend) = tracker_backend(tracker) else {
            warn!(
                camera_id,
                tracker,
                "recording_policy shadow needs a tracker with an observed/predicted contract; not running"
            );
            return None;
        };
        let stale_after_ms = policy.analysis_stale_after_ms;
        let shared = registry.start(
            camera_id,
            ShadowState {
                mode,
                generation,
                policy_revision: policy_revision.to_owned(),
                origin: None,
                active_clip: None,
                diagnostics: RecordingDiagnostics::new(u64::from(pre_roll_secs) * 1_000),
            },
        );
        Some(Self {
            camera_id,
            generation,
            backend,
            post_roll_ms: u64::from(post_roll_secs) * 1_000,
            stale_after_ms,
            sequence: 0,
            summary: EvidenceSummary::new(generation, stale_after_ms),
            policy: None,
            intent: None,
            pass: None,
            minute: None,
            episode: None,
            last: None,
            used_ms: None,
            late: false,
            shared,
        })
    }

    /// [`evidence_counts`] with this policy's tracker backend.
    pub fn counts<'a>(
        &self,
        dynamic: impl Iterator<Item = &'a TrackedObject>,
        total: usize,
        excluded: usize,
    ) -> EvidenceCounts {
        evidence_counts(self.backend, dynamic, total, excluded)
    }

    /// One analysis pass, reduced here. `alert` is an explicit alert
    /// recording on this frame; `legacy_clip` is legacy's open clip once the
    /// frame is done.
    pub fn frame(
        &mut self,
        captured_mono: Instant,
        captured_at: DateTime<Utc>,
        outcome: AnalysisOutcome,
        counts: EvidenceCounts,
        alert: bool,
        legacy_clip: Option<ClipId>,
    ) -> ShadowWrites {
        let origin = *self.shared.lock().origin.get_or_insert(captured_mono);
        let at = millis(captured_mono, origin);
        self.sequence += 1;
        self.summary.reduce(
            self.generation,
            self.sequence,
            Observation {
                captured_ms: at,
                // Analysed on this loop as it arrived: nothing to reorder.
                received_ms: at,
                qualifying_observed: counts.observed,
                predicted_only: counts.predicted,
                outcome,
            },
        );
        let summary = self.summary;
        self.evidence(
            origin,
            captured_mono,
            captured_mono,
            Some((outcome, counts)),
            &summary,
            alert,
        );
        self.record(captured_mono, captured_at, legacy_clip, false)
    }

    /// Apply a producer's coalesced `summary`, reduced on a clock that
    /// starts at `origin`, as of the frame captured at `captured_mono` and
    /// used at `now`: activity runs on the analysis frontier the summary
    /// carries, freshness is judged now. `analysis` is that frame's own
    /// pass, counted by [`Self::record`].
    pub fn evidence(
        &mut self,
        origin: Instant,
        captured_mono: Instant,
        now: Instant,
        analysis: Option<(AnalysisOutcome, EvidenceCounts)>,
        summary: &EvidenceSummary,
        alert: bool,
    ) -> RecordingIntent {
        let origin = *self.shared.lock().origin.get_or_insert(origin);
        let at = millis(captured_mono, origin);
        self.used_ms = self.used_ms.max(Some(at));
        let now_ms = millis(now, origin).max(at);
        let policy = self.policy.get_or_insert_with(|| {
            RecordingActivityPolicy::new(self.post_roll_ms, self.stale_after_ms, at)
        });
        if alert {
            policy.alert(at);
        }
        policy.apply_summary(summary, now_ms);
        let intent = policy.tick(now_ms);
        self.intent = Some(intent);
        if let Some((outcome, counts)) = analysis {
            self.pass = Some((outcome, counts, *summary));
        }
        intent
    }

    /// An explicit alert captured at `captured_mono`, used at `now`.
    pub fn alert(&mut self, captured_mono: Instant, now: Instant) -> RecordingIntent {
        let origin = *self.shared.lock().origin.get_or_insert(captured_mono);
        let at = millis(captured_mono, origin);
        let policy = self.policy.get_or_insert_with(|| {
            RecordingActivityPolicy::new(self.post_roll_ms, self.stale_after_ms, at)
        });
        policy.alert(at);
        let intent = policy.tick(millis(now, origin).max(at));
        self.intent = Some(intent);
        intent
    }

    /// The policy's answer at `now` with no new evidence; `None` before any.
    pub fn tick(&mut self, now: Instant) -> Option<RecordingIntent> {
        let origin = self.shared.lock().origin?;
        let intent = self.policy.as_mut()?.tick(millis(now, origin));
        self.intent = Some(intent);
        Some(intent)
    }

    /// When [`Self::tick`] can next answer differently without new evidence.
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        let origin = self.shared.lock().origin?;
        let at = self.policy.as_ref()?.next_change_ms(millis(now, origin))?;
        Some(origin + std::time::Duration::from_millis(at))
    }

    /// When the minute being counted is cut if nothing traced has passed
    /// its end, so its row is written even through a stall.
    ///
    /// A minute is cut, and its row written, when [`Self::record`] first
    /// traces past its end: evidence at its capture, once the frontier has
    /// passed it, or a wake at the controller's now that changes the traced
    /// state or is this timer's. This timer waits until the minute's end is
    /// the stale deadline (at most half a minute) behind: a frontier still
    /// short of it is then stale, so the cut lands where a stale-driven
    /// transition does, and while analysis is healthy the stale deadline
    /// wakes first. Evidence used late inside a minute not yet cut is
    /// credited at its capture; evidence captured in a minute already cut is
    /// held at the latest traced time, and a change it makes there is
    /// reported as a gap.
    pub fn next_minute(&self) -> Option<Instant> {
        let origin = self.shared.lock().origin?;
        let (index, _) = self.minute?;
        Some(origin + std::time::Duration::from_millis(self.cut_ms(index)))
    }

    /// When the minute timer cuts minute `index`.
    fn cut_ms(&self, index: u64) -> u64 {
        // shortcut: capped so the wake never passes the next minute's end (one row per cut cannot split it); a stale deadline over 30 s can then cut ahead of a still-fresh frontier, reported as a gap. Write a row per elapsed minute if those deadlines matter.
        (index + 1) * MINUTE_MS + self.stale_after_ms.min(MINUTE_MS / 2)
    }

    /// Trace the state once the writer acted on the latest answer, as of
    /// `captured_mono` read with the wall clock `captured_at`: an
    /// evidence-driven state at its frame's capture, one the policy's
    /// deadline or the writer decided at the caller's now. `writer_clip` is
    /// the open clip, `held` that it is open only for the minimum duration.
    /// Returns the summaries this finished.
    ///
    /// A wake past the evidence used that changes no traced state, a file
    /// rotation say, traces nothing until the minute timer is due: evidence
    /// captured before it may still be used, and keeps its capture time.
    /// A change at now (going stale; in observed an alert, a retried open or
    /// the end of a hold, the writer's own decisions) is traced there and
    /// can come before such evidence. A time before the latest traced one
    /// is held there, its wall clock run on with it, so the trace never runs
    /// back; a held record that changes the state, or the newest evidence's
    /// held policy state that differs from the one traced at its capture (it
    /// would have started or ended an interval earlier), is counted as a
    /// clock step back. So a change at now never shortens an
    /// evidence-driven interval silently.
    pub fn record(
        &mut self,
        captured_mono: Instant,
        mut captured_at: DateTime<Utc>,
        writer_clip: Option<ClipId>,
        held: bool,
    ) -> ShadowWrites {
        use RecordingReason::{Activity, DegradedCoverage, MinimumDuration, PostRoll, Quiet};
        let mut writes = ShadowWrites::default();
        let Some(intent) = self.intent else {
            return writes;
        };
        let reason = if held { MinimumDuration } else { intent.reason };
        let mut shared = self.shared.lock();
        let origin = *shared.origin.get_or_insert(captured_mono);
        let captured = millis(captured_mono, origin);
        let mut at = captured;
        if let Some((last, ..)) = self.last.filter(|&(last, ..)| last > at) {
            captured_at += chrono::Duration::milliseconds((last - at) as i64);
            at = last;
        }
        let state = (writer_clip.is_some(), intent.open, reason);
        let unchanged = shared
            .diagnostics
            .state
            .is_some_and(|s| (s.legacy_open, s.shadow_open, s.reason) == state);
        let traced = !(unchanged
            && self.used_ms.is_some_and(|used| captured > used)
            && self
                .minute
                .is_some_and(|(current, _)| at < self.cut_ms(current)));
        let index = at / MINUTE_MS;
        match self.minute {
            // Forward only: a minute index is written once.
            Some((current, started)) if traced && index > current => {
                // Closed at its own end; this frame counts in the next one.
                let counters = shared.diagnostics.take_minute((current + 1) * MINUTE_MS);
                writes.minute = Some(minute_row(
                    self.camera_id,
                    self.generation,
                    &shared.policy_revision,
                    current,
                    started,
                    &counters,
                ));
                self.minute = Some((index, captured_at));
            }
            None => self.minute = Some((index, captured_at)),
            Some(_) => {}
        }

        shared.active_clip = writer_clip;
        let d = &mut shared.diagnostics;
        if traced {
            let mut failed = false;
            if let Some((outcome, counts, summary)) = self.pass.take() {
                failed = outcome == AnalysisOutcome::Failed;
                d.observe(outcome, counts, &summary);
            }
            let then = d
                .ring
                .iter()
                .rev()
                .find(|t| t.at_ms <= captured)
                .map(|t| t.reason);
            let transitions = d.transitions;
            d.transition(at, captured_at, writer_clip.is_some(), intent.open, reason);
            let changed = d.transitions > transitions;
            if at > captured {
                // Held. Only the newest evidence's state is its own (older evidence moves no frontier back);
                // degraded coverage is judged when used unless this pass failed or evidence, not the
                // consumer's clock, shows it began by this capture (a broken success run); a hold is the writer's own.
                let evidenced = self
                    .policy
                    .as_ref()
                    .and_then(RecordingActivityPolicy::degraded_since)
                    .is_some_and(|(since, cause)| {
                        cause != DegradedCause::Stale && since <= captured
                    });
                let driven = matches!(reason, Activity | PostRoll | Quiet)
                    || ((failed || evidenced) && reason == DegradedCoverage);
                let policy = |r| if r == MinimumDuration { Quiet } else { r };
                let earlier = driven
                    && self.used_ms == Some(captured)
                    && then.is_some_and(|r| policy(r) != reason);
                if changed || (earlier && !self.late) {
                    d.step_back();
                    self.late = true;
                }
            } else if changed {
                self.late = false;
            }
            self.last = Some((at, captured_at, reason));
        }

        if self.episode.as_ref().map(|e| e.clip_id) != writer_clip {
            // shortcut: a rotation at a wake that traces nothing splits the clips' elapsed at the latest traced time, not at the rotation; defer the episode row until the trace passes it if per-clip splits must be exact.
            let elapsed = d.elapsed().legacy_open;
            writes.episode = self.episode.take().map(|e| {
                episode_row(
                    self.camera_id,
                    self.generation,
                    &shared.policy_revision,
                    e,
                    captured_at,
                    reason,
                    elapsed,
                )
            });
            self.episode = writer_clip.map(|clip_id| Episode {
                clip_id,
                opened_at: captured_at,
                open_reason: reason,
                elapsed_at_open: elapsed,
            });
        }
        writes
    }

    /// The supervisor ended and its clip was closed: flush what is open,
    /// through the last frame (time after it was not traced).
    pub fn finish(&mut self) -> ShadowWrites {
        let mut writes = ShadowWrites::default();
        let Some((at, wall, reason)) = self.last else {
            return writes;
        };
        let mut shared = self.shared.lock();
        shared.active_clip = None;
        let d = &mut shared.diagnostics;
        d.transition(at, wall, false, reason != RecordingReason::Quiet, reason);
        let elapsed = d.elapsed().legacy_open;
        let minute = d.take_minute(at);
        writes.episode = self.episode.take().map(|e| {
            episode_row(
                self.camera_id,
                self.generation,
                &shared.policy_revision,
                e,
                wall,
                reason,
                elapsed,
            )
        });
        writes.minute = self.minute.take().map(|(index, started)| {
            minute_row(
                self.camera_id,
                self.generation,
                &shared.policy_revision,
                index,
                started,
                &minute,
            )
        });
        writes
    }

    /// A summary was not saved: reported on the camera's diagnostics.
    pub fn collector_failed(&self, error: String) {
        self.shared.lock().diagnostics.collector_failed(error);
    }
}

fn minute_row(
    camera_id: CameraId,
    generation: u64,
    policy_revision: &str,
    minute: u64,
    started: DateTime<Utc>,
    counters: &MinuteCounters,
) -> RecordingActivityMinute {
    RecordingActivityMinute {
        camera_id,
        generation: generation as i64,
        policy_revision: policy_revision.to_owned(),
        minute: minute as i64,
        started_at_ms: started.timestamp_millis(),
        counters_json: to_json(counters),
    }
}

fn episode_row(
    camera_id: CameraId,
    generation: u64,
    policy_revision: &str,
    episode: Episode,
    closed_at: DateTime<Utc>,
    close_reason: RecordingReason,
    elapsed_now: ReasonMs,
) -> RecordingActivityEpisode {
    RecordingActivityEpisode {
        camera_id,
        generation: generation as i64,
        clip_id: episode.clip_id,
        policy_revision: policy_revision.to_owned(),
        opened_at_ms: episode.opened_at.timestamp_millis(),
        closed_at_ms: closed_at.timestamp_millis(),
        open_reason: reason_str(episode.open_reason),
        close_reason: reason_str(close_reason),
        elapsed_json: to_json(&elapsed_now.since(episode.elapsed_at_open)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use RecordingReason::{Activity, DegradedCoverage, PostRoll, Quiet};

    /// A wall clock for tests that do not read it.
    const W: DateTime<Utc> = DateTime::UNIX_EPOCH;

    #[test]
    fn more_than_the_ring_keeps_exact_totals_and_reports_the_gap() {
        let mut d = RecordingDiagnostics::new(0);
        // 300 one-second states alternating shadow open and closed, legacy closed.
        for k in 0..300u64 {
            let open = k % 2 == 0;
            d.transition(
                k * 1_000,
                W,
                false,
                open,
                if open { Activity } else { Quiet },
            );
        }
        assert_eq!(d.transitions().count(), TRANSITION_RING, "bounded");
        assert_eq!(d.transitions().next().map(|t| t.at_ms), Some(44_000));
        let s = d.snapshot(300_000);
        assert_eq!(s.transitions, 300);
        assert!(s.trace_incomplete);
        assert_eq!(s.elapsed.legacy_closed.activity, 150_000);
        assert_eq!(s.elapsed.legacy_closed.quiet, 150_000);
        assert_eq!(s.shadow_recorded_ms, 150_000);
        assert_eq!(s.legacy_recorded_ms, 0);
    }

    #[test]
    fn a_full_ring_alone_is_not_a_gap() {
        let mut d = RecordingDiagnostics::new(0);
        for k in 0..TRANSITION_RING as u64 {
            d.transition(k, W, k % 2 == 0, false, Quiet);
        }
        assert!(!d.snapshot(1_000).trace_incomplete);
    }

    #[test]
    fn overlapping_intervals_count_once() {
        let mut u = IntervalUnion::default();
        u.add(0, 10_000);
        u.add(5_000, 15_000);
        assert_eq!(u.total, 15_000, "[0,10) + [5,15) is 15 s, not 20");
        u.add(15_000, 15_000);
        u.add(20_000, 21_000);
        assert_eq!(u.total, 16_000);

        // Legacy open [0,10) and shadow open [5,15): either is 15 s.
        let mut d = RecordingDiagnostics::new(0);
        d.transition(0, W, true, false, Quiet);
        d.transition(5_000, W, true, true, Activity);
        d.transition(10_000, W, false, true, PostRoll);
        d.transition(15_000, W, false, false, Quiet);
        let e = d.snapshot(20_000).elapsed;
        let either = e.legacy_open.activity
            + e.legacy_open.quiet
            + e.legacy_closed.activity
            + e.legacy_closed.post_roll;
        assert_eq!(either, 15_000);
        assert_eq!(e.legacy_closed.quiet, 5_000);
    }

    #[test]
    fn pre_roll_windows_that_overlap_are_counted_once() {
        // 5 s pre-roll: shadow files [0,9) and [5,15) from opens at 5 and 10.
        let mut d = RecordingDiagnostics::new(5_000);
        d.transition(5_000, W, false, true, Activity);
        d.transition(9_000, W, false, false, Quiet);
        d.transition(10_000, W, false, true, Activity);
        d.transition(15_000, W, false, false, Quiet);
        let s = d.snapshot(30_000);
        assert_eq!(s.shadow_recorded_ms, 15_000);
        assert_eq!(s.elapsed.legacy_closed.activity, 9_000);
        // The open one counts to now, and pre-roll before the first frame counts.
        d.transition(40_000, W, true, false, Quiet);
        assert_eq!(d.snapshot(42_000).legacy_recorded_ms, 7_000);
        let mut d = RecordingDiagnostics::new(5_000);
        d.transition(0, W, true, false, Quiet);
        assert_eq!(d.snapshot(1_000).legacy_recorded_ms, 6_000);
    }

    #[test]
    fn a_clock_step_back_is_a_gap_and_never_double_counts() {
        let mut d = RecordingDiagnostics::new(0);
        d.transition(10_000, W, true, true, Activity);
        d.transition(4_000, W, true, false, Quiet);
        let s = d.snapshot(12_000);
        assert!(s.trace_incomplete);
        assert_eq!(s.elapsed.legacy_open.activity, 0);
        assert_eq!(s.elapsed.legacy_open.quiet, 2_000);
    }

    #[test]
    fn a_collector_failure_is_reported() {
        let mut d = RecordingDiagnostics::new(0);
        d.transition(0, W, false, true, DegradedCoverage);
        d.collector_failed("minute summary not saved: disk full".into());
        let s = d.snapshot(1_000);
        assert!(s.trace_incomplete);
        assert_eq!(
            s.collector_error.as_deref(),
            Some("minute summary not saved: disk full")
        );
        assert!(d.take_minute(1_000).trace_incomplete);
    }

    #[test]
    fn minutes_split_the_elapsed_time_exactly() {
        let mut d = RecordingDiagnostics::new(0);
        d.transition(0, W, true, true, Activity);
        d.transition(50_000, W, true, false, Quiet);
        let first = d.take_minute(60_000);
        assert_eq!(first.elapsed.legacy_open.activity, 50_000);
        assert_eq!(first.elapsed.legacy_open.quiet, 10_000);
        assert_eq!(first.transitions, 2);
        d.transition(70_000, W, false, false, Quiet);
        let second = d.take_minute(120_000);
        assert_eq!(second.elapsed.legacy_open.quiet, 10_000);
        assert_eq!(second.elapsed.legacy_closed.quiet, 50_000);
        assert_eq!(second.transitions, 1);
        assert!(!second.trace_incomplete);
    }

    /// The bundle's trace: the retained transitions on the policy clock, the
    /// wall clock read beside it, and why it is incomplete.
    #[test]
    fn a_trace_places_each_retained_interval_and_names_its_gaps() {
        let registry = RecordingActivityRegistry::default();
        let legacy = registry.start_legacy(4, 2, "rev-2");
        assert!(
            registry.trace(4, Instant::now(), Utc::now()).is_none(),
            "legacy measures nothing"
        );
        drop(legacy);
        let origin = Instant::now();
        let shared = registry.start(
            3,
            ShadowState {
                mode: RecordingPolicyMode::Shadow,
                generation: 9,
                policy_revision: "rev-9".into(),
                origin: None,
                active_clip: None,
                diagnostics: RecordingDiagnostics::new(0),
            },
        );
        let wall = Utc::now();
        let at = |ms| origin + std::time::Duration::from_millis(ms);
        assert_eq!(registry.trace(3, at(5_000), wall).unwrap().clock, None);
        shared.lock().origin = Some(origin);
        // Three shadow intervals inside one minute.
        let intervals = [(1_000, 4_000), (10_000, 12_000), (30_000, 33_000)];
        for (open, close) in intervals {
            let d = &mut shared.lock().diagnostics;
            d.transition(open, W, false, true, Activity);
            d.transition(close, W, false, false, Quiet);
        }
        let trace = registry.trace(3, at(40_000), wall).unwrap();
        assert_eq!(
            (trace.mode, trace.generation, trace.policy_revision.as_str()),
            (RecordingPolicyMode::Shadow, 9, "rev-9")
        );
        assert_eq!(
            trace.clock,
            Some(TraceClock {
                policy_ms: 40_000,
                wall_at: wall
            })
        );
        let expected: Vec<Transition> = intervals
            .iter()
            .flat_map(|&(open, close)| {
                [
                    Transition {
                        at_ms: open,
                        wall_at: W,
                        legacy_open: false,
                        shadow_open: true,
                        reason: Activity,
                    },
                    Transition {
                        at_ms: close,
                        wall_at: W,
                        legacy_open: false,
                        shadow_open: false,
                        reason: Quiet,
                    },
                ]
            })
            .collect();
        assert_eq!(trace.transitions, expected);
        assert!(!trace.trace_incomplete);
        assert_eq!((trace.dropped_transitions, trace.clock_steps_back), (0, 0));

        {
            let d = &mut shared.lock().diagnostics;
            for k in 0..300u64 {
                d.transition(40_000 + k, W, k % 2 == 0, false, Quiet);
            }
            // A changed state whose clock stepped back is held and counted.
            d.transition(1_000, W, true, false, Quiet);
            d.collector_failed("minute summary not saved".into());
        }
        let trace = registry.trace(3, at(41_000), wall).unwrap();
        assert_eq!(trace.transitions.len(), TRANSITION_RING);
        assert!(trace.trace_incomplete);
        assert_eq!(
            (trace.dropped_transitions, trace.clock_steps_back),
            (6 + 301 - TRANSITION_RING as u64, 1)
        );
        assert_eq!(
            trace.collector_error.as_deref(),
            Some("minute summary not saved")
        );
    }

    /// Codex review 4: each exported transition carries the wall clock read
    /// with it, so a +60 s wall-clock correction after some of them (here
    /// from 5 s) moves none: every one still reads its capture time.
    #[test]
    fn a_wall_clock_correction_moves_no_exported_transition() {
        let registry = RecordingActivityRegistry::default();
        let policy = RecordingPolicyConfig {
            mode: RecordingPolicyMode::Shadow,
            analysis_stale_after_ms: 10_000,
        };
        let mut s =
            ShadowRecording::start(&registry, 3, 9, Some(policy), "rev-9", "iou-naive", 1, 0)
                .unwrap();
        let (t0, w0) = (Instant::now(), Utc::now());
        let at = |ms: u64| t0 + std::time::Duration::from_millis(ms);
        let wall = |ms: u64| {
            let corrected = if ms >= 5_000 { 60 } else { 0 };
            w0 + chrono::Duration::milliseconds(ms as i64) + chrono::Duration::seconds(corrected)
        };
        // Recovered at 2 s, quiet at 3 s, someone at 6 s, quiet at 7 s.
        for ms in (0..=8_000).step_by(1_000) {
            let counts = EvidenceCounts {
                observed: u32::from(ms == 6_000),
                ..EvidenceCounts::default()
            };
            s.frame(
                at(ms),
                wall(ms),
                AnalysisOutcome::Success,
                counts,
                false,
                None,
            );
        }
        let trace = registry.trace(3, at(9_000), wall(9_000)).unwrap();
        let json = serde_json::to_value(&trace).unwrap();
        let placed: Vec<(u64, DateTime<Utc>)> = json["transitions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let wall_at = serde_json::from_value(t["wall_at"].clone()).unwrap();
                (t["at_ms"].as_u64().unwrap(), wall_at)
            })
            .collect();
        let captured: Vec<_> = [0, 2_000, 3_000, 6_000, 7_000]
            .into_iter()
            .map(|ms| (ms, wall(ms)))
            .collect();
        assert_eq!(placed, captured, "{json}");
        assert!(!trace.trace_incomplete);
    }

    #[test]
    fn shadow_runs_only_where_it_can_and_ends_with_its_supervisor() {
        let registry = RecordingActivityRegistry::default();
        let mode = |mode| {
            Some(RecordingPolicyConfig {
                mode,
                analysis_stale_after_ms: 10_000,
            })
        };
        let start = |policy, tracker| {
            ShadowRecording::start(&registry, 3, 9, policy, "rev-9", tracker, 2, 0)
        };
        assert!(start(None, "bytetrack").is_none());
        assert!(start(mode(RecordingPolicyMode::Legacy), "bytetrack").is_none());
        assert!(
            start(mode(RecordingPolicyMode::Shadow), "kalman").is_none(),
            "a tracker without an observed/predicted contract"
        );
        assert!(registry.snapshot(3, Instant::now()).is_none());
        let shadow = start(mode(RecordingPolicyMode::Shadow), "iou-naive").unwrap();
        let runtime = registry.snapshot(3, Instant::now()).unwrap();
        let reason = runtime.diagnostics.map(|d| d.reason);
        assert_eq!((runtime.generation, reason), (9, Some(None)));
        assert_eq!(runtime.policy_revision, "rev-9");
        drop(shadow);
        assert!(registry.snapshot(3, Instant::now()).is_none());
    }

    #[test]
    fn shadow_writes_a_row_per_minute_and_per_finalized_clip() {
        let registry = RecordingActivityRegistry::default();
        let policy = RecordingPolicyConfig {
            mode: RecordingPolicyMode::Shadow,
            analysis_stale_after_ms: 10_000,
        };
        let mut s =
            ShadowRecording::start(&registry, 3, 9, Some(policy), "rev-9", "iou-naive", 2, 0)
                .unwrap();
        let (t0, w0) = (Instant::now(), Utc::now());
        let wall = |ms: u64| w0 + chrono::Duration::milliseconds(ms as i64);
        let mut frame = |ms: u64, clip: Option<ClipId>| {
            s.frame(
                t0 + std::time::Duration::from_millis(ms),
                wall(ms),
                AnalysisOutcome::Success,
                EvidenceCounts::default(),
                false,
                clip,
            )
        };
        let quiet = |w: &ShadowWrites| w.minute.is_none() && w.episode.is_none();
        assert!(quiet(&frame(0, None)));
        assert!(quiet(&frame(30_000, Some(1))), "legacy opened clip 1");
        // Legacy rotated to clip 2 in a new minute; analysis was never healthy.
        let w = frame(61_000, Some(2));
        let minute = w.minute.unwrap();
        assert_eq!(
            (minute.camera_id, minute.generation, minute.minute),
            (3, 9, 0)
        );
        assert_eq!(minute.started_at_ms, w0.timestamp_millis());
        let counters: serde_json::Value = serde_json::from_str(&minute.counters_json).unwrap();
        assert_eq!(
            counters["elapsed"]["legacy_closed"]["degraded_coverage"],
            30_000
        );
        assert_eq!(
            counters["elapsed"]["legacy_open"]["degraded_coverage"],
            30_000
        );
        assert_eq!(counters["succeeded"], 2);
        let clip = w.episode.unwrap();
        assert_eq!(
            (clip.clip_id, clip.opened_at_ms, clip.closed_at_ms),
            (
                1,
                wall(30_000).timestamp_millis(),
                wall(61_000).timestamp_millis()
            )
        );
        assert_eq!(
            (clip.open_reason, clip.close_reason),
            ("degraded_coverage", "degraded_coverage")
        );
        assert_eq!(
            clip.elapsed_json,
            r#"{"activity":0,"post_roll":0,"degraded_coverage":31000,"quiet":0}"#
        );
        let w = frame(62_000, None);
        assert!(w.minute.is_none());
        assert!(w
            .episode
            .unwrap()
            .elapsed_json
            .contains(r#""degraded_coverage":1000"#));
        let w = s.finish();
        assert!(w.episode.is_none(), "legacy already closed");
        let last = w.minute.unwrap();
        assert_eq!(last.minute, 1);
        let counters: serde_json::Value = serde_json::from_str(&last.counters_json).unwrap();
        assert_eq!(
            counters["elapsed"]["legacy_open"]["degraded_coverage"],
            2_000
        );
        assert_eq!(counters["succeeded"], 2);
    }

    /// A frame captured at 31 s whose analysis is used at 46 s is 15 s old
    /// under a 10 s stale deadline: degraded coverage, not a quiet scene.
    #[test]
    fn evidence_is_as_fresh_as_it_is_when_used() {
        let registry = RecordingActivityRegistry::default();
        let policy = RecordingPolicyConfig {
            mode: RecordingPolicyMode::Shadow,
            analysis_stale_after_ms: 10_000,
        };
        let mut s =
            ShadowRecording::start(&registry, 3, 9, Some(policy), "rev-9", "iou-naive", 10, 0)
                .unwrap();
        let t0 = Instant::now();
        let at = |ms: u64| t0 + std::time::Duration::from_millis(ms);
        let mut summary = EvidenceSummary::new(9, 10_000);
        let mut used = |ms: u64, now: u64| {
            let seen = Observation {
                captured_ms: ms,
                received_ms: ms,
                qualifying_observed: 0,
                predicted_only: 0,
                outcome: AnalysisOutcome::Success,
            };
            summary.reduce(9, ms / 1_000 + 1, seen);
            s.evidence(t0, at(ms), at(now), None, &summary, false)
                .reason
        };
        for ms in (0..=30_000).step_by(1_000) {
            used(ms, ms);
        }
        assert_eq!(used(30_000, 30_000), Quiet);
        assert_eq!(used(31_000, 46_000), DegradedCoverage);
    }

    /// What the recording controller does with a shadow policy, replayed:
    /// it wakes at each deadline the policy names before evidence is used,
    /// traced then, and traces evidence at its frame's capture.
    struct Replay {
        registry: RecordingActivityRegistry,
        s: ShadowRecording,
        summary: EvidenceSummary,
        t0: Instant,
        w0: DateTime<Utc>,
        /// The controller's clock.
        now: u64,
    }

    impl Replay {
        fn start(post_roll_secs: u32, pre_roll_secs: u32) -> Self {
            let registry = RecordingActivityRegistry::default();
            let policy = RecordingPolicyConfig {
                mode: RecordingPolicyMode::Shadow,
                analysis_stale_after_ms: 10_000,
            };
            let s = ShadowRecording::start(
                &registry,
                3,
                9,
                Some(policy),
                "rev-9",
                "iou-naive",
                post_roll_secs,
                pre_roll_secs,
            )
            .unwrap();
            Self {
                registry,
                s,
                summary: EvidenceSummary::new(9, 10_000),
                t0: Instant::now(),
                w0: Utc::now(),
                now: 0,
            }
        }

        fn at(&self, ms: u64) -> Instant {
            self.t0 + std::time::Duration::from_millis(ms)
        }

        fn wall(&self, ms: u64) -> DateTime<Utc> {
            self.w0 + chrono::Duration::milliseconds(ms as i64)
        }

        /// Run the controller's clock to `until`, waking at each policy
        /// deadline and minute timer before it.
        fn wait(&mut self, until: u64) {
            while let Some(t) = [self.s.next_change(self.at(self.now)), self.s.next_minute()]
                .into_iter()
                .flatten()
                .min()
                .filter(|&t| t < self.at(until))
            {
                let ms = millis(t, self.t0);
                self.s.tick(t);
                self.s.record(t, self.wall(ms), None, false);
                self.now = ms;
            }
            self.now = until;
        }

        /// Use the analysis of the frame captured at `captured`, with
        /// `observed` qualifying objects, at `now`.
        fn used(&mut self, captured: u64, now: u64, observed: u32) -> RecordingReason {
            self.wait(now);
            let sequence = self.summary.through_sequence + 1;
            self.summary.reduce(
                9,
                sequence,
                Observation {
                    captured_ms: captured,
                    received_ms: captured,
                    qualifying_observed: observed,
                    predicted_only: 0,
                    outcome: AnalysisOutcome::Success,
                },
            );
            let summary = self.summary;
            let counts = EvidenceCounts {
                observed,
                ..EvidenceCounts::default()
            };
            let intent = self.s.evidence(
                self.t0,
                self.at(captured),
                self.at(now),
                Some((AnalysisOutcome::Success, counts)),
                &summary,
                false,
            );
            self.s
                .record(self.at(captured), self.wall(captured), None, false);
            intent.reason
        }

        /// Healthy from the recovery gate at 2 s, each pass used as captured.
        fn healthy(&mut self) {
            for ms in (0..=9_000).step_by(1_000) {
                self.used(ms, ms, 0);
            }
        }

        /// The retained transitions from `from_ms`: when, shadow open, why.
        fn trace(&self, from_ms: u64) -> Vec<(u64, bool, RecordingReason)> {
            let trace = self
                .registry
                .trace(3, self.at(self.now), self.wall(self.now))
                .unwrap();
            assert!(!trace.trace_incomplete, "{trace:#?}");
            trace
                .transitions
                .iter()
                .filter(|t| t.at_ms >= from_ms)
                .map(|t| (t.at_ms, t.shadow_open, t.reason))
                .collect()
        }

        fn snapshot(&self) -> DiagnosticsSnapshot {
            self.registry
                .snapshot(3, self.at(self.now))
                .unwrap()
                .diagnostics
                .unwrap()
        }
    }

    /// Codex review 5, P1 1: qualifying frames captured every 1.5 s, each
    /// used 1.5 s later, under 2 s of post-roll and 5 s of pre-roll. The
    /// window is judged on the analysis frontier, so the lag closes nothing
    /// between uses: one window, closed by the first pass captured at or
    /// after the last qualifying capture + 2 s.
    #[test]
    fn steady_analysis_lag_keeps_one_window_open() {
        let mut r = Replay::start(2, 5);
        r.healthy();
        for captured in [10_000, 11_500, 13_000, 14_500] {
            assert_eq!(
                r.used(captured, captured + 1_500, 1),
                Activity,
                "{captured}"
            );
        }
        assert_eq!(r.used(16_000, 17_500, 0), PostRoll, "before 14.5 s + 2 s");
        assert_eq!(r.used(17_500, 19_000, 0), Quiet);
        assert_eq!(
            r.trace(10_000),
            [
                (10_000, true, Activity),
                (16_000, true, PostRoll),
                (17_500, false, Quiet)
            ]
        );
        assert_eq!(
            r.snapshot().shadow_recorded_ms,
            (4_000 + 5_000) + (7_500 + 5_000),
            "[0, 4 s) startup and recovery, [10, 17.5 s), each with 5 s of pre-roll"
        );
    }

    /// Codex review 5, P1 2: a qualifying frame captured at 10 s is used at
    /// 12 s under 1 s of post-roll. Its window ends on capture time: the
    /// first pass captured at or after 11 s closes it, however late it is
    /// used, and the accounting is exactly [10, 11 s). An empty frame
    /// captured before that close but used after it, or an older summary
    /// read again, reopens nothing (Codex review 4).
    #[test]
    fn a_late_qualifying_frame_closes_once_the_frontier_reaches_its_end() {
        let mut r = Replay::start(1, 0);
        r.healthy();
        assert_eq!(r.used(10_000, 12_000, 1), Activity);
        assert_eq!(
            r.used(10_500, 12_200, 0),
            PostRoll,
            "the frontier is short of 11 s"
        );
        let older = r.summary;
        assert_eq!(r.used(11_000, 12_400, 0), Quiet);
        assert_eq!(
            r.used(10_700, 13_000, 0),
            Quiet,
            "captured before the close"
        );
        let at = |ms| r.t0 + std::time::Duration::from_millis(ms);
        let reread =
            r.s.evidence(r.t0, at(10_500), at(13_200), None, &older, false);
        assert_eq!(reread.reason, Quiet, "an older summary");
        assert_eq!(
            r.trace(10_000),
            [
                (10_000, true, Activity),
                (10_500, true, PostRoll),
                (11_000, false, Quiet)
            ]
        );
        let s = r.snapshot();
        assert_eq!(
            s.shadow_recorded_ms,
            3_000 + 1_000,
            "[0, 3 s) and [10, 11 s)"
        );
        assert_eq!(s.elapsed.legacy_closed.post_roll, 500);
    }

    /// Codex review 5, P1 2: with no pass after a late qualifying frame the
    /// window stays open, as on any analysis stall, until analysis is stale
    /// on the consumer's clock; then degraded coverage records. A stall
    /// never cuts activity short.
    #[test]
    fn a_late_qualifying_frame_with_no_later_pass_degrades_when_stale() {
        let mut r = Replay::start(1, 0);
        r.healthy();
        assert_eq!(r.used(10_000, 12_000, 1), Activity);
        r.wait(25_000);
        assert_eq!(
            r.trace(10_000),
            [(10_000, true, Activity), (20_001, true, DegradedCoverage)]
        );
        let s = r.snapshot();
        assert_eq!(
            (
                s.elapsed.legacy_closed.activity,
                s.elapsed.legacy_closed.post_roll
            ),
            (1_000 + 10_001, 0),
            "[2, 3 s) recovery, then [10, 20.001 s)"
        );
        assert_eq!(s.shadow_recorded_ms, 3_000 + 15_000, "open through 25 s");
    }

    /// Codex review 6, the frame above across a minute, with the minute
    /// timer firing: captured at 59.5 s and used at 61 s, closed by an empty
    /// frame captured at 60.5 s and used at 62 s. The minute is not cut on
    /// the controller's clock while its evidence can still arrive fresh, so
    /// the window keeps its capture times.
    #[test]
    fn a_late_qualifying_frame_across_a_minute_keeps_its_capture_time() {
        let mut r = Replay::start(1, 0);
        r.healthy();
        for ms in (10_000..=59_000).step_by(1_000) {
            r.used(ms, ms, 0);
        }
        assert_eq!(r.used(59_500, 61_000, 1), Activity);
        assert_eq!(r.used(60_500, 62_000, 0), Quiet);
        assert_eq!(
            r.trace(59_000),
            [(59_500, true, Activity), (60_500, false, Quiet)]
        );
        assert_eq!(
            r.snapshot().elapsed.legacy_closed.activity,
            1_000 + 1_000,
            "[2, 3 s) recovery and [59.5, 60.5 s)"
        );
    }

    /// A stall under the longest stale deadline, a minute, still writes a
    /// row for each minute, degraded throughout: the minute timer is never
    /// late enough to pass the next minute's end.
    #[test]
    fn a_stall_under_the_longest_stale_deadline_writes_every_minute() {
        let registry = RecordingActivityRegistry::default();
        let policy = RecordingPolicyConfig {
            mode: RecordingPolicyMode::Shadow,
            analysis_stale_after_ms: 60_000,
        };
        let mut s =
            ShadowRecording::start(&registry, 3, 9, Some(policy), "rev-9", "iou-naive", 1, 0)
                .unwrap();
        let (t0, w0) = (Instant::now(), Utc::now());
        let failed = AnalysisOutcome::Failed;
        s.frame(t0, w0, failed, EvidenceCounts::default(), false, None);
        let mut rows = Vec::new();
        while rows.len() < 3 {
            let t = s.next_minute().unwrap();
            s.tick(t);
            let wall = w0 + chrono::Duration::from_std(t - t0).unwrap();
            rows.extend(s.record(t, wall, None, false).minute);
        }
        let degraded: Vec<_> = rows
            .iter()
            .map(|m| {
                let c: serde_json::Value = serde_json::from_str(&m.counters_json).unwrap();
                (
                    m.minute,
                    c["elapsed"]["legacy_closed"]["degraded_coverage"].as_u64(),
                )
            })
            .collect();
        let minute = Some(MINUTE_MS);
        assert_eq!(degraded, [(0, minute), (1, minute), (2, minute)]);
    }

    /// Evidence captured in a minute already cut, here by the stale wake at
    /// 65.001 s, is held at the latest traced time: a change it makes lands
    /// late, and the trace reports the gap. Held evidence that changes
    /// nothing moves no time and is no gap.
    #[test]
    fn evidence_older_than_a_cut_minute_is_a_reported_gap() {
        let mut r = Replay::start(1, 0);
        r.healthy();
        for ms in (10_000..=55_000).step_by(1_000) {
            r.used(ms, ms, 0);
        }
        r.wait(66_000);
        assert_eq!(r.used(59_500, 66_000, 1), Activity);
        let trace = r.registry.trace(3, r.at(r.now), r.wall(r.now)).unwrap();
        assert!(trace.trace_incomplete);
        assert_eq!(trace.clock_steps_back, 1);
        assert_eq!(
            trace.transitions.last().map(|t| (t.at_ms, t.reason)),
            Some((65_001, Activity))
        );
        assert_eq!(r.used(59_800, 66_000, 1), Activity);
        let trace = r.registry.trace(3, r.at(r.now), r.wall(r.now)).unwrap();
        assert_eq!(trace.clock_steps_back, 1, "{trace:#?}");
    }

    #[test]
    fn evidence_ages_and_counts_come_from_the_summary() {
        let mut d = RecordingDiagnostics::new(0);
        let mut summary = EvidenceSummary::new(1, 10_000);
        let seen = |t, outcome| Observation {
            captured_ms: t,
            received_ms: t,
            qualifying_observed: 1,
            predicted_only: 0,
            outcome,
        };
        summary.reduce(1, 1, seen(1_000, AnalysisOutcome::Success));
        let counts = EvidenceCounts {
            observed: 1,
            predicted: 2,
            stationary: 3,
            excluded: 4,
        };
        d.observe(AnalysisOutcome::Success, counts, &summary);
        summary.reduce(1, 2, seen(2_000, AnalysisOutcome::Failed));
        d.observe(AnalysisOutcome::Failed, EvidenceCounts::default(), &summary);
        d.transition(2_000, W, false, true, DegradedCoverage);
        let s = d.snapshot(5_000);
        assert_eq!(s.last_success_age_ms, Some(4_000));
        assert_eq!(s.last_qualifying_age_ms, Some(4_000));
        assert_eq!(s.last_failure_age_ms, Some(3_000));
        assert_eq!(s.counts, counts, "a failed pass has no objects to count");
        let m = d.take_minute(5_000);
        assert_eq!((m.succeeded, m.failed, m.qualifying_frames), (1, 1, 1));
    }
}

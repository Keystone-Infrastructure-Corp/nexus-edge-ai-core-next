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
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::recording_activity::{
    is_observed, AnalysisOutcome, EvidenceSummary, Observation, RecordingActivityPolicy,
    RecordingIntent, RecordingReason,
};

/// Transition summaries kept per camera. Dropping an older one marks the
/// trace incomplete; the aggregate counters stay exact regardless.
pub const TRANSITION_RING: usize = 256;

/// Width of a persisted counter bucket.
const MINUTE_MS: u64 = 60_000;

/// One change of the legacy recorder's state or of the shadow intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Transition {
    pub at_ms: u64,
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
            collector_error: None,
            minute: MinuteCounters::default(),
            counts: EvidenceCounts::default(),
            last_success_ms: None,
            last_qualifying_ms: None,
            last_failure_ms: None,
        }
    }

    /// The state at `at_ms`. Time since the previous call is credited to
    /// the previous state; an unchanged state records no transition.
    pub fn transition(
        &mut self,
        at_ms: u64,
        legacy_open: bool,
        shadow_open: bool,
        reason: RecordingReason,
    ) {
        if at_ms < self.credited_ms {
            // Never credit time twice: hold the clock and say the trace has a gap.
            self.trace_incomplete = true;
        }
        let at_ms = at_ms.max(self.credited_ms);
        self.credit(at_ms);
        let next = Transition {
            at_ms,
            legacy_open,
            shadow_open,
            reason,
        };
        if self
            .state
            .is_some_and(|s| Transition { at_ms, ..s } == next)
        {
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
    /// The mode running; legacy has no runtime.
    pub mode: RecordingPolicyMode,
    /// The supervisor generation running it.
    pub generation: u64,
    /// Content revision of the policy running.
    pub policy_revision: String,
    /// The open motion clip of whichever side writes: legacy, or observed's policy.
    pub active_clip_id: Option<ClipId>,
    #[serde(flatten)]
    pub diagnostics: DiagnosticsSnapshot,
}

/// Each camera running a non-legacy policy. Entries are weak: a supervisor
/// that ends, however it ends, takes its entry with it.
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
            diagnostics: s.diagnostics.snapshot(now_ms),
        })
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
    /// The latest frame's clock, wall clock and shadow reason.
    last: Option<(u64, DateTime<Utc>, RecordingReason)>,
    shared: Arc<Mutex<ShadowState>>,
}

impl ShadowRecording {
    /// `None` for legacy, and for a tracker with no observed/predicted
    /// contract. `observed` runs as shadow, so legacy still writes the
    /// footage: nothing activates observed recording yet (SPEC-089 Task 4).
    pub fn start(
        registry: &RecordingActivityRegistry,
        camera_id: CameraId,
        generation: u64,
        policy: Option<RecordingPolicyConfig>,
        tracker: &str,
        post_roll_secs: u32,
        pre_roll_secs: u32,
    ) -> Option<Self> {
        let policy = policy.filter(|p| p.mode != RecordingPolicyMode::Legacy)?;
        if policy.mode == RecordingPolicyMode::Observed {
            warn!(
                camera_id,
                "recording_policy observed cannot be activated yet; running shadow, legacy records"
            );
        }
        Self::start_as(
            registry,
            camera_id,
            generation,
            policy,
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
                policy_revision: format!("sha256:{:x}", Sha256::digest(to_json(&policy))),
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
            Some((outcome, counts)),
            &summary,
            alert,
        );
        self.record(captured_mono, captured_at, legacy_clip, false)
    }

    /// Apply a producer's coalesced `summary`, reduced on a clock that
    /// starts at `origin`, as of the frame captured at `captured_mono`.
    /// `analysis` is that frame's own pass, counted by [`Self::record`].
    pub fn evidence(
        &mut self,
        origin: Instant,
        captured_mono: Instant,
        analysis: Option<(AnalysisOutcome, EvidenceCounts)>,
        summary: &EvidenceSummary,
        alert: bool,
    ) -> RecordingIntent {
        let origin = *self.shared.lock().origin.get_or_insert(origin);
        let at = millis(captured_mono, origin);
        let policy = self.policy.get_or_insert_with(|| {
            RecordingActivityPolicy::new(self.post_roll_ms, self.stale_after_ms, at)
        });
        if alert {
            policy.alert(at);
        }
        policy.apply_summary(summary, at);
        let intent = policy.tick(at);
        self.intent = Some(intent);
        if let Some((outcome, counts)) = analysis {
            self.pass = Some((outcome, counts, *summary));
        }
        intent
    }

    /// An explicit alert captured at `captured_mono`.
    pub fn alert(&mut self, captured_mono: Instant) -> RecordingIntent {
        let origin = *self.shared.lock().origin.get_or_insert(captured_mono);
        let at = millis(captured_mono, origin);
        let policy = self.policy.get_or_insert_with(|| {
            RecordingActivityPolicy::new(self.post_roll_ms, self.stale_after_ms, at)
        });
        policy.alert(at);
        let intent = policy.tick(at);
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

    /// Trace the state once the writer acted on the latest answer:
    /// `writer_clip` is the open clip, `held` that it is open only for the
    /// minimum duration. Returns the summaries this finished.
    pub fn record(
        &mut self,
        captured_mono: Instant,
        captured_at: DateTime<Utc>,
        writer_clip: Option<ClipId>,
        held: bool,
    ) -> ShadowWrites {
        let mut writes = ShadowWrites::default();
        let Some(intent) = self.intent else {
            return writes;
        };
        let reason = if held {
            RecordingReason::MinimumDuration
        } else {
            intent.reason
        };
        let mut shared = self.shared.lock();
        let origin = *shared.origin.get_or_insert(captured_mono);
        let at = millis(captured_mono, origin);
        let index = at / MINUTE_MS;
        match self.minute {
            // Forward only: a minute index is written once.
            Some((current, started)) if index > current => {
                // Closed at its own end; this frame counts in the next one.
                let counters = shared.diagnostics.take_minute((current + 1) * MINUTE_MS);
                writes.minute = Some(minute_row(
                    self.camera_id,
                    self.generation,
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
        if let Some((outcome, counts, summary)) = self.pass.take() {
            d.observe(outcome, counts, &summary);
        }
        d.transition(at, writer_clip.is_some(), intent.open, reason);
        self.last = Some((at, captured_at, reason));

        if self.episode.as_ref().map(|e| e.clip_id) != writer_clip {
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
        d.transition(at, false, reason != RecordingReason::Quiet, reason);
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
            minute_row(self.camera_id, self.generation, index, started, &minute)
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
    minute: u64,
    started: DateTime<Utc>,
    counters: &MinuteCounters,
) -> RecordingActivityMinute {
    RecordingActivityMinute {
        camera_id,
        generation: generation as i64,
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

    #[test]
    fn more_than_the_ring_keeps_exact_totals_and_reports_the_gap() {
        let mut d = RecordingDiagnostics::new(0);
        // 300 one-second states alternating shadow open and closed, legacy closed.
        for k in 0..300u64 {
            let open = k % 2 == 0;
            d.transition(k * 1_000, false, open, if open { Activity } else { Quiet });
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
            d.transition(k, k % 2 == 0, false, Quiet);
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
        d.transition(0, true, false, Quiet);
        d.transition(5_000, true, true, Activity);
        d.transition(10_000, false, true, PostRoll);
        d.transition(15_000, false, false, Quiet);
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
        d.transition(5_000, false, true, Activity);
        d.transition(9_000, false, false, Quiet);
        d.transition(10_000, false, true, Activity);
        d.transition(15_000, false, false, Quiet);
        let s = d.snapshot(30_000);
        assert_eq!(s.shadow_recorded_ms, 15_000);
        assert_eq!(s.elapsed.legacy_closed.activity, 9_000);
        // The open one counts to now, and pre-roll before the first frame counts.
        d.transition(40_000, true, false, Quiet);
        assert_eq!(d.snapshot(42_000).legacy_recorded_ms, 7_000);
        let mut d = RecordingDiagnostics::new(5_000);
        d.transition(0, true, false, Quiet);
        assert_eq!(d.snapshot(1_000).legacy_recorded_ms, 6_000);
    }

    #[test]
    fn a_clock_step_back_is_a_gap_and_never_double_counts() {
        let mut d = RecordingDiagnostics::new(0);
        d.transition(10_000, true, true, Activity);
        d.transition(4_000, true, false, Quiet);
        let s = d.snapshot(12_000);
        assert!(s.trace_incomplete);
        assert_eq!(s.elapsed.legacy_open.activity, 0);
        assert_eq!(s.elapsed.legacy_open.quiet, 2_000);
    }

    #[test]
    fn a_collector_failure_is_reported() {
        let mut d = RecordingDiagnostics::new(0);
        d.transition(0, false, true, DegradedCoverage);
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
        d.transition(0, true, true, Activity);
        d.transition(50_000, true, false, Quiet);
        let first = d.take_minute(60_000);
        assert_eq!(first.elapsed.legacy_open.activity, 50_000);
        assert_eq!(first.elapsed.legacy_open.quiet, 10_000);
        assert_eq!(first.transitions, 2);
        d.transition(70_000, false, false, Quiet);
        let second = d.take_minute(120_000);
        assert_eq!(second.elapsed.legacy_open.quiet, 10_000);
        assert_eq!(second.elapsed.legacy_closed.quiet, 50_000);
        assert_eq!(second.transitions, 1);
        assert!(!second.trace_incomplete);
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
        let start =
            |policy, tracker| ShadowRecording::start(&registry, 3, 9, policy, tracker, 2, 0);
        assert!(start(None, "bytetrack").is_none());
        assert!(start(mode(RecordingPolicyMode::Legacy), "bytetrack").is_none());
        assert!(
            start(mode(RecordingPolicyMode::Shadow), "kalman").is_none(),
            "a tracker without an observed/predicted contract"
        );
        assert!(registry.snapshot(3, Instant::now()).is_none());
        let shadow = start(mode(RecordingPolicyMode::Shadow), "iou-naive").unwrap();
        let runtime = registry.snapshot(3, Instant::now()).unwrap();
        assert_eq!((runtime.generation, runtime.diagnostics.reason), (9, None));
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
            ShadowRecording::start(&registry, 3, 9, Some(policy), "iou-naive", 2, 0).unwrap();
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
        d.transition(2_000, false, true, DegradedCoverage);
        let s = d.snapshot(5_000);
        assert_eq!(s.last_success_age_ms, Some(4_000));
        assert_eq!(s.last_qualifying_age_ms, Some(4_000));
        assert_eq!(s.last_failure_age_ms, Some(3_000));
        assert_eq!(s.counts, counts, "a failed pass has no objects to count");
        let m = d.take_minute(5_000);
        assert_eq!((m.succeeded, m.failed, m.qualifying_frames), (1, 1, 1));
    }
}

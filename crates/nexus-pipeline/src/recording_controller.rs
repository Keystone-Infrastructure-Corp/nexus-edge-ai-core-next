//! SPEC-089 Task 3: the one task per camera that opens, closes, rotates and
//! retries its motion clips.
//!
//! The supervisor's analysis loop hands it each analysed frame's evidence,
//! then that frame's alerts, over a bounded acknowledged channel, and it
//! decides on the frame's own clock exactly as legacy recording did inside
//! the loop. Between frames it keeps monotonic deadlines of its own, so a
//! detector that never answers no longer stops the five-minute file
//! deadline, the byte cap, a stream-epoch rotation, the post-roll close or a
//! stop (R-4). It owns the clip until the recorder is done with it: a slow
//! open or close is reported and waited for, never raced by a second writer,
//! and the clip of an aborted supervisor is finalized here once its camera's
//! stop is requested (R-5).
//!
//! Legacy is the default and decides as before. Shadow measures the
//! fresh-evidence policy beside it. Observed, an operator's explicit
//! per-camera choice, lets that policy write; a live change back to legacy
//! keeps the open clip and its links.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use nexus_config::{RecordingPolicyConfig, RecordingPolicyMode};
use nexus_store::{MotionEventKind, NewMotionEvent, Store};
use nexus_tracker::{MotionDecision, MotionKind};
use nexus_types::CameraId;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::post_roll::{PostRoll, PostRollAction};
use crate::recorder::{
    ClipFinal, ClipHandle, ClipRecorder, OpenClip, RecorderError, MAX_CLIP_DURATION_MS,
    MIN_CLIP_DURATION_MS,
};
use crate::recording_activity::{
    AnalysisOutcome, EvidenceSummary, Observation, RecordingIntent, RecordingReason,
};
use crate::recording_diagnostics::{millis, EvidenceCounts, ShadowRecording, ShadowWrites};
use crate::stats::FrameStatsRegistry;

/// Recording decisions moved here from the camera supervisor; their log
/// lines keep its target so existing filters and alerting still see them.
const LOG_TARGET: &str = "nexus_pipeline::supervisor";

/// Commands a controller holds before a sender waits.
pub const COMMAND_CAPACITY: usize = 32;

/// How long a requester waits for a receipt. Above the recorder's own worst
/// close (2 s for the live pump plus a 5 s EOS drain), so a normal close is
/// waited for and only a wedged one reads as a timeout.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// A recorder open or close still running after this is reported. The
/// controller keeps waiting for it: the clip stays its own.
const RECORDER_SLOW: Duration = COMMAND_TIMEOUT;

/// How often, in analysed frames, the open clip's size is checked against
/// `max_clip_bytes`. A runaway stream takes many frames to balloon, so the
/// check is sampled; the cold-upload quarantine ceiling
/// (`max_cold_upload_bytes = max_clip_bytes * 2`) absorbs the overshoot.
const SIZE_STAT_INTERVAL_FRAMES: u32 = 60;

/// How often the controller acts on its own clock once no frame has
/// arrived for [`RECORDER_STALL`].
const STALL_TICK: Duration = Duration::from_secs(1);

/// The stale deadline when no policy names one (legacy).
const DEFAULT_STALE_AFTER_MS: u64 = 10_000;

/// How long without a frame before the writer acts on its own clock:
/// legacy's stale deadline whichever policy runs, so no policy parameter
/// moves file safety or legacy's decisions (R-6).
const RECORDER_STALL: Duration = Duration::from_millis(DEFAULT_STALE_AFTER_MS);

/// First open retry delay; doubles per failure up to [`OPEN_RETRY_MAX`].
const OPEN_RETRY_FIRST: Duration = Duration::from_millis(500);
const OPEN_RETRY_MAX: Duration = Duration::from_secs(30);

/// Before the policy has seen a frame there is no evidence of anything.
const UNVERIFIED: RecordingIntent = RecordingIntent {
    open: true,
    reason: RecordingReason::DegradedCoverage,
};

/// What a camera's controller is started with.
#[derive(Clone, Debug)]
pub struct ControllerConfig {
    pub camera_id: CameraId,
    /// The supervisor generation it serves; a command for another is refused.
    pub generation: u64,
    pub policy: Option<RecordingPolicyConfig>,
    /// The camera's saved `recording_policy_revision` of `policy`, which its
    /// stats report as applied.
    pub policy_revision: String,
    /// `nexus_tracker`'s name for the camera's tracker.
    pub tracker: String,
    pub post_roll_secs: u32,
    pub pre_roll_secs: u32,
    /// Byte cap on the open clip; `0` disables it.
    pub max_clip_bytes: u64,
    pub record_motion_clip_on_alert: bool,
    /// The analysis frame size new clips record their boxes against.
    pub frame_width: u32,
    pub frame_height: u32,
}

/// One analysed frame.
#[derive(Clone, Debug)]
pub struct Evidence {
    /// The frame's analysis sequence: `summary` is reduced through it.
    pub sequence: u64,
    pub captured_mono: Instant,
    pub captured_at: DateTime<Utc>,
    /// Origin of the summary's millisecond clock.
    pub origin: Instant,
    /// Every analysis of the generation so far, coalesced.
    pub summary: EvidenceSummary,
    /// This frame's own pass. `None` is the frame arriving, before its
    /// analysis: its file deadlines are checked then, as legacy's loop did.
    pub analysis: Option<(AnalysisOutcome, EvidenceCounts)>,
    /// What the motion lifecycle made of it; `None` when the frame never
    /// reached it (a failed detect, a source rebuild).
    pub motion: Option<Motion>,
}

/// The analysis side's half: every analysis of a generation reduced, in
/// order, into the coalesced summary each [`Evidence`] carries, so evidence
/// that never reaches the controller still counts in the next.
pub struct EvidenceProducer {
    generation: u64,
    origin: Option<Instant>,
    sequence: u64,
    summary: EvidenceSummary,
}

impl EvidenceProducer {
    pub fn new(generation: u64, policy: Option<RecordingPolicyConfig>) -> Self {
        // shortcut: the stale deadline of the policy the camera started with; a live policy change keeps it until the next restart, which only shifts when a long gap resets a success run.
        let stale_after_ms = u64::try_from(stale_after(policy).as_millis()).unwrap_or(u64::MAX);
        Self {
            generation,
            origin: None,
            sequence: 0,
            summary: EvidenceSummary::new(generation, stale_after_ms),
        }
    }

    /// Reduce the analysis of the frame captured at `captured_mono` and
    /// return its evidence.
    pub fn analysed(
        &mut self,
        captured_mono: Instant,
        captured_at: DateTime<Utc>,
        outcome: AnalysisOutcome,
        counts: EvidenceCounts,
        motion: Option<Motion>,
    ) -> Evidence {
        let origin = *self.origin.get_or_insert(captured_mono);
        let at = millis(captured_mono, origin);
        self.sequence += 1;
        self.summary.reduce(
            self.generation,
            self.sequence,
            Observation {
                captured_ms: at,
                // Analysed on the supervisor's loop as it arrived: nothing to reorder.
                received_ms: at,
                qualifying_observed: counts.observed,
                predicted_only: counts.predicted,
                outcome,
            },
        );
        self.evidence(captured_mono, captured_at, Some((outcome, counts)), motion)
    }

    /// Evidence that the frame captured at `captured_mono` arrived, before
    /// its analysis: the file deadlines run on its clock first.
    pub fn arrived(&self, captured_mono: Instant, captured_at: DateTime<Utc>) -> Evidence {
        self.evidence(captured_mono, captured_at, None, None)
    }

    fn evidence(
        &self,
        captured_mono: Instant,
        captured_at: DateTime<Utc>,
        analysis: Option<(AnalysisOutcome, EvidenceCounts)>,
        motion: Option<Motion>,
    ) -> Evidence {
        Evidence {
            sequence: self.sequence,
            captured_mono,
            captured_at,
            origin: self.origin.unwrap_or(captured_mono),
            summary: self.summary,
            analysis,
            motion,
        }
    }
}

/// The motion lifecycle's view of one frame, as legacy recording reads it.
#[derive(Clone, Debug, Default)]
pub struct Motion {
    /// The emitter still holds a live track.
    pub live: bool,
    pub born: bool,
    /// Alert events this frame recorded; an [`RecordingCommand::Alert`]
    /// naming them follows this evidence.
    pub alerts: usize,
    /// The frame's `motion_events` rows, stamped with the clip it leaves open.
    pub decisions: Vec<MotionDecision>,
}

/// Alert events recorded on a frame whose evidence came first.
#[derive(Clone, Debug)]
pub struct Alert {
    pub event_ids: Vec<String>,
    pub captured_mono: Instant,
    pub captured_at: DateTime<Utc>,
}

/// The supervisor rebuilt its frame source at a new analysis size.
#[derive(Clone, Copy, Debug)]
pub struct EpochChange {
    pub captured_at: DateTime<Utc>,
    pub frame_width: u32,
    pub frame_height: u32,
}

pub enum RecordingCommand {
    Evidence(Evidence),
    Alert(Alert),
    StreamEpochChanged(EpochChange),
    /// The camera's `recording_policy`, or its saved revision, changed:
    /// switch without a restart.
    PolicyChanged(Option<RecordingPolicyConfig>, String),
    /// Finalize the open clip, reply, and end.
    Stop,
}

/// A command for one generation and where its receipt goes.
pub struct Request {
    pub generation: u64,
    pub command: RecordingCommand,
    pub reply: oneshot::Sender<RecordingReceipt>,
}

/// What a command left: the clip open now, and why there is none if a
/// failure is the reason.
#[derive(Clone, Debug)]
pub struct RecordingReceipt {
    pub generation: u64,
    pub clip: Option<ClipHandle>,
    pub error: Option<RecordingError>,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum RecordingError {
    #[error("command for generation {sent}, but generation {running} answered")]
    StaleGeneration { sent: u64, running: u64 },
    #[error("recorder refused the clip (storage panic)")]
    Refused,
    #[error("recorder could not open the clip: {0}")]
    OpenFailed(String),
    #[error("recording controller has stopped")]
    Stopped,
    #[error("recording controller did not answer within {0:?}")]
    Timeout(Duration),
}

/// Sends commands to one camera's controller.
#[derive(Clone)]
pub struct RecordingControl {
    pub tx: mpsc::Sender<Request>,
    pub generation: u64,
}

impl RecordingControl {
    /// Send `command` and wait for its receipt, all within `timeout`. A
    /// receipt from another generation is refused.
    pub async fn request(
        &self,
        command: RecordingCommand,
        timeout: Duration,
    ) -> Result<RecordingReceipt, RecordingError> {
        let (reply, receipt) = oneshot::channel();
        let request = Request {
            generation: self.generation,
            command,
            reply,
        };
        let receipt = tokio::time::timeout(timeout, async {
            self.tx
                .send(request)
                .await
                .map_err(|_| RecordingError::Stopped)?;
            receipt.await.map_err(|_| RecordingError::Stopped)
        })
        .await
        .map_err(|_| RecordingError::Timeout(timeout))??;
        if receipt.generation != self.generation {
            return Err(RecordingError::StaleGeneration {
                sent: self.generation,
                running: receipt.generation,
            });
        }
        Ok(receipt)
    }
}

/// Held by whoever drives the camera. Dropping it, however that happens
/// (a return, a panic, an abort), tells the controller to finalize.
pub struct ControllerGuard {
    _alive: watch::Sender<()>,
}

/// Start a camera's controller. `stop` is the camera's stop request, kept
/// until the controller has finalized its clip, so whoever awaits its
/// sender's `closed()` also waits for that. A controller whose guard drops
/// without a [`RecordingCommand::Stop`] (its supervisor was aborted or
/// panicked) makes no further decision and finalizes once that stop is
/// requested or its sender dropped: the reconciler requests it before it
/// aborts, and a bare abort at process exit leaves the clip to the recorder's
/// shutdown drain, as before.
pub fn spawn(
    cfg: ControllerConfig,
    recorder: Arc<dyn ClipRecorder>,
    store: Arc<Store>,
    stats: Arc<FrameStatsRegistry>,
    stop: watch::Receiver<bool>,
) -> (RecordingControl, ControllerGuard) {
    let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
    let (guard, alive) = watch::channel(());
    let control = RecordingControl {
        tx,
        generation: cfg.generation,
    };
    let mut controller = Controller {
        post_roll: PostRoll::new(cfg.post_roll_secs),
        frame_dims: (cfg.frame_width, cfg.frame_height),
        cfg,
        recorder,
        store,
        stats,
        alive,
        cancelled: false,
        shadow: None,
        observed: false,
        want_open: false,
        clip: None,
        clip_opened_at: None,
        frames_since_size_stat: 0,
        hold: None,
        last_open_error: None,
        pending_open: false,
        open_retry_at: None,
        open_retry_delay: OPEN_RETRY_FIRST,
        open_panic_refused: false,
        open_error_logged: false,
        live: false,
        deferred_close: None,
        last_frame: None,
        stall_check: None,
        last_sequence: 0,
    };
    controller.start_policy();
    tokio::spawn(controller.run(rx, stop));
    (control, ControllerGuard { _alive: guard })
}

fn stale_after(policy: Option<RecordingPolicyConfig>) -> Duration {
    Duration::from_millis(policy.map_or(DEFAULT_STALE_AFTER_MS, |p| p.analysis_stale_after_ms))
}

/// The last frame's capture clock and when it arrived here.
#[derive(Clone, Copy)]
struct FrameClock {
    mono: Instant,
    wall: DateTime<Utc>,
    arrived: tokio::time::Instant,
}

// One per loop turn, on the stack: boxing the request would allocate per command.
#[allow(clippy::large_enum_variant)]
enum Wake {
    Cancelled,
    Request(Option<Request>),
    Deadline,
}

/// Holding a short clip for the minimum duration: its media and the time
/// when the hold began, and when it could next have enough.
#[derive(Clone, Copy)]
struct Hold {
    media_ms: u64,
    since: Instant,
    recheck: Instant,
}

struct Controller {
    cfg: ControllerConfig,
    recorder: Arc<dyn ClipRecorder>,
    store: Arc<Store>,
    stats: Arc<FrameStatsRegistry>,
    alive: watch::Receiver<()>,
    cancelled: bool,
    /// The fresh-evidence policy and its diagnostics; `None` in legacy.
    shadow: Option<ShadowRecording>,
    /// The policy writes; legacy only traces.
    observed: bool,
    /// Observed's latest intent.
    want_open: bool,

    clip: Option<ClipHandle>,
    clip_opened_at: Option<Instant>,
    frames_since_size_stat: u32,
    frame_dims: (u32, u32),
    hold: Option<Hold>,
    last_open_error: Option<RecordingError>,

    // Legacy's decision state, as the supervisor's loop kept it.
    post_roll: PostRoll,
    /// Failed opens and rotations retain demand independently of motion sampling.
    pending_open: bool,
    open_retry_at: Option<Instant>,
    open_retry_delay: Duration,
    open_panic_refused: bool,
    open_error_logged: bool,
    /// The last frame's live motion, which a stalled analysis leaves standing.
    live: bool,
    /// A post-roll close waiting for its frame's alerts to be linked first.
    deferred_close: Option<DateTime<Utc>>,

    last_frame: Option<FrameClock>,
    /// When the controller next acts on its own clock for a silent analysis.
    stall_check: Option<tokio::time::Instant>,
    last_sequence: u64,
}

impl Controller {
    async fn run(mut self, mut rx: mpsc::Receiver<Request>, mut stop: watch::Receiver<bool>) {
        loop {
            let wait = self.next_wait();
            let wake = tokio::select! {
                biased;
                _ = self.alive.changed(), if !self.cancelled => Wake::Cancelled,
                request = rx.recv() => Wake::Request(request),
                () = sleep(wait) => Wake::Deadline,
            };
            match wake {
                Wake::Cancelled => self.cancelled = true,
                // Every sender is gone, the supervisor's included.
                Wake::Request(None) => break,
                Wake::Request(Some(request)) => {
                    if self.request(request).await {
                        break;
                    }
                }
                Wake::Deadline => self.wake().await,
            }
            if self.cancelled {
                break;
            }
        }
        if self.cancelled {
            // Err: the camera's handle is gone, and nothing else will ask.
            let _ = stop.wait_for(|requested| *requested).await;
        }
        // Exactly once, whichever way the loop ended; a Stop already did.
        self.finalize().await;
        rx.close();
        while let Ok(request) = rx.try_recv() {
            let _ = request
                .reply
                .send(self.receipt(Some(RecordingError::Stopped)));
        }
    }

    /// Apply one request and acknowledge it. `true` once it was a stop.
    async fn request(&mut self, request: Request) -> bool {
        if request.generation != self.cfg.generation {
            let error = RecordingError::StaleGeneration {
                sent: request.generation,
                running: self.cfg.generation,
            };
            let _ = request.reply.send(self.receipt(Some(error)));
            return false;
        }
        let mut stop = false;
        let error = match request.command {
            RecordingCommand::Evidence(evidence) => self.evidence(evidence).await,
            RecordingCommand::Alert(alert) => self.alert(alert).await,
            RecordingCommand::StreamEpochChanged(change) => {
                self.epoch_changed(change).await;
                None
            }
            RecordingCommand::PolicyChanged(policy, revision) => {
                self.policy_changed(policy, revision).await;
                None
            }
            RecordingCommand::Stop => {
                self.finalize().await;
                stop = true;
                None
            }
        };
        let error = if self.cancelled {
            Some(RecordingError::Stopped)
        } else {
            error
        };
        let _ = request.reply.send(self.receipt(error));
        stop
    }

    fn receipt(&self, error: Option<RecordingError>) -> RecordingReceipt {
        RecordingReceipt {
            generation: self.cfg.generation,
            clip: self.clip,
            error,
        }
    }

    /// The capture clock now: the last frame's, run on by the time since it
    /// arrived, so decisions between frames continue its timeline.
    fn now(&self) -> (Instant, DateTime<Utc>) {
        match self.last_frame {
            Some(f) => {
                let dt = f.arrived.elapsed();
                (
                    f.mono + dt,
                    f.wall + chrono::Duration::from_std(dt).unwrap_or_default(),
                )
            }
            None => (Instant::now(), Utc::now()),
        }
    }

    fn next_wait(&self) -> Option<Duration> {
        let (now, _) = self.now();
        let until = |t: Instant| t.saturating_duration_since(now);
        let mut waits = Vec::new();
        if self.writing() {
            let stall = self
                .stall_check
                .or_else(|| self.last_frame.map(|f| f.arrived + RECORDER_STALL));
            waits.push(stall.map_or(Duration::ZERO, |t| {
                t.saturating_duration_since(tokio::time::Instant::now())
            }));
        }
        if self.deferred_close.is_some() {
            // Its frame's alerts link to the clip before anything else closes it.
            return waits.into_iter().min();
        }
        if let Some(t) = self.file_deadline() {
            waits.push(until(t));
        }
        // The policy's own deadlines and minute buckets, in shadow as in observed.
        if let Some(shadow) = &self.shadow {
            waits.extend(
                [shadow.next_change(now), shadow.next_minute()]
                    .into_iter()
                    .flatten()
                    .map(until),
            );
        }
        if self.observed {
            if let Some(hold) = self.hold {
                waits.push(until(hold.recheck));
            }
            if self.clip.is_none() && self.want_open {
                // A storage panic clears on its own clock: look again each tick.
                waits.push(if self.open_panic_refused {
                    STALL_TICK
                } else {
                    self.open_retry_at.map_or(Duration::ZERO, until)
                });
            }
        }
        waits.into_iter().min()
    }

    /// Whether the writer has a timer of its own: a clip open, opening or
    /// closing, or observed's policy deciding.
    fn writing(&self) -> bool {
        self.clip.is_some() || self.pending_open || self.deferred_close.is_some() || self.observed
    }

    /// M2.1: when the open clip reaches the five-minute cap, on the capture clock.
    fn file_deadline(&self) -> Option<Instant> {
        self.clip
            .and(self.clip_opened_at)
            .map(|at| at + Duration::from_millis(MAX_CLIP_DURATION_MS as u64))
    }

    /// Act on the controller's own clock: no frame needed.
    async fn wake(&mut self) {
        let (now, wall) = self.now();
        let stalled = self.writing()
            && self
                .last_frame
                .is_none_or(|f| f.arrived.elapsed() >= RECORDER_STALL);
        if stalled {
            self.flush_deferred_close().await;
            self.stall_check = Some(tokio::time::Instant::now() + STALL_TICK);
        }
        let due = stalled || self.file_deadline().is_some_and(|t| now >= t);
        let rotated = due && self.rotate_if_due(now, wall, stalled).await;
        if self.observed {
            let intent = self.intent_now(now);
            self.drive(intent, now, wall).await;
        } else if stalled {
            // Legacy on the last frame's liveness: the deadlines a frame would have met.
            let motion = Motion {
                live: self.live,
                ..Motion::default()
            };
            let (close_now, _) = self.legacy(now, wall, &motion).await;
            if close_now {
                self.close(wall, "post-roll").await;
            }
        } else if rotated && self.may_open(now) {
            // Rotation changes files, not the motion lifetime: the next one opens now.
            self.open(now, wall).await;
        }
        if !self.observed {
            if let Some(shadow) = self.shadow.as_mut() {
                shadow.tick(now);
            }
        }
        self.trace(now, wall).await;
    }

    async fn evidence(&mut self, e: Evidence) -> Option<RecordingError> {
        self.flush_deferred_close().await;
        if self.last_sequence > 0 && e.sequence > self.last_sequence + 1 {
            if let Some(shadow) = &self.shadow {
                shadow.collector_failed(format!(
                    "evidence {} to {} never reached the recording controller",
                    self.last_sequence + 1,
                    e.sequence - 1
                ));
            }
        }
        self.last_sequence = self.last_sequence.max(e.sequence);
        // The capture clock never runs back: a frame that waited longer for
        // the analysis loop was captured before the time it already shows.
        let (mono, wall) = self.now();
        let arrived = tokio::time::Instant::now();
        self.last_frame = Some(match self.last_frame {
            Some(_) if mono >= e.captured_mono => FrameClock {
                mono,
                wall,
                arrived,
            },
            _ => FrameClock {
                mono: e.captured_mono,
                wall: e.captured_at,
                arrived,
            },
        });
        let (now, wall) = self.now();
        self.stall_check = None;

        let mut error = None;
        let mut close_now = false;
        if e.analysis.is_none() {
            // The frame arrived: its file deadlines first, as legacy's loop did.
            self.rotate_if_due(e.captured_mono, e.captured_at, false)
                .await;
        } else if let (Some(motion), false) = (&e.motion, self.observed) {
            (close_now, error) = self.legacy(e.captured_mono, e.captured_at, motion).await;
        }
        let alerts = e.motion.as_ref().map_or(0, |m| m.alerts);
        // Arrival alone starts the policy: degraded coverage counts from healthy ingest.
        if let Some(shadow) = self.shadow.as_mut() {
            let alert = !self.observed && self.cfg.record_motion_clip_on_alert && alerts > 0;
            let intent = shadow.evidence(
                e.origin,
                e.captured_mono,
                now,
                e.analysis,
                &e.summary,
                alert,
            );
            if self.observed {
                error = self.drive(intent, e.captured_mono, e.captured_at).await;
            }
        }
        if let Some(motion) = &e.motion {
            self.stamp(&motion.decisions).await;
        }
        // Legacy closes once the frame's rows are in, and after its alerts link.
        if close_now {
            if alerts > 0 {
                self.deferred_close = Some(e.captured_at);
            } else {
                self.close(e.captured_at, "post-roll").await;
            }
        }
        self.trace(now, wall).await;
        error
    }

    /// Legacy's per-frame decision, as the supervisor's loop made it: live
    /// tracks and alerts keep the clip, post-roll ends it. Returns whether
    /// post-roll closes it now.
    async fn legacy(
        &mut self,
        now: Instant,
        wall: DateTime<Utc>,
        motion: &Motion,
    ) -> (bool, Option<RecordingError>) {
        let alert_kept_alive = self.cfg.record_motion_clip_on_alert && motion.alerts > 0;
        self.live = motion.live;
        let action = self.post_roll.tick(now, motion.live || alert_kept_alive);
        let close_now = matches!(action, PostRollAction::CloseNow);
        if close_now {
            self.pending_open = false;
            self.reset_retry();
        }
        // Recorder continuity cannot depend on whether Updated rows are due.
        let should_open = self.clip.is_none()
            && self.may_open(now)
            && (self.pending_open || alert_kept_alive || motion.born);
        let error = if should_open {
            self.open(now, wall).await
        } else {
            None
        };
        (close_now, error)
    }

    fn reset_retry(&mut self) {
        self.open_retry_at = None;
        self.open_retry_delay = OPEN_RETRY_FIRST;
        self.open_panic_refused = false;
        self.open_error_logged = false;
    }

    /// A refused open waits for the storage panic to clear; a failed one backs off.
    fn may_open(&self, now: Instant) -> bool {
        (!self.open_panic_refused || !self.recorder.is_panic())
            && (self.open_panic_refused || self.open_retry_at.is_none_or(|at| now >= at))
    }

    /// Observed: the policy's intent decides; a new clip is held until it has
    /// the recorder's minimum media.
    async fn drive(
        &mut self,
        intent: RecordingIntent,
        now: Instant,
        wall: DateTime<Utc>,
    ) -> Option<RecordingError> {
        self.want_open = intent.open;
        if intent.open {
            self.hold = None;
            if self.clip.is_none() && self.may_open(now) {
                return self.open(now, wall).await;
            }
            return None;
        }
        self.pending_open = false;
        self.reset_retry();
        let handle = self.clip?;
        let media_ms = self.media_ms(handle, now);
        let short_by = (MIN_CLIP_DURATION_MS as u64).saturating_sub(media_ms);
        if short_by > 0 && !self.recorder.ended_on_epoch_change(handle) {
            let hold = self.hold.get_or_insert(Hold {
                media_ms,
                since: now,
                recheck: now,
            });
            // Media that stopped growing for a whole minimum duration is a lost source.
            let lost = media_ms <= hold.media_ms && now >= hold.since + min_clip();
            if !lost {
                hold.recheck = now + Duration::from_millis(short_by);
                return None;
            }
            warn!(
                target: LOG_TARGET,
                camera_id = self.cfg.camera_id,
                clip_id = handle.clip_id,
                media_ms,
                "recording source lost before the minimum clip duration; closing partial media"
            );
        }
        if let Some(meta) = self.close(wall, "activity ended").await {
            if meta.discarded {
                warn!(
                    target: LOG_TARGET,
                    camera_id = self.cfg.camera_id,
                    clip_id = handle.clip_id,
                    duration_ms = meta.duration_ms,
                    "observed clip had no usable media; footage for it is unavailable"
                );
            }
        }
        None
    }

    fn media_ms(&self, handle: ClipHandle, now: Instant) -> u64 {
        self.recorder.inflight_media_ms(handle).unwrap_or_else(|| {
            self.clip_opened_at.map_or(0, |at| {
                u64::try_from(now.saturating_duration_since(at).as_millis()).unwrap_or(u64::MAX)
            })
        })
    }

    fn intent_now(&mut self, now: Instant) -> RecordingIntent {
        self.shadow
            .as_mut()
            .and_then(|s| s.tick(now))
            .unwrap_or(UNVERIFIED)
    }

    async fn alert(&mut self, alert: Alert) -> Option<RecordingError> {
        let mut error = None;
        // Without alert recording an alert only links: it is not activity.
        if self.observed && self.cfg.record_motion_clip_on_alert {
            let intent = self
                .shadow
                .as_mut()
                .map_or(UNVERIFIED, |s| s.alert(alert.captured_mono));
            error = self
                .drive(intent, alert.captured_mono, alert.captured_at)
                .await;
        }
        // Linked only to a clip that opened; a refusal is on the receipt.
        match self.clip {
            Some(handle) => {
                for event_id in &alert.event_ids {
                    if let Err(e) = self
                        .store
                        .link_event_to_clip(event_id, handle.clip_id)
                        .await
                    {
                        warn!(
                            target: LOG_TARGET,
                            event = %event_id,
                            clip_id = handle.clip_id,
                            "link_event_to_clip failed: {e}"
                        );
                    }
                }
            }
            None => error = error.or_else(|| self.last_open_error.clone()),
        }
        self.flush_deferred_close().await;
        if self.observed {
            let (now, wall) = self.now();
            self.trace(now, wall).await;
        }
        error
    }

    async fn epoch_changed(&mut self, change: EpochChange) {
        self.frame_dims = (change.frame_width, change.frame_height);
        if self.clip.is_some() {
            self.close(change.captured_at, "RGB tap resize").await;
            // Resizing rotates the file, preserving motion and post-roll.
            self.pending_open = true;
            self.frames_since_size_stat = 0;
        }
    }

    async fn policy_changed(&mut self, policy: Option<RecordingPolicyConfig>, revision: String) {
        if policy == self.cfg.policy && revision == self.cfg.policy_revision {
            return;
        }
        let was_observed = self.observed;
        self.cfg.policy = policy;
        self.cfg.policy_revision = revision;
        if let Some(mut old) = self.shadow.take() {
            let writes = old.finish();
            persist(&self.store, &old, writes).await;
        }
        self.start_policy();
        if was_observed && !self.observed {
            // Rollback: legacy takes the open clip over, its post-roll from now.
            self.post_roll = PostRoll::new(self.cfg.post_roll_secs);
            self.pending_open = false;
            self.hold = None;
        }
        if self.observed {
            let (now, wall) = self.now();
            let intent = self.intent_now(now);
            self.drive(intent, now, wall).await;
            self.trace(now, wall).await;
        }
        info!(
            target: LOG_TARGET,
            camera_id = self.cfg.camera_id,
            mode = ?policy.map_or(RecordingPolicyMode::Legacy, |p| p.mode),
            observed = self.observed,
            clip_id = ?self.clip.map(|h| h.clip_id),
            "recording policy applied without restarting the camera"
        );
    }

    fn start_policy(&mut self) {
        let cfg = &self.cfg;
        let registry = self.stats.recording_activity();
        let observed = cfg
            .policy
            .is_some_and(|p| p.mode == RecordingPolicyMode::Observed);
        self.shadow = match cfg.policy {
            Some(policy) if observed => ShadowRecording::start_as(
                registry,
                cfg.camera_id,
                cfg.generation,
                policy,
                &cfg.policy_revision,
                RecordingPolicyMode::Observed,
                &cfg.tracker,
                cfg.post_roll_secs,
                cfg.pre_roll_secs,
            ),
            _ => ShadowRecording::start(
                registry,
                cfg.camera_id,
                cfg.generation,
                cfg.policy,
                &cfg.policy_revision,
                &cfg.tracker,
                cfg.post_roll_secs,
                cfg.pre_roll_secs,
            ),
        };
        // A tracker without an observed/predicted contract cannot drive files.
        self.observed = observed && self.shadow.is_some();
    }

    /// M2.1: rotate on the five-minute duration cap, the byte cap and a
    /// stream epoch change. `stalled` checks the size now rather than every
    /// [`SIZE_STAT_INTERVAL_FRAMES`] frames, as no frames arrive to count.
    /// Returns whether it rotated.
    async fn rotate_if_due(&mut self, now: Instant, wall: DateTime<Utc>, stalled: bool) -> bool {
        let (Some(handle), Some(opened_at)) = (self.clip, self.clip_opened_at) else {
            return false;
        };
        let age_ms = now.saturating_duration_since(opened_at).as_millis() as i64;
        let duration_exceeded = age_ms >= MAX_CLIP_DURATION_MS;
        let mut size_exceeded = false;
        if self.cfg.max_clip_bytes > 0 {
            self.frames_since_size_stat += 1;
            if stalled || self.frames_since_size_stat >= SIZE_STAT_INTERVAL_FRAMES {
                self.frames_since_size_stat = 0;
                if let Some(size_bytes) = self.recorder.inflight_size_bytes(handle).await {
                    if size_bytes >= self.cfg.max_clip_bytes {
                        size_exceeded = true;
                        warn!(
                            target: LOG_TARGET,
                            camera_id = self.cfg.camera_id,
                            clip_id = handle.clip_id,
                            size_bytes,
                            max_bytes = self.cfg.max_clip_bytes,
                            "rotating clip: max size reached \
                             (likely corrupt byte-exploding stream)"
                        );
                    }
                }
            }
        }
        let epoch_changed = self.recorder.ended_on_epoch_change(handle);
        if epoch_changed {
            info!(
                target: LOG_TARGET,
                camera_id = self.cfg.camera_id,
                clip_id = handle.clip_id,
                "rotating clip: stream epoch changed"
            );
        }
        if duration_exceeded || size_exceeded || epoch_changed {
            if duration_exceeded {
                debug!(
                    target: LOG_TARGET,
                    camera_id = self.cfg.camera_id,
                    clip_id = handle.clip_id,
                    age_ms,
                    max_ms = MAX_CLIP_DURATION_MS,
                    "rotating clip: max duration reached"
                );
            }
            self.close(wall, "rotation").await;
            self.frames_since_size_stat = 0;
            // Rotation changes files, not the motion/post-roll lifetime.
            self.pending_open = true;
            return true;
        }
        false
    }

    async fn open(&mut self, now: Instant, wall: DateTime<Utc>) -> Option<RecordingError> {
        if self.open_panic_refused {
            // Panic recovery resumes immediately with a fresh I/O retry budget.
            self.open_panic_refused = false;
            self.open_retry_delay = OPEN_RETRY_FIRST;
        }
        let args = OpenClip {
            camera_id: self.cfg.camera_id,
            started_at: wall,
            frame_width: self.frame_dims.0,
            frame_height: self.frame_dims.1,
        };
        let recorder = self.recorder.clone();
        // A cancelled open is dropped: the recorder finalizes a clip it started.
        let opened = self
            .op("open", true, async move { recorder.open(args).await })
            .await?;
        let error = match opened {
            Ok(handle) => {
                self.clip = Some(handle);
                self.clip_opened_at = Some(now);
                self.hold = None;
                self.pending_open = false;
                self.open_retry_at = None;
                self.open_retry_delay = OPEN_RETRY_FIRST;
                self.open_error_logged = false;
                None
            }
            Err(RecorderError::Refused) => {
                // Let the first refusal report health, then wait for disk recovery.
                self.open_panic_refused = self.recorder.is_panic();
                if self.open_retry_at.is_none() {
                    debug!(
                        target: LOG_TARGET,
                        camera_id = self.cfg.camera_id,
                        "recorder refused open; dropping motion frame"
                    );
                }
                Some(RecordingError::Refused)
            }
            Err(e) => {
                if !self.open_error_logged {
                    warn!(target: LOG_TARGET, camera_id = self.cfg.camera_id, "recorder.open failed: {e}");
                    self.open_error_logged = true;
                }
                Some(RecordingError::OpenFailed(e.to_string()))
            }
        };
        if self.clip.is_none() {
            self.pending_open = true;
            // Non-panic failures back off; refusals do not suppress the first error warning.
            self.open_retry_at = Some(now + self.open_retry_delay);
            self.open_retry_delay = self.open_retry_delay.saturating_mul(2).min(OPEN_RETRY_MAX);
        }
        self.last_open_error = error.clone();
        error
    }

    /// Close the open clip. The clip is this controller's until the recorder
    /// has finished, however long that takes; nothing opens meanwhile.
    async fn close(
        &mut self,
        ended_at: DateTime<Utc>,
        what: &str,
    ) -> Option<crate::recorder::ClipMeta> {
        let handle = self.clip?;
        let recorder = self.recorder.clone();
        let closed = self
            .op("close", false, async move {
                recorder.close(handle, ClipFinal { ended_at }).await
            })
            .await;
        self.clip = None;
        self.clip_opened_at = None;
        self.hold = None;
        match closed {
            Some(Ok(meta)) => Some(meta),
            // The recorder's own shutdown drain finalized it first.
            Some(Err(RecorderError::UnknownClip(clip_id))) => {
                debug!(
                    target: LOG_TARGET,
                    camera_id = self.cfg.camera_id,
                    clip_id,
                    "recorder had already finalized the clip ({what})"
                );
                None
            }
            Some(Err(e)) => {
                warn!(target: LOG_TARGET, camera_id = self.cfg.camera_id, "recorder.close ({what}) failed: {e}");
                None
            }
            None => None,
        }
    }

    async fn flush_deferred_close(&mut self) {
        if let Some(at) = self.deferred_close.take() {
            self.close(at, "post-roll").await;
        }
    }

    /// Await a recorder call, reporting it once it is slow. Cancellation
    /// drops a `cancellable` call and waits out any other.
    async fn op<T>(
        &mut self,
        what: &'static str,
        cancellable: bool,
        call: impl Future<Output = T>,
    ) -> Option<T> {
        tokio::pin!(call);
        let slow = tokio::time::sleep(RECORDER_SLOW);
        tokio::pin!(slow);
        let mut reported = false;
        loop {
            let cancelled = tokio::select! {
                biased;
                out = &mut call => return Some(out),
                _ = self.alive.changed(), if !self.cancelled => true,
                () = &mut slow, if !reported => false,
            };
            if cancelled {
                self.cancelled = true;
                if cancellable {
                    return None;
                }
            } else {
                reported = true;
                warn!(
                    target: LOG_TARGET,
                    camera_id = self.cfg.camera_id,
                    after_secs = RECORDER_SLOW.as_secs(),
                    "recorder {what} has not finished; the recording controller keeps the clip \
                     and starts no other writer until it does"
                );
            }
        }
    }

    /// Stamp the frame's motion rows with the clip it leaves open. With
    /// none (a refused open) the rows are dropped: `clip_id` is required.
    async fn stamp(&self, decisions: &[MotionDecision]) {
        let Some(handle) = self.clip else {
            return;
        };
        for d in decisions {
            if let Err(e) = insert_motion_decision(&self.store, handle, d).await {
                warn!(target: LOG_TARGET, camera_id = self.cfg.camera_id, "insert_motion_event failed: {e}");
            }
        }
    }

    /// Trace the writer's state on the policy's diagnostics.
    async fn trace(&mut self, now: Instant, wall: DateTime<Utc>) {
        let clip = match self.deferred_close {
            Some(_) => None,
            None => self.clip.map(|h| h.clip_id),
        };
        let held = self.hold.is_some();
        if let Some(shadow) = self.shadow.as_mut() {
            let writes = shadow.record(now, wall, clip, held);
            persist(&self.store, shadow, writes).await;
        }
    }

    /// Close what is open and flush the diagnostics, once.
    async fn finalize(&mut self) {
        self.flush_deferred_close().await;
        self.close(Utc::now(), "shutdown").await;
        if let Some(mut shadow) = self.shadow.take() {
            let writes = shadow.finish();
            persist(&self.store, &shadow, writes).await;
        }
    }
}

fn min_clip() -> Duration {
    Duration::from_millis(MIN_CLIP_DURATION_MS as u64)
}

async fn sleep(wait: Option<Duration>) {
    match wait {
        Some(wait) => tokio::time::sleep(wait).await,
        None => std::future::pending().await,
    }
}

/// Persist what a traced state finished. A summary the store refuses is
/// reported on the camera's diagnostics, not retried: the trace says it has
/// a gap.
// shortcut: awaited on the controller like motion rows (at most a minute row and a clip row per change), move off-loop if store latency ever delays recorder decisions.
async fn persist(store: &Store, shadow: &ShadowRecording, writes: ShadowWrites) {
    if let Some(minute) = writes.minute {
        if let Err(e) = store.insert_recording_activity_minute(&minute).await {
            shadow.collector_failed(format!("minute summary not saved: {e}"));
        }
    }
    if let Some(episode) = writes.episode {
        if let Err(e) = store.insert_recording_activity_episode(&episode).await {
            shadow.collector_failed(format!("episode summary not saved: {e}"));
        }
    }
}

/// Translate one [`MotionDecision`] into a `motion_events` row write.
async fn insert_motion_decision(
    store: &Store,
    handle: ClipHandle,
    d: &MotionDecision,
) -> Result<(), nexus_store::StoreError> {
    let kind = match d.kind {
        MotionKind::Born => MotionEventKind::Born,
        MotionKind::Updated => MotionEventKind::Updated,
        MotionKind::Died => MotionEventKind::Died,
    };
    // Fast-path the common empty-attributes case (avoids cloning the
    // map + allocating a serde_json::Value::Object wrapper for every
    // motion event in a busy scene).
    #[expect(
        clippy::expect_used,
        reason = "serializing a string-keyed JSON map cannot fail"
    )]
    let attrs_json = if d.attributes.is_empty() {
        "{}".to_string()
    } else {
        serde_json::to_string(&d.attributes)
            .expect("string-keyed JSON attributes are infallible to serialize")
    };
    let new = NewMotionEvent {
        camera_id: d.camera_id,
        clip_id: handle.clip_id,
        track_id: d.track_id,
        kind,
        captured_at: d.captured_at,
        bbox: d.bbox,
        label: d.label.clone(),
        confidence: d.confidence,
        attributes_json: attrs_json,
    };
    store.insert_motion_event(&new).await.map(|_id| ())
}

//! ADR-095 — the rule responder: when a rule with a `talk_down` binding
//! fires, play its stored audio on the camera's (or the rule's) speaker,
//! within the ADR's bounds.
//!
//! Each `alert.event` runs through these gates, cheapest first; the first
//! that fails is counted under its reason and nothing plays:
//!
//! 1. the rule has `talk_down` (`no_rule_action`);
//! 2. a speaker resolves (`no_speaker`);
//! 3. the cloud allows automatic plays (`cloud_disallowed`);
//! 4. the core and the speaker are armed (`disarmed`);
//! 5. the rule's talk-down schedule, or its delivery cascade (`off_schedule`);
//! 6. a `verify` rule opted in with `play_before_verification`
//!    (`awaiting_verification`);
//! 7. the event is at most 10 s old (`stale`);
//! 8. no manual or live session ended on the speaker in the last 120 s
//!    (`holdoff`);
//! 9. the escalation sequence: the next step within `escalate_within_secs`
//!    of the last one ending (`sequence_done` past the last step), else the
//!    escalating cooldown (`cooldown`), else step 1;
//! 10. the speaker's hourly cap of automatic plays (`hourly_cap`);
//! 11. the step's item is ready (`audio_missing`, `audio_not_ready`);
//! 12. the speaker is free (`busy`; a rule play never pre-empts).
//!
//! The sequence and the hourly count are stamped before the play starts,
//! so a burst cannot double-start, and the play is spawned so the loop
//! never waits on a call. Every play is a `talkdown_plays` row and every
//! start or failure an audit row by `system:talkdown-rule:<rule_id>`;
//! skips are counted only. A lagged bus subscription is logged and counted
//! (`lagged`), never silent.
//!
//! The responder subscribes to `alert.event` only, which the pipeline's
//! rule evaluator publishes: no admin route, cloud RPC or Sentinel path
//! reaches it (ADR-095 (a), (g)).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use futures::StreamExt;
use nexus_bus::{topic, Bus, BusError, BusExt};
use nexus_config::{RuleTalkDown, SpeakerConfig, SpeakerId};
use nexus_rules::RuleEvaluator;
use nexus_sinks::dispatcher::DeliveryVerdict;
use nexus_sinks::policy::{evaluate_cascade, CascadingPolicy};
use nexus_store::audit::{AuditActorKind, AuditOutcome, NewAuditEntry};
use nexus_store::{AudioStatus, NewPlay, PlayFilter, SpeakerOutput, Store, TalkdownPlay};
use nexus_types::{AlertEvent, CameraId, DeliverySchedule};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use super::session::{Audio, Ended, Origin, StartError, StartRequest, TalkdownSessions};
use super::sip::TalkdownError;

/// Events older than this are not answered.
pub const STALE: Duration = Duration::seconds(10);
/// Rule plays wait this long after a manual or live session on the speaker.
pub const HOLDOFF: Duration = Duration::seconds(120);
const HOUR: Duration = Duration::hours(1);
const MAX_COOLDOWN_SECS: i64 = 3600;
/// Plays older than this are pruned, once a day.
pub const PLAYS_KEPT: Duration = Duration::days(90);
/// The reasons that make a rule `talkdown_rule_failing` in heartbeat health.
const FAILING: [&str; 3] = ["no_speaker", "audio_missing", "unreachable"];

pub struct ResponderDeps {
    pub store: Arc<Store>,
    pub bus: Arc<dyn Bus>,
    pub sessions: TalkdownSessions,
    pub evaluator: Arc<RuleEvaluator>,
    pub policy: Arc<CascadingPolicy>,
    pub activity: Activity,
}

/// A rule's last attempt, as the status route lists it. `last_result` is
/// `skipped`, `playing`, or a play's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleAttempt {
    pub rule_id: String,
    pub last_attempt_at: DateTime<Utc>,
    pub last_result: &'static str,
    pub last_reason: Option<String>,
}

/// What the responder did: each rule's last attempt and the skips per hour.
/// Shared with the status route and the heartbeat's health roll-up.
#[derive(Clone, Default)]
pub struct Activity(Arc<Mutex<ActivityInner>>);

#[derive(Default)]
struct ActivityInner {
    rules: HashMap<String, RuleAttempt>,
    /// Skips per (unix hour, reason), kept for 7 days.
    skips: BTreeMap<(i64, &'static str), u64>,
}

impl Activity {
    fn skip(&self, rule_id: Option<&str>, reason: &'static str, n: u64, now: DateTime<Utc>) {
        let mut a = self.0.lock();
        let hour = now.timestamp().div_euclid(3600);
        *a.skips.entry((hour, reason)).or_default() += n;
        a.skips.retain(|(h, _), _| hour - h < 7 * 24);
        // A rule without a binding is not a talk-down rule: not listed.
        if let Some(rule_id) = rule_id.filter(|_| reason != "no_rule_action") {
            a.set(rule_id, now, "skipped", Some(reason.to_string()));
        }
    }

    fn attempt(&self, rule_id: &str, result: &'static str, reason: Option<String>) {
        self.0.lock().set(rule_id, Utc::now(), result, reason);
    }

    /// A rule whose last attempt failed for `reason`, for another module's test.
    #[cfg(test)]
    pub(crate) fn failed_for_test(&self, rule_id: &str, reason: &str) {
        self.attempt(rule_id, "failed", Some(reason.into()));
    }

    /// Every talk-down rule's last attempt, by rule id.
    pub fn rules(&self) -> Vec<RuleAttempt> {
        let mut out: Vec<_> = self.0.lock().rules.values().cloned().collect();
        out.sort_by(|a, b| a.rule_id.cmp(&b.rule_id));
        out
    }

    /// `(rule_id, reason)` of each rule whose last attempt in the 24 h
    /// before `now` failed for a reason an admin has to fix.
    pub fn failing(&self, now: DateTime<Utc>) -> Vec<(String, String)> {
        self.rules()
            .into_iter()
            .filter(|r| now - r.last_attempt_at < Duration::hours(24))
            .filter_map(|r| {
                let reason = r.last_reason?;
                FAILING
                    .contains(&reason.as_str())
                    .then_some((r.rule_id, reason))
            })
            .collect()
    }

    /// Skips by reason since `since` (hour resolution).
    pub fn skipped_since(&self, since: DateTime<Utc>) -> BTreeMap<&'static str, u64> {
        let from = since.timestamp().div_euclid(3600);
        let mut out = BTreeMap::new();
        for (&(h, reason), n) in &self.0.lock().skips {
            if h >= from {
                *out.entry(reason).or_default() += n;
            }
        }
        out
    }
}

impl ActivityInner {
    fn set(
        &mut self,
        rule_id: &str,
        at: DateTime<Utc>,
        result: &'static str,
        reason: Option<String>,
    ) {
        self.rules.insert(
            rule_id.to_string(),
            RuleAttempt {
                rule_id: rule_id.to_string(),
                last_attempt_at: at,
                last_result: result,
                last_reason: reason,
            },
        );
    }
}

/// One (rule, speaker)'s escalation state.
#[derive(Default)]
struct Seq {
    /// The index of the last step started.
    step: usize,
    playing: bool,
    /// When the last step ended.
    ended_at: Option<DateTime<Utc>>,
    /// When the last attempt failed at the speaker; cleared once a step plays.
    last_attempt_at: Option<DateTime<Utc>>,
    /// When each sequence (a step 1) started, within the last hour.
    starts: VecDeque<DateTime<Utc>>,
    /// The rule's last fire for this speaker, for the outcome estimate.
    last_fire: Option<DateTime<Utc>>,
}

type Seqs = Arc<Mutex<HashMap<(String, SpeakerId), Seq>>>;

/// A play the gates let through.
struct Plan {
    rule_id: String,
    step: usize,
    td: RuleTalkDown,
    speaker: SpeakerConfig,
    output: SpeakerOutput,
    camera_id: CameraId,
    event_id: String,
    /// The sequence's `(step, ended_at)` before this plan, restored if it
    /// never plays: the next step follows a step that ended.
    prev: (usize, Option<DateTime<Utc>>),
    /// When this plan was decided: a failed attempt's cooldown runs from here.
    at: DateTime<Utc>,
}

struct Responder {
    d: ResponderDeps,
    seqs: Seqs,
    /// When each automatic play on a speaker started, within the last hour.
    hourly: HashMap<SpeakerId, VecDeque<DateTime<Utc>>>,
}

/// The escalating cooldown: `cooldown_secs` doubled for each further
/// sequence within the hour, at most an hour.
fn cooldown(cooldown_secs: u32, sequences_in_hour: usize) -> Duration {
    let doublings = u32::try_from(sequences_in_hour.max(1) - 1).unwrap_or(u32::MAX);
    let secs = i64::from(cooldown_secs)
        .checked_mul(2i64.checked_pow(doublings).unwrap_or(i64::MAX))
        .unwrap_or(i64::MAX);
    Duration::seconds(secs.min(MAX_COOLDOWN_SECS))
}

/// How long after a rule play its outcome is judged: `max(60 s, the
/// rule's cooldown + 15 s)`, since the rule cannot fire again sooner.
pub fn outcome_window(rule_cooldown_ms: u64) -> Duration {
    let ms = i64::try_from(rule_cooldown_ms).unwrap_or(i64::MAX / 2);
    Duration::milliseconds(ms.saturating_add(15_000)).max(Duration::seconds(60))
}

/// A play's `started_at` (SQLite `YYYY-MM-DD HH:MM:SS`, UTC).
pub fn started_at(p: &TalkdownPlay) -> Option<DateTime<Utc>> {
    sqlite_time(&p.started_at)
}

fn sqlite_time(s: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|n| n.and_utc())
}

/// A finished play's `talkdown_plays.result` and `reason`.
pub fn play_result(
    res: &Result<Result<Ended, TalkdownError>, JoinError>,
) -> (&'static str, Option<String>) {
    match res {
        Ok(Ok(Ended::Played)) => ("played", None),
        Ok(Ok(Ended::Stopped)) => ("stopped", None),
        Ok(Ok(Ended::Preempted)) => ("preempted", None),
        Ok(Err(TalkdownError::EndedBySpeaker)) => ("ended_by_speaker", None),
        Ok(Err(e)) => ("failed", Some(e.to_string())),
        Err(e) => ("failed", Some(e.to_string())),
    }
}

/// Wait for a started play and finish its row.
pub async fn finish_play(
    store: &Store,
    play_id: i64,
    playing: JoinHandle<Result<Ended, TalkdownError>>,
) -> (&'static str, Option<String>) {
    let (result, reason) = play_result(&playing.await);
    if let Err(e) = store
        .finish_talkdown_play(play_id, result, reason.as_deref())
        .await
    {
        warn!(error = %e, "talk-down: could not finish a play row");
    }
    (result, reason)
}

/// Delete plays older than [`PLAYS_KEPT`]; returns how many.
pub async fn prune_plays(store: &Store, now: DateTime<Utc>) -> u64 {
    match store.prune_talkdown_plays(now - PLAYS_KEPT).await {
        Ok(n) => n,
        Err(e) => {
            warn!(error = %e, "talk-down: pruning old plays failed");
            0
        }
    }
}

async fn audit(
    store: &Store,
    rule_id: &str,
    action: &str,
    camera_id: CameraId,
    outcome: AuditOutcome,
    after: serde_json::Value,
) {
    let label = format!("system:talkdown-rule:{rule_id}");
    let after = after.to_string();
    let camera = camera_id.to_string();
    let entry = NewAuditEntry {
        actor_kind: Some(AuditActorKind::System),
        actor_id: Some("0"),
        actor_label: &label,
        action,
        resource_kind: Some("camera"),
        resource_id: Some(&camera),
        before_json: None,
        after_json: Some(&after),
        outcome,
        ip: None,
        user_agent: None,
    };
    if let Err(e) = store.record_audit_event_standalone(&entry).await {
        warn!(error = %e, "talk-down: could not audit a rule play");
    }
}

impl Responder {
    /// The hourly counts and the manual hold-off survive a restart: they
    /// are read back from the plays log.
    async fn new(d: ResponderDeps) -> Self {
        let mut hourly: HashMap<SpeakerId, VecDeque<DateTime<Utc>>> = HashMap::new();
        let since = Utc::now() - HOUR;
        let recent = d
            .store
            .list_talkdown_plays(&PlayFilter {
                since: Some(since),
                limit: Some(1000),
                ..Default::default()
            })
            .await
            .unwrap_or_default();
        for p in recent.iter().rev() {
            if p.origin == "rule" {
                if let Some(t) = started_at(p) {
                    hourly.entry(p.speaker_id).or_default().push_back(t);
                }
            } else if let Some(t) = p.ended_at.as_deref().and_then(sqlite_time) {
                // A manual session lasts at most 10 minutes, so the hour covers it.
                d.sessions.restore_manual_end(p.speaker_id, t);
            }
        }
        // shortcut: the escalation sequence and cooldown restart empty after an engine restart; persist them if restarts prove frequent.
        Self {
            d,
            seqs: Seqs::default(),
            hourly,
        }
    }

    /// Run the gates on `ev` at `now`. Returns the play's task when one
    /// started, or the skip reason.
    async fn handle(
        &mut self,
        ev: &AlertEvent,
        now: DateTime<Utc>,
    ) -> Result<JoinHandle<()>, &'static str> {
        match self.decide(ev, now).await {
            Ok(plan) => Ok(self.play(plan)),
            Err(reason) => {
                self.d.activity.skip(Some(&ev.rule_id), reason, 1, now);
                Err(reason)
            }
        }
    }

    async fn decide(&mut self, ev: &AlertEvent, now: DateTime<Utc>) -> Result<Plan, &'static str> {
        let store = &self.d.store;
        let (td, verify) = self
            .d
            .evaluator
            .talk_down_for(&ev.rule_id)
            .ok_or("no_rule_action")?;
        let speaker = match td.speaker_id {
            Some(id) => store.get_speaker(id).await,
            None => store.speaker_for_camera(ev.camera_id).await,
        }
        .ok()
        .flatten()
        .ok_or("no_speaker")?;
        let key = (ev.rule_id.clone(), speaker.id);
        self.seqs.lock().entry(key.clone()).or_default().last_fire = Some(now);

        let core = store.talkdown_core().await.map_err(|_| "internal")?;
        if !core.cloud_allowed {
            return Err("cloud_disallowed");
        }
        let sp_arming = store
            .speaker_arming(speaker.id)
            .await
            .ok()
            .flatten()
            .ok_or("no_speaker")?;
        if !core.auto_armed || !sp_arming.armed {
            return Err("disarmed");
        }
        // An elapsed re-arm time reads as armed; write it back so the
        // stored state says so too. Conditional in SQL, so a disarm made
        // since the read is never overwritten.
        if core.auto_rearm_at.is_some() || sp_arming.rearm_at.is_some() {
            if let Err(e) = store.rearm_elapsed_talkdown(Utc::now()).await {
                warn!(error = %e, "talk-down: could not store an elapsed re-arm");
            }
        }
        let on_schedule = match &td.schedule {
            Some(s) => own_schedule_allows(store, s, now).await,
            None => self.d.policy.would_deliver(&ev.rule_id, now),
        };
        if !on_schedule {
            return Err("off_schedule");
        }
        if verify && !td.play_before_verification {
            return Err("awaiting_verification");
        }
        if now - ev.captured_at > STALE {
            return Err("stale");
        }
        if self
            .d
            .sessions
            .manual_ended_at(speaker.id)
            .is_some_and(|t| now - t < HOLDOFF)
        {
            return Err("holdoff");
        }
        let step = {
            let mut seqs = self.seqs.lock();
            let seq = seqs.entry(key.clone()).or_default();
            seq.starts.retain(|&t| now - t < HOUR);
            if seq.playing {
                return Err("busy");
            }
            if seq
                .last_attempt_at
                .is_some_and(|t| now - t < cooldown(td.cooldown_secs, seq.starts.len()))
            {
                return Err("cooldown");
            }
            match seq.ended_at {
                Some(end) if now - end <= Duration::seconds(td.escalate_within_secs.into()) => {
                    if seq.step + 1 < td.steps.len() {
                        seq.step + 1
                    } else {
                        return Err("sequence_done");
                    }
                }
                Some(end) if now - end < cooldown(td.cooldown_secs, seq.starts.len()) => {
                    return Err("cooldown");
                }
                _ => 0,
            }
        };
        let output = store
            .speaker_output(speaker.id)
            .await
            .map_err(|_| "no_speaker")?;
        let plays = self.hourly.entry(speaker.id).or_default();
        plays.retain(|&t| now - t < HOUR);
        if plays.len() >= usize::from(output.auto_max_per_hour) {
            return Err("hourly_cap");
        }
        let uid = &td.steps.get(step).ok_or("sequence_done")?.audio_uid;
        match store.get_talkdown_audio(uid).await {
            Ok(Some(a)) if a.status == AudioStatus::Ready => {}
            Ok(Some(_)) => return Err("audio_not_ready"),
            Ok(None) => return Err("audio_missing"),
            Err(_) => return Err("internal"),
        }
        if self.d.sessions.is_busy(speaker.id) {
            return Err("busy");
        }
        // Stamp before the play starts, so the next event sees it.
        plays.push_back(now);
        let mut seqs = self.seqs.lock();
        let seq = seqs.entry(key).or_default();
        let prev = (seq.step, seq.ended_at);
        seq.step = step;
        seq.playing = true;
        if step == 0 {
            seq.starts.push_back(now);
        }
        Ok(Plan {
            rule_id: ev.rule_id.clone(),
            step,
            td,
            speaker,
            output,
            camera_id: ev.camera_id,
            event_id: ev.event_id.to_string(),
            prev,
            at: now,
        })
    }

    /// Start `plan`'s play in its own task, which records and audits it.
    fn play(&self, plan: Plan) -> JoinHandle<()> {
        let store = self.d.store.clone();
        let sessions = self.d.sessions.clone();
        let activity = self.d.activity.clone();
        let seqs = self.seqs.clone();
        tokio::spawn(async move {
            let Plan {
                rule_id,
                step,
                td,
                speaker,
                output,
                camera_id,
                event_id,
                prev,
                at,
            } = plan;
            let key = (rule_id.clone(), speaker.id);
            let step_cfg = &td.steps[step];
            let audio = Audio::Stored {
                audio_uid: step_cfg.audio_uid.clone(),
                volume: td.volume,
                repeat: step_cfg.repeat,
            };
            // The step number as operators count it: 1-3.
            let step_no = u8::try_from(step + 1).unwrap_or(u8::MAX);
            // Refuse once the speaker this resolved is no longer the one.
            let paired = async {
                match td.speaker_id {
                    Some(id) => {
                        matches!(store.get_speaker(id).await, Ok(Some(now)) if now == speaker)
                    }
                    None => {
                        matches!(store.speaker_for_camera(camera_id).await, Ok(Some(now)) if now == speaker)
                    }
                }
            };
            let req = StartRequest {
                speaker: &speaker,
                output,
                camera_id: Some(camera_id),
                origin: Origin::Rule {
                    rule_id: rule_id.clone(),
                    step: step_no,
                },
                audio: &audio,
                event_id: Some(event_id.clone()),
            };
            let detail = |session_id: &str| {
                serde_json::json!({
                    "rule_id": rule_id,
                    "step": step_no,
                    "event_id": event_id,
                    "speaker_id": speaker.id,
                    "audio_uid": step_cfg.audio_uid,
                    "session_id": session_id,
                })
            };
            let row = |session_id: String| NewPlay {
                session_id,
                origin: "rule".into(),
                rule_id: Some(rule_id.clone()),
                step: Some(i64::from(step_no)),
                event_id: Some(event_id.clone()),
                camera_id: Some(camera_id),
                speaker_id: speaker.id,
                audio_uid: Some(step_cfg.audio_uid.clone()),
                mode: "stored".into(),
            };
            let end = |playing: bool| {
                if let Some(seq) = seqs.lock().get_mut(&key) {
                    seq.playing = playing;
                    seq.ended_at = Some(Utc::now());
                    seq.last_attempt_at = None;
                }
            };
            match sessions.start(req, paired).await {
                Ok((session_id, _, playing)) => {
                    let session_id = session_id.to_string();
                    info!(
                        speaker_id = speaker.id,
                        step = step_no,
                        "talk-down: a rule play started"
                    );
                    let play_id = store.insert_talkdown_play(&row(session_id.clone())).await;
                    audit(
                        &store,
                        &rule_id,
                        "talk_down.auto_start",
                        camera_id,
                        AuditOutcome::Success,
                        detail(&session_id),
                    )
                    .await;
                    activity.attempt(&rule_id, "playing", None);
                    let (result, reason) = match play_id {
                        Ok(id) => finish_play(&store, id, playing).await,
                        Err(ref e) => {
                            warn!(error = %e, "talk-down: could not record a rule play");
                            play_result(&playing.await)
                        }
                    };
                    end(false);
                    let ended_at = Utc::now();
                    if result == "failed" {
                        let mut after = detail(&session_id);
                        after["error"] = reason.clone().into();
                        audit(
                            &store,
                            &rule_id,
                            "talk_down.auto_end",
                            camera_id,
                            AuditOutcome::Failure,
                            after,
                        )
                        .await;
                    }
                    activity.attempt(
                        &rule_id,
                        result,
                        (result == "failed").then_some("unreachable".to_string()),
                    );
                    if let (Ok(id), false) = (play_id, result == "failed") {
                        let rule_cooldown = store
                            .list_rules()
                            .await
                            .ok()
                            .and_then(|rs| rs.into_iter().find(|r| r.id == rule_id))
                            .map_or(0, |r| r.debounce.cooldown_ms);
                        let window = outcome_window(rule_cooldown);
                        let (store, seqs) = (store.clone(), seqs.clone());
                        tokio::spawn(async move {
                            tokio::time::sleep(window.to_std().unwrap_or_default()).await;
                            settle_outcome(&store, &seqs, &key, id, ended_at, window).await;
                        });
                    }
                }
                Err(e) => {
                    // Never played: the attempt still counts in `starts` and the hourly cap.
                    if let Some(seq) = seqs.lock().get_mut(&key) {
                        (seq.step, seq.ended_at) = prev;
                        seq.playing = false;
                    }
                    let reason = match e {
                        StartError::Busy => "busy",
                        StartError::Unpaired => "no_speaker",
                        StartError::Stopped => "stopped",
                        StartError::AudioNotFound => "audio_missing",
                        StartError::AudioNotReady => "audio_not_ready",
                        StartError::Call(ref call) => {
                            // An attempt that failed at the speaker: recorded and audited.
                            let session_id = Uuid::now_v7().to_string();
                            let why = call.to_string();
                            match store.insert_talkdown_play(&row(session_id.clone())).await {
                                Ok(id) => {
                                    let _ =
                                        store.finish_talkdown_play(id, "failed", Some(&why)).await;
                                }
                                Err(e) => {
                                    warn!(error = %e, "talk-down: could not record a rule play")
                                }
                            }
                            let mut after = detail(&session_id);
                            after["error"] = why.into();
                            audit(
                                &store,
                                &rule_id,
                                "talk_down.auto_start",
                                camera_id,
                                AuditOutcome::Failure,
                                after,
                            )
                            .await;
                            // On another call: busy, not a fault an admin has to fix.
                            if matches!(call, TalkdownError::Rejected(486 | 480)) {
                                activity.skip(Some(&rule_id), "busy", 1, Utc::now());
                            } else {
                                if let Some(seq) = seqs.lock().get_mut(&key) {
                                    seq.last_attempt_at = Some(at);
                                }
                                activity.attempt(&rule_id, "failed", Some("unreachable".into()));
                            }
                            return;
                        }
                        _ => "internal",
                    };
                    activity.skip(Some(&rule_id), reason, 1, Utc::now());
                }
            }
        })
    }
}

/// Set a rule play's estimated outcome: `persisted` if the rule fired again
/// for the speaker within `window` of the play ending, else `cleared`.
async fn settle_outcome(
    store: &Store,
    seqs: &Seqs,
    key: &(String, SpeakerId),
    play_id: i64,
    ended_at: DateTime<Utc>,
    window: Duration,
) {
    let fired = seqs
        .lock()
        .get(key)
        .and_then(|s| s.last_fire)
        .is_some_and(|t| t > ended_at && t <= ended_at + window);
    let outcome = if fired { "persisted" } else { "cleared" };
    if let Err(e) = store.set_talkdown_play_outcome(play_id, outcome).await {
        warn!(error = %e, "talk-down: could not set a play's outcome");
    }
}

/// The rule's own talk-down schedule at `now`, in the delivery timezone.
async fn own_schedule_allows(
    store: &Store,
    schedule: &DeliverySchedule,
    now: DateTime<Utc>,
) -> bool {
    // An unreadable delivery setting fails closed: no automatic play.
    let Ok(settings) = store.delivery_settings_get().await else {
        return false;
    };
    let tz = settings.timezone.parse().unwrap_or(chrono_tz::Tz::UTC);
    let only_this = nexus_types::DeliverySettings {
        enabled: true,
        schedule: Some(schedule.clone()),
        ..settings
    };
    matches!(
        evaluate_cascade(&only_this, tz, None, now),
        DeliveryVerdict::Deliver
    )
}

/// Answer every rule fire until `cancel`, and prune old plays once a day.
pub async fn run(d: ResponderDeps, cancel: CancellationToken) {
    let mut events = match d.bus.subscribe::<AlertEvent>(topic::ALERT_EVENT).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "talk-down responder: cannot subscribe to alerts; rule plays are off");
            return;
        }
    };
    let mut r = Responder::new(d).await;
    let mut prune = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
    prune.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = prune.tick() => {
                let n = prune_plays(&r.d.store, Utc::now()).await;
                if n > 0 {
                    info!(deleted = n, "talk-down: pruned old plays");
                }
            }
            ev = events.next() => match ev {
                Some(Ok(ev)) => {
                    let _ = r.handle(&ev, Utc::now()).await;
                }
                // Dropped events cannot be answered: say so and count them.
                Some(Err(BusError::Lagged(n))) => {
                    warn!(missed = n, "talk-down responder lagged behind the alert bus");
                    r.d.activity.skip(None, "lagged", n, Utc::now());
                }
                Some(Err(e)) => warn!(error = %e, "talk-down responder: bad alert event"),
                None => {
                    warn!("talk-down responder: the alert bus closed");
                    return;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::talkdown::library::Library;
    use crate::talkdown::sip::tests::{answer, fake_uas, reply, SDP};
    use nexus_config::{
        RuleConfig, RuleDebounce, RuleGates, RulePredicate, RulesConfig, TalkDownStep,
    };
    use nexus_store::audit::AuditFilter;
    use nexus_store::NewAudio;

    const U1: &str = "a1b2c3d4-0000-4000-8000-000000000001";
    const U2: &str = "a1b2c3d4-0000-4000-8000-000000000002";
    const PENDING: &str = "a1b2c3d4-0000-4000-8000-000000000003";

    fn talk_down() -> RuleTalkDown {
        RuleTalkDown {
            steps: vec![
                TalkDownStep {
                    audio_uid: U1.into(),
                    repeat: 1,
                },
                TalkDownStep {
                    audio_uid: U2.into(),
                    repeat: 1,
                },
            ],
            escalate_within_secs: 30,
            cooldown_secs: 60,
            schedule: None,
            volume: None,
            speaker_id: None,
            play_before_verification: false,
        }
    }

    fn rule(id: &str, td: Option<RuleTalkDown>, verify: bool) -> RuleConfig {
        RuleConfig {
            id: id.into(),
            name: format!("Rule {id}"),
            predicate: RulePredicate {
                when: "true".into(),
                severity: "low".into(),
            },
            gates: RuleGates::default(),
            debounce: RuleDebounce {
                min_track_age_ms: 0,
                consecutive_frames: 1,
                cooldown_ms: 0,
            },
            enabled: true,
            sinks: vec![],
            verify,
            talk_down: td,
        }
    }

    struct Rig {
        r: Responder,
        store: Arc<Store>,
        sessions: TalkdownSessions,
        speaker: SpeakerConfig,
        _dir: tempfile::TempDir,
    }

    async fn open(dir: &std::path::Path) -> Arc<Store> {
        Arc::new(
            Store::open(&nexus_config::StoreConfig {
                url: format!("sqlite://{}?mode=rwc", dir.join("n.db").display()),
                ..Default::default()
            })
            .await
            .unwrap(),
        )
    }

    async fn ready(store: &Store, uid: &str, samples: usize) {
        store
            .upsert_talkdown_pending(&NewAudio {
                uid: uid.into(),
                name: "clip".into(),
                description: None,
                kind: nexus_store::AudioKind::Clip,
                tts_text: None,
                tts_voice: None,
                version_token: None,
            })
            .await
            .unwrap();
        if samples > 0 {
            store
                .set_talkdown_ready(uid, &vec![1000; samples])
                .await
                .unwrap();
        }
    }

    /// Camera 1 has the speaker at `port`; camera 2 has none. Rule `r1`
    /// carries `td`, rule `plain` none; the cloud allows automatic plays.
    async fn rig_with(td: RuleTalkDown, verify: bool, port: u16, bus: Arc<dyn Bus>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).await;
        for id in [1, 2] {
            let cam = nexus_config::CameraConfig {
                id,
                name: format!("cam-{id}"),
                ingest: nexus_config::CameraIngest {
                    url: "rtsp://127.0.0.1/stream".parse().unwrap(),
                    analysis_url: None,
                    enabled: true,
                    max_fps: 0,
                    codec: None,
                },
                detector: nexus_config::CameraDetector::default(),
                behavior: nexus_config::CameraBehavior::default(),
                onvif: nexus_config::CameraOnvif::default(),
                talk_down: Default::default(),
                zones: vec![],
            };
            store.upsert_camera(&cam).await.unwrap();
        }
        let mut speaker: SpeakerConfig = serde_json::from_value(
            serde_json::json!({ "name": "spk", "host": "127.0.0.1", "port": port }),
        )
        .unwrap();
        let mut tx = store.begin_tx().await.unwrap();
        store
            .create_speaker_tx(&mut tx, &mut speaker)
            .await
            .unwrap();
        store
            .set_camera_speaker_tx(&mut tx, 1, Some(speaker.id))
            .await
            .unwrap();
        Store::commit_tx(tx).await.unwrap();
        ready(&store, U1, 800).await;
        ready(&store, U2, 800).await;
        ready(&store, PENDING, 0).await;
        store.set_talkdown_core_cloud_allowed(true).await.unwrap();
        let rules = [rule("r1", Some(td), verify), rule("plain", None, false)];
        for r in &rules {
            store.upsert_rule(r).await.unwrap();
        }
        let (library, _worker) = Library::spawn(store.clone(), bus.clone(), true, None);
        let sessions = TalkdownSessions::with_library(library);
        let d = ResponderDeps {
            store: store.clone(),
            bus,
            sessions: sessions.clone(),
            evaluator: Arc::new(RuleEvaluator::new(&RulesConfig::default(), &rules).unwrap()),
            policy: Arc::new(CascadingPolicy::hydrate(&store).await.unwrap()),
            activity: Activity::default(),
        };
        Rig {
            r: Responder::new(d).await,
            store,
            sessions,
            speaker,
            _dir: dir,
        }
    }

    fn bus() -> Arc<dyn Bus> {
        Arc::new(nexus_bus::BroadcastBus::new(16))
    }

    async fn rig(td: RuleTalkDown) -> Rig {
        rig_with(td, false, 9, bus()).await
    }

    fn ev(rule_id: &str, camera_id: CameraId, captured_at: DateTime<Utc>) -> AlertEvent {
        AlertEvent {
            event_id: Uuid::now_v7(),
            camera_id,
            rule_id: rule_id.into(),
            track_id: None,
            label: "person".into(),
            severity: nexus_types::Severity::High,
            bbox: None,
            frame_id: 1,
            captured_at,
            trace_id: String::new(),
            frame_w: 0,
            frame_h: 0,
            artifacts: Default::default(),
            context: Default::default(),
        }
    }

    impl Rig {
        async fn decide(
            &mut self,
            rule_id: &str,
            camera: CameraId,
            now: DateTime<Utc>,
        ) -> Result<usize, &'static str> {
            self.r
                .decide(&ev(rule_id, camera, now), now)
                .await
                .map(|p| p.step)
        }

        /// The step `r1` last started on the speaker ended at `end`.
        fn ended(&self, end: DateTime<Utc>) {
            let mut seqs = self.r.seqs.lock();
            let seq = seqs.get_mut(&("r1".to_string(), self.speaker.id)).unwrap();
            seq.playing = false;
            seq.ended_at = Some(end);
        }
    }

    // ---- one test per gate reason, in gate order ------------------------

    #[tokio::test]
    async fn gate_1_no_rule_action_is_counted_but_not_listed() {
        let mut g = rig(talk_down()).await;
        let now = Utc::now();
        assert_eq!(g.decide("plain", 1, now).await, Err("no_rule_action"));
        assert_eq!(
            g.r.handle(&ev("unknown", 1, now), now).await.err(),
            Some("no_rule_action")
        );
        assert!(g.r.d.activity.rules().is_empty());
        assert_eq!(
            g.r.d.activity.skipped_since(now - HOUR)["no_rule_action"],
            1
        );
    }

    #[tokio::test]
    async fn gate_2_no_speaker_for_the_camera_or_the_named_speaker() {
        let now = Utc::now();
        let mut g = rig(talk_down()).await;
        assert_eq!(g.decide("r1", 2, now).await, Err("no_speaker"));
        let mut g = rig(RuleTalkDown {
            speaker_id: Some(999),
            ..talk_down()
        })
        .await;
        assert_eq!(g.decide("r1", 1, now).await, Err("no_speaker"));
        // A named speaker plays even for a camera without one.
        let mut g = rig(talk_down()).await;
        let named = RuleTalkDown {
            speaker_id: Some(g.speaker.id),
            ..talk_down()
        };
        g.r.d
            .evaluator
            .reload(&[rule("r1", Some(named), false)])
            .unwrap();
        assert_eq!(g.decide("r1", 2, now).await, Ok(0));
    }

    #[tokio::test]
    async fn gate_3_cloud_disallowed_until_the_cloud_says_true_and_after_it_says_false() {
        let mut g = rig(talk_down()).await;
        let now = Utc::now();
        g.store
            .set_talkdown_core_cloud_allowed(false)
            .await
            .unwrap();
        assert_eq!(g.decide("r1", 1, now).await, Err("cloud_disallowed"));
        // A core that never heard `true` plays nothing automatically.
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !open(dir.path())
                .await
                .talkdown_core()
                .await
                .unwrap()
                .cloud_allowed
        );
    }

    #[tokio::test]
    async fn gate_4_disarmed_core_or_speaker_and_an_elapsed_rearm_is_written_back() {
        let mut g = rig(talk_down()).await;
        let now = Utc::now();
        let id = g.speaker.id;
        let later = Some(now + Duration::hours(1));
        g.store
            .set_talkdown_core_arming(false, later, Some("works"))
            .await
            .unwrap();
        assert_eq!(g.decide("r1", 1, now).await, Err("disarmed"));
        g.store
            .set_talkdown_core_arming(true, None, None)
            .await
            .unwrap();
        g.store
            .set_speaker_arming(id, false, later, Some("works"))
            .await
            .unwrap();
        assert_eq!(g.decide("r1", 1, now).await, Err("disarmed"));

        // The re-arm time passed: armed again, and stored as armed.
        let past = Some(now - Duration::seconds(1));
        g.store
            .set_talkdown_core_arming(false, past, Some("works"))
            .await
            .unwrap();
        g.store
            .set_speaker_arming(id, false, past, Some("works"))
            .await
            .unwrap();
        assert_eq!(g.decide("r1", 1, now).await, Ok(0));
        let core = g.store.talkdown_core().await.unwrap();
        let sp = g.store.speaker_arming(id).await.unwrap().unwrap();
        assert!(core.auto_armed && core.auto_rearm_at.is_none());
        assert!(sp.armed && sp.rearm_at.is_none() && sp.reason.is_none());
    }

    #[tokio::test]
    async fn gate_5_off_the_rule_schedule_or_the_delivery_cascade() {
        let now = Utc::now();
        let mut g = rig(RuleTalkDown {
            schedule: Some(DeliverySchedule::never()),
            ..talk_down()
        })
        .await;
        assert_eq!(g.decide("r1", 1, now).await, Err("off_schedule"));

        // The rule's own schedule replaces the delivery cascade.
        let mut g = rig(RuleTalkDown {
            schedule: Some(DeliverySchedule::always()),
            ..talk_down()
        })
        .await;
        let mut closed = g.store.delivery_settings_get().await.unwrap();
        closed.schedule = Some(DeliverySchedule::never());
        g.store.delivery_settings_put(&closed).await.unwrap();
        g.r.d.policy.reload_settings(&g.store).await.unwrap();
        assert_eq!(g.decide("r1", 1, now).await, Ok(0));

        // Without one, the delivery cascade decides.
        let mut g = rig(talk_down()).await;
        g.store.delivery_settings_put(&closed).await.unwrap();
        g.r.d.policy.reload_settings(&g.store).await.unwrap();
        assert_eq!(g.decide("r1", 1, now).await, Err("off_schedule"));
    }

    #[tokio::test]
    async fn gate_6_a_verify_rule_waits_unless_it_opted_in() {
        let now = Utc::now();
        let mut g = rig_with(talk_down(), true, 9, bus()).await;
        assert_eq!(g.decide("r1", 1, now).await, Err("awaiting_verification"));
        let opted = RuleTalkDown {
            play_before_verification: true,
            ..talk_down()
        };
        let mut g = rig_with(opted, true, 9, bus()).await;
        assert_eq!(g.decide("r1", 1, now).await, Ok(0));
    }

    #[tokio::test]
    async fn gate_7_events_older_than_10_s_are_stale() {
        let mut g = rig(talk_down()).await;
        let now = Utc::now();
        let old = ev("r1", 1, now - Duration::seconds(11));
        assert_eq!(g.r.decide(&old, now).await.err(), Some("stale"));
        let fresh = ev("r1", 1, now - Duration::seconds(9));
        assert!(g.r.decide(&fresh, now).await.is_ok());
    }

    #[tokio::test]
    async fn gate_8_holdoff_120_s_after_a_manual_session() {
        let mut g = rig(talk_down()).await;
        let now = Utc::now();
        g.sessions
            .record_manual_end_for_test(g.speaker.id, now - Duration::seconds(119));
        assert_eq!(g.decide("r1", 1, now).await, Err("holdoff"));
        g.sessions
            .record_manual_end_for_test(g.speaker.id, now - Duration::seconds(121));
        assert_eq!(g.decide("r1", 1, now).await, Ok(0));
    }

    /// ADR-095 (e): steps advance only on a fire within the window, never
    /// past the last one.
    #[tokio::test]
    async fn gate_9_escalates_within_the_window_then_sequence_done_then_cooldown() {
        let mut g = rig(talk_down()).await;
        let t = Utc::now();
        let s = |n| t + Duration::seconds(n);
        assert_eq!(g.decide("r1", 1, t).await, Ok(0));
        assert_eq!(
            g.decide("r1", 1, t).await,
            Err("busy"),
            "step 1 still playing"
        );
        g.ended(t);
        assert_eq!(g.decide("r1", 1, s(20)).await, Ok(1), "step 2 within 30 s");
        g.ended(s(25));
        assert_eq!(g.decide("r1", 1, s(50)).await, Err("sequence_done"));
        // Past the window, inside the 60 s cooldown.
        assert_eq!(g.decide("r1", 1, s(60)).await, Err("cooldown"));
        // Past the cooldown: a new sequence from step 1.
        assert_eq!(g.decide("r1", 1, s(86)).await, Ok(0));
    }

    /// The hold-off survives a restart: it is read back from the plays log.
    #[tokio::test]
    async fn gate_8_the_holdoff_survives_a_restart() {
        let mut g = rig(talk_down()).await;
        let id = g
            .store
            .insert_talkdown_play(&NewPlay {
                session_id: "m1".into(),
                origin: "manual".into(),
                rule_id: None,
                step: None,
                event_id: None,
                camera_id: Some(1),
                speaker_id: g.speaker.id,
                audio_uid: None,
                mode: "tone".into(),
            })
            .await
            .unwrap();
        g.store
            .finish_talkdown_play(id, "played", None)
            .await
            .unwrap();
        let d = ResponderDeps {
            store: g.store.clone(),
            bus: g.r.d.bus.clone(),
            sessions: TalkdownSessions::default(),
            evaluator: g.r.d.evaluator.clone(),
            policy: g.r.d.policy.clone(),
            activity: Activity::default(),
        };
        g.r = Responder::new(d).await;
        assert_eq!(g.decide("r1", 1, Utc::now()).await, Err("holdoff"));
    }

    #[tokio::test]
    async fn gate_9_the_cooldown_doubles_per_sequence_in_the_hour() {
        assert_eq!(cooldown(60, 0), Duration::seconds(60));
        assert_eq!(cooldown(60, 1), Duration::seconds(60));
        assert_eq!(cooldown(60, 2), Duration::seconds(120));
        assert_eq!(cooldown(60, 3), Duration::seconds(240));
        assert_eq!(
            cooldown(60, 7),
            Duration::seconds(3600),
            "capped at an hour"
        );
        assert_eq!(cooldown(3600, 64), Duration::seconds(3600), "no overflow");

        let mut g = rig(RuleTalkDown {
            steps: talk_down().steps[..1].to_vec(),
            ..talk_down()
        })
        .await;
        let t = Utc::now();
        let s = |n| t + Duration::seconds(n);
        assert_eq!(g.decide("r1", 1, t).await, Ok(0));
        g.ended(t);
        assert_eq!(g.decide("r1", 1, s(61)).await, Ok(0), "first cooldown 60 s");
        g.ended(s(61));
        assert_eq!(
            g.decide("r1", 1, s(61 + 100)).await,
            Err("cooldown"),
            "now 120 s"
        );
        assert_eq!(g.decide("r1", 1, s(61 + 121)).await, Ok(0));
    }

    #[tokio::test]
    async fn gate_10_the_hourly_cap_stops_play_7_and_survives_a_restart() {
        let mut g = rig(RuleTalkDown {
            steps: talk_down().steps[..1].to_vec(),
            cooldown_secs: 10,
            ..talk_down()
        })
        .await;
        let mut now = Utc::now();
        for n in 1..=6 {
            assert_eq!(g.decide("r1", 1, now).await, Ok(0), "play {n}");
            g.ended(now);
            // Past both the 30 s escalation window and the doubling cooldown.
            now += cooldown(10, n).max(Duration::seconds(30)) + Duration::seconds(1);
        }
        assert_eq!(g.decide("r1", 1, now).await, Err("hourly_cap"), "play 7");

        // Six rule plays logged within the hour: a fresh responder is capped too.
        for i in 0..6 {
            g.store
                .insert_talkdown_play(&NewPlay {
                    session_id: format!("s{i}"),
                    origin: "rule".into(),
                    rule_id: Some("r1".into()),
                    step: Some(1),
                    event_id: None,
                    camera_id: Some(1),
                    speaker_id: g.speaker.id,
                    audio_uid: Some(U1.into()),
                    mode: "stored".into(),
                })
                .await
                .unwrap();
        }
        let d = ResponderDeps {
            store: g.store.clone(),
            bus: g.r.d.bus.clone(),
            sessions: g.sessions.clone(),
            evaluator: g.r.d.evaluator.clone(),
            policy: g.r.d.policy.clone(),
            activity: Activity::default(),
        };
        g.r = Responder::new(d).await;
        assert_eq!(g.decide("r1", 1, Utc::now()).await, Err("hourly_cap"));
    }

    #[tokio::test]
    async fn gate_11_audio_missing_or_not_ready() {
        let now = Utc::now();
        let step = |uid: &str| RuleTalkDown {
            steps: vec![TalkDownStep {
                audio_uid: uid.into(),
                repeat: 1,
            }],
            ..talk_down()
        };
        let mut g = rig(step("a1b2c3d4-0000-4000-8000-00000000dead")).await;
        assert_eq!(g.decide("r1", 1, now).await, Err("audio_missing"));
        let mut g = rig(step(PENDING)).await;
        assert_eq!(g.decide("r1", 1, now).await, Err("audio_not_ready"));
    }

    /// ADR-095 (d): a rule play never takes a speaker a manual session holds.
    #[tokio::test]
    async fn gate_12_busy_a_rule_never_preempts_a_manual_session() {
        let mut g = rig(talk_down()).await;
        let speaker = g.speaker.clone();
        let req = StartRequest {
            speaker: &speaker,
            output: g.store.speaker_output(speaker.id).await.unwrap(),
            camera_id: Some(1),
            origin: Origin::Manual,
            audio: &Audio::Tone,
            event_id: None,
        };
        let sessions = g.sessions.clone();
        let held = sessions.start(req, std::future::pending::<bool>());
        tokio::pin!(held);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut held)
                .await
                .is_err()
        );
        assert_eq!(g.decide("r1", 1, Utc::now()).await, Err("busy"));
        assert_eq!(sessions.origin_of(speaker.id), Some(Origin::Manual));
    }

    // ---- the bus -----------------------------------------------------

    /// A flooded subscription is logged and counted as `lagged`, and the
    /// events after it are still answered.
    #[tokio::test]
    async fn a_lagged_bus_is_counted_and_later_events_are_still_answered() {
        let bus: Arc<dyn Bus> = Arc::new(nexus_bus::BroadcastBus::new(1));
        let g = rig_with(talk_down(), false, 9, bus.clone()).await;
        let activity = g.r.d.activity.clone();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(g.r.d, cancel.clone()));
        while !bus.has_subscribers(topic::ALERT_EVENT) {
            tokio::task::yield_now().await;
        }
        let now = Utc::now();
        for _ in 0..20 {
            bus.publish(topic::ALERT_EVENT, &ev("plain", 1, now))
                .await
                .unwrap();
        }
        let counts = |a: &Activity| a.skipped_since(now - HOUR);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !counts(&activity).contains_key("lagged") {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the lag is counted");
        let answered = |a: &Activity| counts(a).get("no_rule_action").copied().unwrap_or(0);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let before = answered(&activity);
        bus.publish(topic::ALERT_EVENT, &ev("plain", 1, now))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while answered(&activity) <= before {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("an event after the lag is answered");
        cancel.cancel();
        task.await.unwrap();
    }

    // ---- plays -------------------------------------------------------

    /// A speaker answering PCMU on the returned RTP socket.
    async fn speaker() -> (u16, tokio::net::UdpSocket) {
        let rtp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rtp_port = rtp.local_addr().unwrap().port();
        let (port, _uas) = fake_uas("NEVER", move |req| {
            if req.starts_with("INVITE") {
                vec![reply(req, "200 OK", SDP, &answer(rtp_port, "0"))]
            } else if req.starts_with("BYE") {
                vec![reply(req, "200 OK", "", "")]
            } else {
                vec![]
            }
        })
        .await;
        (port, rtp)
    }

    async fn plays(store: &Store) -> Vec<TalkdownPlay> {
        store
            .list_talkdown_plays(&PlayFilter::default())
            .await
            .unwrap()
    }

    async fn audits(store: &Store) -> Vec<nexus_store::AuditEntry> {
        store
            .list_audit_filtered(&AuditFilter::default(), 100, 0)
            .await
            .unwrap()
    }

    /// RTP packets until 300 ms of silence.
    async fn count_rtp(rtp: &tokio::net::UdpSocket) -> usize {
        let mut buf = [0u8; 2048];
        let mut n = 0;
        while tokio::time::timeout(std::time::Duration::from_millis(300), rtp.recv(&mut buf))
            .await
            .is_ok()
        {
            n += 1;
        }
        n
    }

    /// ADR-095 (f): the play is a `talkdown_plays` row linked to the alert,
    /// and its start an audit row by the rule.
    #[tokio::test]
    async fn a_rule_play_writes_a_play_row_and_an_audit_row_naming_the_rule() {
        let (port, rtp) = speaker().await;
        let mut g = rig_with(talk_down(), false, port, bus()).await;
        let e = ev("r1", 1, Utc::now());
        let task = g.r.handle(&e, Utc::now()).await.unwrap();
        task.await.unwrap();
        // The frames of the audio and its silent tail (the driver may add one).
        let frames = (800 + 2400) / 160;
        assert!((frames..=frames + 1).contains(&count_rtp(&rtp).await));
        let p = &plays(&g.store).await[0];
        assert_eq!(
            (
                p.origin.as_str(),
                p.rule_id.as_deref(),
                p.step,
                p.result.as_deref()
            ),
            ("rule", Some("r1"), Some(1), Some("played"))
        );
        let event = e.event_id.to_string();
        assert_eq!(p.event_id.as_deref(), Some(event.as_str()));
        assert_eq!((p.camera_id, p.audio_uid.as_deref()), (Some(1), Some(U1)));
        let a = audits(&g.store).await;
        let start = a
            .iter()
            .find(|a| a.action == "talk_down.auto_start")
            .unwrap();
        assert_eq!(start.actor_label, "system:talkdown-rule:r1");
        assert_eq!(start.outcome, AuditOutcome::Success);
        assert!(start.after_json.as_deref().unwrap().contains(&event));
        let r = &g.r.d.activity.rules()[0];
        assert_eq!((r.rule_id.as_str(), r.last_result), ("r1", "played"));
    }

    #[tokio::test]
    async fn a_step_with_repeat_2_plays_its_audio_twice_in_one_call() {
        let (port, rtp) = speaker().await;
        let twice = RuleTalkDown {
            steps: vec![TalkDownStep {
                audio_uid: U1.into(),
                repeat: 2,
            }],
            ..talk_down()
        };
        let mut g = rig_with(twice, false, port, bus()).await;
        let task =
            g.r.handle(&ev("r1", 1, Utc::now()), Utc::now())
                .await
                .unwrap();
        task.await.unwrap();
        let frames = (800 + 8000 + 800 + 2400) / 160;
        assert!((frames..=frames + 1).contains(&count_rtp(&rtp).await));
        assert_eq!(plays(&g.store).await.len(), 1, "one call, one play");
    }

    #[tokio::test]
    async fn a_second_fire_within_the_window_plays_step_2() {
        let (port, _rtp) = speaker().await;
        let mut g = rig_with(talk_down(), false, port, bus()).await;
        for _ in 0..2 {
            let task =
                g.r.handle(&ev("r1", 1, Utc::now()), Utc::now())
                    .await
                    .unwrap();
            task.await.unwrap();
        }
        let p = plays(&g.store).await;
        assert_eq!(
            p.iter()
                .map(|p| (p.step, p.audio_uid.clone()))
                .collect::<Vec<_>>(),
            [(Some(2), Some(U2.into())), (Some(1), Some(U1.into()))]
        );
    }

    /// D10, ADR-095 (e): a step that never played does not advance the
    /// sequence, though it still counts against the hourly cap.
    #[tokio::test]
    async fn a_step_that_found_the_speaker_busy_is_tried_again_not_skipped() {
        let mut g = rig(talk_down()).await;
        let now = Utc::now();
        let plan = g.r.decide(&ev("r1", 1, now), now).await.unwrap();
        assert_eq!(plan.step, 0);
        // A manual session takes the speaker between the gates and the start.
        let speaker = g.speaker.clone();
        let req = StartRequest {
            speaker: &speaker,
            output: g.store.speaker_output(speaker.id).await.unwrap(),
            camera_id: Some(1),
            origin: Origin::Manual,
            audio: &Audio::Tone,
            event_id: None,
        };
        let sessions = g.sessions.clone();
        let mut held = Box::pin(sessions.start(req, std::future::pending::<bool>()));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut held)
                .await
                .is_err()
        );
        g.r.play(plan).await.unwrap();
        drop(held);
        g.sessions
            .record_manual_end_for_test(g.speaker.id, now - Duration::hours(1));
        let then = now + Duration::seconds(5);
        assert_eq!(g.decide("r1", 1, then).await, Ok(0), "step 1 again");
        assert_eq!(g.r.hourly[&g.speaker.id].len(), 2, "both attempts count");
    }

    /// ADR-095 (f): an attempt that fails at the speaker is recorded and
    /// audited, and the rule reads as failing.
    #[tokio::test]
    async fn a_failed_call_is_recorded_audited_and_marks_the_rule_failing() {
        let (port, _uas) = fake_uas("NEVER", |req| {
            if req.starts_with("INVITE") {
                vec![reply(req, "503 Service Unavailable", "", "")]
            } else {
                vec![]
            }
        })
        .await;
        let mut g = rig_with(talk_down(), false, port, bus()).await;
        let task =
            g.r.handle(&ev("r1", 1, Utc::now()), Utc::now())
                .await
                .unwrap();
        task.await.unwrap();
        let p = &plays(&g.store).await[0];
        assert_eq!(
            (p.origin.as_str(), p.result.as_deref()),
            ("rule", Some("failed"))
        );
        assert!(p.reason.as_deref().unwrap().contains("503"));
        // The failed step 1 is tried again past the cooldown, not followed by step 2.
        assert_eq!(
            g.decide("r1", 1, Utc::now() + Duration::seconds(61)).await,
            Ok(0)
        );
        let a = audits(&g.store).await;
        let start = a
            .iter()
            .find(|a| a.action == "talk_down.auto_start")
            .unwrap();
        assert_eq!(start.outcome, AuditOutcome::Failure);
        assert_eq!(start.actor_label, "system:talkdown-rule:r1");
        assert_eq!(
            g.r.d.activity.failing(Utc::now()),
            [("r1".to_string(), "unreachable".to_string())]
        );
        assert!(g
            .r
            .d
            .activity
            .failing(Utc::now() + Duration::hours(25))
            .is_empty());
    }

    /// A speaker that keeps failing is retried on the doubling cooldown, not
    /// on every fire until the hourly cap.
    #[tokio::test]
    async fn a_failing_speaker_is_retried_on_the_cooldown_not_every_fire() {
        let (port, _uas) = fake_uas("NEVER", |req| {
            if req.starts_with("INVITE") {
                vec![reply(req, "503 Service Unavailable", "", "")]
            } else {
                vec![]
            }
        })
        .await;
        let mut g = rig_with(talk_down(), false, port, bus()).await;
        let t = Utc::now();
        let s = |n| t + Duration::seconds(n);
        g.r.handle(&ev("r1", 1, t), t).await.unwrap().await.unwrap();
        assert_eq!(g.decide("r1", 1, s(30)).await, Err("cooldown"));
        g.r.handle(&ev("r1", 1, s(61)), s(61))
            .await
            .unwrap()
            .await
            .unwrap();
        assert_eq!(
            g.decide("r1", 1, s(61 + 100)).await,
            Err("cooldown"),
            "second failure: 120 s"
        );
        assert_eq!(g.decide("r1", 1, s(61 + 121)).await, Ok(0));
        assert_eq!(plays(&g.store).await.len(), 2);
    }

    /// A speaker that answers 486 or 480 is busy on another call: a skip,
    /// not a fault.
    #[tokio::test]
    async fn a_busy_answer_from_the_speaker_is_a_busy_skip_not_a_failure() {
        for code in ["486 Busy Here", "480 Temporarily Unavailable"] {
            let (port, _uas) = fake_uas("NEVER", move |req| {
                if req.starts_with("INVITE") {
                    vec![reply(req, code, "", "")]
                } else {
                    vec![]
                }
            })
            .await;
            let mut g = rig_with(talk_down(), false, port, bus()).await;
            let task =
                g.r.handle(&ev("r1", 1, Utc::now()), Utc::now())
                    .await
                    .unwrap();
            task.await.unwrap();
            assert!(g.r.d.activity.failing(Utc::now()).is_empty(), "{code}");
            let r = &g.r.d.activity.rules()[0];
            assert_eq!(
                (r.last_result, r.last_reason.as_deref()),
                ("skipped", Some("busy")),
                "{code}"
            );
            // It reached the speaker, so ADR-095 (f) still records and audits it.
            let p = &plays(&g.store).await[0];
            assert_eq!(
                (p.origin.as_str(), p.result.as_deref()),
                ("rule", Some("failed")),
                "{code}"
            );
            assert!(
                audits(&g.store)
                    .await
                    .iter()
                    .any(|a| a.action == "talk_down.auto_start"
                        && a.outcome == AuditOutcome::Failure),
                "{code}"
            );
        }
    }

    #[tokio::test]
    async fn the_cameras_stop_ends_a_rule_play() {
        let (port, _rtp) = speaker().await;
        let mut g = rig_with(talk_down(), false, port, bus()).await;
        ready(&g.store, U1, 80_000).await;
        let task =
            g.r.handle(&ev("r1", 1, Utc::now()), Utc::now())
                .await
                .unwrap();
        while plays(&g.store).await.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let stopped = g.sessions.stop_where(|_, camera, _| camera == Some(1));
        assert_eq!(stopped, [g.speaker.id]);
        task.await.unwrap();
        assert_eq!(plays(&g.store).await[0].result.as_deref(), Some("stopped"));
    }

    #[test]
    fn the_outcome_window_follows_the_rules_cooldown() {
        assert_eq!(outcome_window(0), Duration::seconds(60));
        assert_eq!(outcome_window(30_000), Duration::seconds(60));
        assert_eq!(outcome_window(120_000), Duration::seconds(135));
        assert!(outcome_window(u64::MAX) > Duration::days(1));
    }

    #[tokio::test]
    async fn the_outcome_is_persisted_when_the_rule_fires_again_in_the_window_else_cleared() {
        let g = rig(talk_down()).await;
        let row = |s: &str| NewPlay {
            session_id: s.into(),
            origin: "rule".into(),
            rule_id: Some("r1".into()),
            step: Some(1),
            event_id: None,
            camera_id: Some(1),
            speaker_id: g.speaker.id,
            audio_uid: None,
            mode: "stored".into(),
        };
        let a = g.store.insert_talkdown_play(&row("a")).await.unwrap();
        let b = g.store.insert_talkdown_play(&row("b")).await.unwrap();
        let key = ("r1".to_string(), g.speaker.id);
        let ended = Utc::now();
        let window = Duration::seconds(60);
        settle_outcome(&g.store, &g.r.seqs, &key, a, ended, window).await;
        g.r.seqs.lock().entry(key.clone()).or_default().last_fire =
            Some(ended + Duration::seconds(30));
        settle_outcome(&g.store, &g.r.seqs, &key, b, ended, window).await;
        let p = plays(&g.store).await;
        assert_eq!(p[1].outcome.as_deref(), Some("cleared"));
        assert_eq!(p[0].outcome.as_deref(), Some("persisted"));
    }

    #[tokio::test]
    async fn plays_older_than_90_days_are_pruned() {
        let g = rig(talk_down()).await;
        let row = NewPlay {
            session_id: "s".into(),
            origin: "manual".into(),
            rule_id: None,
            step: None,
            event_id: None,
            camera_id: Some(1),
            speaker_id: g.speaker.id,
            audio_uid: None,
            mode: "tone".into(),
        };
        let old = g.store.insert_talkdown_play(&row).await.unwrap();
        g.store.insert_talkdown_play(&row).await.unwrap();
        sqlx::query(
            "UPDATE talkdown_plays SET started_at = datetime('now', '-91 days') WHERE id = ?",
        )
        .bind(old)
        .execute(g.store.pool())
        .await
        .unwrap();
        assert_eq!(prune_plays(&g.store, Utc::now()).await, 1);
        assert_eq!(plays(&g.store).await.len(), 1);
    }
}

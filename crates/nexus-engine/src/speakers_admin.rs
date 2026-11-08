//! ADR-091 — admin API for IP speakers and each camera's talk-down speaker.
//!
//! Routes (admin-gated in [`crate::api::router`], beside the camera routes):
//!
//! * `GET|POST       /api/v1/admin/speakers`
//! * `GET|PUT|DELETE /api/v1/admin/speakers/{id}`
//! * `PUT            /api/v1/admin/speakers/{id}/cameras` — `{"camera_ids":[…]}`,
//!   the speaker's whole camera set
//! * `GET|PUT        /api/v1/admin/cameras/{camera_id}/talk-down` —
//!   `{"speaker_id": id | null}`; the GET also says whether the speaker is
//!   `busy`, the `session_id` this camera started on it, and which `modes`
//!   it can play
//! * `POST           /api/v1/admin/cameras/{camera_id}/talk-down` — play a
//!   [`Audio`] on the camera's speaker; answers once the speaker picks up,
//!   and for `live` audio once the edge has its `sdp_answer` for the SFU
//! * `POST           /api/v1/admin/cameras/{camera_id}/talk-down/stop` — end
//!   the session this camera started; an optional `{"session_id": …}` ends
//!   it only when it is that session
//! * `POST           /api/v1/admin/speakers/{id}/test` — play the chime and
//!   answer when it has finished
//! * `PUT            /api/v1/admin/speakers/{id}/output` —
//!   `{"volume": 0-100, "boost": bool, "auto_max_per_hour": 1-60}`; stored,
//!   and a session on the speaker follows without ending. A speaker with an
//!   `onvif_endpoint` gets the volume on the device itself (`hw_volume`
//!   true, the software level then stays at unity); one that refuses stays
//!   on software volume and the answer carries `hw_error`
//! * `POST           /api/v1/admin/cameras/{camera_id}/talk-down/volume` —
//!   `{"volume": 0-100, "boost"?: bool}`; only the session this camera
//!   started, only while it plays, never stored
//! * `GET            /api/v1/admin/talkdown/audio` — the core's audio
//!   library: `[{uid, name, description, kind, status, error, duration_ms,
//!   fleet_managed, has_previous}]`
//! * `POST           /api/v1/admin/talkdown/tts/preview` — `{text, voice}`;
//!   the rendered `audio/wav`. Local only: no RPC action maps it
//! * `POST           /api/v1/admin/talkdown/tts/preview-upload` —
//!   `{text, voice, upload_url}`; renders, PUTs the WAV to the cloud's write
//!   SAS `upload_url` (never logged or echoed) and answers
//!   `{sha256, duration_ms}`
//! * `PUT            /api/v1/admin/talkdown/arming`,
//!   `PUT            /api/v1/admin/speakers/{id}/arming` and
//!   `PUT            /api/v1/admin/cameras/{camera_id}/talk-down/arming` —
//!   `{"armed": bool, "rearm_at"?: rfc3339, "reason"?: 1-200 chars}` for
//!   automatic (rule) plays, core-wide, on one speaker, or on the camera's
//!   speaker (ADR-095). A disarm with no `rearm_at`, or one more than 24 h
//!   ahead, needs a `reason`; arming clears both. Audited.
//! * `GET            /api/v1/admin/talkdown/status` — caps, voices, arming,
//!   cloud allowance, speakers, coverage, library readiness, each rule's
//!   last attempt and 24 h / 7 d play stats (outcomes are estimates)
//! * `GET            /api/v1/admin/talkdown/plays?event_id=&camera_id=&since=&limit=`
//!   — the play history, newest first
//!
//! The talk-down GET also carries the playing session's `origin`
//! (`manual`/`rule`), `rule_id` and `rule_name`, and the speaker's `armed`,
//! `rearm_at` and `disarm_reason`. A start body may name the alert it
//! answers (`event_id`), which its play row records; every camera start is
//! a `talkdown_plays` row with `origin` `manual`, whatever the body says.
//!
//! A session ends as soon as its camera and speaker part: a delete of the
//! speaker, an assignment that moves the camera off it, or an edit of the
//! speaker (the session dialled it as it was).
//!
//! A talk-down action refused for a reason the cloud acts on answers
//! [`Refusal`]'s `{"error": code, "message": …}`: `talk_down_unsupported`,
//! `talk_down_busy`, `audio_not_found` (404), `audio_not_ready` (409),
//! `speaker_unreachable`, `talk_down_negotiation_failed`, `tts_unavailable`
//! (409), `tts_invalid` (422), `tts_failed` (502) or `preview_upload_failed`
//! (502).
//!
//! A create or update body is a [`SpeakerConfig`] without `id`. On an update
//! an absent `password` keeps the stored one and `""` clears it. The password
//! stays on the edge: responses, audit rows and bus events carry
//! `has_password` only.

use std::net::SocketAddr;

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use nexus_bus::{topic, BusExt};
use nexus_config::{SpeakerConfig, SpeakerDriver, SpeakerId};
use nexus_store::{
    ArmingState, AudioStatus, AuditOutcome, NewPlay, PlayFilter, SpeakerOutput, SqliteTx, Store,
    StoreError, TalkdownPlay,
};
use nexus_types::CameraId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{ApiError, ApiState};
use crate::auth::admin_audit::{audit_admin_action, audit_admin_action_in_tx};
use crate::auth::require_role::SessionContext;
use crate::talkdown::health::SpeakerStatus;
use crate::talkdown::responder::{finish_play, started_at};
use crate::talkdown::session::{Audio, Origin, StartError, StartRequest};
use crate::talkdown::tts;

/// A speaker as the API shows it: no password, only whether one is set.
#[derive(Serialize)]
pub(crate) struct SpeakerView {
    id: SpeakerId,
    name: String,
    host: String,
    port: u16,
    driver: SpeakerDriver,
    username: Option<String>,
    has_password: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    onvif_endpoint: Option<String>,
    camera_ids: Vec<CameraId>,
    /// The last probe of the speaker; only on the GETs, and absent until the
    /// first probe has run.
    #[serde(skip_serializing_if = "Option::is_none")]
    health: Option<SpeakerStatus>,
    /// The stored output, for the console's sliders; only on the GETs.
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<SpeakerOutput>,
}

impl SpeakerView {
    fn new(sp: &SpeakerConfig, camera_ids: Vec<CameraId>) -> Self {
        Self {
            id: sp.id,
            name: sp.name.clone(),
            host: sp.host.clone(),
            port: sp.port,
            driver: sp.driver,
            username: sp.username.clone(),
            has_password: sp.password.is_some(),
            onvif_endpoint: sp.onvif_endpoint.clone(),
            camera_ids,
            health: None,
            output: None,
        }
    }

    fn audit_json(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }
}

#[derive(Deserialize)]
pub(crate) struct CameraIdsInput {
    camera_ids: Vec<CameraId>,
}

#[derive(Deserialize)]
pub(crate) struct TalkDownInput {
    speaker_id: Option<SpeakerId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutputInput {
    volume: i64,
    boost: bool,
    auto_max_per_hour: i64,
}

/// A start body: the [`Audio`] and, optionally, the alert it answers. Any
/// other field (an `origin`, a `rule_id`) is ignored: a camera start is
/// always manual (ADR-095 (a), (g)).
#[derive(Deserialize)]
pub(crate) struct StartBody {
    #[serde(flatten)]
    audio: Audio,
    #[serde(default)]
    event_id: Option<Uuid>,
}

/// An arming change for automatic plays (ADR-095).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArmingInput {
    armed: bool,
    #[serde(default)]
    rearm_at: Option<DateTime<Utc>>,
    #[serde(default)]
    reason: Option<String>,
}

/// The longest disarm that needs no reason.
const DISARM_WITHOUT_REASON: Duration = Duration::hours(24);

impl ArmingInput {
    /// The state to store: arming clears the re-arm time and the reason;
    /// a disarm with no re-arm time, or one more than 24 h ahead, needs a
    /// reason of 1-200 characters.
    fn checked(self, now: DateTime<Utc>) -> Result<ArmingState, Refusal> {
        let bad = |why: &str| Refusal(StatusCode::BAD_REQUEST, "bad_request", why.into());
        let reason = self.reason.map(|r| r.trim().to_string());
        if reason
            .as_ref()
            .is_some_and(|r| !(1..=200).contains(&r.chars().count()))
        {
            return Err(bad("reason must be 1 to 200 characters"));
        }
        if self.armed {
            return Ok(ArmingState {
                armed: true,
                rearm_at: None,
                reason: None,
            });
        }
        if self.rearm_at.is_some_and(|t| t <= now) {
            return Err(bad("rearm_at must be in the future"));
        }
        let long = self
            .rearm_at
            .is_none_or(|t| t - now > DISARM_WITHOUT_REASON);
        if long && reason.is_none() {
            return Err(bad(
                "a disarm without a re-arm time within 24 hours needs a reason",
            ));
        }
        Ok(ArmingState {
            armed: false,
            rearm_at: self.rearm_at,
            reason,
        })
    }
}

fn arming_json(a: &ArmingState) -> serde_json::Value {
    serde_json::json!({ "armed": a.armed, "rearm_at": a.rearm_at, "reason": a.reason })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlaysQuery {
    event_id: Option<String>,
    camera_id: Option<CameraId>,
    since: Option<DateTime<Utc>>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VolumeInput {
    volume: i64,
    boost: Option<bool>,
}

/// `value` as a `u8` within `range`, else a 400 naming `what`.
fn in_range(what: &str, value: i64, range: std::ops::RangeInclusive<u8>) -> Result<u8, ApiError> {
    u8::try_from(value)
        .ok()
        .filter(|v| range.contains(v))
        .ok_or_else(|| {
            bad_request(format!(
                "{what} must be {} to {}",
                range.start(),
                range.end()
            ))
        })
}

/// A misspelled field is a 400, not a stop of every session on the camera.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopInput {
    session_id: Option<Uuid>,
}

/// Who asked, for the audit rows.
struct Caller<'a> {
    s: &'a ApiState,
    session: Option<SessionContext>,
    headers: HeaderMap,
    peer: SocketAddr,
}

impl Caller<'_> {
    /// The camera handlers' tx-merge (`api::upsert_camera`): the change and
    /// its audit row commit together, and a change that fails still leaves a
    /// failure row.
    async fn commit(
        &self,
        change: Result<SqliteTx<'_>, StoreError>,
        action: &str,
        kind: &str,
        id: Option<&str>,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<(), StoreError> {
        let res = async {
            let mut tx = change?;
            audit_admin_action_in_tx(
                &self.s.store,
                &mut tx,
                self.session.as_ref(),
                &self.headers,
                self.peer.ip(),
                action,
                kind,
                id,
                before,
                after,
            )
            .await?;
            Store::commit_tx(tx).await
        }
        .await;
        if res.is_err() {
            audit_admin_action(
                &self.s.store,
                self.session.as_ref(),
                &self.headers,
                self.peer.ip(),
                action,
                kind,
                id,
                AuditOutcome::Failure,
                before,
                after,
            )
            .await;
        }
        res
    }

    /// One audit row for a talk-down action, which changes no stored state:
    /// `after` on success, the refusal's code on failure.
    async fn audit(
        &self,
        action: &str,
        kind: &str,
        id: &str,
        outcome: Result<&serde_json::Value, &Refusal>,
    ) {
        let (outcome, after) = match outcome {
            Ok(after) => (AuditOutcome::Success, after.to_string()),
            Err(r) => (
                AuditOutcome::Failure,
                serde_json::json!({ "error": r.1 }).to_string(),
            ),
        };
        audit_admin_action(
            &self.s.store,
            self.session.as_ref(),
            &self.headers,
            self.peer.ip(),
            action,
            kind,
            Some(id),
            outcome,
            None,
            Some(&after),
        )
        .await;
    }
}

/// A talk-down refusal: `{"error": code, "message": …}`.
pub(crate) struct Refusal(StatusCode, &'static str, String);

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.1, "message": self.2 });
        (self.0, Json(body)).into_response()
    }
}

impl From<ApiError> for Refusal {
    fn from(ApiError(status, message): ApiError) -> Self {
        let code = if status == StatusCode::NOT_FOUND {
            "not_found"
        } else {
            "internal"
        };
        Self(status, code, message)
    }
}

impl From<StoreError> for Refusal {
    fn from(e: StoreError) -> Self {
        ApiError::from(e).into()
    }
}

impl From<StartError> for Refusal {
    fn from(e: StartError) -> Self {
        match e {
            StartError::Busy => Self(
                StatusCode::CONFLICT,
                "talk_down_busy",
                "the speaker is already playing".into(),
            ),
            StartError::Unpaired => Self(
                StatusCode::CONFLICT,
                "talk_down_unsupported",
                "the camera's talk-down speaker changed".into(),
            ),
            StartError::Stopped => Self(
                StatusCode::CONFLICT,
                "talk_down_unsupported",
                "the session was stopped before it played".into(),
            ),
            StartError::AudioNotFound => Self(
                StatusCode::NOT_FOUND,
                "audio_not_found",
                "no talk-down audio has that uid".into(),
            ),
            StartError::AudioNotReady => Self(
                StatusCode::CONFLICT,
                "audio_not_ready",
                "the talk-down audio has not finished downloading".into(),
            ),
            StartError::Internal(why) => Self(StatusCode::INTERNAL_SERVER_ERROR, "internal", why),
            StartError::TtsUnavailable => Self(
                StatusCode::CONFLICT,
                "tts_unavailable",
                "text-to-speech is not available on this core".into(),
            ),
            StartError::TtsInvalid(why) => {
                Self(StatusCode::UNPROCESSABLE_ENTITY, "tts_invalid", why)
            }
            StartError::TtsFailed(why) => Self(StatusCode::BAD_GATEWAY, "tts_failed", why),
            StartError::Call(e) => Self(
                StatusCode::BAD_GATEWAY,
                "speaker_unreachable",
                e.to_string(),
            ),
            #[cfg(not(feature = "gstreamer-webrtc"))]
            StartError::LiveUnsupported => Self(
                StatusCode::CONFLICT,
                "talk_down_unsupported",
                "live audio is not built into this engine".into(),
            ),
            #[cfg(feature = "gstreamer-webrtc")]
            StartError::Negotiation(why) => {
                Self(StatusCode::BAD_GATEWAY, "talk_down_negotiation_failed", why)
            }
        }
    }
}

/// A store `NotFound` is the caller's mistake, answered with `status`.
fn refused(status: StatusCode) -> impl FnOnce(StoreError) -> ApiError {
    move |e| match e {
        StoreError::NotFound(what) => ApiError(status, format!("unknown {what}")),
        e => e.into(),
    }
}

fn bad_request(why: String) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, why)
}

async fn stored(s: &ApiState, id: SpeakerId) -> Result<SpeakerConfig, ApiError> {
    s.store
        .get_speaker(id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("unknown speaker {id}")))
}

async fn view(s: &ApiState, sp: &SpeakerConfig) -> Result<SpeakerView, ApiError> {
    Ok(SpeakerView::new(
        sp,
        s.store.speaker_camera_ids(sp.id).await?,
    ))
}

/// [`view`] with the speaker's last probe and its stored output, for the
/// GETs. Not for the audit rows, which record the speaker's configuration
/// only.
async fn view_with_health(s: &ApiState, sp: &SpeakerConfig) -> Result<SpeakerView, ApiError> {
    let mut v = view(s, sp).await?;
    v.health = s.health.speaker_health().get(sp.id);
    v.output = Some(s.store.speaker_output(sp.id).await?);
    Ok(v)
}

async fn publish(s: &ApiState, event: serde_json::Value) {
    let _ = s.bus.publish(topic::CONFIG_CHANGED, &event).await;
}

fn talk_down(camera_id: CameraId, sp: Option<&SpeakerConfig>) -> serde_json::Value {
    serde_json::json!({
        "camera_id": camera_id,
        "speaker": sp.map(|sp| serde_json::json!({
            "id": sp.id,
            "name": sp.name,
            "driver": sp.driver,
        })),
    })
}

pub(crate) async fn list_speakers(
    State(s): State<ApiState>,
) -> Result<Json<Vec<SpeakerView>>, ApiError> {
    let mut out = Vec::new();
    for sp in s.store.list_speakers().await? {
        out.push(view_with_health(&s, &sp).await?);
    }
    Ok(Json(out))
}

pub(crate) async fn get_speaker(
    State(s): State<ApiState>,
    Path(id): Path<SpeakerId>,
) -> Result<Json<SpeakerView>, ApiError> {
    let sp = stored(&s, id).await?;
    Ok(Json(view_with_health(&s, &sp).await?))
}

pub(crate) async fn create_speaker(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    Json(mut sp): Json<SpeakerConfig>,
) -> Result<Json<SpeakerView>, ApiError> {
    sp.id = 0;
    sp.password = sp.password.filter(|p| !p.is_empty());
    sp.validate().map_err(bad_request)?;
    let change: Result<SqliteTx<'_>, StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store.create_speaker_tx(&mut tx, &mut sp).await?;
        Ok(tx)
    }
    .await;
    let out = SpeakerView::new(&sp, Vec::new());
    let id = (sp.id != 0).then(|| sp.id.to_string());
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    caller
        .commit(
            change,
            "speaker.create",
            "speaker",
            id.as_deref(),
            None,
            out.audit_json().as_deref(),
        )
        .await?;
    publish(
        &s,
        serde_json::json!({ "kind": "speaker", "action": "create", "speaker_id": sp.id }),
    )
    .await;
    Ok(Json(out))
}

pub(crate) async fn update_speaker(
    State(s): State<ApiState>,
    Path(id): Path<SpeakerId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    Json(mut sp): Json<SpeakerConfig>,
) -> Result<Json<SpeakerView>, ApiError> {
    let before = stored(&s, id).await?;
    sp.id = id;
    sp.password = match sp.password {
        None => before.password.clone(),
        Some(p) if p.is_empty() => None,
        p => p,
    };
    sp.validate().map_err(bad_request)?;
    let before = view(&s, &before).await?;
    let out = SpeakerView::new(&sp, before.camera_ids.clone());
    let change: Result<SqliteTx<'_>, StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store.update_speaker_tx(&mut tx, &sp).await?;
        Ok(tx)
    }
    .await;
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    caller
        .commit(
            change,
            "speaker.update",
            "speaker",
            Some(&id.to_string()),
            before.audit_json().as_deref(),
            out.audit_json().as_deref(),
        )
        .await
        .map_err(refused(StatusCode::NOT_FOUND))?;
    s.talkdown.stop_where(|speaker_id, _, _| speaker_id == id);
    publish(
        &s,
        serde_json::json!({ "kind": "speaker", "action": "update", "speaker_id": id }),
    )
    .await;
    Ok(Json(out))
}

pub(crate) async fn delete_speaker(
    State(s): State<ApiState>,
    Path(id): Path<SpeakerId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
) -> Result<StatusCode, ApiError> {
    let before = view(&s, &stored(&s, id).await?).await?;
    let change: Result<SqliteTx<'_>, StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store.delete_speaker_tx(&mut tx, id).await?;
        Ok(tx)
    }
    .await;
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    caller
        .commit(
            change,
            "speaker.delete",
            "speaker",
            Some(&id.to_string()),
            before.audit_json().as_deref(),
            None,
        )
        .await
        .map_err(refused(StatusCode::NOT_FOUND))?;
    s.talkdown.stop_where(|speaker_id, _, _| speaker_id == id);
    publish(
        &s,
        serde_json::json!({ "kind": "speaker", "action": "delete", "speaker_id": id }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Set semantics: the listed cameras move to this speaker, and a camera it
/// had that is not listed is unassigned. An unknown camera is a 400.
pub(crate) async fn put_speaker_cameras(
    State(s): State<ApiState>,
    Path(id): Path<SpeakerId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    Json(body): Json<CameraIdsInput>,
) -> Result<Json<SpeakerView>, ApiError> {
    let sp = stored(&s, id).await?;
    let before = view(&s, &sp).await?;
    let mut camera_ids = body.camera_ids;
    camera_ids.sort_unstable();
    camera_ids.dedup();
    let out = SpeakerView::new(&sp, camera_ids);
    let change: Result<SqliteTx<'_>, StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store
            .set_speaker_cameras_tx(&mut tx, id, &out.camera_ids)
            .await?;
        Ok(tx)
    }
    .await;
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    caller
        .commit(
            change,
            "speaker.assign_cameras",
            "speaker",
            Some(&id.to_string()),
            before.audit_json().as_deref(),
            out.audit_json().as_deref(),
        )
        .await
        .map_err(refused(StatusCode::BAD_REQUEST))?;
    // A listed camera's session on another speaker, and this speaker's
    // session from a camera no longer listed, have lost their pairing.
    s.talkdown.stop_where(|speaker_id, camera, _| {
        camera.is_some_and(|c| out.camera_ids.contains(&c) != (speaker_id == id))
    });
    publish(
        &s,
        serde_json::json!({ "kind": "speaker", "action": "assign_cameras", "speaker_id": id }),
    )
    .await;
    Ok(Json(out))
}

/// 404 for an unknown camera.
async fn known_camera(s: &ApiState, camera_id: CameraId) -> Result<(), ApiError> {
    let (readable, unreadable) = s.store.list_readable_cameras().await?;
    if !readable.iter().any(|c| c.id == camera_id) && !unreadable.contains(&camera_id) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("unknown camera {camera_id}"),
        ));
    }
    Ok(())
}

/// The camera's talk-down speaker; 404 for an unknown camera.
async fn camera_speaker(
    s: &ApiState,
    camera_id: CameraId,
) -> Result<Option<SpeakerConfig>, ApiError> {
    known_camera(s, camera_id).await?;
    Ok(s.store.speaker_for_camera(camera_id).await?)
}

pub(crate) async fn get_talk_down(
    State(s): State<ApiState>,
    Path(camera_id): Path<CameraId>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sp = camera_speaker(&s, camera_id).await?;
    let mut out = talk_down(camera_id, sp.as_ref());
    out["busy"] = sp
        .as_ref()
        .is_some_and(|sp| s.talkdown.is_busy(sp.id))
        .into();
    // The session on the speaker, only when this camera started it.
    out["session_id"] = sp
        .as_ref()
        .and_then(|sp| s.talkdown.session_of(sp.id))
        .filter(|(camera, _)| *camera == Some(camera_id))
        .map(|(_, id)| id.to_string())
        .into();
    out["modes"] = serde_json::json!(s.talkdown.modes());
    // Who is playing on the speaker, so an operator can tell a rule play
    // from a person's.
    let origin = sp.as_ref().and_then(|sp| s.talkdown.origin_of(sp.id));
    let rule_id = match &origin {
        Some(Origin::Rule { rule_id, .. }) => Some(rule_id.clone()),
        _ => None,
    };
    let rule_name = match &rule_id {
        Some(id) => s
            .store
            .list_rules()
            .await?
            .into_iter()
            .find(|r| &r.id == id)
            .map(|r| r.name),
        None => None,
    };
    out["origin"] = origin
        .map(|o| match o {
            Origin::Manual => "manual",
            Origin::Rule { .. } => "rule",
        })
        .into();
    out["rule_id"] = rule_id.into();
    out["rule_name"] = rule_name.into();
    let arming = match &sp {
        Some(sp) => s.store.speaker_arming(sp.id).await?,
        None => None,
    };
    out["armed"] = arming.as_ref().map(|a| a.armed).into();
    out["rearm_at"] = serde_json::json!(arming.as_ref().and_then(|a| a.rearm_at));
    out["disarm_reason"] = arming.and_then(|a| a.reason).into();
    Ok(Json(out))
}

pub(crate) async fn put_talk_down(
    State(s): State<ApiState>,
    Path(camera_id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    Json(body): Json<TalkDownInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = match body.speaker_id {
        Some(id) => Some(stored(&s, id).await?),
        None => None,
    };
    let before = talk_down(
        camera_id,
        s.store.speaker_for_camera(camera_id).await?.as_ref(),
    );
    let out = talk_down(camera_id, target.as_ref());
    let change: Result<SqliteTx<'_>, StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store
            .set_camera_speaker_tx(&mut tx, camera_id, body.speaker_id)
            .await?;
        Ok(tx)
    }
    .await;
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    caller
        .commit(
            change,
            "camera.talk_down_assign",
            "camera",
            Some(&camera_id.to_string()),
            Some(&before.to_string()),
            Some(&out.to_string()),
        )
        .await
        .map_err(refused(StatusCode::NOT_FOUND))?;
    s.talkdown.stop_where(|speaker_id, camera, _| {
        camera == Some(camera_id) && Some(speaker_id) != body.speaker_id
    });
    publish(
        &s,
        serde_json::json!({
            "kind": "speaker",
            "action": "talk_down_assign",
            "camera_id": camera_id,
            "speaker_id": body.speaker_id,
        }),
    )
    .await;
    Ok(Json(out))
}

pub(crate) async fn start_talk_down(
    State(s): State<ApiState>,
    Path(camera_id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    body: Result<Json<StartBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, Refusal> {
    let res = async {
        let sp = camera_speaker(&s, camera_id).await?.ok_or_else(|| {
            Refusal(
                StatusCode::CONFLICT,
                "talk_down_unsupported",
                format!("camera {camera_id} has no talk-down speaker"),
            )
        })?;
        let Json(StartBody { audio, event_id }) =
            body.map_err(|e| Refusal(StatusCode::BAD_REQUEST, "bad_request", e.body_text()))?;
        if let Audio::Stored {
            volume: Some(101..),
            ..
        }
        | Audio::Tts {
            volume: Some(101..),
            ..
        } = audio
        {
            return Err(Refusal(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "volume must be 0 to 100".into(),
            ));
        }
        let event_id = event_id.map(|e| e.to_string());
        let (session_id, sdp_answer) = start_on(&s, camera_id, &sp, &audio, event_id).await?;
        let mut started = serde_json::json!({
            "session_id": session_id,
            "speaker_id": sp.id,
            "camera_id": camera_id,
            "mode": audio.mode(),
        });
        // Typed speech is audited with what the speaker said (as spoken).
        if let Audio::Tts { text, .. } = &audio {
            started["text"] = tts::validate_text(text, tts::ADHOC_MAX_CHARS)
                .unwrap_or_default()
                .into();
        }
        Ok::<_, Refusal>((started, sdp_answer))
    }
    .await;
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    let id = camera_id.to_string();
    // The audit row leaves out the SDP answer: it carries ICE credentials.
    caller
        .audit(
            "talk_down.start",
            "camera",
            &id,
            res.as_ref().map(|(started, _)| started),
        )
        .await;
    res.map(|(mut started, sdp_answer)| {
        if let Some(sdp) = sdp_answer {
            started["sdp_answer"] = sdp.into();
        }
        Json(started)
    })
}

/// The speaker's output. A speaker deleted since the caller read it is the
/// same refusal as a parting, which the claim's re-read would have made.
async fn output_of(s: &ApiState, sp: &SpeakerConfig) -> Result<SpeakerOutput, Refusal> {
    s.store.speaker_output(sp.id).await.map_err(|e| match e {
        StoreError::NotFound(_) => StartError::Unpaired.into(),
        e => e.into(),
    })
}

/// Play `audio` on `sp`, which the caller read as the camera's speaker;
/// refused once the camera no longer pairs with it as read (an edit too).
/// The play is a `talkdown_plays` row (origin `manual`, linked to
/// `event_id`), finished when the play ends.
pub(crate) async fn start_on(
    s: &ApiState,
    camera_id: CameraId,
    sp: &SpeakerConfig,
    audio: &Audio,
    event_id: Option<String>,
) -> Result<(Uuid, Option<String>), Refusal> {
    // A failed re-read cannot confirm the pairing: refuse rather than call.
    let paired = async {
        matches!(s.store.speaker_for_camera(camera_id).await, Ok(Some(now)) if now == *sp)
    };
    let req = StartRequest {
        speaker: sp,
        output: output_of(s, sp).await?,
        camera_id: Some(camera_id),
        origin: Origin::Manual,
        audio,
        event_id: event_id.clone(),
    };
    let (session_id, sdp_answer, playing) = s.talkdown.start(req, paired).await?;
    let play = NewPlay {
        session_id: session_id.to_string(),
        origin: "manual".into(),
        rule_id: None,
        step: None,
        event_id,
        camera_id: Some(camera_id),
        speaker_id: sp.id,
        audio_uid: match audio {
            Audio::Stored { audio_uid, .. } => Some(audio_uid.clone()),
            _ => None,
        },
        mode: audio.mode().into(),
    };
    let store = s.store.clone();
    tokio::spawn(async move {
        match store.insert_talkdown_play(&play).await {
            Ok(id) => {
                finish_play(&store, id, playing).await;
            }
            Err(e) => tracing::warn!(error = %e, "talk-down: could not record a play"),
        }
    });
    Ok((session_id, sdp_answer))
}

/// End the session this camera started, on whichever speaker it plays; a
/// session another camera started on a shared speaker is not this camera's
/// to end. A body naming a `session_id` ends only that session, so a late
/// stop for a session that has ended leaves the camera's next one playing.
/// Idempotent: 204 whether or not there was one.
pub(crate) async fn stop_talk_down(
    State(s): State<ApiState>,
    Path(camera_id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    body: Bytes,
) -> Result<StatusCode, Refusal> {
    let res = async {
        let StopInput { session_id } = if body.is_empty() {
            StopInput::default()
        } else {
            serde_json::from_slice(&body)
                .map_err(|e| Refusal(StatusCode::BAD_REQUEST, "bad_request", e.to_string()))?
        };
        known_camera(&s, camera_id).await?;
        let stopped = s.talkdown.stop_where(|_, camera, id| {
            camera == Some(camera_id) && session_id.is_none_or(|want| want == id)
        });
        let mut out = serde_json::json!({ "speaker_ids": stopped, "stopped": !stopped.is_empty() });
        if let Some(id) = session_id {
            out["session_id"] = serde_json::json!(id);
        }
        Ok(out)
    }
    .await;
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    let id = camera_id.to_string();
    caller
        .audit("talk_down.stop", "camera", &id, res.as_ref())
        .await;
    res.map(|_| StatusCode::NO_CONTENT)
}

/// The answer to a `PUT …/output`: the stored output, and why the device
/// would not take the volume when the speaker has an `onvif_endpoint` and
/// refused.
#[derive(Serialize)]
pub(crate) struct OutputView {
    #[serde(flatten)]
    output: SpeakerOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    hw_error: Option<String>,
}

/// Set the speaker's own output level to `volume` (0-100 of the device's
/// `OutputLevelRange`). The error is the device's refusal; it never carries
/// the speaker's password, which only feeds the WS-Security digest.
async fn set_device_volume(sp: &SpeakerConfig, endpoint: &str, volume: u8) -> Result<(), String> {
    use crate::discovery::onvif_media::{get_audio_output_level, set_audio_output_level};
    let user = sp.username.as_deref().unwrap_or_default();
    let pass = sp.password.as_deref().unwrap_or_default();
    let cfg = get_audio_output_level(endpoint, user, pass).await?;
    set_audio_output_level(endpoint, user, pass, &cfg, cfg.level_for(volume)).await
}

/// Store the speaker's output. It is not a speaker edit: a session on the
/// speaker keeps playing and follows the new level. A speaker with an
/// `onvif_endpoint` takes the volume on the device (`hw_volume`); when it
/// refuses, the software level stays in charge and `hw_error` says why.
pub(crate) async fn put_speaker_output(
    State(s): State<ApiState>,
    Path(id): Path<SpeakerId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    body: Result<Json<OutputInput>, JsonRejection>,
) -> Result<Json<OutputView>, ApiError> {
    let Json(body) = body.map_err(|e| bad_request(e.body_text()))?;
    let sp = stored(&s, id).await?;
    let before = s
        .store
        .speaker_output(id)
        .await
        .map_err(refused(StatusCode::NOT_FOUND))?;
    let volume = in_range("volume", body.volume, 0..=100)?;
    let auto_max_per_hour = in_range("auto_max_per_hour", body.auto_max_per_hour, 1..=60)?;
    let (hw_volume, hw_error) = match sp.onvif_endpoint.as_deref() {
        None => (false, None),
        Some(endpoint) => match set_device_volume(&sp, endpoint, volume).await {
            Ok(()) => (true, None),
            Err(e) => {
                tracing::warn!(speaker_id = id, error = %e, "speaker refused its hardware volume");
                (false, Some(e))
            }
        },
    };
    let out = SpeakerOutput {
        volume,
        boost: body.boost,
        hw_volume,
        auto_max_per_hour,
    };
    let change: Result<SqliteTx<'_>, StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store.set_speaker_output_tx(&mut tx, id, &out).await?;
        Ok(tx)
    }
    .await;
    let json = |o: &SpeakerOutput| serde_json::to_string(o).ok();
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    caller
        .commit(
            change,
            "speaker.output",
            "speaker",
            Some(&id.to_string()),
            json(&before).as_deref(),
            json(&out).as_deref(),
        )
        .await
        .map_err(refused(StatusCode::NOT_FOUND))?;
    s.talkdown.set_speaker_output(id, &out);
    publish(
        &s,
        serde_json::json!({ "kind": "speaker", "action": "output", "speaker_id": id }),
    )
    .await;
    Ok(Json(OutputView {
        output: out,
        hw_error,
    }))
}

/// The live slider: the volume of the session this camera started on its
/// speaker, for as long as it plays. Not stored. `boost` defaults to the
/// speaker's. 404 when the camera has no session of its own.
pub(crate) async fn set_talk_down_volume(
    State(s): State<ApiState>,
    Path(camera_id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    body: Result<Json<VolumeInput>, JsonRejection>,
) -> Result<Json<serde_json::Value>, Refusal> {
    let res = async {
        let Json(body) =
            body.map_err(|e| Refusal(StatusCode::BAD_REQUEST, "bad_request", e.body_text()))?;
        let volume = in_range("volume", body.volume, 0..=100)
            .map_err(|ApiError(_, why)| Refusal(StatusCode::BAD_REQUEST, "bad_request", why))?;
        let sp = camera_speaker(&s, camera_id).await?;
        let boost = match (body.boost, &sp) {
            (Some(boost), _) => boost,
            (None, Some(sp)) => s.store.speaker_output(sp.id).await?.boost,
            (None, None) => false,
        };
        if !s.talkdown.set_session_volume(camera_id, volume, boost) {
            return Err(Refusal(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("camera {camera_id} has no talk-down session"),
            ));
        }
        Ok::<_, Refusal>(serde_json::json!({
            "camera_id": camera_id,
            "volume": volume,
            "boost": boost,
        }))
    }
    .await;
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    caller
        .audit(
            "talk_down.volume",
            "camera",
            &camera_id.to_string(),
            res.as_ref(),
        )
        .await;
    res.map(Json)
}

/// The installer's check: no camera needed.
pub(crate) async fn test_speaker(
    State(s): State<ApiState>,
    Path(id): Path<SpeakerId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
) -> Result<Json<serde_json::Value>, Refusal> {
    let res = async { test_on(&s, &stored(&s, id).await?).await }.await;
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    caller
        .audit("speaker.test", "speaker", &id.to_string(), res.as_ref())
        .await;
    res.map(Json)
}

/// Play the chime on `sp`, which the caller read, and answer when it has
/// finished; refused once `sp` has been deleted or edited.
pub(crate) async fn test_on(
    s: &ApiState,
    sp: &SpeakerConfig,
) -> Result<serde_json::Value, Refusal> {
    // The claim's re-read, as in `start_on`: a delete or an edit commits
    // before its stop, which either finds this claim or this re-read sees it.
    let unchanged =
        async { matches!(s.store.get_speaker(sp.id).await, Ok(Some(now)) if now == *sp) };
    let req = StartRequest {
        speaker: sp,
        output: output_of(s, sp).await?,
        camera_id: None,
        origin: Origin::Manual,
        audio: &Audio::Tone,
        event_id: None,
    };
    let (_, _, playing) = s.talkdown.start(req, unchanged).await?;
    match playing.await {
        Ok(Ok(_)) => Ok(serde_json::json!({ "ok": true })),
        Ok(Err(e)) => Err(StartError::Call(e).into()),
        Err(e) => Err(Refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            e.to_string(),
        )),
    }
}

/// The core's audio library, samples left out. `fleet_managed` marks the
/// items the last `talkdown_audio` fleet apply owns.
pub(crate) async fn list_talkdown_audio(
    State(s): State<ApiState>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let managed = s
        .store
        .fleet_marker_get("talkdown_audio")
        .await?
        .and_then(|m| m.managed_keys)
        .unwrap_or_default();
    let items = s.store.list_talkdown_audio().await?;
    Ok(Json(
        items
            .iter()
            .map(|a| {
                serde_json::json!({
                    "uid": a.uid,
                    "name": a.name,
                    "description": a.description,
                    "kind": a.kind.as_str(),
                    "status": a.status.as_str(),
                    "error": a.error,
                    "duration_ms": a.duration_ms,
                    "fleet_managed": managed.contains(&a.uid),
                    "has_previous": a.has_previous,
                })
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreviewInput {
    text: String,
    voice: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreviewUploadInput {
    text: String,
    voice: String,
    /// A write SAS: a bearer credential, never logged or echoed.
    upload_url: String,
}

/// Render `text` in `voice` for a preview, under the library item limits.
async fn render_preview(s: &ApiState, text: &str, voice: &str) -> Result<Vec<i16>, Refusal> {
    let tts = s
        .talkdown
        .library()
        .and_then(|l| l.tts())
        .ok_or(StartError::TtsUnavailable)?;
    let text = tts::validate_text(text, tts::LIBRARY_MAX_CHARS).map_err(StartError::TtsInvalid)?;
    if tts.voice(Some(voice)).is_none() {
        return Err(StartError::TtsInvalid("unknown voice".into()).into());
    }
    Ok(tts
        .speak(voice, &text, tts::PREVIEW_TIMEOUT)
        .await
        .map_err(StartError::TtsFailed)?)
}

/// The local console's preview: the rendered WAV.
pub(crate) async fn tts_preview(
    State(s): State<ApiState>,
    body: Result<Json<PreviewInput>, JsonRejection>,
) -> Result<Response, Refusal> {
    let Json(input) =
        body.map_err(|e| Refusal(StatusCode::BAD_REQUEST, "bad_request", e.body_text()))?;
    let pcm = render_preview(&s, &input.text, &input.voice).await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "audio/wav")],
        tts::wav(&pcm),
    )
        .into_response())
}

/// The cloud's preview (ADR-091 rule 7: media moves only by SAS): render,
/// PUT the WAV to the cloud-minted write SAS, answer its digest and length.
pub(crate) async fn tts_preview_upload(
    State(s): State<ApiState>,
    body: Result<Json<PreviewUploadInput>, JsonRejection>,
) -> Result<Json<serde_json::Value>, Refusal> {
    // A parse error can quote the body, and the body holds the SAS.
    let Json(input) = body.map_err(|_| {
        Refusal(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "the body must be {text, voice, upload_url}".into(),
        )
    })?;
    let pcm = render_preview(&s, &input.text, &input.voice).await?;
    let file = tts::wav(&pcm);
    let sha256 = crate::fleet_apply::sha256_hex(&file);
    let allow_http = s.talkdown.library().is_some_and(|l| l.allow_http());
    tts::upload(&input.upload_url, file, allow_http)
        .await
        .map_err(|why| Refusal(StatusCode::BAD_GATEWAY, "preview_upload_failed", why))?;
    Ok(Json(serde_json::json!({
        "sha256": sha256,
        "duration_ms": pcm.len() / 8,
    })))
}

/// Arm or disarm automatic plays core-wide (ADR-095). Audited.
pub(crate) async fn put_talkdown_arming(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    body: Result<Json<ArmingInput>, JsonRejection>,
) -> Result<Json<serde_json::Value>, Refusal> {
    let res = async {
        let Json(body) =
            body.map_err(|e| Refusal(StatusCode::BAD_REQUEST, "bad_request", e.body_text()))?;
        let a = body.checked(Utc::now())?;
        s.store
            .set_talkdown_core_arming(a.armed, a.rearm_at, a.reason.as_deref())
            .await?;
        let mut out = arming_json(&a);
        out["scope"] = "core".into();
        Ok::<_, Refusal>(out)
    }
    .await;
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    caller
        .audit("talk_down.arming", "talkdown", "core", res.as_ref())
        .await;
    res.map(Json)
}

/// Arm or disarm automatic plays on one speaker, with or without a camera
/// (ADR-095 D8). 404 for an unknown speaker. Audited.
pub(crate) async fn put_speaker_arming(
    State(s): State<ApiState>,
    Path(id): Path<SpeakerId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    body: Result<Json<ArmingInput>, JsonRejection>,
) -> Result<Json<serde_json::Value>, Refusal> {
    let res = async {
        let Json(body) =
            body.map_err(|e| Refusal(StatusCode::BAD_REQUEST, "bad_request", e.body_text()))?;
        let a = body.checked(Utc::now())?;
        s.store
            .set_speaker_arming(id, a.armed, a.rearm_at, a.reason.as_deref())
            .await
            .map_err(refused(StatusCode::NOT_FOUND))?;
        let mut out = arming_json(&a);
        out["speaker_id"] = id.into();
        Ok::<_, Refusal>(out)
    }
    .await;
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    caller
        .audit("talk_down.arming", "speaker", &id.to_string(), res.as_ref())
        .await;
    res.map(Json)
}

/// Arm or disarm automatic plays on the camera's speaker (ADR-095). Audited.
pub(crate) async fn put_talk_down_arming(
    State(s): State<ApiState>,
    Path(camera_id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    body: Result<Json<ArmingInput>, JsonRejection>,
) -> Result<Json<serde_json::Value>, Refusal> {
    let res = async {
        let Json(body) =
            body.map_err(|e| Refusal(StatusCode::BAD_REQUEST, "bad_request", e.body_text()))?;
        let a = body.checked(Utc::now())?;
        let sp = camera_speaker(&s, camera_id).await?.ok_or_else(|| {
            Refusal(
                StatusCode::CONFLICT,
                "talk_down_unsupported",
                format!("camera {camera_id} has no talk-down speaker"),
            )
        })?;
        s.store
            .set_speaker_arming(sp.id, a.armed, a.rearm_at, a.reason.as_deref())
            .await
            .map_err(|e| match e {
                StoreError::NotFound(_) => StartError::Unpaired.into(),
                e => Refusal::from(e),
            })?;
        let mut out = arming_json(&a);
        out["camera_id"] = camera_id.into();
        out["speaker_id"] = sp.id.into();
        Ok::<_, Refusal>(out)
    }
    .await;
    let caller = Caller {
        s: &s,
        session,
        headers,
        peer,
    };
    caller
        .audit(
            "talk_down.arming",
            "camera",
            &camera_id.to_string(),
            res.as_ref(),
        )
        .await;
    res.map(Json)
}

fn play_json(p: &TalkdownPlay) -> serde_json::Value {
    serde_json::json!({
        "id": p.id,
        "session_id": p.session_id,
        "origin": p.origin,
        "rule_id": p.rule_id,
        "step": p.step,
        "event_id": p.event_id,
        "camera_id": p.camera_id,
        "speaker_id": p.speaker_id,
        "audio_uid": p.audio_uid,
        "mode": p.mode,
        "started_at": started_at(p),
        "ended_at": p.ended_at,
        "result": p.result,
        "reason": p.reason,
        // An estimate (ADR-095): whether the rule fired again soon after.
        "outcome": p.outcome,
        "outcome_estimated": p.outcome.is_some(),
    })
}

/// The play history, newest first; `limit` 1-500, default 100.
pub(crate) async fn list_talkdown_plays(
    State(s): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<PlaysQuery>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let limit = match q.limit {
        None => 100,
        Some(n @ 1..=500) => n,
        Some(_) => return Err(bad_request("limit must be 1 to 500".into())),
    };
    let plays = s
        .store
        .list_talkdown_plays(&PlayFilter {
            event_id: q.event_id,
            camera_id: q.camera_id,
            since: q.since,
            limit: Some(limit),
            ..Default::default()
        })
        .await?;
    Ok(Json(plays.iter().map(play_json).collect()))
}

/// Everything an admin needs to see whether talk-down works here: what
/// the core can do, arming and the cloud's allowance, each speaker's
/// health, which camera has which speaker, the library's readiness, each
/// rule's last attempt, and 24 h / 7 d rule-play stats (outcomes are
/// estimates; skips are counted since the engine started).
pub(crate) async fn talkdown_status(
    State(s): State<ApiState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let now = Utc::now();
    let core = s.store.talkdown_core().await?;
    let health = s.health.speaker_health();
    let mut speakers = Vec::new();
    let mut armed_speakers = Vec::new();
    for sp in s.store.list_speakers().await? {
        let probe = health.get(sp.id);
        let output = s.store.speaker_output(sp.id).await?;
        speakers.push(serde_json::json!({
            "id": sp.id,
            "name": sp.name,
            "online": probe.as_ref().map(|p| p.online),
            "since": probe.map(|p| p.since),
            "hw_volume": output.hw_volume,
        }));
        if let Some(a) = s.store.speaker_arming(sp.id).await? {
            let mut j = arming_json(&a);
            j["id"] = sp.id.into();
            armed_speakers.push(j);
        }
    }
    let (cameras, _) = s.store.list_readable_cameras().await?;
    let mut coverage = Vec::new();
    for c in &cameras {
        let speaker = s.store.speaker_for_camera(c.id).await?;
        coverage.push(serde_json::json!({
            "camera_id": c.id,
            "camera_name": c.name,
            "speaker_id": speaker.map(|sp| sp.id),
        }));
    }
    let items = s.store.list_talkdown_audio().await?;
    let count = |st: AudioStatus| items.iter().filter(|a| a.status == st).count();
    let plays = s
        .store
        .list_talkdown_plays(&PlayFilter {
            since: Some(now - Duration::days(7)),
            limit: Some(10_000),
            ..Default::default()
        })
        .await?;
    let activity = s.health.talkdown_activity();
    let stats = |since: DateTime<Utc>| {
        let rule: Vec<_> = plays
            .iter()
            .filter(|p| p.origin == "rule" && started_at(p).is_some_and(|t| t >= since))
            .collect();
        let outcome = |o: &str| {
            rule.iter()
                .filter(|p| p.outcome.as_deref() == Some(o))
                .count()
        };
        serde_json::json!({
            "plays": rule.len(),
            "cleared": outcome("cleared"),
            "persisted": outcome("persisted"),
            "skipped_by_reason": activity.skipped_since(since),
        })
    };
    let hw = s.store.any_hw_volume().await.unwrap_or(false);
    // The same flags the heartbeat advertises.
    let tts_available = tts::AVAILABLE.load(std::sync::atomic::Ordering::Relaxed);
    let caps: Vec<String> = crate::cloud_tunnel::heartbeat_caps(None, hw, tts_available)
        .into_iter()
        .filter(|c| c.starts_with("talkdown_"))
        .collect();
    // Empty only when this engine has no text-to-speech.
    let voices: Vec<_> = s
        .talkdown
        .library()
        .and_then(|l| l.tts())
        .map(|t| t.voices().to_vec())
        .unwrap_or_default()
        .into_iter()
        .map(|v| serde_json::json!({ "id": v.id, "label": v.label, "language": v.language }))
        .collect();
    Ok(Json(serde_json::json!({
        "caps": caps,
        "modes": s.talkdown.modes(),
        "voices": voices,
        "armed": {
            "core": {
                "armed": core.auto_armed,
                "rearm_at": core.auto_rearm_at,
                "reason": core.auto_disarm_reason,
            },
            "speakers": armed_speakers,
        },
        "cloud_allowed": core.cloud_allowed,
        "speakers": speakers,
        "coverage": coverage,
        "audio": {
            "ready": count(AudioStatus::Ready),
            "pending": count(AudioStatus::Pending),
            "failed": count(AudioStatus::Failed),
            "items": items.iter().map(|a| serde_json::json!({
                "uid": a.uid,
                "name": a.name,
                "kind": a.kind.as_str(),
                "status": a.status.as_str(),
                "error": a.error,
            })).collect::<Vec<_>>(),
        },
        "rules": activity.rules(),
        "stats": { "24h": stats(now - Duration::hours(24)), "7d": stats(now - Duration::days(7)) },
        "outcomes_estimated": true,
    })))
}

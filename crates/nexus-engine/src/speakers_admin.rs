//! ADR-090 — admin API for IP speakers and each camera's talk-down speaker.
//!
//! Routes (admin-gated in [`crate::api::router`], beside the camera routes):
//!
//! * `GET|POST       /api/v1/admin/speakers`
//! * `GET|PUT|DELETE /api/v1/admin/speakers/{id}`
//! * `PUT            /api/v1/admin/speakers/{id}/cameras` — `{"camera_ids":[…]}`,
//!   the speaker's whole camera set
//! * `GET|PUT        /api/v1/admin/cameras/{camera_id}/talk-down` —
//!   `{"speaker_id": id | null}`; the GET also says whether the speaker is
//!   `busy` and which `modes` it can play
//! * `POST           /api/v1/admin/cameras/{camera_id}/talk-down` — play a
//!   [`Audio`] on the camera's speaker; answers once the speaker picks up,
//!   and for `live` audio once the edge has its `sdp_answer` for the SFU
//! * `POST           /api/v1/admin/cameras/{camera_id}/talk-down/stop`
//! * `POST           /api/v1/admin/speakers/{id}/test` — play the chime and
//!   answer when it has finished
//!
//! A talk-down action refused for a reason the cloud acts on answers
//! [`Refusal`]'s `{"error": code, "message": …}`: `talk_down_unsupported`,
//! `talk_down_busy`, `clip_invalid`, `speaker_unreachable` or
//! `talk_down_negotiation_failed`.
//!
//! A create or update body is a [`SpeakerConfig`] without `id`. On an update
//! an absent `password` keeps the stored one and `""` clears it. The password
//! stays on the edge: responses, audit rows and bus events carry
//! `has_password` only.

use std::net::SocketAddr;

use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nexus_bus::{topic, BusExt};
use nexus_config::{SpeakerConfig, SpeakerDriver, SpeakerId};
use nexus_store::{AuditOutcome, SqliteTx, Store, StoreError};
use nexus_types::CameraId;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiState};
use crate::auth::admin_audit::{audit_admin_action, audit_admin_action_in_tx};
use crate::auth::require_role::SessionContext;
use crate::talkdown::session::{Audio, StartError, MODES};

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
    camera_ids: Vec<CameraId>,
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
            camera_ids,
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
            StartError::Clip(why) => Self(StatusCode::UNPROCESSABLE_ENTITY, "clip_invalid", why),
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
        out.push(view(&s, &sp).await?);
    }
    Ok(Json(out))
}

pub(crate) async fn get_speaker(
    State(s): State<ApiState>,
    Path(id): Path<SpeakerId>,
) -> Result<Json<SpeakerView>, ApiError> {
    let sp = stored(&s, id).await?;
    Ok(Json(view(&s, &sp).await?))
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
    publish(
        &s,
        serde_json::json!({ "kind": "speaker", "action": "assign_cameras", "speaker_id": id }),
    )
    .await;
    Ok(Json(out))
}

/// The camera's talk-down speaker; 404 for an unknown camera.
async fn camera_speaker(
    s: &ApiState,
    camera_id: CameraId,
) -> Result<Option<SpeakerConfig>, ApiError> {
    let (readable, unreadable) = s.store.list_readable_cameras().await?;
    if !readable.iter().any(|c| c.id == camera_id) && !unreadable.contains(&camera_id) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("unknown camera {camera_id}"),
        ));
    }
    Ok(s.store.speaker_for_camera(camera_id).await?)
}

pub(crate) async fn get_talk_down(
    State(s): State<ApiState>,
    Path(camera_id): Path<CameraId>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sp = camera_speaker(&s, camera_id).await?;
    let mut out = talk_down(camera_id, sp.as_ref());
    out["busy"] = sp.is_some_and(|sp| s.talkdown.is_busy(sp.id)).into();
    out["modes"] = serde_json::json!(MODES);
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
    body: Result<Json<Audio>, JsonRejection>,
) -> Result<Json<serde_json::Value>, Refusal> {
    let res = async {
        let sp = camera_speaker(&s, camera_id).await?.ok_or_else(|| {
            Refusal(
                StatusCode::CONFLICT,
                "talk_down_unsupported",
                format!("camera {camera_id} has no talk-down speaker"),
            )
        })?;
        let Json(audio) =
            body.map_err(|e| Refusal(StatusCode::BAD_REQUEST, "bad_request", e.body_text()))?;
        let (session_id, sdp_answer, _playing) =
            s.talkdown.start(&sp, Some(camera_id), &audio).await?;
        let started = serde_json::json!({
            "session_id": session_id,
            "speaker_id": sp.id,
            "camera_id": camera_id,
            "mode": audio.mode(),
        });
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

/// Idempotent: 204 whether or not the speaker was playing.
pub(crate) async fn stop_talk_down(
    State(s): State<ApiState>,
    Path(camera_id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
) -> Result<StatusCode, Refusal> {
    let res = camera_speaker(&s, camera_id)
        .await
        .map(|sp| {
            let speaker_id = sp.map(|sp| sp.id);
            let stopped = speaker_id.is_some_and(|id| s.talkdown.stop(id));
            serde_json::json!({ "speaker_id": speaker_id, "stopped": stopped })
        })
        .map_err(Refusal::from);
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

/// The installer's check: no camera needed.
pub(crate) async fn test_speaker(
    State(s): State<ApiState>,
    Path(id): Path<SpeakerId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
) -> Result<Json<serde_json::Value>, Refusal> {
    let res: Result<_, Refusal> = async {
        let sp = stored(&s, id).await?;
        let (_, _, playing) = s.talkdown.start(&sp, None, &Audio::Tone).await?;
        match playing.await {
            Ok(Ok(())) => Ok(serde_json::json!({ "ok": true })),
            Ok(Err(e)) => Err(StartError::Call(e).into()),
            Err(e) => Err(Refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                e.to_string(),
            )),
        }
    }
    .await;
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

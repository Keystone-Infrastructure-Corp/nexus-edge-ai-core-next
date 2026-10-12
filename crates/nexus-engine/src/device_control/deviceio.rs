//! ONVIF device-control HTTP handlers: device I/O.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nexus_store::audit::AuditOutcome;
use nexus_types::{CameraId, Role};
use serde::Deserialize;

use super::{audit, finish_void, gateway_err, onvif_target, rbac};
use crate::api::{ApiError, ApiState};
use crate::auth::require_role::SessionContext;
use crate::discovery::onvif_deviceio;

// ---------------------------------------------------------------------------
// Device I/O
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct OsdQuery {
    #[serde(default)]
    config_token: Option<String>,
}

/// `GET /v1/admin/cameras/{id}/deviceio/relays` — list relays.
pub async fn relays_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
) -> Result<Json<Vec<onvif_deviceio::RelayOutput>>, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let relays = onvif_deviceio::get_relay_outputs(&t.endpoint, &t.username, &t.password)
        .await
        .map_err(gateway_err)?;
    Ok(Json(relays))
}

#[derive(Deserialize)]
pub struct RelayPut {
    relay_token: String,
    active: bool,
}

/// `PUT /v1/admin/cameras/{id}/deviceio/relays` — set relay state.
pub async fn relays_put(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(req): Json<RelayPut>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let res = onvif_deviceio::set_relay_output_state(
        &t.endpoint,
        &t.username,
        &t.password,
        &req.relay_token,
        req.active,
    )
    .await;
    finish_void(
        &s,
        &ctx,
        &headers,
        peer.ip(),
        "camera.onvif.deviceio.relay",
        id,
        res,
    )
    .await
}

/// `GET /v1/admin/cameras/{id}/deviceio/inputs` — digital inputs.
pub async fn digital_inputs_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
) -> Result<Json<Vec<onvif_deviceio::DigitalInput>>, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let inputs = onvif_deviceio::get_digital_inputs(&t.endpoint, &t.username, &t.password)
        .await
        .map_err(gateway_err)?;
    Ok(Json(inputs))
}

/// `GET /v1/admin/cameras/{id}/deviceio/audio-sources` — audio
/// source tokens.
pub async fn audio_sources_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
) -> Result<Json<Vec<String>>, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let sources = onvif_deviceio::get_audio_sources(&t.endpoint, &t.username, &t.password)
        .await
        .map_err(gateway_err)?;
    Ok(Json(sources))
}

/// `GET /v1/admin/cameras/{id}/deviceio/osds` — list OSDs.
pub async fn osds_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
    Query(q): Query<OsdQuery>,
) -> Result<Json<Vec<onvif_deviceio::Osd>>, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let osds = onvif_deviceio::get_osds(
        &t.endpoint,
        &t.username,
        &t.password,
        q.config_token.as_deref(),
    )
    .await
    .map_err(gateway_err)?;
    Ok(Json(osds))
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum OsdCommand {
    Create {
        video_source_config_token: String,
        position: String,
        text: String,
    },
    Update {
        osd_token: String,
        video_source_config_token: String,
        position: String,
        text: String,
    },
    Delete {
        osd_token: String,
    },
}

/// `POST /v1/admin/cameras/{id}/deviceio/osd` — create / update /
/// delete a text OSD overlay.
pub async fn osd_command(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(cmd): Json<OsdCommand>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let ip = peer.ip();
    match cmd {
        OsdCommand::Create {
            video_source_config_token,
            position,
            text,
        } => {
            let res = onvif_deviceio::create_text_osd(
                &t.endpoint,
                &t.username,
                &t.password,
                &video_source_config_token,
                &position,
                &text,
            )
            .await;
            match res {
                Ok(token) => {
                    audit(
                        &s,
                        &ctx,
                        &headers,
                        ip,
                        "camera.onvif.deviceio.osd_create",
                        id,
                        AuditOutcome::Success,
                        Some(&token),
                    )
                    .await;
                    Ok((
                        StatusCode::OK,
                        Json(serde_json::json!({ "osd_token": token })),
                    )
                        .into_response())
                }
                Err(e) => {
                    audit(
                        &s,
                        &ctx,
                        &headers,
                        ip,
                        "camera.onvif.deviceio.osd_create",
                        id,
                        AuditOutcome::Failure,
                        Some(&e),
                    )
                    .await;
                    Err(gateway_err(e))
                }
            }
        }
        OsdCommand::Update {
            osd_token,
            video_source_config_token,
            position,
            text,
        } => {
            let res = onvif_deviceio::set_text_osd(
                &t.endpoint,
                &t.username,
                &t.password,
                &osd_token,
                &video_source_config_token,
                &position,
                &text,
            )
            .await;
            finish_void(
                &s,
                &ctx,
                &headers,
                ip,
                "camera.onvif.deviceio.osd_update",
                id,
                res,
            )
            .await
        }
        OsdCommand::Delete { osd_token } => {
            let res =
                onvif_deviceio::delete_osd(&t.endpoint, &t.username, &t.password, &osd_token).await;
            finish_void(
                &s,
                &ctx,
                &headers,
                ip,
                "camera.onvif.deviceio.osd_delete",
                id,
                res,
            )
            .await
        }
    }
}

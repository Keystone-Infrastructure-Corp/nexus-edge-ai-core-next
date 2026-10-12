//! ONVIF device-control HTTP handlers: imaging.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;
use nexus_types::{CameraId, Role};
use serde::Deserialize;

use super::{finish_void, gateway_err, onvif_target, rbac};
use crate::api::{ApiError, ApiState};
use crate::auth::require_role::SessionContext;
use crate::discovery::onvif_imaging;

// ---------------------------------------------------------------------------
// Imaging
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct VideoSourceQuery {
    video_source_token: String,
}

/// `GET /v1/admin/cameras/{id}/imaging` — current settings + the
/// option ranges (best-effort) for one video source.
pub async fn imaging_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
    Query(q): Query<VideoSourceQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let settings = onvif_imaging::get_imaging_settings(
        &t.endpoint,
        &t.username,
        &t.password,
        &q.video_source_token,
    )
    .await
    .map_err(gateway_err)?;
    let options =
        onvif_imaging::get_options(&t.endpoint, &t.username, &t.password, &q.video_source_token)
            .await
            .ok();
    Ok(Json(
        serde_json::json!({ "settings": settings, "options": options }),
    ))
}

#[derive(Deserialize)]
pub struct ImagingPut {
    video_source_token: String,
    settings: onvif_imaging::ImagingSettings,
    #[serde(default)]
    force_persistence: bool,
}

/// `PUT /v1/admin/cameras/{id}/imaging` — write imaging settings.
pub async fn imaging_put(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(req): Json<ImagingPut>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let res = onvif_imaging::set_imaging_settings(
        &t.endpoint,
        &t.username,
        &t.password,
        &req.video_source_token,
        &req.settings,
        req.force_persistence,
    )
    .await;
    finish_void(
        &s,
        &ctx,
        &headers,
        peer.ip(),
        "camera.onvif.imaging.set",
        id,
        res,
    )
    .await
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FocusCommand {
    ContinuousMove {
        video_source_token: String,
        speed: f32,
    },
    AbsoluteMove {
        video_source_token: String,
        position: f32,
        #[serde(default)]
        speed: Option<f32>,
    },
    Stop {
        video_source_token: String,
    },
}

/// `POST /v1/admin/cameras/{id}/imaging/focus` — drive the focus
/// motor.
pub async fn imaging_focus(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(cmd): Json<FocusCommand>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let ip = peer.ip();
    match cmd {
        FocusCommand::ContinuousMove {
            video_source_token,
            speed,
        } => {
            let res = onvif_imaging::focus_continuous_move(
                &t.endpoint,
                &t.username,
                &t.password,
                &video_source_token,
                speed,
            )
            .await;
            finish_void(
                &s,
                &ctx,
                &headers,
                ip,
                "camera.onvif.imaging.focus_move",
                id,
                res,
            )
            .await
        }
        FocusCommand::AbsoluteMove {
            video_source_token,
            position,
            speed,
        } => {
            let res = onvif_imaging::focus_absolute_move(
                &t.endpoint,
                &t.username,
                &t.password,
                &video_source_token,
                position,
                speed,
            )
            .await;
            finish_void(
                &s,
                &ctx,
                &headers,
                ip,
                "camera.onvif.imaging.focus_absolute",
                id,
                res,
            )
            .await
        }
        FocusCommand::Stop { video_source_token } => {
            let res = onvif_imaging::focus_stop(
                &t.endpoint,
                &t.username,
                &t.password,
                &video_source_token,
            )
            .await;
            finish_void(
                &s,
                &ctx,
                &headers,
                ip,
                "camera.onvif.imaging.focus_stop",
                id,
                res,
            )
            .await
        }
    }
}

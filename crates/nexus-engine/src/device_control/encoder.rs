//! ONVIF device-control HTTP handlers: encoder.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;
use nexus_types::{CameraId, Role};
use serde::Deserialize;

use super::{finish_void, gateway_err, onvif_target, rbac};
use crate::api::{ApiError, ApiState};
use crate::auth::require_role::SessionContext;
use crate::discovery::onvif_encoder;

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// `GET /v1/admin/cameras/{id}/encoder` — encoder configs + the
/// generic option ranges (best-effort).
pub async fn encoder_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
) -> Result<Json<serde_json::Value>, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let configs =
        onvif_encoder::get_video_encoder_configurations(&t.endpoint, &t.username, &t.password)
            .await
            .map_err(gateway_err)?;
    let options = onvif_encoder::get_video_encoder_configuration_options(
        &t.endpoint,
        &t.username,
        &t.password,
        None,
        None,
    )
    .await
    .ok();
    Ok(Json(
        serde_json::json!({ "configs": configs, "options": options }),
    ))
}

#[derive(Deserialize)]
pub struct EncoderPut {
    config: onvif_encoder::VideoEncoderConfig,
    #[serde(default)]
    force_persistence: bool,
}

/// `PUT /v1/admin/cameras/{id}/encoder` — write an encoder config.
pub async fn encoder_put(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(req): Json<EncoderPut>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let res = onvif_encoder::set_video_encoder_configuration(
        &t.endpoint,
        &t.username,
        &t.password,
        &req.config,
        req.force_persistence,
    )
    .await;
    finish_void(
        &s,
        &ctx,
        &headers,
        peer.ip(),
        "camera.onvif.encoder.set",
        id,
        res,
    )
    .await
}

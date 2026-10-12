//! ONVIF device-control HTTP handlers: PTZ.

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
use crate::discovery::onvif_ptz;

// ---------------------------------------------------------------------------
// PTZ
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ProfileQuery {
    profile_token: String,
}

#[derive(Deserialize)]
pub struct ConfigTokenQuery {
    config_token: String,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PtzRequest {
    Move {
        profile_token: String,
        velocity: onvif_ptz::PtzVector,
        #[serde(default)]
        timeout_secs: Option<f32>,
    },
    RelativeMove {
        profile_token: String,
        translation: onvif_ptz::PtzVector,
        #[serde(default)]
        speed: Option<onvif_ptz::PtzVector>,
    },
    AbsoluteMove {
        profile_token: String,
        position: onvif_ptz::PtzVector,
        #[serde(default)]
        speed: Option<onvif_ptz::PtzVector>,
    },
    Stop {
        profile_token: String,
        #[serde(default = "default_true")]
        pan_tilt: bool,
        #[serde(default = "default_true")]
        zoom: bool,
    },
    GotoPreset {
        profile_token: String,
        preset_token: String,
        #[serde(default)]
        speed: Option<onvif_ptz::PtzVector>,
    },
    GotoHome {
        profile_token: String,
        #[serde(default)]
        speed: Option<onvif_ptz::PtzVector>,
    },
    SetPreset {
        profile_token: String,
        #[serde(default)]
        preset_name: Option<String>,
        #[serde(default)]
        preset_token: Option<String>,
    },
    RemovePreset {
        profile_token: String,
        preset_token: String,
    },
    SetHome {
        profile_token: String,
    },
    Aux {
        profile_token: String,
        command: String,
    },
}

impl PtzRequest {
    /// Required role + audit-action label for this command. Jog +
    /// recall are operator-level; writes are admin-level.
    fn rbac(&self) -> (Role, &'static str) {
        match self {
            PtzRequest::Move { .. } => (Role::Operator, "camera.onvif.ptz.move"),
            PtzRequest::RelativeMove { .. } => (Role::Operator, "camera.onvif.ptz.relative_move"),
            PtzRequest::AbsoluteMove { .. } => (Role::Operator, "camera.onvif.ptz.absolute_move"),
            PtzRequest::Stop { .. } => (Role::Operator, "camera.onvif.ptz.stop"),
            PtzRequest::GotoPreset { .. } => (Role::Operator, "camera.onvif.ptz.goto_preset"),
            PtzRequest::GotoHome { .. } => (Role::Operator, "camera.onvif.ptz.goto_home"),
            PtzRequest::SetPreset { .. } => (Role::Admin, "camera.onvif.ptz.set_preset"),
            PtzRequest::RemovePreset { .. } => (Role::Admin, "camera.onvif.ptz.remove_preset"),
            PtzRequest::SetHome { .. } => (Role::Admin, "camera.onvif.ptz.set_home"),
            PtzRequest::Aux { .. } => (Role::Admin, "camera.onvif.ptz.aux"),
        }
    }
}

/// `POST /v1/admin/cameras/{id}/ptz` — drive the pan/tilt/zoom
/// head. The body's `action` tag selects the operation.
pub async fn ptz_command(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(req): Json<PtzRequest>,
) -> Result<Response, ApiError> {
    let (need, action) = req.rbac();
    rbac(&ctx, need)?;
    let t = onvif_target(&s, id).await?;
    let ip = peer.ip();
    match req {
        PtzRequest::Move {
            profile_token,
            velocity,
            timeout_secs,
        } => {
            let res = onvif_ptz::continuous_move(
                &t.endpoint,
                &t.username,
                &t.password,
                &profile_token,
                velocity,
                timeout_secs,
            )
            .await;
            finish_void(&s, &ctx, &headers, ip, action, id, res).await
        }
        PtzRequest::RelativeMove {
            profile_token,
            translation,
            speed,
        } => {
            let res = onvif_ptz::relative_move(
                &t.endpoint,
                &t.username,
                &t.password,
                &profile_token,
                translation,
                speed,
            )
            .await;
            finish_void(&s, &ctx, &headers, ip, action, id, res).await
        }
        PtzRequest::AbsoluteMove {
            profile_token,
            position,
            speed,
        } => {
            let res = onvif_ptz::absolute_move(
                &t.endpoint,
                &t.username,
                &t.password,
                &profile_token,
                position,
                speed,
            )
            .await;
            finish_void(&s, &ctx, &headers, ip, action, id, res).await
        }
        PtzRequest::Stop {
            profile_token,
            pan_tilt,
            zoom,
        } => {
            let res = onvif_ptz::stop(
                &t.endpoint,
                &t.username,
                &t.password,
                &profile_token,
                pan_tilt,
                zoom,
            )
            .await;
            finish_void(&s, &ctx, &headers, ip, action, id, res).await
        }
        PtzRequest::GotoPreset {
            profile_token,
            preset_token,
            speed,
        } => {
            let res = onvif_ptz::goto_preset(
                &t.endpoint,
                &t.username,
                &t.password,
                &profile_token,
                &preset_token,
                speed,
            )
            .await;
            finish_void(&s, &ctx, &headers, ip, action, id, res).await
        }
        PtzRequest::GotoHome {
            profile_token,
            speed,
        } => {
            let res = onvif_ptz::goto_home_position(
                &t.endpoint,
                &t.username,
                &t.password,
                &profile_token,
                speed,
            )
            .await;
            finish_void(&s, &ctx, &headers, ip, action, id, res).await
        }
        PtzRequest::SetPreset {
            profile_token,
            preset_name,
            preset_token,
        } => {
            let res = onvif_ptz::set_preset(
                &t.endpoint,
                &t.username,
                &t.password,
                &profile_token,
                preset_name.as_deref(),
                preset_token.as_deref(),
            )
            .await;
            match res {
                Ok(token) => {
                    audit(
                        &s,
                        &ctx,
                        &headers,
                        ip,
                        action,
                        id,
                        AuditOutcome::Success,
                        Some(&token),
                    )
                    .await;
                    Ok((
                        StatusCode::OK,
                        Json(serde_json::json!({ "preset_token": token })),
                    )
                        .into_response())
                }
                Err(e) => {
                    audit(
                        &s,
                        &ctx,
                        &headers,
                        ip,
                        action,
                        id,
                        AuditOutcome::Failure,
                        Some(&e),
                    )
                    .await;
                    Err(gateway_err(e))
                }
            }
        }
        PtzRequest::RemovePreset {
            profile_token,
            preset_token,
        } => {
            let res = onvif_ptz::remove_preset(
                &t.endpoint,
                &t.username,
                &t.password,
                &profile_token,
                &preset_token,
            )
            .await;
            finish_void(&s, &ctx, &headers, ip, action, id, res).await
        }
        PtzRequest::SetHome { profile_token } => {
            let res =
                onvif_ptz::set_home_position(&t.endpoint, &t.username, &t.password, &profile_token)
                    .await;
            finish_void(&s, &ctx, &headers, ip, action, id, res).await
        }
        PtzRequest::Aux {
            profile_token,
            command,
        } => {
            let res = onvif_ptz::send_auxiliary_command(
                &t.endpoint,
                &t.username,
                &t.password,
                &profile_token,
                &command,
            )
            .await;
            finish_void(&s, &ctx, &headers, ip, action, id, res).await
        }
    }
}

/// `GET /v1/admin/cameras/{id}/ptz/presets` — list stored presets.
pub async fn ptz_presets(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
    Query(q): Query<ProfileQuery>,
) -> Result<Json<Vec<onvif_ptz::Preset>>, ApiError> {
    rbac(&ctx, Role::Operator)?;
    let t = onvif_target(&s, id).await?;
    let presets = onvif_ptz::get_presets(&t.endpoint, &t.username, &t.password, &q.profile_token)
        .await
        .map_err(gateway_err)?;
    Ok(Json(presets))
}

/// `GET /v1/admin/cameras/{id}/ptz/nodes` — list PTZ heads + caps.
pub async fn ptz_nodes(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
) -> Result<Json<Vec<onvif_ptz::PtzNode>>, ApiError> {
    rbac(&ctx, Role::Operator)?;
    let t = onvif_target(&s, id).await?;
    let nodes = onvif_ptz::get_nodes(&t.endpoint, &t.username, &t.password)
        .await
        .map_err(gateway_err)?;
    Ok(Json(nodes))
}

/// `GET /v1/admin/cameras/{id}/ptz/status` — live PTZ status.
pub async fn ptz_status(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
    Query(q): Query<ProfileQuery>,
) -> Result<Json<onvif_ptz::PtzStatus>, ApiError> {
    rbac(&ctx, Role::Operator)?;
    let t = onvif_target(&s, id).await?;
    let status = onvif_ptz::get_status(&t.endpoint, &t.username, &t.password, &q.profile_token)
        .await
        .map_err(gateway_err)?;
    Ok(Json(status))
}

/// `GET /v1/admin/cameras/{id}/ptz/config-options` — absolute-move
/// ranges for a PTZ configuration (joystick clamping).
pub async fn ptz_config_options(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
    Query(q): Query<ConfigTokenQuery>,
) -> Result<Json<onvif_ptz::PtzSpaces>, ApiError> {
    rbac(&ctx, Role::Operator)?;
    let t = onvif_target(&s, id).await?;
    let spaces = onvif_ptz::get_configuration_options(
        &t.endpoint,
        &t.username,
        &t.password,
        &q.config_token,
    )
    .await
    .map_err(gateway_err)?;
    Ok(Json(spaces))
}

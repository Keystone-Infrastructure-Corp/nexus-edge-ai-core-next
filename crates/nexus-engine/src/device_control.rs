//! Phase 7.6.3 — ONVIF device-control admin routes.
//!
//! The HTTP surface that drives the SOAP client modules in
//! [`crate::discovery`] (`onvif_ptz`, `onvif_imaging`,
//! `onvif_device`, `onvif_deviceio`, `onvif_encoder`,
//! `onvif_snapshot`) against a *specific* camera. Each handler
//! loads that camera's ONVIF endpoint + credentials from the
//! edge-resident config (`CameraConfig.onvif`) and never from the
//! cloud — the credentials are pinned edge-side (AGENTS.md Rule 6
//! / REPO_BOUNDARY R5b) and only the SOAP round-trip leaves the
//! box, straight to the camera on the LAN.
//!
//! ## Routing
//!
//! Every route lives under `/v1/admin/cameras/{id}/…` so it sits
//! behind the same `admin_auth` gate as the rest of `/v1/admin/*`.
//! The cloud reaches these through the generic `/admin/*`
//! passthrough proxy (`engine_rpc::handle_admin_passthrough`),
//! which verifies the human's `actor_token` *upstream* before the
//! request ever reaches axum — so a cloud-initiated mutation is
//! already actor-attributed (AGENTS.md Rule 6, R4c). Direct
//! local-UI access is gated by the operator's own access JWT.
//!
//! ## RBAC carve-out
//!
//! A PTZ jog is a routine operator action; everything that
//! reconfigures the device is owner/admin. So:
//!
//! * **Operator** — PTZ move / relative / absolute / stop, preset
//!   and home *recall*, and the PTZ read-backs (presets / nodes /
//!   status / config-options) the joystick UI needs.
//! * **Admin** — preset/home *writes*, auxiliary commands, and the
//!   entire imaging / device / device-I/O / encoder / snapshot /
//!   raw-SOAP surface.
//!
//! The gate authenticates; [`SessionContext::require`] authorises.
//! Reached via the cloud loopback passthrough the context is the
//! legacy-admin identity, so the cloud-side ACL is the effective
//! fine-grained gate there — mirroring the existing
//! `users_admin` / `audit_admin` handlers.
//!
//! ## Auditing
//!
//! Every *mutating* action records an `audit_log` row on the edge
//! (`audit_admin_action`, fire-and-forget) with outcome
//! Success/Failure, so an operator sees the edge-side half of the
//! "who poked this camera" trail even when the action originated
//! in the cloud. Reads are not audited (consistent with the rest
//! of the admin read surface).

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nexus_store::audit::AuditOutcome;
use nexus_types::{CameraId, Role};
use serde::Deserialize;

use crate::api::{ApiError, ApiState};
use crate::auth::admin_audit::audit_admin_action;
use crate::auth::require_role::{SessionContext, SessionRejection};
use crate::discovery::onvif_soap::{OnvifService, DEVICE, DEVICEIO, IMAGING, MEDIA1, MEDIA2, PTZ};
use crate::discovery::{
    onvif_device, onvif_deviceio, onvif_encoder, onvif_firmware, onvif_imaging, onvif_media,
    onvif_ptz, onvif_snapshot,
};

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// The per-camera ONVIF connection parameters, resolved from the
/// edge-resident camera config.
struct OnvifTarget {
    endpoint: String,
    username: String,
    password: String,
}

/// Look up the camera by id and extract its ONVIF endpoint +
/// credentials. `404` when the camera is unknown, `400` when it
/// has no ONVIF endpoint configured (the operator hasn't filled in
/// the device-control fields).
async fn onvif_target(s: &ApiState, id: CameraId) -> Result<OnvifTarget, ApiError> {
    let cam = s
        .store
        .list_cameras()
        .await?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "camera not found".into()))?;
    let onvif = cam.onvif;
    let endpoint = onvif
        .endpoint
        .filter(|e| !e.trim().is_empty())
        .ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "camera has no ONVIF endpoint configured".into(),
            )
        })?;
    Ok(OnvifTarget {
        endpoint,
        username: onvif.username.unwrap_or_default(),
        password: onvif.password.unwrap_or_default(),
    })
}

/// Authorise `ctx` for `required`, mapping the rejection into the
/// module's `ApiError` shape (403 insufficient role, else 401).
fn rbac(ctx: &SessionContext, required: Role) -> Result<(), ApiError> {
    ctx.require(required).map_err(|r| match r {
        SessionRejection::InsufficientRole { .. } => {
            ApiError(StatusCode::FORBIDDEN, "insufficient role".into())
        }
        _ => ApiError(StatusCode::UNAUTHORIZED, "authentication required".into()),
    })
}

/// Record one edge-side audit row for a device-control mutation.
#[allow(clippy::too_many_arguments)]
async fn audit(
    s: &ApiState,
    ctx: &SessionContext,
    headers: &HeaderMap,
    ip: IpAddr,
    action: &str,
    id: CameraId,
    outcome: AuditOutcome,
    detail: Option<&str>,
) {
    audit_admin_action(
        &s.store,
        Some(ctx),
        headers,
        ip,
        action,
        "camera",
        Some(&id.to_string()),
        outcome,
        None,
        detail,
    )
    .await;
}

/// Finish a SOAP call that returns no body: audit success/failure
/// and render `204 No Content` or `502 Bad Gateway`.
#[allow(clippy::too_many_arguments)]
async fn finish_void(
    s: &ApiState,
    ctx: &SessionContext,
    headers: &HeaderMap,
    ip: IpAddr,
    action: &str,
    id: CameraId,
    res: Result<(), String>,
) -> Result<Response, ApiError> {
    match res {
        Ok(()) => {
            audit(s, ctx, headers, ip, action, id, AuditOutcome::Success, None).await;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Err(e) => {
            audit(
                s,
                ctx,
                headers,
                ip,
                action,
                id,
                AuditOutcome::Failure,
                Some(&e),
            )
            .await;
            Err(ApiError(StatusCode::BAD_GATEWAY, e))
        }
    }
}

/// Map a SOAP read error onto a `502`.
fn gateway_err(e: String) -> ApiError {
    ApiError(StatusCode::BAD_GATEWAY, e)
}

// ---------------------------------------------------------------------------
// Query shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ProfileQuery {
    profile_token: String,
}

#[derive(Deserialize)]
pub struct VideoSourceQuery {
    video_source_token: String,
}

/// Query params for the snapshot route.
///
/// * `profile_token` — required ONVIF media profile token.
/// * `upload_sas` — when present, the edge PUTs the JPEG to this
///   cloud-minted single-blob Write SAS (the bytes leave the box
///   straight to Blob storage, never through the gateway — Hard
///   Rule 7) and returns a small JSON receipt instead of the image.
///   This is the path the cloud admin-proxy drives by default.
/// * `encoding` — `base64` returns the JPEG as a base64 string
///   inside JSON (the cloud's fallback when no Blob storage is
///   configured, since binary cannot ride the rpc_call tunnel).
///   Any other value (or absent) returns raw `image/jpeg` bytes for
///   direct loopback use.
#[derive(Deserialize)]
pub struct SnapshotQuery {
    profile_token: String,
    upload_sas: Option<String>,
    encoding: Option<String>,
}

#[derive(Deserialize)]
pub struct ConfigTokenQuery {
    config_token: String,
}

#[derive(Deserialize)]
pub struct LogQuery {
    #[serde(default = "default_log_type")]
    log_type: String,
}

fn default_log_type() -> String {
    "System".into()
}

#[derive(Deserialize)]
pub struct OsdQuery {
    #[serde(default)]
    config_token: Option<String>,
}

// ---------------------------------------------------------------------------
// PTZ
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Imaging
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// `GET /v1/admin/cameras/{id}/device` — identity + capabilities +
/// services + clock + NTP, each best-effort. A `502` only when the
/// device is wholly unreachable.
pub async fn device_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
) -> Result<Json<serde_json::Value>, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let information = onvif_device::get_device_information(&t.endpoint, &t.username, &t.password)
        .await
        .ok();
    let capabilities = onvif_device::get_capabilities(&t.endpoint, &t.username, &t.password)
        .await
        .ok();
    let services = onvif_device::get_services(&t.endpoint, &t.username, &t.password)
        .await
        .ok();
    let system_date_time =
        onvif_device::get_system_date_and_time(&t.endpoint, &t.username, &t.password)
            .await
            .ok();
    let ntp = onvif_device::get_ntp(&t.endpoint, &t.username, &t.password)
        .await
        .ok();
    if information.is_none()
        && capabilities.is_none()
        && services.is_none()
        && system_date_time.is_none()
        && ntp.is_none()
    {
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "ONVIF device unreachable".into(),
        ));
    }
    Ok(Json(serde_json::json!({
        "information": information,
        "capabilities": capabilities,
        "services": services,
        "system_date_time": system_date_time,
        "ntp": ntp,
    })))
}

/// `GET /v1/admin/cameras/{id}/device/log` — fetch the device log.
pub async fn device_log(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
    Query(q): Query<LogQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let log = onvif_device::get_system_log(&t.endpoint, &t.username, &t.password, &q.log_type)
        .await
        .map_err(gateway_err)?;
    Ok(Json(serde_json::json!({ "log": log })))
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DeviceCommand {
    Reboot,
    SetTimeNtp {
        #[serde(default)]
        timezone: String,
        #[serde(default)]
        daylight_savings: bool,
    },
    SetTimeManual {
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        #[serde(default)]
        timezone: String,
        #[serde(default)]
        daylight_savings: bool,
    },
    SetNtp {
        from_dhcp: bool,
        #[serde(default)]
        server: Option<String>,
    },
}

/// `POST /v1/admin/cameras/{id}/device` — clock / NTP / reboot.
pub async fn device_command(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(cmd): Json<DeviceCommand>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let ip = peer.ip();
    match cmd {
        DeviceCommand::Reboot => {
            let res = onvif_device::system_reboot(&t.endpoint, &t.username, &t.password).await;
            match res {
                Ok(msg) => {
                    audit(
                        &s,
                        &ctx,
                        &headers,
                        ip,
                        "camera.onvif.device.reboot",
                        id,
                        AuditOutcome::Success,
                        Some(&msg),
                    )
                    .await;
                    Ok(
                        (StatusCode::OK, Json(serde_json::json!({ "message": msg })))
                            .into_response(),
                    )
                }
                Err(e) => {
                    audit(
                        &s,
                        &ctx,
                        &headers,
                        ip,
                        "camera.onvif.device.reboot",
                        id,
                        AuditOutcome::Failure,
                        Some(&e),
                    )
                    .await;
                    Err(gateway_err(e))
                }
            }
        }
        DeviceCommand::SetTimeNtp {
            timezone,
            daylight_savings,
        } => {
            let res = onvif_device::set_system_time_ntp(
                &t.endpoint,
                &t.username,
                &t.password,
                &timezone,
                daylight_savings,
            )
            .await;
            finish_void(
                &s,
                &ctx,
                &headers,
                ip,
                "camera.onvif.device.set_time_ntp",
                id,
                res,
            )
            .await
        }
        DeviceCommand::SetTimeManual {
            year,
            month,
            day,
            hour,
            minute,
            second,
            timezone,
            daylight_savings,
        } => {
            let res = onvif_device::set_system_time_manual(
                &t.endpoint,
                &t.username,
                &t.password,
                year,
                month,
                day,
                hour,
                minute,
                second,
                &timezone,
                daylight_savings,
            )
            .await;
            finish_void(
                &s,
                &ctx,
                &headers,
                ip,
                "camera.onvif.device.set_time_manual",
                id,
                res,
            )
            .await
        }
        DeviceCommand::SetNtp { from_dhcp, server } => {
            let res = onvif_device::set_ntp(
                &t.endpoint,
                &t.username,
                &t.password,
                from_dhcp,
                server.as_deref(),
            )
            .await;
            finish_void(
                &s,
                &ctx,
                &headers,
                ip,
                "camera.onvif.device.set_ntp",
                id,
                res,
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Device I/O
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// `GET /v1/admin/cameras/{id}/snapshot` — pull a full-resolution
/// still. Three response shapes, selected by query params (see
/// [`SnapshotQuery`]):
///
/// * `?upload_sas=<url>` → PUT the JPEG straight to Blob and return
///   a JSON receipt `{ uploaded, content_type, bytes }`.
/// * `?encoding=base64` → JSON `{ image_base64, content_type, bytes }`.
/// * neither → raw `image/jpeg` bytes (loopback / local use).
///
/// The image fetch always leaves the box straight to the camera (not
/// the gateway); with `upload_sas` the bytes go on to Blob storage
/// without ever crossing the cloud tunnel (Hard Rule 7).
pub async fn snapshot_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
    Query(q): Query<SnapshotQuery>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let bytes =
        onvif_snapshot::fetch_snapshot(&t.endpoint, &t.username, &t.password, &q.profile_token)
            .await
            .map_err(gateway_err)?;

    // SAS-preferred: PUT the still straight to Blob storage and
    // return a tiny JSON receipt. The image never crosses the tunnel.
    if let Some(sas) = q.upload_sas.as_deref().filter(|v| !v.trim().is_empty()) {
        let n = onvif_snapshot::put_snapshot_to_sas(sas, &bytes)
            .await
            .map_err(gateway_err)?;
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "uploaded": true,
                "content_type": "image/jpeg",
                "bytes": n,
            })),
        )
            .into_response());
    }

    // Base64-in-JSON fallback: lets the non-binary rpc_call tunnel
    // carry the image when no Blob SAS is available (dev / no storage).
    if q.encoding.as_deref() == Some("base64") {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "content_type": "image/jpeg",
                "bytes": bytes.len(),
                "image_base64": B64.encode(&bytes),
            })),
        )
            .into_response());
    }

    // Default: raw bytes for direct loopback use.
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Firmware upgrade (Phase 7.6.8)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct FirmwareUpgradeRequest {
    /// Cloud-minted single-blob **Read SAS** the firmware is pulled
    /// from (Hard Rule 7 — straight from Blob storage, never the
    /// tunnel). Treated as an opaque bearer credential: never logged.
    sas_get_url: String,
    /// Expected SHA-256 (lower-case hex) of the firmware blob,
    /// verified before the upgrade window is opened.
    sha256: String,
    /// Expected device manufacturer, matched against the live
    /// `GetDeviceInformation` before apply.
    expected_make: String,
    /// Expected device model, matched against the live
    /// `GetDeviceInformation` before apply.
    expected_model: String,
}

/// `POST /v1/admin/cameras/{id}/firmware:upgrade` — modern ONVIF
/// firmware upgrade (`StartFirmwareUpgrade` flow).
///
/// Edge-side this requires the top tier (`Admin`); the **owner-only**
/// restriction and the type-token confirmation are enforced
/// cloud-side (the actor_token the mutating `rpc_call` already
/// carries). The blob is pulled straight from Blob storage via the
/// Read SAS — never the gateway tunnel — and **both** guards run
/// before the irreversible upgrade window is opened: the blob's
/// SHA-256 is verified, and the camera's reported make / model is
/// matched. Then `StartFirmwareUpgrade` → upload → `SystemReboot`.
///
/// A camera that does not implement the modern flow surfaces as a
/// clean `200 OK` with `{ "supported": false }` (rather than a gateway
/// error) so the cloud proxy passes the signal through verbatim and the
/// console can show an "unsupported" banner. Every outcome lands one
/// edge-side audit row whose detail is credential-free (byte count +
/// the camera's own downtime hint only — never the SAS URL).
pub async fn firmware_upgrade(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(req): Json<FirmwareUpgradeRequest>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let ip = peer.ip();
    const ACTION: &str = "camera.onvif.firmware.upgrade";

    // 1. Pull the firmware from the cloud SAS (never the tunnel) and
    //    verify its checksum BEFORE the upgrade window is opened. The
    //    download error is already SAS-credential-stripped by
    //    `onvif_firmware::download_firmware`.
    let firmware = match onvif_firmware::download_firmware(&req.sas_get_url).await {
        Ok(b) => b,
        Err(e) => {
            audit(
                &s,
                &ctx,
                &headers,
                ip,
                ACTION,
                id,
                AuditOutcome::Failure,
                Some(&e),
            )
            .await;
            return Err(gateway_err(e));
        }
    };
    if let Err(e) = onvif_firmware::verify_checksum(&firmware, &req.sha256) {
        audit(
            &s,
            &ctx,
            &headers,
            ip,
            ACTION,
            id,
            AuditOutcome::Failure,
            Some(&e),
        )
        .await;
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, e));
    }

    // 2. Match make / model against the live device identity.
    let info =
        match onvif_device::get_device_information(&t.endpoint, &t.username, &t.password).await {
            Ok(i) => i,
            Err(e) => {
                let msg = format!("could not read device information: {e}");
                audit(
                    &s,
                    &ctx,
                    &headers,
                    ip,
                    ACTION,
                    id,
                    AuditOutcome::Failure,
                    Some(&msg),
                )
                .await;
                return Err(gateway_err(msg));
            }
        };
    if let Err(e) =
        onvif_firmware::verify_make_model(&info, &req.expected_make, &req.expected_model)
    {
        audit(
            &s,
            &ctx,
            &headers,
            ip,
            ACTION,
            id,
            AuditOutcome::Failure,
            Some(&e),
        )
        .await;
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, e));
    }

    // 3. Open the upgrade window. A camera without the modern flow
    //    surfaces as a clean 501 "unsupported".
    let start =
        match onvif_firmware::start_firmware_upgrade(&t.endpoint, &t.username, &t.password).await {
            Ok(start) => start,
            Err(e) => {
                audit(
                    &s,
                    &ctx,
                    &headers,
                    ip,
                    ACTION,
                    id,
                    AuditOutcome::Failure,
                    Some(&e),
                )
                .await;
                if onvif_firmware::is_unsupported_fault(&e) {
                    return Ok((
                        StatusCode::OK,
                        Json(serde_json::json!({ "supported": false, "message": e })),
                    )
                        .into_response());
                }
                return Err(gateway_err(e));
            }
        };

    // 4. Upload the (verified) blob to the camera.
    if let Err(e) =
        onvif_firmware::upload_firmware(&start.upload_uri, &firmware, &t.username, &t.password)
            .await
    {
        audit(
            &s,
            &ctx,
            &headers,
            ip,
            ACTION,
            id,
            AuditOutcome::Failure,
            Some(&e),
        )
        .await;
        return Err(gateway_err(e));
    }

    // 5. Reboot to apply. Best-effort — some cameras reboot on their
    //    own after the upload, so a reboot error does not undo the
    //    upgrade that already landed.
    let reboot_message = onvif_device::system_reboot(&t.endpoint, &t.username, &t.password)
        .await
        .unwrap_or_default();

    // 6. Audit success. Credential-free detail: byte count + checksum
    //    + the camera's downtime hint — never the SAS URL.
    let detail = format!(
        "uploaded {} bytes (sha256 {}); expected_down_time={}",
        firmware.len(),
        req.sha256.trim().to_ascii_lowercase(),
        start.expected_down_time.as_deref().unwrap_or("unknown"),
    );
    audit(
        &s,
        &ctx,
        &headers,
        ip,
        ACTION,
        id,
        AuditOutcome::Success,
        Some(&detail),
    )
    .await;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "supported": true,
            "rebooting": true,
            "bytes": firmware.len(),
            "upload_delay": start.upload_delay,
            "expected_down_time": start.expected_down_time,
            "reboot_message": reboot_message,
        })),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Speakers / talk-down (Phase 7.6.7)
// ---------------------------------------------------------------------------

/// `GET /v1/admin/cameras/{id}/speakers` — surface the camera's
/// talk-down (speaker) capability: the stored `talk_down` config block
/// (edge-resident, populated during discovery) plus a best-effort live
/// `GetAudioOutputs` probe against the configured ONVIF endpoint. This
/// is the discovery surface the Phase 10.5 talk-down session builds on.
///
/// Read-only (operator+, like the PTZ reads) and not audited. The live
/// probe is best-effort: a fixed camera, an unreachable box, or a
/// camera without an ONVIF endpoint yields `audio_outputs: null`
/// rather than an error, so the stored capability is always returned.
pub async fn speakers_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
) -> Result<Json<serde_json::Value>, ApiError> {
    rbac(&ctx, Role::Operator)?;
    let cam = s
        .store
        .list_cameras()
        .await?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "camera not found".into()))?;

    let audio_outputs = match cam
        .onvif
        .endpoint
        .as_deref()
        .filter(|e| !e.trim().is_empty())
    {
        Some(ep) => onvif_media::query_audio_outputs(
            ep,
            cam.onvif.username.as_deref().unwrap_or_default(),
            cam.onvif.password.as_deref().unwrap_or_default(),
        )
        .await
        .ok(),
        None => None,
    };

    Ok(Json(serde_json::json!({
        "talk_down": cam.talk_down,
        "audio_outputs": audio_outputs,
    })))
}

// ---------------------------------------------------------------------------
// Raw SOAP console
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RawSoapRequest {
    /// Logical service: `ptz` | `imaging` | `device` | `deviceio`
    /// | `media1` | `media2`.
    service: String,
    /// SOAP operation name (the `op` in `<ns>/<op>`).
    operation: String,
    /// Inner SOAP body fragment (without envelope / WS-Security
    /// header — those are added by the client).
    body: String,
}

fn soap_service(name: &str) -> Option<&'static OnvifService> {
    match name.to_ascii_lowercase().as_str() {
        "ptz" => Some(&PTZ),
        "imaging" => Some(&IMAGING),
        "device" => Some(&DEVICE),
        "deviceio" => Some(&DEVICEIO),
        "media" | "media1" => Some(&MEDIA1),
        "media2" => Some(&MEDIA2),
        _ => None,
    }
}

/// `POST /v1/admin/cameras/{id}/onvif/raw` — admin SOAP console.
/// Sends an arbitrary operation against the camera's *configured*
/// ONVIF endpoint (never an arbitrary URL) and returns the raw
/// response body. Admin-only; every call is audited.
pub async fn onvif_raw(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(req): Json<RawSoapRequest>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let svc = soap_service(&req.service).ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown onvif service: {}", req.service),
        )
    })?;
    let t = onvif_target(&s, id).await?;
    let ip = peer.ip();
    let res = svc
        .call(
            &t.endpoint,
            &t.username,
            &t.password,
            &req.operation,
            &req.body,
        )
        .await;
    match res {
        Ok(xml) => {
            audit(
                &s,
                &ctx,
                &headers,
                ip,
                "camera.onvif.raw",
                id,
                AuditOutcome::Success,
                Some(&req.operation),
            )
            .await;
            Ok((
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/soap+xml")],
                xml,
            )
                .into_response())
        }
        Err(e) => {
            audit(
                &s,
                &ctx,
                &headers,
                ip,
                "camera.onvif.raw",
                id,
                AuditOutcome::Failure,
                Some(&e),
            )
            .await;
            Err(gateway_err(e))
        }
    }
}

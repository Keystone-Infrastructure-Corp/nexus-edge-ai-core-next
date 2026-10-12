//! ONVIF device-control HTTP handlers: device.

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
use crate::discovery::onvif_device;

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct LogQuery {
    #[serde(default = "default_log_type")]
    log_type: String,
}

fn default_log_type() -> String {
    "System".into()
}

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

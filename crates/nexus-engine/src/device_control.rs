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

use std::net::IpAddr;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use nexus_store::audit::AuditOutcome;
use nexus_types::{CameraId, Role};

use crate::api::{ApiError, ApiState};
use crate::auth::admin_audit::audit_admin_action;
use crate::auth::require_role::{SessionContext, SessionRejection};

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// The per-camera ONVIF connection parameters, resolved from the
/// edge-resident camera config.
pub(super) struct OnvifTarget {
    pub(super) endpoint: String,
    pub(super) username: String,
    pub(super) password: String,
}

/// Look up the camera by id and extract its ONVIF endpoint +
/// credentials. `404` when the camera is unknown, `400` when it
/// has no ONVIF endpoint configured (the operator hasn't filled in
/// the device-control fields).
pub(super) async fn onvif_target(s: &ApiState, id: CameraId) -> Result<OnvifTarget, ApiError> {
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
pub(super) fn rbac(ctx: &SessionContext, required: Role) -> Result<(), ApiError> {
    ctx.require(required).map_err(|r| match r {
        SessionRejection::InsufficientRole { .. } => {
            ApiError(StatusCode::FORBIDDEN, "insufficient role".into())
        }
        _ => ApiError(StatusCode::UNAUTHORIZED, "authentication required".into()),
    })
}

/// Record one edge-side audit row for a device-control mutation.
#[allow(clippy::too_many_arguments)]
pub(super) async fn audit(
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
pub(super) async fn finish_void(
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
pub(super) fn gateway_err(e: String) -> ApiError {
    ApiError(StatusCode::BAD_GATEWAY, e)
}

mod device;
mod deviceio;
mod encoder;
mod firmware;
mod imaging;
mod media;
mod network;
mod ptz;
mod snapshot;
mod soap;
mod speakers;

pub use device::{device_command, device_get, device_log};
pub use deviceio::{
    audio_sources_get, digital_inputs_get, osd_command, osds_get, relays_get, relays_put,
};
pub use encoder::{encoder_get, encoder_put};
pub use firmware::firmware_upgrade;
pub use imaging::{imaging_focus, imaging_get, imaging_put};
pub use media::media_profiles_get;
pub use network::{network_get, network_interface_put, network_safe_put};
pub use ptz::{ptz_command, ptz_config_options, ptz_nodes, ptz_presets, ptz_status};
pub use snapshot::snapshot_get;
pub use soap::onvif_raw;
pub use speakers::speakers_get;

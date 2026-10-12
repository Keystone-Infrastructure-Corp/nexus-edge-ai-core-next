//! ONVIF device-control HTTP handlers: raw SOAP console.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nexus_store::audit::AuditOutcome;
use nexus_types::{CameraId, Role};
use serde::Deserialize;

use super::{audit, gateway_err, onvif_target, rbac};
use crate::api::{ApiError, ApiState};
use crate::auth::require_role::SessionContext;
use crate::discovery::onvif_soap::{OnvifService, DEVICE, DEVICEIO, IMAGING, MEDIA1, MEDIA2, PTZ};

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

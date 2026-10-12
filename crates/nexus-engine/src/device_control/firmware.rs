//! ONVIF device-control HTTP handlers: firmware-upgrade.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nexus_store::audit::AuditOutcome;
use nexus_types::{CameraId, Role};
use serde::Deserialize;

use super::{audit, gateway_err, onvif_target, rbac};
use crate::api::{ApiError, ApiState};
use crate::auth::require_role::SessionContext;
use crate::discovery::{onvif_device, onvif_firmware};

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

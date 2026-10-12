//! ONVIF device-control HTTP handlers: snapshot.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nexus_types::{CameraId, Role};
use serde::Deserialize;

use super::{gateway_err, onvif_target, rbac};
use crate::api::{ApiError, ApiState};
use crate::auth::require_role::SessionContext;
use crate::discovery::onvif_snapshot;

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

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
/// * `fallback` — `analysis` opts the caller into the analysis-frame
///   fallback: when the camera has no ONVIF endpoint configured, or
///   answers `GetSnapshotUri` with a SOAP fault (`ter:ActionNotSupported`
///   is very common on Profile-S-only devices), serve the pipeline's
///   most recent supervisor frame instead of failing with `502`. Absent
///   (the default) preserves the strict behaviour ONVIF diagnostics
///   want — an unsupported camera surfaces its fault verbatim.
#[derive(Deserialize)]
pub struct SnapshotQuery {
    profile_token: String,
    upload_sas: Option<String>,
    encoding: Option<String>,
    fallback: Option<String>,
}

/// `source` value when the still came from the camera's own ONVIF
/// `GetSnapshotUri` endpoint (full native resolution).
const SNAPSHOT_SOURCE_ONVIF: &str = "onvif";

/// `source` value when the still is the pipeline's most recent
/// supervisor / analysis frame (the decoded low-bit-rate stream).
/// Lower resolution and always 16:9, but available on every running
/// camera — including ones with no ONVIF service at all.
const SNAPSHOT_SOURCE_ANALYSIS: &str = "analysis_frame";

/// Header carrying [`SNAPSHOT_SOURCE_ONVIF`] / [`SNAPSHOT_SOURCE_ANALYSIS`]
/// on the raw-`image/jpeg` response shape, where there is no JSON
/// envelope to put it in.
const SNAPSHOT_SOURCE_HEADER: &str = "x-nexus-snapshot-source";

/// Fetch the camera's native ONVIF still. Split out of
/// [`snapshot_get`] so the "no ONVIF endpoint configured" `400` and
/// the SOAP-fault `502` are both catchable by the fallback arm.
async fn onvif_still(s: &ApiState, id: CameraId, profile_token: &str) -> Result<Vec<u8>, ApiError> {
    let t = onvif_target(s, id).await?;
    onvif_snapshot::fetch_snapshot(&t.endpoint, &t.username, &t.password, profile_token)
        .await
        .map_err(gateway_err)
}

/// `GET /v1/admin/cameras/{id}/snapshot` — pull a still. Three
/// response shapes, selected by query params (see [`SnapshotQuery`]):
///
/// * `?upload_sas=<url>` → PUT the JPEG straight to Blob and return
///   a JSON receipt `{ uploaded, content_type, bytes, source }`.
/// * `?encoding=base64` → JSON `{ image_base64, content_type, bytes, source }`.
/// * neither → raw `image/jpeg` bytes (loopback / local use), with the
///   source in the `x-nexus-snapshot-source` header.
///
/// The image fetch always leaves the box straight to the camera (not
/// the gateway); with `upload_sas` the bytes go on to Blob storage
/// without ever crossing the cloud tunnel (Hard Rule 7).
///
/// With `?fallback=analysis`, a camera that cannot serve an ONVIF
/// still — no endpoint configured, or a `ter:ActionNotSupported` SOAP
/// fault from `GetSnapshotUri` — degrades to the pipeline's latest
/// supervisor frame rather than erroring. `source` tells the caller
/// which one it got.
pub async fn snapshot_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
    Query(q): Query<SnapshotQuery>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;

    let allow_fallback = q.fallback.as_deref() == Some("analysis");
    let (bytes, source) = match onvif_still(&s, id, &q.profile_token).await {
        Ok(bytes) => (bytes, SNAPSHOT_SOURCE_ONVIF),
        Err(onvif_err) if allow_fallback => {
            tracing::info!(
                camera_id = id,
                error = %onvif_err.1,
                "ONVIF still unavailable; serving latest analysis frame instead",
            );
            let bytes = crate::api::latest_frame_jpeg(&s.cache, id).map_err(|frame_err| {
                ApiError(
                    frame_err.0,
                    format!(
                        "onvif snapshot unavailable ({}); analysis-frame fallback also failed ({})",
                        onvif_err.1, frame_err.1
                    ),
                )
            })?;
            (bytes, SNAPSHOT_SOURCE_ANALYSIS)
        }
        Err(onvif_err) => return Err(onvif_err),
    };

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
                "source": source,
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
                "source": source,
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
            (
                header::HeaderName::from_static(SNAPSHOT_SOURCE_HEADER),
                source,
            ),
        ],
        bytes,
    )
        .into_response())
}

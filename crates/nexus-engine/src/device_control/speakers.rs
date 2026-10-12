//! ONVIF device-control HTTP handlers: speakers / talk-down.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use nexus_types::{CameraId, Role};

use super::rbac;
use crate::api::{ApiError, ApiState};
use crate::auth::require_role::SessionContext;
use crate::discovery::onvif_media;

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

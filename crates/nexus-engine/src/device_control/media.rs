//! ONVIF device-control HTTP handlers: media-profile.

use axum::extract::{Path, State};
use axum::Json;
use nexus_types::{CameraId, Role};

use super::{gateway_err, onvif_target, rbac};
use crate::api::{ApiError, ApiState};
use crate::auth::require_role::SessionContext;
use crate::discovery::onvif_media;

// ---------------------------------------------------------------------------
// Media profiles (token discovery)
// ---------------------------------------------------------------------------

/// `GET /v1/admin/cameras/{id}/media/profiles` — list the camera's
/// ONVIF media profiles reduced to the opaque tokens the
/// device-control UI needs: the profile token (PTZ + preview), the
/// video-source token (imaging), and the video-source-config token
/// (OSD). Read-only and operator-level so a PTZ operator can resolve
/// their profile token without holding admin.
pub async fn media_profiles_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
) -> Result<Json<serde_json::Value>, ApiError> {
    rbac(&ctx, Role::Operator)?;
    let t = onvif_target(&s, id).await?;
    let profiles = onvif_media::query_profiles(&t.endpoint, &t.username, &t.password)
        .await
        .map_err(gateway_err)?;
    Ok(Json(serde_json::json!({ "profiles": profiles })))
}

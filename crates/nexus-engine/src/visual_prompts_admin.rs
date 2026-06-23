//! M3.1 Phase H — admin REST API for visual prompts.
//!
//! Routes (all gated by the admin-auth middleware in
//! [`crate::api::router`] alongside the other `/v1/admin/*` writes):
//!
//! * `GET    /api/v1/admin/visual-prompts`               — list
//! * `POST   /api/v1/admin/visual-prompts`               — multipart upload
//!     - `name` (text, required, unique)
//!     - `description` (text, optional)
//!     - `image` (file, required, PNG / JPEG)
//! * `GET    /api/v1/admin/visual-prompts/{id}`          — detail (no embedding)
//! * `DELETE /api/v1/admin/visual-prompts/{id}`          — delete (conflicts when attached)
//! * `GET    /api/v1/admin/cameras/{cam_id}/visual-prompts` — list attached
//! * `POST   /api/v1/admin/cameras/{cam_id}/visual-prompts/{vp_id}` — attach
//! * `DELETE /api/v1/admin/cameras/{cam_id}/visual-prompts/{vp_id}` — detach
//!
//! The upload handler writes the original image to
//! `cfg.runtime.visual_prompts_dir/<sha256>.<ext>` first (so the file
//! exists for later re-encode on encoder upgrades), then calls the
//! lazily-initialised [`nexus_inference::ImageEncoder`] (when the
//! `ort` cargo feature is on) to produce a `Vec<f32>` embedding,
//! and finally persists the row via
//! [`nexus_store::Store::create_visual_prompt`].
//!
//! Without the `ort` cargo feature (the no-deps dev build), the
//! upload route returns `503 Service Unavailable` with a
//! `{"error":"encoder_not_available"}` body. List / get / delete /
//! attach / detach all work unconditionally — they don't touch the
//! encoder.

use std::path::PathBuf;
#[cfg(feature = "ort")]
use std::sync::Arc;

use axum::extract::{ConnectInfo, Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;
use sha2::{Digest, Sha256};
#[cfg(feature = "ort")]
use tokio::sync::OnceCell;

use crate::api::{ApiError, ApiState};
use nexus_store::visual_prompts::{
    NewVisualPrompt, VisualPrompt, VisualPromptError, VisualPromptSummary,
};
use nexus_types::{CameraId, VisualPromptId};

/// Per-engine runtime state for the visual-prompts admin handlers.
/// Lives on [`ApiState`] as a single field so the router stays
/// declarative.
#[derive(Clone)]
pub struct VisualPromptsAdminState {
    /// Directory the upload handler writes original images to.
    /// Created on demand at engine boot
    /// (`tokio::fs::create_dir_all`).
    pub visual_prompts_dir: PathBuf,
    /// Full path to the image-encoder ONNX (e.g.
    /// `<inference.model.pack_path>/yoloe26_s_image_encoder.onnx`).
    /// `None` when the operator hasn't set a pack path — the upload
    /// handler 503s in that case.
    pub encoder_model_path: Option<PathBuf>,
    /// Stable id recorded on each row's `encoder_model_id` column —
    /// used downstream to invalidate cached embeddings on encoder
    /// upgrades.
    pub encoder_model_id: String,
    /// Embedding dim (= length of the `Vec<f32>` the encoder emits).
    /// Used to size the `embedding_dim` column on each row.
    /// Defaults to 512 (yoloe26_s_image_encoder).
    pub encoder_embedding_dim: usize,
    /// EP priority forwarded to ORT on encoder session creation.
    /// Mirrors `cfg.inference.ep_priority`.
    pub encoder_ep_priority: Vec<String>,
    /// Lazy-init handle for the actual encoder session. First POST
    /// to `/visual-prompts` loads the ONNX; subsequent POSTs share
    /// the same `Session` (which is internally `Mutex`-guarded by
    /// `ImageEncoder`).
    #[cfg(feature = "ort")]
    pub encoder: Arc<OnceCell<Arc<nexus_inference::ImageEncoder>>>,
}

impl VisualPromptsAdminState {
    /// Build from runtime + inference config. Resolves the encoder
    /// ONNX path against `inference.model.pack_path` (the same path
    /// the detector uses to find its own ONNX) — when the pack path
    /// is unset, `encoder_model_path` is `None` and the upload route
    /// will 503 until the operator configures one.
    pub fn from_config(
        runtime: &nexus_config::RuntimeConfig,
        inference: &nexus_config::InferenceConfig,
    ) -> Self {
        let encoder_model_path = inference
            .model
            .pack_path
            .as_ref()
            .map(|p| p.join("yoloe26_s_image_encoder.onnx"));
        Self {
            visual_prompts_dir: runtime.visual_prompts_dir.clone(),
            encoder_model_path,
            encoder_model_id: "yoloe26_s_image_encoder".to_string(),
            encoder_embedding_dim: 512,
            encoder_ep_priority: inference.ep_priority.clone(),
            #[cfg(feature = "ort")]
            encoder: Arc::new(OnceCell::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

pub async fn list_visual_prompts(
    State(s): State<ApiState>,
) -> Result<Json<Vec<VisualPromptSummary>>, ApiError> {
    s.store
        .list_visual_prompts()
        .await
        .map(Json)
        .map_err(visual_prompt_error_to_api)
}

pub async fn get_visual_prompt(
    State(s): State<ApiState>,
    Path(id): Path<VisualPromptId>,
) -> Result<Json<VisualPrompt>, ApiError> {
    let row = s
        .store
        .get_visual_prompt(id)
        .await
        .map_err(visual_prompt_error_to_api)?;
    match row {
        Some((vp, _embedding)) => Ok(Json(vp)),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            "visual prompt not found".to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

#[derive(Default)]
struct UploadFields {
    name: Option<String>,
    description: Option<String>,
    image_bytes: Option<Vec<u8>>,
    image_content_type: Option<String>,
}

/// Shared create path for visual prompts: decode → hash → persist the
/// original pixels to disk → encode the embedding → insert the row.
///
/// Used by the multipart upload handler ([`post_visual_prompt`]) and
/// the fleet-config create-by-image path
/// ([`crate::fleet_apply::apply_visual_prompts`]) so both flows agree
/// on filename derivation, the encoder gate, and the on-disk layout.
///
/// Returns the new prompt's id. The caller owns any audit logging and
/// re-reading the persisted row if it needs the full projection.
/// `503` when no encoder pack is configured, `400` on undecodable
/// bytes.
pub(crate) async fn create_visual_prompt_from_image(
    s: &ApiState,
    name: &str,
    description: Option<&str>,
    image_bytes: &[u8],
    content_type: Option<&str>,
) -> Result<VisualPromptId, ApiError> {
    let extension = guess_extension(content_type, image_bytes);

    let admin = &s.visual_prompts;
    // Encoder ONNX path is required for the create path; the
    // listing / detach paths still work without an encoder.
    let encoder_model_path = match admin.encoder_model_path.as_ref() {
        Some(p) => p.clone(),
        None => {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "encoder_not_configured: set inference.model.pack_path".to_string(),
            ))
        }
    };

    // Decode the image first — both to fail-fast on garbage bytes
    // AND to get the (width, height, RGB bytes) the encoder needs.
    let (rgb_bytes, src_w, src_h) = match decode_to_rgb(image_bytes) {
        Ok(t) => t,
        Err(e) => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("image decode: {e}"),
            ))
        }
    };

    // SHA256 of the original bytes — keys the on-disk filename so
    // re-supplying the same crop reuses the same file (and the
    // store's UNIQUE constraint surfaces "name taken" only when the
    // caller actually picked a different name with the same image).
    let mut hasher = Sha256::new();
    hasher.update(image_bytes);
    let sha = hex_digest(&hasher.finalize());

    // Write to disk before computing the embedding — the file is
    // the source of truth for the original pixels (used on encoder
    // upgrades to re-embed without asking the operator to re-supply).
    tokio::fs::create_dir_all(&admin.visual_prompts_dir)
        .await
        .map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "create_dir_all({}): {e}",
                    admin.visual_prompts_dir.display()
                ),
            )
        })?;
    let image_filename = format!("{sha}.{extension}");
    let on_disk_path = admin.visual_prompts_dir.join(&image_filename);
    tokio::fs::write(&on_disk_path, image_bytes)
        .await
        .map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write {}: {e}", on_disk_path.display()),
            )
        })?;

    // Compute the embedding. Encoder lazy-init on first call.
    let embedding = encode_image(admin, &encoder_model_path, rgb_bytes, src_w, src_h).await?;

    // Persist. The store layer validates `embedding.len() ==
    // embedding_dim` and rejects an empty embedding.
    let new = NewVisualPrompt {
        name,
        description,
        image_path: image_filename.as_str(),
        image_sha256: sha.as_str(),
        embedding: embedding.as_slice(),
        encoder_model_id: admin.encoder_model_id.as_str(),
    };
    s.store
        .create_visual_prompt(&new)
        .await
        .map_err(visual_prompt_error_to_api)
}

pub async fn post_visual_prompt(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<VisualPrompt>), ApiError> {
    let mut fields = UploadFields::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("multipart: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "name" => {
                let v = field
                    .text()
                    .await
                    .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("name: {e}")))?;
                fields.name = Some(v.trim().to_string());
            }
            "description" => {
                let v = field
                    .text()
                    .await
                    .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("description: {e}")))?;
                let v = v.trim();
                if !v.is_empty() {
                    fields.description = Some(v.to_string());
                }
            }
            "image" => {
                let ct = field.content_type().map(|s| s.to_string());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("image: {e}")))?;
                fields.image_content_type = ct;
                fields.image_bytes = Some(bytes.to_vec());
            }
            _ => {
                // Unknown field — ignore. Keeps the API
                // forward-compatible with future fields that older
                // engine versions don't recognise.
            }
        }
    }

    let name = match fields.name {
        Some(n) if !n.is_empty() => n,
        _ => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "missing field: name".to_string(),
            ))
        }
    };
    let image_bytes = match fields.image_bytes {
        Some(b) if !b.is_empty() => b,
        _ => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "missing field: image".to_string(),
            ))
        }
    };
    let id = create_visual_prompt_from_image(
        &s,
        &name,
        fields.description.as_deref(),
        &image_bytes,
        fields.image_content_type.as_deref(),
    )
    .await?;
    let stored = s
        .store
        .get_visual_prompt(id)
        .await
        .map_err(visual_prompt_error_to_api)?
        .ok_or_else(|| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "visual prompt vanished between insert and read".to_string(),
            )
        })?
        .0;

    // Audit (best-effort — we don't undo the upload if audit fails).
    let after_str = serde_json::to_string(&stored).ok();
    crate::auth::admin_audit::audit_admin_action(
        &s.store,
        session.as_ref(),
        &headers,
        peer.ip(),
        "visual_prompt.create",
        "visual_prompt",
        Some(id.to_string().as_str()),
        nexus_store::audit::AuditOutcome::Success,
        None,
        after_str.as_deref(),
    )
    .await;

    Ok((StatusCode::CREATED, Json(stored)))
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

pub async fn delete_visual_prompt(
    State(s): State<ApiState>,
    Path(id): Path<VisualPromptId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
) -> Result<StatusCode, ApiError> {
    // Capture before-state for audit, then attempt the delete.
    let before = s
        .store
        .get_visual_prompt(id)
        .await
        .ok()
        .flatten()
        .map(|(vp, _)| vp);
    let before_str = before.as_ref().and_then(|b| serde_json::to_string(b).ok());
    match s.store.delete_visual_prompt(id).await {
        Ok(()) => {
            crate::auth::admin_audit::audit_admin_action(
                &s.store,
                session.as_ref(),
                &headers,
                peer.ip(),
                "visual_prompt.delete",
                "visual_prompt",
                Some(id.to_string().as_str()),
                nexus_store::audit::AuditOutcome::Success,
                before_str.as_deref(),
                None,
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(VisualPromptError::Conflict(n)) => Err(ApiError(
            StatusCode::CONFLICT,
            format!("attached to {n} camera(s); detach first"),
        )),
        Err(e) => Err(visual_prompt_error_to_api(e)),
    }
}

// ---------------------------------------------------------------------------
// Camera attach / detach / list
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct AttachedVisualPrompt {
    pub id: VisualPromptId,
    pub name: String,
    pub description: Option<String>,
    pub embedding_dim: i64,
    pub encoder_model_id: String,
    pub image_path: String,
}

pub async fn list_camera_visual_prompts(
    State(s): State<ApiState>,
    Path(camera_id): Path<CameraId>,
) -> Result<Json<Vec<AttachedVisualPrompt>>, ApiError> {
    let rows = s
        .store
        .list_camera_visual_prompts(camera_id)
        .await
        .map_err(visual_prompt_error_to_api)?;
    let out: Vec<AttachedVisualPrompt> = rows
        .into_iter()
        .map(|(vp, _embedding)| AttachedVisualPrompt {
            id: vp.id,
            name: vp.name,
            description: vp.description,
            embedding_dim: vp.embedding_dim,
            encoder_model_id: vp.encoder_model_id,
            image_path: vp.image_path,
        })
        .collect();
    Ok(Json(out))
}

pub async fn attach_camera_visual_prompt(
    State(s): State<ApiState>,
    Path((camera_id, visual_prompt_id)): Path<(CameraId, VisualPromptId)>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
) -> Result<StatusCode, ApiError> {
    match s
        .store
        .attach_camera_visual_prompt(camera_id, visual_prompt_id)
        .await
    {
        Ok(()) => {
            crate::auth::admin_audit::audit_admin_action(
                &s.store,
                session.as_ref(),
                &headers,
                peer.ip(),
                "camera.visual_prompt.attach",
                "camera",
                Some(camera_id.to_string().as_str()),
                nexus_store::audit::AuditOutcome::Success,
                None,
                Some(visual_prompt_id.to_string().as_str()),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(VisualPromptError::NotFound) => Err(ApiError(
            StatusCode::NOT_FOUND,
            "camera or visual prompt not found".to_string(),
        )),
        Err(e) => Err(visual_prompt_error_to_api(e)),
    }
}

pub async fn detach_camera_visual_prompt(
    State(s): State<ApiState>,
    Path((camera_id, visual_prompt_id)): Path<(CameraId, VisualPromptId)>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
) -> Result<StatusCode, ApiError> {
    match s
        .store
        .detach_camera_visual_prompt(camera_id, visual_prompt_id)
        .await
    {
        Ok(()) => {
            crate::auth::admin_audit::audit_admin_action(
                &s.store,
                session.as_ref(),
                &headers,
                peer.ip(),
                "camera.visual_prompt.detach",
                "camera",
                Some(camera_id.to_string().as_str()),
                nexus_store::audit::AuditOutcome::Success,
                Some(visual_prompt_id.to_string().as_str()),
                None,
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(VisualPromptError::NotFound) => Err(ApiError(
            StatusCode::NOT_FOUND,
            "attachment not found".to_string(),
        )),
        Err(e) => Err(visual_prompt_error_to_api(e)),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn visual_prompt_error_to_api(e: VisualPromptError) -> ApiError {
    match e {
        VisualPromptError::Store(se) => ApiError::from(se),
        VisualPromptError::NameTaken => {
            ApiError(StatusCode::CONFLICT, "visual prompt name taken".into())
        }
        VisualPromptError::NotFound => {
            ApiError(StatusCode::NOT_FOUND, "visual prompt not found".into())
        }
        VisualPromptError::Conflict(n) => ApiError(
            StatusCode::CONFLICT,
            format!("attached to {n} camera(s); detach first"),
        ),
        VisualPromptError::EmptyEmbedding => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "encoder produced empty embedding".into(),
        ),
        VisualPromptError::EmbeddingShape {
            dim,
            expected,
            actual,
        } => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("embedding shape mismatch: dim={dim}, expected {expected} bytes, got {actual}"),
        ),
    }
}

fn hex_digest(d: &[u8]) -> String {
    let mut out = String::with_capacity(d.len() * 2);
    for b in d {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn guess_extension(content_type: Option<&str>, bytes: &[u8]) -> &'static str {
    match content_type {
        Some(s) if s.eq_ignore_ascii_case("image/png") => return "png",
        Some(s) if s.eq_ignore_ascii_case("image/jpeg") || s.eq_ignore_ascii_case("image/jpg") => {
            return "jpg"
        }
        Some(s) if s.eq_ignore_ascii_case("image/webp") => return "webp",
        _ => {}
    }
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        "png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "jpg"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else {
        "bin"
    }
}

fn decode_to_rgb(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    if w == 0 || h == 0 {
        return Err("zero-dimension image".to_string());
    }
    Ok((rgb.into_raw(), w, h))
}

#[cfg(feature = "ort")]
async fn encode_image(
    admin: &VisualPromptsAdminState,
    encoder_model_path: &std::path::Path,
    rgb_bytes: Vec<u8>,
    src_w: u32,
    src_h: u32,
) -> Result<Vec<f32>, ApiError> {
    let path = encoder_model_path.to_path_buf();
    let embedding_dim = admin.encoder_embedding_dim;
    let model_id = admin.encoder_model_id.clone();
    let ep_priority = admin.encoder_ep_priority.clone();
    let encoder_cell = admin.encoder.clone();
    // Lazy-init the encoder on first POST. The load itself is sync
    // (opens an ORT session); wrap in spawn_blocking so the multipart
    // path doesn't park the runtime.
    let encoder = encoder_cell
        .get_or_try_init(|| async move {
            let path_c = path.clone();
            let model_id_c = model_id.clone();
            let ep_c = ep_priority.clone();
            tokio::task::spawn_blocking(move || {
                nexus_inference::ImageEncoder::load(&path_c, embedding_dim, model_id_c, &ep_c)
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| format!("encoder lazy-init join: {e}"))?
        })
        .await
        .map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("encoder load: {e}"),
            )
        })?
        .clone();
    // encode_rgb is sync; spawn_blocking so the ORT session call
    // doesn't park the runtime either.
    let embedding =
        tokio::task::spawn_blocking(move || encoder.encode_rgb(&rgb_bytes, src_w, src_h))
            .await
            .map_err(|e| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("encoder join: {e}"),
                )
            })?
            .map_err(|e| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("encoder run: {e}"),
                )
            })?;
    if embedding.is_empty() {
        return Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "encoder produced empty embedding".into(),
        ));
    }
    Ok(embedding)
}

#[cfg(not(feature = "ort"))]
async fn encode_image(
    _admin: &VisualPromptsAdminState,
    _encoder_model_path: &std::path::Path,
    _rgb_bytes: Vec<u8>,
    _src_w: u32,
    _src_h: u32,
) -> Result<Vec<f32>, ApiError> {
    // This is unreachable from the POST handler because
    // encoder_model_path-presence already gates the call, but
    // having the cfg-off body keeps the build green without ort.
    Err(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "encoder_not_available: this build was compiled without the `ort` feature".into(),
    ))
}

//! Phase 7.5 · Step 7.5.4 — edge fleet-settings apply endpoint.
//!
//! Route (gated by the admin-auth middleware in
//! [`crate::api::router`] alongside the other `/v1/admin/*` writes):
//!
//! * `POST /api/v1/admin/fleet/{category}`
//!
//! The cloud api-gateway's fleet-apply pipeline (Phase 7.5.3) fans a
//! resolved configuration out to every enrolled core as an `rpc_call`
//! envelope with path `/admin/fleet/{db_key}`; the `engine_rpc`
//! dispatcher prepends `/api/v1`, producing
//! `/api/v1/admin/fleet/{db_key}`. The body the cloud sends is:
//!
//! ```json
//! { "effective": <category-specific payload>,
//!   "scope": { "type": "org|site|core|camera", "id": "<uuid>" } }
//! ```
//!
//! where `effective` is the cloud-resolved (org → site → core folded)
//! value for the category. The `db_key` segment is one of `rules`,
//! `text_prompts`, `visual_prompts`, `detector_config`,
//! `delivery_settings`.
//!
//! # Apply semantics (v1)
//!
//! * **Replace, not overlay.** Phase 7.5.5 — a fleet apply *overwrites*
//!   the local state for the category: the edge reconciles its
//!   configuration to exactly the `effective` payload, deleting local
//!   items the fleet no longer lists (`rules` not in the set are
//!   removed; `visual_prompts` not in the set are detached). Fleet
//!   management wins over operator-local edits. The apply is idempotent:
//!   applying the same payload twice is a no-op.
//! * **Per-category fleet-managed marker.** Every successful apply
//!   upserts a [`nexus_store::FleetManagedMarker`] row recording that the
//!   category is fleet-managed, the apply scope, and the canonical
//!   SHA-256 of the effective payload. The local admin UI reads these to
//!   badge a category as "Fleet-managed".
//! * **Per-camera categories apply core-wide.** `text_prompts`,
//!   `visual_prompts`, and `detector_config` are applied to **every**
//!   local camera on this core, regardless of the `scope.type`. The
//!   edge stores cameras under local integer ids only — there is no
//!   cloud-camera-UUID mapping — so `scope.id` cannot select a single
//!   camera. The cloud already dispatches at core granularity
//!   (Phase 7.5.3); `scope` is recorded for audit / tracing only.
//! * **`visual_prompts` attaches by name, and creates from a pushed
//!   reference image.** The reconcile attaches prompts that already
//!   exist locally (matched by `name`) and detaches any
//!   currently-attached prompt the fleet no longer lists. When a name
//!   is *not* present locally but the entry carries an `image_url`
//!   (a signed read SAS minted by the cloud, Phase 7.5.9), the edge
//!   downloads the reference image over HTTPS with `reqwest`, verifies
//!   its SHA-256 against the entry's `sha256` (content-address
//!   integrity), encodes the embedding (needs the `ort` feature + an
//!   ONNX encoder) and creates the prompt locally before attaching it.
//!   An unknown name *without* an `image_url` still yields a `400` so
//!   the cloud records that target as failed. Media never crosses the
//!   gateway — only the signed SAS URL does (AGENTS.md §7).

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use nexus_bus::{topic, BusExt};
use nexus_config::{ModelConfig, RuleConfig};
use nexus_store::audit::AuditOutcome;
use nexus_types::{RuleId, VisualPromptId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::api::{ApiError, ApiState};
use crate::auth::require_role::SessionContext;

/// Inbound body for `POST /v1/admin/fleet/{category}`.
#[derive(Debug, Deserialize)]
pub struct FleetApplyReq {
    /// Cloud-resolved value for the category. Shape depends on the
    /// `{category}` path segment — see the per-category helpers.
    pub effective: Value,
    /// Original apply scope. Recorded for audit / tracing only; it
    /// does not influence targeting on the edge (see module docs).
    #[serde(default)]
    pub scope: Option<FleetScope>,
}

/// The `scope` half of [`FleetApplyReq`].
#[derive(Debug, Deserialize)]
pub struct FleetScope {
    #[serde(rename = "type")]
    pub scope_type: String,
    pub id: String,
}

/// Response body — echoes the applied category and the number of
/// targets (rules upserted, cameras touched, or prompts attached).
#[derive(Debug, Serialize)]
pub struct FleetApplyResponse {
    pub category: String,
    pub targets: usize,
}

/// A single `visual_prompts` entry. `name` is always consumed; the
/// remaining fields drive the create-by-image path (Phase 7.5.9):
/// when `name` is unknown locally, the edge fetches `image_url`,
/// verifies the bytes hash to `sha256`, and encodes a new prompt.
/// Unknown serde fields (e.g. the cloud's `ext`) are ignored.
#[derive(Debug, Deserialize)]
struct FleetVisualPromptEntry {
    name: String,
    /// Lowercase hex SHA-256 of the reference image's original bytes.
    /// Present when the cloud has an uploaded image for this prompt.
    #[serde(default)]
    sha256: Option<String>,
    /// Signed read SAS URL for the reference image. Present alongside
    /// `sha256` when the cloud has an uploaded image.
    #[serde(default)]
    image_url: Option<String>,
    /// Optional operator-supplied description carried through to the
    /// created prompt.
    #[serde(default)]
    description: Option<String>,
}

/// `POST /v1/admin/fleet/{category}` — apply a cloud-resolved fleet
/// setting to this core. See module docs for semantics.
pub async fn post_admin_fleet_apply(
    State(s): State<ApiState>,
    Path(category): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<SessionContext>,
    Json(req): Json<FleetApplyReq>,
) -> Result<Json<FleetApplyResponse>, ApiError> {
    let result = apply_category(&s, &category, &req.effective).await;
    let outcome = if result.is_ok() {
        AuditOutcome::Success
    } else {
        AuditOutcome::Failure
    };
    let after_str = serde_json::to_string(&req.effective).ok();
    crate::auth::admin_audit::audit_admin_action(
        &s.store,
        session.as_ref(),
        &headers,
        peer.ip(),
        "fleet.apply",
        "fleet",
        Some(category.as_str()),
        outcome,
        None,
        after_str.as_deref(),
    )
    .await;
    let targets = result?;
    // Record that the category is now fleet-managed (badge source for
    // the local admin UI) along with the canonical hash of what we
    // applied and the original apply scope.
    let effective_sha = crate::fleet_hash::sha256_canonical(&req.effective);
    let (scope_type, scope_id) = match &req.scope {
        Some(scope) => (Some(scope.scope_type.as_str()), Some(scope.id.as_str())),
        None => (None, None),
    };
    if let Err(e) = s
        .store
        .fleet_marker_upsert(&category, scope_type, scope_id, Some(&effective_sha))
        .await
    {
        tracing::warn!(category = %category, error = %e, "fleet-managed marker upsert failed");
    }
    match &req.scope {
        Some(scope) => tracing::info!(
            category = %category,
            scope_type = %scope.scope_type,
            scope_id = %scope.id,
            targets,
            "fleet apply ok"
        ),
        None => tracing::info!(category = %category, targets, "fleet apply ok"),
    }
    Ok(Json(FleetApplyResponse { category, targets }))
}

/// Response body for `GET /v1/admin/fleet/managed`.
#[derive(Debug, Serialize)]
pub struct FleetManagedResponse {
    pub managed: Vec<nexus_store::FleetManagedMarker>,
}

/// `GET /v1/admin/fleet/managed` — list the fleet-settings categories
/// currently under cloud fleet management on this core, so the local
/// admin UI can badge them as "Fleet-managed". Read-only; admin-gated.
pub async fn get_admin_fleet_managed(
    State(s): State<ApiState>,
    _admin: crate::auth::require_role::AdminContext,
) -> Result<Json<FleetManagedResponse>, ApiError> {
    let managed = s.store.list_fleet_managed_markers().await?;
    Ok(Json(FleetManagedResponse { managed }))
}

/// Dispatch on the `db_key` category segment.
async fn apply_category(
    s: &ApiState,
    category: &str,
    effective: &Value,
) -> Result<usize, ApiError> {
    match category {
        "rules" => apply_rules(s, effective).await,
        "text_prompts" => apply_text_prompts(s, effective).await,
        "visual_prompts" => apply_visual_prompts(s, effective).await,
        "detector_config" => apply_detector_config(s, effective).await,
        "delivery_settings" => apply_delivery_settings(s, effective).await,
        other => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("unknown fleet category: {other:?}"),
        )),
    }
}

/// `rules` — `effective` is a JSON array of full [`RuleConfig`]
/// objects. Every rule's CEL `when` clause is compiled up-front so a
/// bad payload fails atomically (nothing is persisted). REPLACE
/// semantics: rules in the payload are upserted by id and any local
/// rule whose id is not in the payload is deleted, all within one
/// transaction. The live evaluator is hot-reloaded afterwards.
async fn apply_rules(s: &ApiState, effective: &Value) -> Result<usize, ApiError> {
    let rules: Vec<RuleConfig> = serde_json::from_value(effective.clone())
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("rules payload: {e}")))?;
    for rule in &rules {
        if rule.id.trim().is_empty() {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("rule {:?} has an empty id", rule.name),
            ));
        }
        if let Err(msg) = crate::api::compile_cel_safely(rule) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("invalid CEL in rule {:?}: {msg}", rule.id),
            ));
        }
    }
    let keep: std::collections::HashSet<&str> = rules.iter().map(|r| r.id.as_str()).collect();
    let existing = s.store.list_rules().await?;
    let mut tx = s.store.begin_tx().await?;
    for rule in &existing {
        if !keep.contains(rule.id.as_str()) {
            s.store.delete_rule_tx(&mut tx, &rule.id).await?;
        }
    }
    for rule in &rules {
        s.store.upsert_rule_tx(&mut tx, rule).await?;
    }
    nexus_store::Store::commit_tx(tx).await?;
    let reload_id: RuleId = "fleet-apply".to_string();
    crate::api::reload_rules_into_evaluator(s, "fleet-apply", &reload_id).await;
    Ok(rules.len())
}

/// `text_prompts` — `effective` is a JSON array of strings. The list
/// is written to every local camera's `detector.prompts`.
async fn apply_text_prompts(s: &ApiState, effective: &Value) -> Result<usize, ApiError> {
    let prompts: Vec<String> = serde_json::from_value(effective.clone()).map_err(|e| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("text_prompts payload: {e}"),
        )
    })?;
    let mut cameras = s.store.list_cameras().await?;
    let mut tx = s.store.begin_tx().await?;
    for cam in &mut cameras {
        cam.detector.prompts = prompts.clone();
        s.store.upsert_camera_tx(&mut tx, cam).await?;
    }
    nexus_store::Store::commit_tx(tx).await?;
    let _ = s
        .bus
        .publish(topic::CONFIG_CHANGED, &serde_json::json!({}))
        .await;
    Ok(cameras.len())
}

/// `detector_config` — `effective` is a [`ModelConfig`] object,
/// applied as every local camera's `detector.model_override`.
async fn apply_detector_config(s: &ApiState, effective: &Value) -> Result<usize, ApiError> {
    let model: ModelConfig = serde_json::from_value(effective.clone()).map_err(|e| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("detector_config payload: {e}"),
        )
    })?;
    let mut cameras = s.store.list_cameras().await?;
    let mut tx = s.store.begin_tx().await?;
    for cam in &mut cameras {
        cam.detector.model_override = Some(model.clone());
        s.store.upsert_camera_tx(&mut tx, cam).await?;
    }
    nexus_store::Store::commit_tx(tx).await?;
    let _ = s
        .bus
        .publish(topic::CONFIG_CHANGED, &serde_json::json!({}))
        .await;
    Ok(cameras.len())
}

/// `delivery_settings` — `effective` is a `{enabled, schedule?,
/// timezone?}` object (the same shape `PUT /v1/admin/delivery`
/// accepts). The singleton row is upserted.
async fn apply_delivery_settings(s: &ApiState, effective: &Value) -> Result<usize, ApiError> {
    #[derive(Deserialize)]
    struct DeliveryPayload {
        enabled: bool,
        #[serde(default)]
        schedule: Option<nexus_types::DeliverySchedule>,
        #[serde(default)]
        timezone: Option<String>,
    }
    let payload: DeliveryPayload = serde_json::from_value(effective.clone()).map_err(|e| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("delivery_settings payload: {e}"),
        )
    })?;
    let timezone = payload
        .timezone
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "UTC".to_string());
    if timezone.parse::<chrono_tz::Tz>().is_err() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown IANA timezone: {timezone:?}"),
        ));
    }
    let settings = nexus_types::DeliverySettings {
        enabled: payload.enabled,
        schedule: payload.schedule,
        timezone,
        updated_at: chrono::Utc::now(),
    };
    let mut tx = s.store.begin_tx().await?;
    s.store.delivery_settings_put_tx(&mut tx, &settings).await?;
    nexus_store::Store::commit_tx(tx).await?;
    let _ = s
        .bus
        .publish(topic::DELIVERY_SETTINGS_CHANGED, &serde_json::json!({}))
        .await;
    Ok(1)
}

/// `visual_prompts` — attach/detach reconcile with create-by-image.
/// `effective` is a JSON array of `{name, sha256?, image_url?, …}`
/// entries. A name that already exists locally is attached. A name
/// that is *not* present locally but carries an `image_url` is
/// downloaded (the bytes verified against `sha256`), encoded, and
/// created before attaching; an unknown name *without* an `image_url`
/// is a `400` so the cloud records the target as failed. REPLACE
/// semantics: on every local camera the resolved prompts are attached
/// and any currently-attached prompt the fleet no longer lists is
/// detached. A repeated identical apply is a no-op (the create path
/// is skipped because the name now resolves locally).
async fn apply_visual_prompts(s: &ApiState, effective: &Value) -> Result<usize, ApiError> {
    let entries: Vec<FleetVisualPromptEntry> =
        serde_json::from_value(effective.clone()).map_err(|e| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("visual_prompts payload: {e}"),
            )
        })?;
    let mut ids: Vec<VisualPromptId> = Vec::with_capacity(entries.len());
    let mut unknown: Vec<String> = Vec::new();
    for entry in &entries {
        match s.store.get_visual_prompt_by_name(&entry.name).await {
            Ok(Some(summary)) => ids.push(summary.id),
            // Unknown locally. If the fleet payload carries a reference
            // image, download + verify + encode + create it; otherwise
            // it's a legacy attach-by-name miss → 400.
            Ok(None) if entry.image_url.is_some() => {
                ids.push(fetch_and_create_visual_prompt(s, entry).await?);
            }
            Ok(None) => unknown.push(entry.name.clone()),
            Err(e) => return Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        }
    }
    if !unknown.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown visual prompt(s) not present on this core: {unknown:?}"),
        ));
    }
    let keep: std::collections::HashSet<VisualPromptId> = ids.iter().copied().collect();
    let cameras = s.store.list_cameras().await?;
    let mut changed = false;
    for cam in &cameras {
        let current = s
            .store
            .list_camera_visual_prompt_ids(cam.id)
            .await
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let current_set: std::collections::HashSet<VisualPromptId> =
            current.iter().copied().collect();
        // Detach prompts the fleet no longer lists.
        for vp_id in &current {
            if !keep.contains(vp_id) {
                s.store
                    .detach_camera_visual_prompt(cam.id, *vp_id)
                    .await
                    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                changed = true;
            }
        }
        // Attach the listed prompts (idempotent).
        for vp_id in &ids {
            if !current_set.contains(vp_id) {
                s.store
                    .attach_camera_visual_prompt(cam.id, *vp_id)
                    .await
                    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                changed = true;
            }
        }
    }
    if changed {
        let _ = s
            .bus
            .publish(topic::CONFIG_CHANGED, &serde_json::json!({}))
            .await;
    }
    Ok(ids.len())
}

/// Upper bound on a fleet-pushed reference image. Mirrors the cloud
/// api-gateway's upload cap so a tampered or oversized SAS target
/// can't make the edge buffer an unbounded body.
const FLEET_VP_MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// Lowercase hex SHA-256 of `bytes`. Local helper so the verify path
/// doesn't depend on the admin module's private `hex_digest`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Download a fleet-pushed reference image, verify it hashes to the
/// advertised `sha256`, then encode + create the visual prompt
/// locally. Returns the new prompt's id.
///
/// The SHA-256 check is a security boundary, not a nicety: the blob
/// lives at a content-addressed path the cloud minted, so a body that
/// doesn't hash to `sha256` means corruption or tampering and is
/// rejected *before* the bytes ever reach the image decoder.
async fn fetch_and_create_visual_prompt(
    s: &ApiState,
    entry: &FleetVisualPromptEntry,
) -> Result<VisualPromptId, ApiError> {
    let url = entry.image_url.as_deref().ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "visual prompt {:?}: image_url required to create by image",
                entry.name
            ),
        )
    })?;
    let expected_sha = entry.sha256.as_deref().ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("visual prompt {:?}: image_url without sha256", entry.name),
        )
    })?;

    let resp = reqwest::Client::new().get(url).send().await.map_err(|e| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            format!("visual prompt {:?} image fetch: {e}", entry.name),
        )
    })?;
    if !resp.status().is_success() {
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            format!(
                "visual prompt {:?} image fetch: HTTP {}",
                entry.name,
                resp.status()
            ),
        ));
    }
    if let Some(len) = resp.content_length() {
        if len > FLEET_VP_MAX_IMAGE_BYTES {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!(
                    "visual prompt {:?} image too large: {len} bytes > {FLEET_VP_MAX_IMAGE_BYTES} max",
                    entry.name
                ),
            ));
        }
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = resp.bytes().await.map_err(|e| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            format!("visual prompt {:?} image body: {e}", entry.name),
        )
    })?;
    if bytes.len() as u64 > FLEET_VP_MAX_IMAGE_BYTES {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "visual prompt {:?} image too large: {} bytes > {FLEET_VP_MAX_IMAGE_BYTES} max",
                entry.name,
                bytes.len()
            ),
        ));
    }

    let actual_sha = sha256_hex(&bytes);
    if !actual_sha.eq_ignore_ascii_case(expected_sha) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "visual prompt {:?} image sha256 mismatch: expected {expected_sha}, got {actual_sha}",
                entry.name
            ),
        ));
    }

    crate::visual_prompts_admin::create_visual_prompt_from_image(
        s,
        &entry.name,
        entry.description.as_deref(),
        &bytes,
        content_type.as_deref(),
    )
    .await
}

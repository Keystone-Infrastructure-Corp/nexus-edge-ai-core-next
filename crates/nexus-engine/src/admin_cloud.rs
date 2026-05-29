//! M-Cloud Phase 1 — UI-driven cloud enrollment.
//!
//! Two endpoints live here, both gated by [`admin_auth_layer`] AND the
//! per-handler [`AdminContext`] extractor (defense in depth: gate
//! authenticates, extractor authorises):
//!
//! 1. **Status probe** — `GET /v1/admin/cloud/enrollment`. Returns a
//!    redacted view of the local `cloud_enrollment` row so the UI can
//!    decide whether to show "Connect to cloud" (unenrolled) or
//!    "Connected to <gateway> as <core_id>" (enrolled). MUST NOT
//!    return the mTLS private key or the entitlement JWT — those stay
//!    in SQLite and never cross the admin HTTP surface.
//!
//! 2. **Enroll** — `POST /v1/admin/cloud/enroll`. Wraps the same
//!    [`crate::cloud_enroll::perform_enrollment`] helper the
//!    `nexus-engine enroll` CLI uses, so the two paths can never drift.
//!    Phase 1.16: after the row is persisted the handler fires
//!    [`ApiState::cloud_enrollment_changed`] so the tunnel supervisor
//!    parked in its pre-connection wait state hot-activates the WSS
//!    tunnel within seconds — no engine restart required for the
//!    first-time enrollment case. Re-enrolling an already-connected
//!    engine still requires a restart to swap the live cert material
//!    (the tunnel client, trace uploader, and RPC dispatcher are
//!    built once from the artefact and reused for the rest of the
//!    process lifetime).
//!
//! ## Why no DELETE
//!
//! Disconnecting from the cloud is a heavy-handed operation that
//! should be deliberate; today the operator does it by stopping the
//! engine and clearing the SQLite row (or wiping the appliance via
//! `nexus-engine factory-reset` once that lands). Adding a one-click
//! "unlink" button now would invite accidental disconnections in
//! production. Re-enrollment via POST overwrites the existing row
//! atomically.
//!
//! ## Audit trail
//!
//! `cloud.enroll.put` audit row is written in the same transaction as
//! the `cloud_enrollment` row update so a failed audit insert rolls
//! back the enrollment. `before`/`after` capture `core_id` +
//! `gateway_url` only — never any PEM, never the entitlement JWT.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiState};
use crate::auth::require_role::AdminContext;
use crate::cloud_enroll::perform_enrollment;

/// Redacted view of the local `cloud_enrollment` row. Used by both the
/// status probe and the post-enroll response so the UI can keep a
/// single discriminated-union type.
#[derive(Debug, Clone, Serialize)]
pub struct CloudEnrollmentStatus {
    /// `true` iff a row exists in `cloud_enrollment`.
    pub enrolled: bool,
    /// Cloud-assigned `cores.id`. `None` when `enrolled == false`.
    pub core_id: Option<String>,
    /// `wss://` URL the engine will dial on its next boot. `None` when
    /// `enrolled == false`.
    pub gateway_url: Option<String>,
    /// UTC timestamp of the most recent successful enrollment
    /// round-trip. `None` when `enrolled == false`.
    pub enrolled_at: Option<DateTime<Utc>>,
}

impl CloudEnrollmentStatus {
    fn unenrolled() -> Self {
        Self {
            enrolled: false,
            core_id: None,
            gateway_url: None,
            enrolled_at: None,
        }
    }
}

/// `POST /v1/admin/cloud/enroll` request body.
#[derive(Debug, Deserialize)]
pub struct PostCloudEnrollReq {
    /// Short single-use enrollment code minted from the cloud console
    /// ("Add Core" flow). E.g. `XJ4K-PMQ7-9NAB`. Required.
    pub code: String,
    /// Cloud-console base URL. Must include scheme. E.g.
    /// `https://cloud.example`. Required.
    pub cloud_host: String,
    /// Human-friendly label baked into the CSR's CommonName. Defaults
    /// to the local hostname when omitted.
    #[serde(default)]
    pub label: Option<String>,
    /// Phase 2 · Step 2.9 — when `true`, the local motion-clip
    /// backlog from the past `history_days` will be replayed into
    /// the cloud after the next `serve` boot picks up the new
    /// enrollment. Defaults to `false` so most operators don't end
    /// up with pre-cloud noise in their fresh console.
    #[serde(default)]
    pub keep_history: bool,
    /// Phase 2 · Step 2.9 — how many days of local history to replay
    /// when `keep_history == true`. Capped at 365 server-side.
    #[serde(default = "default_history_days")]
    pub history_days: u32,
}

fn default_history_days() -> u32 {
    30
}

/// `GET /v1/admin/cloud/enrollment` — surfaced verbatim by the
/// `AdminServerPage` "Cloud connection" card and the setup wizard's
/// Cloud step so the UI can render the enrolled / unenrolled state.
///
/// Returns the redacted status view. Never includes the mTLS private
/// key or the entitlement JWT.
pub async fn get_cloud_enrollment(
    State(s): State<ApiState>,
    _admin: AdminContext,
) -> Result<Json<CloudEnrollmentStatus>, ApiError> {
    let row = s
        .store
        .get_cloud_enrollment()
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(match row {
        Some(e) => CloudEnrollmentStatus {
            enrolled: true,
            core_id: Some(e.core_id),
            gateway_url: Some(e.gateway_url),
            enrolled_at: Some(e.enrolled_at),
        },
        None => CloudEnrollmentStatus::unenrolled(),
    }))
}

/// `POST /v1/admin/cloud/enroll` — runs the same enrollment flow as
/// the `nexus-engine enroll` CLI subcommand and persists the result
/// into the local `cloud_enrollment` row.
///
/// Restart-required: the WSS tunnel is spawned exactly once at boot
/// from the persisted row. The UI surfaces this as a "Restart engine
/// to connect" affordance — same pattern as
/// `/v1/admin/server/inference`.
pub async fn post_cloud_enroll(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    admin: AdminContext,
    Json(req): Json<PostCloudEnrollReq>,
) -> Result<Json<CloudEnrollmentStatus>, ApiError> {
    // ---- input validation (cheap, run before we touch the network) ----
    let code = req.code.trim();
    if code.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "code must be a non-empty string".into(),
        ));
    }
    let cloud_host = req.cloud_host.trim();
    if cloud_host.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "cloud_host must be a non-empty URL (e.g. https://cloud.example)".into(),
        ));
    }
    // Reject obvious typos before issuing a network call. The
    // enrollment-svc strictly requires HTTPS in prod; allow plain HTTP
    // only for `127.0.0.1` / `localhost` so a dev box pointing at a
    // local mock cloud still works.
    if !(cloud_host.starts_with("https://")
        || cloud_host.starts_with("http://127.0.0.1")
        || cloud_host.starts_with("http://localhost"))
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "cloud_host must start with https:// (or http://127.0.0.1 / http://localhost \
             for local development)"
                .into(),
        ));
    }
    if req.history_days > 365 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "history_days must be in 1..=365 (got {}); server-side cap mirrors \
                 the CLI's --history-days clamp",
                req.history_days
            ),
        ));
    }

    // ---- before-snapshot for audit (only the non-sensitive fields) ----
    let before_row = s
        .store
        .get_cloud_enrollment()
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let before_audit = before_row.as_ref().map(|e| {
        serde_json::json!({
            "core_id": e.core_id,
            "gateway_url": e.gateway_url,
            "enrolled_at": e.enrolled_at.to_rfc3339(),
        })
    });

    // ---- perform the round-trip ----
    let persisted = perform_enrollment(
        &s.store,
        code,
        cloud_host,
        req.label.as_deref(),
        req.keep_history,
        req.history_days,
    )
    .await
    .map_err(|e| {
        // Bubble the upstream error message verbatim so the UI can
        // distinguish "bad code" (400 from enrollment-svc) from "DNS
        // failure" (transport). The cloud-side enrollment-svc returns
        // a JSON error envelope; we wrap it in our own envelope.
        tracing::warn!(error = %e, "cloud enrollment failed");
        ApiError(StatusCode::BAD_GATEWAY, format!("enrollment failed: {e}"))
    })?;

    // ---- audit (fire-and-forget by design — the enrollment is
    //      already persisted, and `audit_admin_action` swallows write
    //      failures internally with a `warn!` so the operator still
    //      sees a 200 OK) ----
    let after_audit = serde_json::json!({
        "core_id": persisted.core_id,
        "gateway_url": persisted.gateway_url,
        "enrolled_at": persisted.enrolled_at.to_rfc3339(),
    });
    let before_audit_str = before_audit.as_ref().map(ToString::to_string);
    let after_audit_str = after_audit.to_string();
    crate::auth::admin_audit::audit_admin_action(
        &s.store,
        Some(&admin.0),
        &headers,
        peer.ip(),
        "cloud.enroll.put",
        "admin/cloud/enrollment",
        Some("singleton"),
        nexus_store::audit::AuditOutcome::Success,
        before_audit_str.as_deref(),
        Some(&after_audit_str),
    )
    .await;

    // Phase 1.16 — wake the tunnel supervisor. If it's currently
    // parked in the pre-connection wait state (no prior
    // enrollment), it will re-probe `cloud_enrollment` and dial the
    // gateway within seconds. If it's already connected to a prior
    // enrollment, the notification is a no-op — re-enrollment of a
    // running engine still requires a restart to swap the live
    // cert/key material baked into the in-process TunnelClient.
    s.cloud_enrollment_changed.notify_one();

    tracing::info!(
        core_id = %persisted.core_id,
        gateway_url = %persisted.gateway_url,
        "admin enrolled this core with the cloud; tunnel supervisor notified, WSS tunnel will activate within seconds",
    );

    Ok(Json(CloudEnrollmentStatus {
        enrolled: true,
        core_id: Some(persisted.core_id),
        gateway_url: Some(persisted.gateway_url),
        enrolled_at: Some(persisted.enrolled_at),
    }))
}

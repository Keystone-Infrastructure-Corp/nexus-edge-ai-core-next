//! M6 Phase 4 Step 4.2 + 4.3 — read API for the audit log.
//!
//! Two endpoints, both gated by `admin_auth_layer` AND the
//! per-handler `AdminContext` extractor (defense in depth: gate
//! authenticates the request, extractor confirms the role):
//!
//! - `GET /api/v1/admin/audit/resource/{kind}/{id}` — last N rows
//!   for a specific resource (camera, rule, sink, user). Powers
//!   the per-resource History panel in each detail view (Step 4.2).
//! - `GET /api/v1/admin/audit` — global filtered feed for the
//!   `/admin/audit` table. Supports actor / action / resource_kind
//!   / outcome / since / until filters + pagination (Step 4.3).
//!
//! Both reuse `Store::list_audit_for_resource` and
//! `Store::list_audit_filtered` which were shipped in
//! [`Step 1` of Phase 1](../../docs/M6_IDENTITY.md#phase-1). The
//! handlers do no further filtering / sorting in-process — the
//! `audit_log` table has covering indexes for every supported
//! query shape and we never want to materialise more than the
//! page-sized result set into memory.

use std::sync::Arc;

use axum::{
    extract::{FromRef, Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::admin_auth::AdminAuthState;
use crate::api::ApiError;
use crate::auth::require_role::AdminContext;
use nexus_store::audit::{AuditEntry, AuditFilter, AuditOutcome};
use nexus_store::Store;

/// Substate for the audit-admin routes. Mirrors the pattern used
/// by `LoginState` / `UsersAdminState` / `OidcLoginState` —
/// extract only the store from `ApiState` so the handler signature
/// stays minimal and the `FromRef` bridge keeps the routing setup
/// uniform.
#[derive(Clone)]
pub struct AuditAdminState {
    pub store: Arc<Store>,
    pub admin_auth: Arc<AdminAuthState>,
}

impl FromRef<crate::api::ApiState> for AuditAdminState {
    fn from_ref(input: &crate::api::ApiState) -> Self {
        Self {
            store: input.store.clone(),
            admin_auth: input.admin_auth.clone(),
        }
    }
}

// Bridge so `AdminContext` can extract from the substate when an
// integration test wires a lean router with `State<AuditAdminState>`
// directly (same pattern as `UsersAdminState`).
impl FromRef<AuditAdminState> for Arc<AdminAuthState> {
    fn from_ref(input: &AuditAdminState) -> Self {
        input.admin_auth.clone()
    }
}

// ---------------------------------------------------------------------------
// Per-resource history (Step 4.2)
// ---------------------------------------------------------------------------

/// `GET /api/v1/admin/audit/resource/{kind}/{id}?limit=N`.
/// Newest-first. `limit` defaults to 50 and is capped at 200 so a
/// pathological client can't pull a million rows in one request.
#[derive(Debug, Deserialize)]
pub struct ResourceQuery {
    pub limit: Option<i64>,
}

/// Wire shape for one audit row. Renames `outcome` to a
/// human-readable lowercase string and stamps `created_at` as
/// RFC 3339 UTC. Otherwise mirrors `AuditEntry` 1:1.
#[derive(Debug, Serialize)]
pub struct AuditRowOut {
    pub id: i64,
    pub actor_kind: &'static str,
    pub actor_id: Option<String>,
    pub actor_label: String,
    pub action: String,
    pub resource_kind: Option<String>,
    pub resource_id: Option<String>,
    pub before_json: Option<String>,
    pub after_json: Option<String>,
    pub outcome: &'static str,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<AuditEntry> for AuditRowOut {
    fn from(e: AuditEntry) -> Self {
        Self {
            id: e.id,
            actor_kind: e.actor_kind.as_str(),
            actor_id: e.actor_id,
            actor_label: e.actor_label,
            action: e.action,
            resource_kind: e.resource_kind,
            resource_id: e.resource_id,
            before_json: e.before_json,
            after_json: e.after_json,
            outcome: e.outcome.as_str(),
            ip: e.ip,
            user_agent: e.user_agent,
            created_at: e.created_at,
        }
    }
}

pub async fn get_resource_audit(
    State(s): State<AuditAdminState>,
    _admin: AdminContext,
    Path((kind, id)): Path<(String, String)>,
    Query(q): Query<ResourceQuery>,
) -> Result<Json<Vec<AuditRowOut>>, ApiError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows = s
        .store
        .list_audit_for_resource(&kind, &id, limit)
        .await
        .map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("audit lookup failed: {e}"),
            )
        })?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

// ---------------------------------------------------------------------------
// Global filtered feed (Step 4.3)
// ---------------------------------------------------------------------------

/// Query string for the global table. Every field is optional;
/// missing fields drop the corresponding `AND col = ?` clause. The
/// `outcome` string is parsed lazily inside the handler so a bad
/// value gives the operator a 400 rather than silently being
/// dropped (which would mask a typo in the URL).
#[derive(Debug, Deserialize)]
pub struct GlobalQuery {
    pub actor_id: Option<String>,
    pub action: Option<String>,
    pub resource_kind: Option<String>,
    pub resource_id: Option<String>,
    pub outcome: Option<String>,
    /// Inclusive RFC 3339 lower bound on `created_at`.
    pub since: Option<DateTime<Utc>>,
    /// Inclusive RFC 3339 upper bound on `created_at`.
    pub until: Option<DateTime<Utc>>,
    /// Page size. Defaults to 50; capped at 500.
    pub limit: Option<i64>,
    /// Row offset. Defaults to 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct GlobalAuditPage {
    pub rows: Vec<AuditRowOut>,
    pub limit: i64,
    pub offset: i64,
}

pub async fn get_global_audit(
    State(s): State<AuditAdminState>,
    _admin: AdminContext,
    Query(q): Query<GlobalQuery>,
) -> Result<Json<GlobalAuditPage>, ApiError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let outcome = match q.outcome.as_deref() {
        Some("success") => Some(AuditOutcome::Success),
        Some("failure") => Some(AuditOutcome::Failure),
        Some("denied") => Some(AuditOutcome::Denied),
        Some(other) => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("unknown outcome '{other}' (expected success|failure|denied)"),
            ));
        }
        None => None,
    };
    let filter = AuditFilter {
        actor_id: q.actor_id.as_deref(),
        action: q.action.as_deref(),
        resource_kind: q.resource_kind.as_deref(),
        resource_id: q.resource_id.as_deref(),
        outcome,
        since: q.since,
        until: q.until,
    };
    let rows = s
        .store
        .list_audit_filtered(&filter, limit, offset)
        .await
        .map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("audit lookup failed: {e}"),
            )
        })?;
    Ok(Json(GlobalAuditPage {
        rows: rows.into_iter().map(Into::into).collect(),
        limit,
        offset,
    }))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::StoreConfig;
    use nexus_store::audit::{AuditActorKind, NewAuditEntry};
    use nexus_store::Store;
    use std::path::PathBuf;
    use tempfile::TempDir;

    async fn open_store() -> (Arc<Store>, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("audit.db");
        let store = Arc::new(
            Store::open(&StoreConfig {
                url: format!("sqlite:{}?mode=rwc", db.display()),
                seed_from_config: false,
                duckdb_attach: false,
                duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
            })
            .await
            .unwrap(),
        );
        (store, dir)
    }

    /// `AuditRowOut::from(AuditEntry)` preserves the `outcome` and
    /// `actor_kind` discriminants exactly. Regression: if we ever
    /// add a new variant to either enum and forget to update
    /// `as_str`, the wire shape would silently mis-label rows.
    #[tokio::test]
    async fn audit_row_out_outcome_string_matches_enum() {
        let (store, _dir) = open_store().await;
        let mut tx = store.pool().begin().await.unwrap();
        for (action, outcome) in [
            ("camera.upsert", nexus_store::audit::AuditOutcome::Success),
            ("camera.delete", nexus_store::audit::AuditOutcome::Failure),
            ("rule.upsert", nexus_store::audit::AuditOutcome::Denied),
        ] {
            store
                .record_audit_event(
                    &mut tx,
                    &NewAuditEntry {
                        actor_kind: Some(AuditActorKind::LocalUser),
                        actor_id: Some("42"),
                        actor_label: "user:42",
                        action,
                        resource_kind: Some("camera"),
                        resource_id: Some("1"),
                        before_json: None,
                        after_json: None,
                        outcome,
                        ip: None,
                        user_agent: None,
                    },
                )
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let rows = store
            .list_audit_for_resource("camera", "1", 10)
            .await
            .unwrap();
        let outs: Vec<AuditRowOut> = rows.into_iter().map(Into::into).collect();
        // Newest first: rule.upsert (denied), camera.delete (failure),
        // camera.upsert (success). But all three have the same
        // `resource_kind=camera` filter so order is by id desc.
        // Three of the inserts above target `camera/1`, the
        // `rule.upsert` row targets `camera/1` too (we wrote
        // resource_kind=camera for it).
        assert_eq!(outs.len(), 3);
        let outcomes: Vec<&str> = outs.iter().map(|r| r.outcome).collect();
        assert!(outcomes.contains(&"success"));
        assert!(outcomes.contains(&"failure"));
        assert!(outcomes.contains(&"denied"));
        // actor_kind is always "local_user" for these rows.
        for r in &outs {
            assert_eq!(r.actor_kind, "local_user");
            assert_eq!(r.actor_label, "user:42");
        }
    }

    /// `list_audit_for_resource` respects the limit AND returns
    /// newest-first. Useful as a regression on the index choice
    /// (`idx_audit_resource ON (resource_kind, resource_id,
    /// created_at DESC, id DESC)`).
    #[tokio::test]
    async fn per_resource_returns_newest_first_within_limit() {
        let (store, _dir) = open_store().await;
        let mut tx = store.pool().begin().await.unwrap();
        for i in 0..5 {
            store
                .record_audit_event(
                    &mut tx,
                    &NewAuditEntry {
                        actor_kind: Some(AuditActorKind::LocalUser),
                        actor_id: Some("1"),
                        actor_label: "user:1",
                        action: "camera.upsert",
                        resource_kind: Some("camera"),
                        resource_id: Some("7"),
                        before_json: None,
                        after_json: Some(&format!("{{\"v\":{i}}}")),
                        outcome: nexus_store::audit::AuditOutcome::Success,
                        ip: None,
                        user_agent: None,
                    },
                )
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let rows = store
            .list_audit_for_resource("camera", "7", 3)
            .await
            .unwrap();
        assert_eq!(rows.len(), 3, "limit honoured");
        // ids are monotonic; newest first means descending ids.
        assert!(rows[0].id > rows[1].id);
        assert!(rows[1].id > rows[2].id);
    }

    /// `list_audit_filtered` with no filters returns everything.
    /// With an outcome filter set, only matching rows return.
    #[tokio::test]
    async fn global_filter_outcome_narrows() {
        let (store, _dir) = open_store().await;
        let mut tx = store.pool().begin().await.unwrap();
        for outcome in [
            nexus_store::audit::AuditOutcome::Success,
            nexus_store::audit::AuditOutcome::Failure,
            nexus_store::audit::AuditOutcome::Success,
        ] {
            store
                .record_audit_event(
                    &mut tx,
                    &NewAuditEntry {
                        actor_kind: Some(AuditActorKind::LocalUser),
                        actor_id: Some("1"),
                        actor_label: "user:1",
                        action: "rule.upsert",
                        resource_kind: Some("rule"),
                        resource_id: Some("r1"),
                        before_json: None,
                        after_json: None,
                        outcome,
                        ip: None,
                        user_agent: None,
                    },
                )
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let all = store
            .list_audit_filtered(&AuditFilter::default(), 100, 0)
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
        let only_failure = store
            .list_audit_filtered(
                &AuditFilter {
                    outcome: Some(nexus_store::audit::AuditOutcome::Failure),
                    ..Default::default()
                },
                100,
                0,
            )
            .await
            .unwrap();
        assert_eq!(only_failure.len(), 1);
        assert!(matches!(
            only_failure[0].outcome,
            nexus_store::audit::AuditOutcome::Failure
        ));
    }
}

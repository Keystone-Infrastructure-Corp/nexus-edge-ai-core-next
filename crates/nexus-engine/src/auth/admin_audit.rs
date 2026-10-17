//! M6 Phase 4 Step 4.1 — audit hooks on every admin mutation.
//!
//! The pre-Phase-4 admin mutations called `Store::write_audit`
//! which hard-coded the actor as `"api"` and only fired on the
//! success path. That left two structural gaps:
//!
//! 1. **No actor attribution.** Every camera / rule / delivery
//!    / storage change in the `audit_log` looks like it was
//!    performed by the system, not by the human (or service
//!    token) that triggered it. Forensics is impossible.
//! 2. **No failure outcomes.** A DELETE that 5xx'd at the DB
//!    layer never showed up in the audit log at all, because
//!    the handler bailed with `?` before reaching the audit
//!    write. Operators can't tell apart "the change was never
//!    attempted" from "the change failed".
//!
//! This module fixes both with a single helper —
//! [`audit_admin_action`] — that:
//!
//! * Takes the [`SessionContext`] the handler's extractor decoded
//!   from the bearer, not the extension `admin_auth_layer`
//!   inserts (nothing reads that one). When present we know the
//!   actor (a legacy bearer's `sub` claim, a session token's user
//!   id, or the no-secret `dev-no-secret` label); when absent we
//!   fall back to `system:unknown` rather than silently dropping
//!   the row.
//! * Records the client IP and User-Agent so cross-tab forensics
//!   can correlate a write to a specific browser session.
//! * Takes an [`AuditOutcome`] so failure-path callers can emit
//!   the same row shape as success paths.
//! * Is a fire-and-forget tracing-only failure mode — if the
//!   audit write itself fails (DB constraint violation, disk
//!   full) we log at `warn!` and swallow the error so the
//!   actual mutation's response code is preserved. This mirrors
//!   the existing `Store::write_audit` behaviour and the
//!   `users_admin::record_user_audit` pattern.
//!
//! ## Why this is NOT `tx_with_audit`
//!
//! The original Phase 4 plan called for a transactional
//! `tx_with_audit(handler, action, kind, id)` helper that runs
//! the domain mutation + audit row in a single SQLite tx so
//! the two roll back together on failure. That requires each
//! `Store::upsert_*` / `Store::delete_*` method to expose a
//! tx-aware sibling, which is a separate API-shape change
//! across `nexus-store`. This module ships the *call-site*
//! refactor (actor attribution + failure-outcome coverage)
//! without touching the store API. The tx-merging variant
//! lands in a follow-up under the same Step 4.1 when the store
//! gains tx-aware mutators. The audit row landing in its own
//! tx today is the same behaviour as `Store::write_audit` and
//! `users_admin::record_user_audit`, both of which are already
//! in production.

use std::net::IpAddr;

use axum::http::HeaderMap;
use nexus_store::audit::{AuditActorKind, AuditOutcome, NewAuditEntry};
use nexus_store::{SqliteTx, Store, StoreError};

use super::require_role::SessionContext;

/// Best-effort `(actor_kind, actor_id, actor_label)` triple for
/// the audit row.
///
/// * When a [`SessionContext`] is present (the post-Phase-4
///   admin path), we use it. Legacy HS256 bearers map to
///   `LocalUser` with `actor_id = "0"` (no user row) and the
///   `jti` field (which the layer populates from the bearer's
///   `sub` claim, or `"loopback"` / `"allow-remote"` for the
///   no-secret paths) as the actor label.
/// * When no context is present, we tag the row as
///   `system:unknown` so the operator can spot the gap. This
///   should never happen on a properly-mounted route — flag
///   it in tracing so we notice if a handler regresses.
fn actor_triple(ctx: Option<&SessionContext>) -> (AuditActorKind, String, String) {
    match ctx {
        Some(sc) if sc.is_legacy_admin => (
            AuditActorKind::LocalUser,
            sc.user_id.to_string(),
            format!("system:legacy-admin:{}", sc.jti),
        ),
        Some(sc) => (
            AuditActorKind::LocalUser,
            sc.user_id.to_string(),
            format!("user:{}", sc.user_id),
        ),
        None => {
            tracing::warn!(
                "admin audit write reached without a SessionContext — handler is not behind admin_auth_layer; falling back to system:unknown"
            );
            (
                AuditActorKind::System,
                "0".to_string(),
                "system:unknown".to_string(),
            )
        }
    }
}

/// Read the `User-Agent` header value (if any) as `&str`,
/// silently skipping non-UTF8.
pub fn user_agent_from(headers: &HeaderMap) -> Option<&str> {
    headers.get("user-agent").and_then(|v| v.to_str().ok())
}

/// Write one `audit_log` row for an admin mutation.
///
/// `actor` is the verified [`SessionContext`] from the request
/// (handlers extract it via the existing `FromRequestParts`
/// impl); `None` is accepted so callers that fire outside the
/// admin gate (e.g. the OAuth callback) can still emit a row
/// tagged `system:unknown`.
///
/// `headers` + `peer_ip` come from the request; both are
/// recorded so cross-tab forensics can correlate the action
/// to a specific browser session. The handler typically pulls
/// them via the `HeaderMap` + `ConnectInfo<SocketAddr>`
/// extractors.
///
/// `before_json` + `after_json` are caller-shaped — typically
/// the pre- and post-mutation snapshots serialised via
/// `serde_json::to_string`. Either can be `None` (e.g.
/// `after` is None for a delete).
///
/// Fire-and-forget — failures are logged at `warn!` and never
/// returned, so the caller can chain `audit_admin_action`
/// before returning the response without affecting the HTTP
/// status code.
#[allow(clippy::too_many_arguments)]
pub async fn audit_admin_action(
    store: &Store,
    actor: Option<&SessionContext>,
    headers: &HeaderMap,
    peer_ip: IpAddr,
    action: &str,
    resource_kind: &str,
    resource_id: Option<&str>,
    outcome: AuditOutcome,
    before_json: Option<&str>,
    after_json: Option<&str>,
) {
    let (actor_kind, actor_id, actor_label) = actor_triple(actor);
    let ua = user_agent_from(headers);
    let ip = peer_ip.to_string();

    let entry = NewAuditEntry {
        actor_kind: Some(actor_kind),
        actor_id: Some(actor_id.as_str()),
        actor_label: actor_label.as_str(),
        action,
        resource_kind: Some(resource_kind),
        resource_id,
        before_json,
        after_json,
        outcome,
        ip: Some(ip.as_str()),
        user_agent: ua,
    };
    if let Err(e) = store.record_audit_event_standalone(&entry).await {
        tracing::warn!(
            error = %e,
            action,
            resource_kind,
            resource_id,
            outcome = ?outcome,
            "admin audit write failed",
        );
    }
}

/// Tx-merged sibling of [`audit_admin_action`] for the
/// success path of an admin mutation.
///
/// Where [`audit_admin_action`] opens its own transaction and
/// swallows write failures (fire-and-forget), this variant
/// writes the row inside the caller's transaction and returns
/// any error so the caller can roll back the matching domain
/// mutation. The intended call shape is:
///
/// ```ignore
/// let mut tx = state.store.begin_tx().await?;
/// state.store.upsert_camera_tx(&mut tx, &cam).await?;
/// audit_admin_action_in_tx(
///     &state.store, &mut tx, session.as_ref(), &headers, peer.ip(),
///     "camera.upsert", "camera", Some(&id_str),
///     before_str.as_deref(), after_str.as_deref(),
/// ).await?;
/// tx.commit().await.map_err(StoreError::from)?;
/// ```
///
/// Outcome is always [`AuditOutcome::Success`] \u2014 a failure
/// outcome belongs on the rolled-back side, where the
/// fire-and-forget [`audit_admin_action`] is the right call.
#[allow(clippy::too_many_arguments)]
pub async fn audit_admin_action_in_tx(
    store: &Store,
    tx: &mut SqliteTx<'_>,
    actor: Option<&SessionContext>,
    headers: &HeaderMap,
    peer_ip: IpAddr,
    action: &str,
    resource_kind: &str,
    resource_id: Option<&str>,
    before_json: Option<&str>,
    after_json: Option<&str>,
) -> Result<(), StoreError> {
    let (actor_kind, actor_id, actor_label) = actor_triple(actor);
    let ua = user_agent_from(headers);
    let ip = peer_ip.to_string();

    let entry = NewAuditEntry {
        actor_kind: Some(actor_kind),
        actor_id: Some(actor_id.as_str()),
        actor_label: actor_label.as_str(),
        action,
        resource_kind: Some(resource_kind),
        resource_id,
        before_json,
        after_json,
        outcome: AuditOutcome::Success,
        ip: Some(ip.as_str()),
        user_agent: ua,
    };
    store.record_audit_event(tx, &entry).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_types::Role;

    #[test]
    fn actor_triple_legacy_admin_uses_system_label_with_jti() {
        let ctx = SessionContext {
            user_id: 0,
            role: Role::Admin,
            jti: "admin-cli".to_string(),
            is_legacy_admin: true,
            chain_id: None,
        };
        let (kind, id, label) = actor_triple(Some(&ctx));
        assert_eq!(kind, AuditActorKind::LocalUser);
        assert_eq!(id, "0");
        assert_eq!(label, "system:legacy-admin:admin-cli");
    }

    #[test]
    fn actor_triple_non_legacy_uses_user_id_label() {
        let ctx = SessionContext {
            user_id: 42,
            role: Role::Admin,
            jti: "abc".to_string(),
            is_legacy_admin: false,
            chain_id: None,
        };
        let (kind, id, label) = actor_triple(Some(&ctx));
        assert_eq!(kind, AuditActorKind::LocalUser);
        assert_eq!(id, "42");
        assert_eq!(label, "user:42");
    }

    #[test]
    fn actor_triple_missing_context_falls_back_to_system_unknown() {
        let (kind, id, label) = actor_triple(None);
        assert_eq!(kind, AuditActorKind::System);
        assert_eq!(id, "0");
        assert_eq!(label, "system:unknown");
    }

    #[test]
    fn actor_triple_loopback_session_distinguishable_via_jti() {
        let ctx = SessionContext {
            user_id: 0,
            role: Role::Admin,
            jti: "loopback".to_string(),
            is_legacy_admin: true,
            chain_id: None,
        };
        let (_, _, label) = actor_triple(Some(&ctx));
        assert_eq!(label, "system:legacy-admin:loopback");
    }

    // ----- bearer → audit row, end to end (edge-core #378) -----------------

    const SECRET: &[u8] = b"test-shared-admin-secret-32-bytes!";

    async fn put_camera_47(base: &str, token: &str) {
        let res = reqwest::Client::new()
            .put(format!("{base}/api/v1/admin/cameras/47"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::NO_CONTENT);
    }

    /// The field report: an operator's HS256 bearer carrying `sub` and no
    /// `jti` was logged by `admin_auth` with its subject, but the audit row
    /// read `system:legacy-admin:legacy`. The row must name the subject.
    #[tokio::test]
    async fn bearer_without_jti_records_its_subject_on_the_audit_row() {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        let (base, store, _dir) = test_support::spawn_audited_admin_api(
            crate::admin_auth::AdminAuthState::from_secret_bytes(Some(SECRET), false),
        )
        .await;
        let now = chrono::Utc::now().timestamp();
        let token = encode(
            &Header::new(Algorithm::HS256),
            &serde_json::json!({
                "sub": "operator:claude-code-session",
                "iat": now,
                "exp": now + 300,
            }),
            &EncodingKey::from_secret(SECRET),
        )
        .unwrap();

        put_camera_47(&base, &token).await;

        let row = test_support::only_camera_row(&store, "47").await;
        assert_eq!(row.action, "camera.upsert");
        assert_eq!(row.actor_kind, AuditActorKind::LocalUser);
        assert_eq!(
            row.actor_label,
            "system:legacy-admin:operator:claude-code-session"
        );
    }

    /// A role-bearing session token is unaffected: the row names the user,
    /// and the request still bumps the session's refresh chain.
    #[tokio::test]
    async fn session_token_audit_row_still_names_the_user() {
        let (tx, mut bumps) = tokio::sync::mpsc::channel(8);
        let (base, store, _dir) = test_support::spawn_audited_admin_api(
            crate::admin_auth::AdminAuthState::from_secret_bytes(Some(SECRET), false)
                .with_idle_bump_tx(tx),
        )
        .await;
        let token = crate::auth::sessions::issue_access_token(
            42,
            Role::Operator,
            SECRET,
            chrono::Utc::now(),
            chrono::Duration::minutes(15),
            Some("chain-1"),
        )
        .unwrap();

        put_camera_47(&base, &token).await;

        let row = test_support::only_camera_row(&store, "47").await;
        assert_eq!(row.actor_kind, AuditActorKind::LocalUser);
        assert_eq!(row.actor_id.as_deref(), Some("42"));
        assert_eq!(row.actor_label, "user:42");
        let (chain, _) = bumps.try_recv().expect("the session's chain is bumped");
        assert_eq!(chain, "chain-1");
    }
}

/// The bearer-authorised admin write path end to end, for tests here and
/// in `engine_rpc`: `admin_auth_layer` in front of a route, the handler's
/// `Option<SessionContext>` extractor, and [`audit_admin_action`] into a
/// real SQLite store, served over TCP with `ConnectInfo` as `main.rs`
/// serves the API. The route is `PUT /api/v1/admin/cameras/{id}` writing
/// `camera.upsert`, the shape `api::upsert_camera` has.
#[cfg(test)]
pub(crate) mod test_support {
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::Arc;

    use axum::extract::{ConnectInfo, FromRef, Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::put;
    use axum::Router;
    use nexus_config::StoreConfig;
    use nexus_store::audit::{AuditEntry, AuditOutcome};
    use nexus_store::Store;

    use crate::admin_auth::{admin_auth_layer, AdminAuthState};
    use crate::auth::require_role::SessionContext;

    #[derive(Clone)]
    struct Fixture {
        auth: Arc<AdminAuthState>,
        store: Arc<Store>,
    }

    impl FromRef<Fixture> for Arc<AdminAuthState> {
        fn from_ref(f: &Fixture) -> Self {
            f.auth.clone()
        }
    }

    async fn upsert_camera(
        State(f): State<Fixture>,
        Path(id): Path<String>,
        headers: HeaderMap,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
        session: Option<SessionContext>,
    ) -> StatusCode {
        super::audit_admin_action(
            &f.store,
            session.as_ref(),
            &headers,
            peer.ip(),
            "camera.upsert",
            "camera",
            Some(&id),
            AuditOutcome::Success,
            None,
            None,
        )
        .await;
        StatusCode::NO_CONTENT
    }

    /// Serve the audited admin write route on an OS-assigned loopback
    /// port. Returns the base URL (`http://127.0.0.1:NNNNN`), the store
    /// the rows land in, and the temp dir that must outlive the test.
    pub(crate) async fn spawn_audited_admin_api(
        auth: AdminAuthState,
    ) -> (String, Arc<Store>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Store::open(&StoreConfig {
                url: format!("sqlite:{}?mode=rwc", dir.path().join("nexus.db").display()),
                seed_from_config: false,
                duckdb_attach: false,
                duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
            })
            .await
            .unwrap(),
        );
        let auth = Arc::new(auth);
        let app = Router::new()
            .route("/api/v1/admin/cameras/{id}", put(upsert_camera))
            .route_layer(axum::middleware::from_fn_with_state(
                auth.clone(),
                admin_auth_layer,
            ))
            .with_state(Fixture {
                auth,
                store: store.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        (format!("http://{addr}"), store, dir)
    }

    /// The single audit row the request wrote for camera `id`.
    pub(crate) async fn only_camera_row(store: &Store, id: &str) -> AuditEntry {
        let mut rows = store
            .list_audit_for_resource("camera", id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "expected one audit row: {rows:?}");
        rows.remove(0)
    }
}

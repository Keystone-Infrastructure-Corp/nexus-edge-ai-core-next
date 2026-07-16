//! Phase 1.7 — `AuditSink` implementation backed by the local SQLite
//! [`Store::record_audit_event_standalone`] API.
//!
//! The engine wires one of these into the [`RpcDispatcher`] at boot so
//! every successfully-verified cloud-initiated `rpc_call` deposits a
//! row in `audit_log` BEFORE the handler runs. The row's shape mirrors
//! every other auth-side audit emitter in the engine
//! (`auth/login.rs`, `auth/admin_audit.rs`, `auth/users_admin.rs`) so a
//! single `/admin/audit` query can surface human-, dev-token-, and
//! cloud-initiated mutations in one feed.
//!
//! ## Actor mapping
//!
//! The `audit_log.actor_kind` column has a 4-value CHECK constraint
//! (`local_user | oidc_user | dev_token | system`). Cloud-issued
//! `actor_token` JWTs carry one of two `sub` shapes:
//!
//! * `system:<svc-name>` — minted by a cloud control-plane service
//!   (entitlement webhook, scheduled scan, etc.). Maps to
//!   [`AuditActorKind::System`].
//! * `<uuid>` — minted on behalf of a human operator who authenticated
//!   to the cloud-console via Entra OIDC. Maps to
//!   [`AuditActorKind::OidcUser`] — the cloud's identity provider IS
//!   OIDC even though the JWT crosses the tunnel as a separate token.
//!
//! ## Action verb
//!
//! Cloud-initiated rows use the prefix `cloud.rpc.` so the
//! `/admin/audit` view can filter them apart from local actions. The
//! suffix is the dispatcher-derived method name (last non-empty path
//! segment); the full HTTP method and path land on `before_json` for
//! diagnosability.
//!
//! ## Failure handling
//!
//! Audit writes MUST NOT block the handler. If the SQLite write fails
//! we `tracing::warn!` and return `()` — the handler still runs. This
//! is the same pattern every other engine audit emitter follows; the
//! retention sweeper guarantees the table stays bounded under failure.

use std::sync::Arc;

use async_trait::async_trait;
use nexus_cloud_client::{AuditSink, EnvelopeContext, VerifiedActor};
use nexus_store::audit::{AuditActorKind, AuditOutcome, NewAuditEntry};
use nexus_store::Store;
use tracing::warn;

/// Local SQLite-backed [`AuditSink`].
///
/// Phase 1.7 lands the type + trait impl; the engine boot path
/// instantiates it once the [`crate::cloud_audit::StoreAuditSink`] gets
/// plumbed into the `TunnelClient` -> `RpcDispatcher` wiring (Phase
/// 1.11 — `cloud-tunnel-wired` row in `docs/cloud-console/PHASES.md`).
/// Until then `cargo build` warns dead-code; the suppression is scoped
/// to this struct so any genuine drift surfaces elsewhere.
#[allow(dead_code)]
pub struct StoreAuditSink {
    store: Arc<Store>,
}

#[allow(dead_code)]
impl StoreAuditSink {
    /// Build a sink that writes to the local engine [`Store`].
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl AuditSink for StoreAuditSink {
    async fn record(&self, method: &str, envelope: EnvelopeContext<'_>, actor: &VerifiedActor) {
        let actor_kind = if actor.sub.starts_with("system:") {
            AuditActorKind::System
        } else {
            AuditActorKind::OidcUser
        };
        let action = format!("cloud.rpc.{method}");
        let before = build_before_json(method, envelope, actor);
        let entry = NewAuditEntry {
            actor_kind: Some(actor_kind),
            actor_id: Some(&actor.sub),
            actor_label: &actor.role,
            action: &action,
            resource_kind: Some("cloud_rpc"),
            resource_id: Some(&actor.jti),
            before_json: Some(&before),
            after_json: None,
            outcome: AuditOutcome::Success,
            ip: None,
            user_agent: None,
        };
        if let Err(e) = self.store.record_audit_event_standalone(&entry).await {
            warn!(
                error = %e,
                method = %method,
                jti = %actor.jti,
                sub = %actor.sub,
                "cloud audit write failed; handler still proceeds",
            );
        }
    }
}

/// Serialise the envelope context + actor's `org_id` into a compact
/// JSON blob suitable for `audit_log.before_json`. The blob is keyed
/// by short field names because every byte ends up in SQLite.
// Dead until `StoreAuditSink` is wired into boot (Phase 1.11); the sink's
// struct + inherent impl carry the same suppression.
#[allow(dead_code)]
fn build_before_json(method: &str, envelope: EnvelopeContext<'_>, actor: &VerifiedActor) -> String {
    // Hand-rolled to avoid a serde_json::to_string round-trip for a
    // shape with zero possibility of nested escaping (every value is
    // a constrained ASCII identifier).
    format!(
        r#"{{"method":"{}","http_method":"{}","path":"{}","org_id":"{}"}}"#,
        json_escape(method),
        json_escape(envelope.method),
        json_escape(envelope.path),
        json_escape(&actor.org_id),
    )
}

/// Minimal JSON-string escaper for the subset of characters that can
/// appear in a method name, HTTP method, path, or UUID.
// Dead in non-test builds until `StoreAuditSink` is wired (Phase 1.11); the
// unit tests below exercise it directly.
#[allow(dead_code)]
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use nexus_cloud_client::actor_token::EnvelopeContext;
    use nexus_cloud_client::AuditSink;
    use nexus_config::StoreConfig;
    use nexus_store::audit::AuditFilter;
    use nexus_store::Store;
    use tempfile::TempDir;

    async fn fresh_store() -> (Arc<Store>, TempDir) {
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
            .expect("open"),
        );
        (store, dir)
    }

    fn human_actor() -> VerifiedActor {
        VerifiedActor {
            sub: "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c100".into(),
            role: "operator".into(),
            jti: "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c101".into(),
            org_id: "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c102".into(),
        }
    }

    fn system_actor() -> VerifiedActor {
        VerifiedActor {
            sub: "system:entitlement-svc".into(),
            role: "system".into(),
            jti: "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c200".into(),
            org_id: "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c201".into(),
        }
    }

    #[tokio::test]
    async fn human_actor_writes_oidc_user_audit_row() {
        let (store, _tmp) = fresh_store().await;
        let sink = StoreAuditSink::new(Arc::clone(&store));
        let env = EnvelopeContext {
            method: "POST",
            path: "/admin/v1/cameras",
        };
        let actor = human_actor();
        sink.record("cameras", env, &actor).await;

        let rows = store
            .list_audit_filtered(&AuditFilter::default(), 100, 0)
            .await
            .expect("list");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.actor_kind, AuditActorKind::OidcUser);
        assert_eq!(row.actor_id.as_deref(), Some(actor.sub.as_str()));
        assert_eq!(row.actor_label, "operator");
        assert_eq!(row.action, "cloud.rpc.cameras");
        assert_eq!(row.resource_kind.as_deref(), Some("cloud_rpc"));
        assert_eq!(row.resource_id.as_deref(), Some(actor.jti.as_str()));
        assert_eq!(row.outcome, AuditOutcome::Success);
        let before = row.before_json.as_deref().expect("before_json populated");
        assert!(before.contains(r#""method":"cameras""#));
        assert!(before.contains(r#""http_method":"POST""#));
        assert!(before.contains(r#""path":"/admin/v1/cameras""#));
        assert!(before.contains(&actor.org_id));
    }

    #[tokio::test]
    async fn system_sub_writes_system_audit_row() {
        let (store, _tmp) = fresh_store().await;
        let sink = StoreAuditSink::new(Arc::clone(&store));
        let env = EnvelopeContext {
            method: "POST",
            path: "/internal/entitlement-update",
        };
        let actor = system_actor();
        sink.record("entitlement_update", env, &actor).await;

        let rows = store
            .list_audit_filtered(&AuditFilter::default(), 100, 0)
            .await
            .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].actor_kind, AuditActorKind::System);
        assert_eq!(rows[0].action, "cloud.rpc.entitlement_update");
    }

    #[test]
    fn json_escape_handles_quotes_and_backslash() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        // Newlines and control chars.
        assert_eq!(json_escape("x\ny"), "x\\ny");
        assert_eq!(json_escape("x\x07y"), "x\\u0007y");
    }
}

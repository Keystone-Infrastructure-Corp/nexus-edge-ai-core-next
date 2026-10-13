//! M6 Phase 1 Step 1.2 — schema + CRUD coverage for the
//! `audit_log` table (migration `0009_audit_log`) and the
//! `Store::record_audit_event*` API.
//!
//! Exercises:
//!
//! * Migration 0009 applies cleanly on a fresh DB and registers
//!   itself in `schema_migrations`.
//! * `record_audit_event_standalone` round-trips a row and read
//!   path decodes every column.
//! * `record_audit_event` honours caller-controlled transaction
//!   rollback — a failed handler does not leave half-audit
//!   shrapnel behind.
//! * `list_audit_for_resource` returns rows in descending
//!   `created_at` order and respects `limit`.
//! * `list_audit_filtered` filters by actor_id, action,
//!   resource_kind, outcome, and time window — and combines them.
//! * CHECK constraints reject bad `actor_kind`, bad `outcome`,
//!   and partial `resource_kind`/`resource_id` pairs.
//! * `delete_audit_older_than` deletes everything strictly older
//!   than `cutoff` and reports the count.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use chrono::{Duration, Utc};
use nexus_config::StoreConfig;
use nexus_store::{AuditActorKind, AuditFilter, AuditOutcome, NewAuditEntry, Store};
use tempfile::TempDir;

async fn fresh_store() -> (Store, TempDir) {
    let dir = tempfile::tempdir().expect("tmpdir");
    let db_path = dir.path().join("nexus.db");
    let cfg = StoreConfig {
        url: format!("sqlite:{}?mode=rwc", db_path.display()),
        seed_from_config: false,
        duckdb_attach: false,
        duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
    };
    let store = Store::open(&cfg).await.expect("Store::open");
    (store, dir)
}

#[tokio::test]
async fn audit_migration_registers() {
    let (store, _tmp) = fresh_store().await;
    let row: (String,) =
        sqlx::query_as("SELECT id FROM schema_migrations WHERE id = '0009_audit_log'")
            .fetch_one(store.pool())
            .await
            .expect("migration registered");
    assert_eq!(row.0, "0009_audit_log");
}

#[tokio::test]
async fn standalone_insert_round_trips_all_columns() {
    let (store, _tmp) = fresh_store().await;

    let id = store
        .record_audit_event_standalone(&NewAuditEntry {
            actor_kind: Some(AuditActorKind::LocalUser),
            actor_id: Some("42"),
            actor_label: "alice",
            action: "camera.update",
            resource_kind: Some("camera"),
            resource_id: Some("7"),
            before_json: Some(r#"{"name":"old"}"#),
            after_json: Some(r#"{"name":"new"}"#),
            outcome: AuditOutcome::Success,
            ip: Some("10.0.0.4"),
            user_agent: Some("nexus-ui/test"),
        })
        .await
        .expect("standalone insert");
    assert!(id > 0);

    let rows = store
        .list_audit_for_resource("camera", "7", 50)
        .await
        .expect("list per resource");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.id, id);
    assert_eq!(row.actor_kind, AuditActorKind::LocalUser);
    assert_eq!(row.actor_id.as_deref(), Some("42"));
    assert_eq!(row.actor_label, "alice");
    assert_eq!(row.action, "camera.update");
    assert_eq!(row.resource_kind.as_deref(), Some("camera"));
    assert_eq!(row.resource_id.as_deref(), Some("7"));
    assert_eq!(row.before_json.as_deref(), Some(r#"{"name":"old"}"#));
    assert_eq!(row.after_json.as_deref(), Some(r#"{"name":"new"}"#));
    assert_eq!(row.outcome, AuditOutcome::Success);
    assert_eq!(row.ip.as_deref(), Some("10.0.0.4"));
    assert_eq!(row.user_agent.as_deref(), Some("nexus-ui/test"));
    // SQLite CURRENT_TIMESTAMP default fired.
    let drift = (Utc::now() - row.created_at).num_seconds();
    assert!(
        drift.abs() < 60,
        "created_at within 60s of now (drift={drift}s)"
    );
}

#[tokio::test]
async fn record_audit_event_honours_caller_rollback() {
    // The whole point of taking a caller-supplied transaction is
    // so a failed domain mutation rolls back the audit row too.
    // Drop the tx without committing; the row must NOT survive.
    let (store, _tmp) = fresh_store().await;

    {
        let mut tx = store.pool().begin().await.unwrap();
        store
            .record_audit_event(
                &mut tx,
                &NewAuditEntry {
                    actor_kind: Some(AuditActorKind::System),
                    actor_label: "system",
                    action: "test.rollback",
                    outcome: AuditOutcome::Success,
                    ..Default::default()
                },
            )
            .await
            .expect("insert succeeds inside tx");
        // explicit rollback to be doubly sure
        tx.rollback().await.unwrap();
    }

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count.0, 0, "rollback must leave audit_log empty");
}

#[tokio::test]
async fn per_resource_listing_is_newest_first_and_respects_limit() {
    let (store, _tmp) = fresh_store().await;

    // Insert 5 rows for the same resource. Each row's created_at
    // is server-default CURRENT_TIMESTAMP at second resolution —
    // ties are broken by `id DESC` in the listing query, which
    // matches "newest insert first" since AUTOINCREMENT is
    // monotonic.
    for n in 0..5 {
        store
            .record_audit_event_standalone(&NewAuditEntry {
                actor_kind: Some(AuditActorKind::LocalUser),
                actor_label: "alice",
                action: "camera.update",
                resource_kind: Some("camera"),
                resource_id: Some("11"),
                after_json: Some(&format!(r#"{{"v":{n}}}"#)),
                outcome: AuditOutcome::Success,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    // And one for a *different* resource — must not leak in.
    store
        .record_audit_event_standalone(&NewAuditEntry {
            actor_kind: Some(AuditActorKind::LocalUser),
            actor_label: "alice",
            action: "camera.update",
            resource_kind: Some("camera"),
            resource_id: Some("99"),
            outcome: AuditOutcome::Success,
            ..Default::default()
        })
        .await
        .unwrap();

    let rows = store
        .list_audit_for_resource("camera", "11", 3)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3, "limit honoured");
    // Latest-inserted row is the v=4 row (last loop iteration).
    assert_eq!(rows[0].after_json.as_deref(), Some(r#"{"v":4}"#));
    assert_eq!(rows[1].after_json.as_deref(), Some(r#"{"v":3}"#));
    assert_eq!(rows[2].after_json.as_deref(), Some(r#"{"v":2}"#));
    // The unrelated resource row didn't leak in.
    for r in &rows {
        assert_eq!(r.resource_id.as_deref(), Some("11"));
    }
}

#[tokio::test]
async fn list_audit_filtered_combines_actor_action_resource_outcome() {
    let (store, _tmp) = fresh_store().await;
    // Build a small population that we'll slice by every filter.
    let cases = [
        (
            "alice",
            "camera.update",
            "camera",
            "1",
            AuditOutcome::Success,
        ),
        (
            "alice",
            "camera.delete",
            "camera",
            "1",
            AuditOutcome::Failure,
        ),
        (
            "alice",
            "camera.update",
            "camera",
            "2",
            AuditOutcome::Success,
        ),
        ("bob", "camera.update", "camera", "1", AuditOutcome::Denied),
        (
            "bob",
            "login.success",
            "session",
            "x",
            AuditOutcome::Success,
        ),
        (
            "bob",
            "login.failure",
            "session",
            "x",
            AuditOutcome::Failure,
        ),
    ];
    for (actor, action, rk, rid, outcome) in cases {
        store
            .record_audit_event_standalone(&NewAuditEntry {
                actor_kind: Some(AuditActorKind::LocalUser),
                actor_id: Some(actor),
                actor_label: actor,
                action,
                resource_kind: Some(rk),
                resource_id: Some(rid),
                outcome,
                ..Default::default()
            })
            .await
            .unwrap();
    }

    // actor_id alone
    let alice = store
        .list_audit_filtered(
            &AuditFilter {
                actor_id: Some("alice"),
                ..Default::default()
            },
            100,
            0,
        )
        .await
        .unwrap();
    assert_eq!(alice.len(), 3);
    assert!(alice.iter().all(|r| r.actor_label == "alice"));

    // action alone
    let updates = store
        .list_audit_filtered(
            &AuditFilter {
                action: Some("camera.update"),
                ..Default::default()
            },
            100,
            0,
        )
        .await
        .unwrap();
    assert_eq!(updates.len(), 3);

    // outcome alone
    let failures = store
        .list_audit_filtered(
            &AuditFilter {
                outcome: Some(AuditOutcome::Failure),
                ..Default::default()
            },
            100,
            0,
        )
        .await
        .unwrap();
    assert_eq!(failures.len(), 2);

    // combined: alice + camera.update
    let alice_updates = store
        .list_audit_filtered(
            &AuditFilter {
                actor_id: Some("alice"),
                action: Some("camera.update"),
                ..Default::default()
            },
            100,
            0,
        )
        .await
        .unwrap();
    assert_eq!(alice_updates.len(), 2);

    // combined: bob + denied + camera + 1
    let bob_denied = store
        .list_audit_filtered(
            &AuditFilter {
                actor_id: Some("bob"),
                outcome: Some(AuditOutcome::Denied),
                resource_kind: Some("camera"),
                resource_id: Some("1"),
                ..Default::default()
            },
            100,
            0,
        )
        .await
        .unwrap();
    assert_eq!(bob_denied.len(), 1);

    // limit + offset paginate the same ORDER BY
    let page1 = store
        .list_audit_filtered(&AuditFilter::default(), 3, 0)
        .await
        .unwrap();
    let page2 = store
        .list_audit_filtered(&AuditFilter::default(), 3, 3)
        .await
        .unwrap();
    assert_eq!(page1.len(), 3);
    assert_eq!(page2.len(), 3);
    let page1_ids: Vec<_> = page1.iter().map(|r| r.id).collect();
    let page2_ids: Vec<_> = page2.iter().map(|r| r.id).collect();
    for id in &page2_ids {
        assert!(!page1_ids.contains(id), "page2 must not duplicate page1");
    }
}

#[tokio::test]
async fn list_audit_filtered_respects_time_window() {
    let (store, _tmp) = fresh_store().await;

    // We can't backdate the CURRENT_TIMESTAMP default cheaply, so
    // insert one row at now-ish, then use a `since` cutoff in the
    // far future to filter it out, and `until` in the far past
    // likewise. The opposite cases keep it.
    store
        .record_audit_event_standalone(&NewAuditEntry {
            actor_kind: Some(AuditActorKind::System),
            actor_label: "system",
            action: "test.window",
            outcome: AuditOutcome::Success,
            ..Default::default()
        })
        .await
        .unwrap();

    let kept = store
        .list_audit_filtered(
            &AuditFilter {
                since: Some(Utc::now() - Duration::hours(1)),
                until: Some(Utc::now() + Duration::hours(1)),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(kept.len(), 1);

    let filtered_out_since = store
        .list_audit_filtered(
            &AuditFilter {
                since: Some(Utc::now() + Duration::hours(1)),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert!(filtered_out_since.is_empty());

    let filtered_out_until = store
        .list_audit_filtered(
            &AuditFilter {
                until: Some(Utc::now() - Duration::hours(1)),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert!(filtered_out_until.is_empty());
}

#[tokio::test]
async fn check_constraint_rejects_bad_actor_kind() {
    let (store, _tmp) = fresh_store().await;
    let err = sqlx::query(
        "INSERT INTO audit_log (actor_kind, actor_label, action, outcome)
         VALUES ('intruder', 'eve', 'camera.delete', 'success')",
    )
    .execute(store.pool())
    .await
    .expect_err("bad actor_kind must be rejected");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("check"),
        "expected CHECK violation, got: {msg}"
    );
}

#[tokio::test]
async fn check_constraint_rejects_bad_outcome() {
    let (store, _tmp) = fresh_store().await;
    let err = sqlx::query(
        "INSERT INTO audit_log (actor_kind, actor_label, action, outcome)
         VALUES ('system', 'system', 'test', 'partial')",
    )
    .execute(store.pool())
    .await
    .expect_err("bad outcome must be rejected");
    assert!(err.to_string().to_lowercase().contains("check"));
}

#[tokio::test]
async fn check_constraint_rejects_partial_resource_pair() {
    let (store, _tmp) = fresh_store().await;
    let err = sqlx::query(
        "INSERT INTO audit_log (actor_kind, actor_label, action, outcome, resource_kind)
         VALUES ('system', 'system', 'test', 'success', 'camera')",
    )
    .execute(store.pool())
    .await
    .expect_err("kind without id must be rejected");
    assert!(err.to_string().to_lowercase().contains("check"));

    let err = sqlx::query(
        "INSERT INTO audit_log (actor_kind, actor_label, action, outcome, resource_id)
         VALUES ('system', 'system', 'test', 'success', '42')",
    )
    .execute(store.pool())
    .await
    .expect_err("id without kind must be rejected");
    assert!(err.to_string().to_lowercase().contains("check"));
}

#[tokio::test]
async fn delete_audit_older_than_drops_strictly_older_rows() {
    let (store, _tmp) = fresh_store().await;

    // Insert one row at "now" (CURRENT_TIMESTAMP default), then
    // immediately delete with a far-future cutoff — everything goes.
    store
        .record_audit_event_standalone(&NewAuditEntry {
            actor_kind: Some(AuditActorKind::System),
            actor_label: "system",
            action: "test.retention",
            outcome: AuditOutcome::Success,
            ..Default::default()
        })
        .await
        .unwrap();
    let count_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count_before.0, 1);

    // Cutoff in the past: nothing strictly older than that → 0 deleted.
    let removed = store
        .delete_audit_older_than(Utc::now() - Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(removed, 0);

    // Cutoff in the future: every existing row is strictly older.
    let removed = store
        .delete_audit_older_than(Utc::now() + Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(removed, 1);

    let count_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count_after.0, 0);
}

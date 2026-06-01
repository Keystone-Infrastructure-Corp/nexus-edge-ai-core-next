//! M7 Phase 1 Step 2 — schema + CRUD coverage for the `alert_sink_outbox`
//! table and the `Store::record_event_and_enqueue` transactional API.
//!
//! Exercises:
//!
//! * Migration 0006 applies cleanly on a fresh DB and registers itself
//!   in `schema_migrations`.
//! * `record_event_and_enqueue` is atomic — when one of the outbox
//!   inserts fails (UNIQUE violation), neither the event row NOR
//!   any outbox row survives.
//! * Empty sink list still records the event (no outbox rows).
//! * `outbox_pending` honours `next_attempt_at <= now` ordering.
//! * State transitions: `mark_sent`, `mark_failed`, `mark_dead`,
//!   `mark_suppressed` land the right columns + CHECK constraints
//!   hold.
//! * `events.event_id` ON DELETE CASCADE propagates to
//!   `alert_sink_outbox` (M2.1 clip eviction invariant).

use std::path::PathBuf;

use chrono::{Duration, Utc};
use nexus_config::{CameraConfig, StoreConfig};
use nexus_store::{OutboxStatus, Store, SuppressionReason};
use nexus_types::{AlertEvent, Artifacts, Severity};
use tempfile::TempDir;
use url::Url;
use uuid::Uuid;

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

fn sample_camera(id: i64, name: &str) -> CameraConfig {
    CameraConfig {
        id,
        name: name.into(),
        ingest: nexus_config::CameraIngest {
            url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
            enabled: true,
            max_fps: 0,
            codec: None,
        },
        detector: nexus_config::CameraDetector {
            prompts: vec![],
            visual_prompts: vec![],
            model_override: None,
        },
        behavior: nexus_config::CameraBehavior {
            parking_lot_mode: false,
            anchor_ttl_secs: None,
            ..Default::default()
        },
        zones: vec![],
    }
}

fn sample_alert(camera_id: i64, rule: &str) -> AlertEvent {
    AlertEvent {
        event_id: Uuid::now_v7(),
        camera_id,
        rule_id: rule.into(),
        track_id: Some(7),
        label: "person".into(),
        severity: Severity::High,
        bbox: None,
        frame_id: 1,
        captured_at: Utc::now(),
        trace_id: "trace-out".into(),
        artifacts: Artifacts::default(),
        context: serde_json::Map::new(),
    }
}

#[tokio::test]
async fn outbox_migration_registers() {
    let (store, _tmp) = fresh_store().await;
    let row: (String,) =
        sqlx::query_as("SELECT id FROM schema_migrations WHERE id = '0006_alert_sink_outbox'")
            .fetch_one(store.pool())
            .await
            .expect("migration registered");
    assert_eq!(row.0, "0006_alert_sink_outbox");
}

#[tokio::test]
async fn record_event_and_enqueue_inserts_event_and_outbox_rows() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let alert = sample_alert(1, "rule.intrusion");
    store
        .record_event_and_enqueue(&alert, &["webhook:slack", "sureview:control"])
        .await
        .unwrap();

    let n_events: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(n_events.0, 1);

    let rows = store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let sink_ids: Vec<_> = rows.iter().map(|r| r.sink_id.as_str()).collect();
    assert!(sink_ids.contains(&"webhook:slack"));
    assert!(sink_ids.contains(&"sureview:control"));
    assert!(rows.iter().all(|r| r.status == OutboxStatus::Pending));
    assert!(rows.iter().all(|r| r.attempts == 0));
    assert!(rows.iter().all(|r| r.next_attempt_at.is_none()));
    assert!(rows.iter().all(|r| r.delivered_at.is_none()));
    assert!(rows.iter().all(|r| r.last_error.is_none()));
    assert!(rows.iter().all(|r| r.suppression_reason.is_none()));
}

#[tokio::test]
async fn record_event_and_enqueue_with_no_sinks_records_event_only() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let alert = sample_alert(1, "rule.test");
    store.record_event_and_enqueue(&alert, &[]).await.unwrap();

    let n_events: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(n_events.0, 1);

    let outbox_rows = store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .unwrap();
    assert!(outbox_rows.is_empty());
}

#[tokio::test]
async fn record_event_and_enqueue_rolls_back_on_duplicate_sink() {
    // Caller mistake: passes the same sink_id twice. The second
    // INSERT trips the UNIQUE (event_id, sink_id) constraint; the
    // whole tx must roll back including the events row.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let alert = sample_alert(1, "rule.dup");
    let err = store
        .record_event_and_enqueue(&alert, &["webhook:slack", "webhook:slack"])
        .await
        .expect_err("duplicate sink should fail");

    // Rolled back: no event row, no outbox rows.
    let n_events: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(n_events.0, 0);
    let n_outbox: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alert_sink_outbox")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(n_outbox.0, 0);

    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("unique") || msg.to_lowercase().contains("constraint"),
        "expected UNIQUE/constraint error, got: {msg}"
    );
}

#[tokio::test]
async fn outbox_pending_honours_next_attempt_at_filter() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let a = sample_alert(1, "rule.a");
    let b = sample_alert(1, "rule.b");
    store
        .record_event_and_enqueue(&a, &["webhook:s1"])
        .await
        .unwrap();
    store
        .record_event_and_enqueue(&b, &["webhook:s2"])
        .await
        .unwrap();

    // Mark b's row as failed with next_attempt_at = future.
    let b_rows = store
        .outbox_for_event(&b.event_id.to_string())
        .await
        .unwrap();
    let b_id = b_rows[0].id;
    store
        .outbox_mark_failed(b_id, "boom", Utc::now() + Duration::seconds(60))
        .await
        .unwrap();

    let pending = store.outbox_pending(100).await.unwrap();
    assert_eq!(pending.len(), 1, "only a's row should be due now");
    assert_eq!(pending[0].event_id, a.event_id.to_string());

    // Now backdate b's next_attempt_at to past — should surface.
    sqlx::query("UPDATE alert_sink_outbox SET next_attempt_at = ? WHERE id = ?")
        .bind((Utc::now() - Duration::seconds(60)).to_rfc3339())
        .bind(b_id)
        .execute(store.pool())
        .await
        .unwrap();

    let pending = store.outbox_pending(100).await.unwrap();
    assert_eq!(pending.len(), 2);
}

#[tokio::test]
async fn outbox_state_transitions() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let alert = sample_alert(1, "rule.tx");
    store
        .record_event_and_enqueue(
            &alert,
            &[
                "webhook:sent",
                "webhook:failed",
                "webhook:dead",
                "webhook:supp",
            ],
        )
        .await
        .unwrap();

    let rows = store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .unwrap();
    let by_sink = |s: &str| -> i64 {
        rows.iter()
            .find(|r| r.sink_id == s)
            .map(|r| r.id)
            .unwrap_or_else(|| panic!("no row for {s}"))
    };

    store
        .outbox_mark_sent(by_sink("webhook:sent"))
        .await
        .unwrap();
    store
        .outbox_mark_failed(
            by_sink("webhook:failed"),
            "transient: 503",
            Utc::now() + Duration::seconds(30),
        )
        .await
        .unwrap();
    store
        .outbox_mark_dead(by_sink("webhook:dead"), "permanent: 400 bad payload")
        .await
        .unwrap();
    store
        .outbox_mark_suppressed(by_sink("webhook:supp"), SuppressionReason::RuleDisabled)
        .await
        .unwrap();

    let rows = store
        .outbox_for_event(&alert.event_id.to_string())
        .await
        .unwrap();

    let sent = rows.iter().find(|r| r.sink_id == "webhook:sent").unwrap();
    assert_eq!(sent.status, OutboxStatus::Sent);
    assert_eq!(sent.attempts, 1);
    assert!(sent.delivered_at.is_some());
    assert!(sent.last_error.is_none());

    let failed = rows.iter().find(|r| r.sink_id == "webhook:failed").unwrap();
    // mark_failed bounces back to 'pending' for the retry loop.
    assert_eq!(failed.status, OutboxStatus::Pending);
    assert_eq!(failed.attempts, 1);
    assert_eq!(failed.last_error.as_deref(), Some("transient: 503"));
    assert!(failed.next_attempt_at.is_some());

    let dead = rows.iter().find(|r| r.sink_id == "webhook:dead").unwrap();
    assert_eq!(dead.status, OutboxStatus::Dead);
    assert_eq!(dead.attempts, 1);
    assert_eq!(
        dead.last_error.as_deref(),
        Some("permanent: 400 bad payload")
    );
    assert!(dead.next_attempt_at.is_none());

    let supp = rows.iter().find(|r| r.sink_id == "webhook:supp").unwrap();
    assert_eq!(supp.status, OutboxStatus::Suppressed);
    assert_eq!(
        supp.suppression_reason,
        Some(SuppressionReason::RuleDisabled)
    );
    assert_eq!(supp.attempts, 0);
    assert!(supp.next_attempt_at.is_none());
}

#[tokio::test]
async fn outbox_unique_event_sink_pair_enforced() {
    // Two distinct calls inserting the same (event_id, sink_id) —
    // the second tx must roll back cleanly. (Real-world surface:
    // an at-least-once enqueue retry loop after a crash-replay
    // sweep, or a buggy supervisor.)
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let alert = sample_alert(1, "rule.u");
    store
        .record_event_and_enqueue(&alert, &["webhook:once"])
        .await
        .unwrap();

    // Second call uses the SAME event — duplicate event_id PK trip
    // first (events.event_id PRIMARY KEY), but we'd see the
    // UNIQUE(event_id, sink_id) trip if the events PK weren't
    // there. Either way, the tx rolls back and the row count stays
    // at 1.
    let err = store
        .record_event_and_enqueue(&alert, &["webhook:once"])
        .await
        .expect_err("duplicate event_id must fail");
    assert!(
        err.to_string().to_lowercase().contains("unique")
            || err.to_string().to_lowercase().contains("constraint")
    );

    let n_outbox: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alert_sink_outbox")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(n_outbox.0, 1);
}

#[tokio::test]
async fn outbox_cascades_when_event_is_deleted() {
    // Surface case: M2.1 clip-eviction cascades through events
    // (clip_id ON DELETE CASCADE from migration 0003), and from
    // events through alert_sink_outbox (event_id ON DELETE CASCADE
    // from this migration). Result: clip drop sweeps the whole
    // delivery state for every alert that referenced the clip.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let alert = sample_alert(1, "rule.cascade");
    store
        .record_event_and_enqueue(&alert, &["webhook:a", "webhook:b"])
        .await
        .unwrap();

    // Direct event delete (simulates what the cascade-from-clip
    // would do). Pre-delete: 2 outbox rows.
    let pre_n: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alert_sink_outbox WHERE event_id = ?")
        .bind(alert.event_id.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(pre_n.0, 2);

    sqlx::query("DELETE FROM events WHERE event_id = ?")
        .bind(alert.event_id.to_string())
        .execute(store.pool())
        .await
        .unwrap();

    let post_n: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM alert_sink_outbox WHERE event_id = ?")
            .bind(alert.event_id.to_string())
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(post_n.0, 0);
}

#[tokio::test]
async fn outbox_pending_limit_and_ordering() {
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    for _ in 0..5 {
        let a = sample_alert(1, "rule.x");
        store
            .record_event_and_enqueue(&a, &["webhook:s"])
            .await
            .unwrap();
    }

    let pending = store.outbox_pending(3).await.unwrap();
    assert_eq!(pending.len(), 3);
    // Ascending by id — i.e. oldest enqueued first.
    let ids: Vec<i64> = pending.iter().map(|r| r.id).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);

    let pending_all = store.outbox_pending(100).await.unwrap();
    assert_eq!(pending_all.len(), 5);

    // Verify the partial index path: a 'sent' row should NOT
    // appear in `outbox_pending`.
    let id = pending_all[0].id;
    store.outbox_mark_sent(id).await.unwrap();
    let pending_after = store.outbox_pending(100).await.unwrap();
    assert_eq!(pending_after.len(), 4);
}

#[tokio::test]
async fn outbox_suppressed_row_check_constraint() {
    // The migration's paired CHECK guarantees suppression_reason
    // NULL ⇔ status NOT 'suppressed'. Verify both halves bite.
    let (store, _tmp) = fresh_store().await;
    store
        .upsert_camera(&sample_camera(1, "front"))
        .await
        .unwrap();

    let a = sample_alert(1, "rule.check");
    store
        .record_event_and_enqueue(&a, &["webhook:c"])
        .await
        .unwrap();
    let rows = store
        .outbox_for_event(&a.event_id.to_string())
        .await
        .unwrap();
    let id = rows[0].id;

    // (a) try to set suppression_reason while status='pending'.
    let err = sqlx::query(
        "UPDATE alert_sink_outbox SET suppression_reason = 'rule_disabled' WHERE id = ?",
    )
    .bind(id)
    .execute(store.pool())
    .await
    .expect_err("must violate CHECK");
    assert!(err.to_string().to_lowercase().contains("check"));

    // (b) try to set status='suppressed' without a reason.
    let err = sqlx::query("UPDATE alert_sink_outbox SET status = 'suppressed' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .expect_err("must violate CHECK");
    assert!(err.to_string().to_lowercase().contains("check"));
}

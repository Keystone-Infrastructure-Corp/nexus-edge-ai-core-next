//! M7 Phase 1 Step 5 — existing `events` rows survive the 0007–0009
//! delivery migrations cleanly.
//!
//! Regression guard for the upgrade path: an engine that has been
//! recording alerts since before M7 already has `events` (and, post
//! 0006, `alert_sink_outbox`) rows on disk. Applying the M7 delivery
//! migrations — 0007 `delivery_settings`, 0008 `rules.delivery_policy_json`,
//! 0009 `audit_log` — MUST NOT drop, rewrite, or cascade-delete those
//! rows. A botched parent-table rebuild (the exact failure mode the
//! `-- nexus:no-transaction` recipe in 0004 guards against) would
//! silently nuke history.
//!
//! Strategy: bootstrap a database at the pre-0007 baseline (apply
//! 0001–0006 and stamp them into `schema_migrations`), seed a camera,
//! a rule, an event, and a pending outbox row, then hand the file to
//! the REAL [`Store::open`] migration runner. `apply_schema` sees
//! 0001–0006 already applied and runs 0007–0021 through the same code
//! path production uses. We then assert the pre-existing rows are
//! intact and the new 0007–0009 schema is present.

use std::path::PathBuf;

use nexus_config::StoreConfig;
use nexus_store::Store;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

/// Migration ids that predate M7 delivery — must match the leading
/// entries of `nexus_store`'s private `MIGRATIONS` array exactly so
/// the real runner skips them instead of re-applying (0002's
/// `ALTER TABLE events ADD COLUMN clip_id` is not idempotent).
const PRE_M7_MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_initial",
        include_str!("../migrations/0001_initial.sql"),
    ),
    (
        "0002_motion_clips",
        include_str!("../migrations/0002_motion_clips.sql"),
    ),
    (
        "0003_events_clip_cascade",
        include_str!("../migrations/0003_events_clip_cascade.sql"),
    ),
    (
        "0004_storage_backends",
        include_str!("../migrations/0004_storage_backends.sql"),
    ),
    (
        "0005_runtime_settings",
        include_str!("../migrations/0005_runtime_settings.sql"),
    ),
    (
        "0006_alert_sink_outbox",
        include_str!("../migrations/0006_alert_sink_outbox.sql"),
    ),
];

#[tokio::test]
async fn events_survive_m7_delivery_migrations() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let db_path = dir.path().join("nexus.db");

    // --- Phase 1: bootstrap the pre-0007 baseline --------------------
    {
        let opts = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);
        // Single connection so each migration's PRAGMA state (notably
        // 0004's `foreign_keys=OFF/ON` dance) applies coherently.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("bootstrap pool");

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 id          TEXT PRIMARY KEY,
                 applied_at  TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
             )",
        )
        .execute(&pool)
        .await
        .unwrap();

        for (id, sql) in PRE_M7_MIGRATIONS {
            // raw_sql feeds the whole multi-statement script (comments,
            // BEGIN/COMMIT, PRAGMAs and all) straight to SQLite, which
            // parses `--` comments natively — mirroring how the engine
            // applies these files.
            sqlx::raw_sql(sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("apply {id}: {e}"));
            sqlx::query("INSERT INTO schema_migrations (id) VALUES (?)")
                .bind(*id)
                .execute(&pool)
                .await
                .unwrap();
        }

        // Seed history that must survive the upgrade.
        sqlx::query("INSERT INTO cameras (id, name, url, config_json) VALUES (1, 'front', 'rtsp://127.0.0.1/s', '{}')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO rules (id, name, config_json) VALUES ('rule.keep', 'Keep', '{}')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO events
                 (event_id, camera_id, rule_id, label, severity, frame_id, captured_at, trace_id, payload_json)
             VALUES
                 ('evt-keep', 1, 'rule.keep', 'person', 'high', 1, '2024-01-01T00:00:00Z', 'trace-keep', '{\"k\":\"v\"}')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO alert_sink_outbox (event_id, sink_id, status) VALUES ('evt-keep', 'webhook:keep', 'pending')",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool.close().await;
    }

    // --- Phase 2: run the REAL migration runner (applies 0007+) ------
    let cfg = StoreConfig {
        url: format!("sqlite:{}?mode=rwc", db_path.display()),
        seed_from_config: false,
        duckdb_attach: false,
        duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
    };
    let store = Store::open(&cfg).await.expect("Store::open applies 0007+");
    let pool = store.pool();

    // --- Phase 3: pre-existing rows are intact ----------------------
    let (n_events,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(n_events, 1, "the pre-0007 event must survive");

    let (payload,): (String,) =
        sqlx::query_as("SELECT payload_json FROM events WHERE event_id = 'evt-keep'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(payload, "{\"k\":\"v\"}", "event payload must be untouched");

    let (n_cameras,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM cameras")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(n_cameras, 1);

    let (n_outbox,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM alert_sink_outbox WHERE event_id = 'evt-keep' AND status = 'pending'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        n_outbox, 1,
        "the pending outbox row must survive (no FK cascade)"
    );

    // --- Phase 4: the new 0007–0009 schema is present ---------------
    // 0007: delivery_settings singleton seeded.
    let (enabled,): (i64,) = sqlx::query_as("SELECT enabled FROM delivery_settings WHERE id = 1")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(enabled, 1, "0007 seed row present and enabled");

    // 0008: rules.delivery_policy_json column exists (NULL on the
    // pre-existing rule).
    let (policy,): (Option<String>,) =
        sqlx::query_as("SELECT delivery_policy_json FROM rules WHERE id = 'rule.keep'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(policy.is_none(), "0008 column backfills NULL for old rules");

    // 0009: audit_log table exists and is queryable.
    let (n_audit,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(n_audit, 0, "0009 audit_log table created, empty");

    // The runner recorded the M7 migrations.
    let (n_applied,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM schema_migrations
         WHERE id IN ('0007_delivery_settings', '0008_rules_delivery_policy', '0009_audit_log')",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(n_applied, 3, "0007–0009 stamped into schema_migrations");
}

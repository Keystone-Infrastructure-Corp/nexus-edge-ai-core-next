//! SQLite-backed store for cameras, rules, events, and audit.
//!
//! Schema migrations live in `migrations/NNNN_*.sql` and are applied at
//! `open()` time. Each file is registered in `schema_migrations` so that
//! non-idempotent statements (e.g. `ALTER TABLE … ADD COLUMN`) only run
//! once per database. DuckDB attach for read-side analytics is wired
//! through `Store::attach_duckdb` (M0 stub: returns `Unimplemented`).

#![forbid(unsafe_code)]

pub mod audit;
pub mod cloud;
pub mod motion;
pub mod outbox;
pub mod sessions;
pub mod users;
pub mod visual_prompts;
pub use audit::{AuditActorKind, AuditEntry, AuditFilter, AuditOutcome, NewAuditEntry};
pub use motion::{
    ClipClose, ClipColdMark, ClipId, ClipRow, ColdReplicaRow, ColdReplicaStats, DeleteBackendError,
    MotionEventId, MotionEventKind, MotionEventRow, MotionHistogramBucket, NewClip, NewMotionEvent,
    PerCameraClipStats, StorageBackendRow,
};
pub use outbox::{OutboxRow, OutboxSinkCounts, OutboxStatus, SuppressionReason};
pub use sessions::{NewRefreshToken, RefreshToken, RefreshTokenId, SessionsError};
pub use users::{NewUser, User, UserId, UsersError};
pub use visual_prompts::{NewVisualPrompt, VisualPrompt, VisualPromptError, VisualPromptSummary};

/// Re-export the SQLite transaction type so downstream crates
/// (notably nexus-engine, which doesn't depend on `sqlx`
/// directly outside dev-deps) can name the parameter type for
/// the `*_tx` methods added in M6 Phase 4 Step 4.1.
pub type SqliteTx<'c> = sqlx::Transaction<'c, sqlx::Sqlite>;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nexus_config::{CameraConfig, Config, RuleConfig, StoreConfig};
use nexus_types::{
    AlertEvent, CameraId, DeliverySchedule, DeliverySettings, RuleDeliveryPolicy, RuleId,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use thiserror::Error;
use tracing::info;

const SEEDED_KEY: &str = "seeded_from_toml";

/// Migration files in apply order. Each entry is `(id, sql)`. Adding a
/// new file: bump the id, append it here, ship it; the runner records
/// it in `schema_migrations` after a successful apply so it never runs
/// twice on the same DB.
const MIGRATIONS: &[(&str, &str)] = &[
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
    (
        "0007_delivery_settings",
        include_str!("../migrations/0007_delivery_settings.sql"),
    ),
    (
        "0008_rules_delivery_policy",
        include_str!("../migrations/0008_rules_delivery_policy.sql"),
    ),
    (
        "0009_audit_log",
        include_str!("../migrations/0009_audit_log.sql"),
    ),
    (
        "0010_local_users",
        include_str!("../migrations/0010_local_users.sql"),
    ),
    (
        "0011_auth_refresh_tokens",
        include_str!("../migrations/0011_auth_refresh_tokens.sql"),
    ),
    (
        "0012_visual_prompts",
        include_str!("../migrations/0012_visual_prompts.sql"),
    ),
    (
        "0013_cloud_enrollment",
        include_str!("../migrations/0013_cloud_enrollment.sql"),
    ),
    (
        "0014_storage_backends_azure_blob",
        include_str!("../migrations/0014_storage_backends_azure_blob.sql"),
    ),
    (
        "0015_motion_clips_priority",
        include_str!("../migrations/0015_motion_clips_priority.sql"),
    ),
];

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("serde_json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unimplemented: {0}")]
    Unimplemented(&'static str),
    #[error("decode: {0}")]
    Decode(String),
}

// ---------------------------------------------------------------------------
// EventStore trait — keeps the rule engine decoupled from the concrete store.
// ---------------------------------------------------------------------------

#[async_trait]
pub trait EventStore: Send + Sync {
    async fn record_event(&self, event: &AlertEvent) -> Result<(), StoreError>;
    async fn list_recent_events(&self, limit: i64) -> Result<Vec<AlertEvent>, StoreError>;
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    pub async fn open(cfg: &StoreConfig) -> Result<Self, StoreError> {
        // Strip leading `sqlite:` so SqliteConnectOptions::from_str-style parsing works.
        let url = cfg.url.trim_start_matches("sqlite:");
        let opts = SqliteConnectOptions::new()
            .filename(url.split('?').next().unwrap_or(url))
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;

        Self::apply_schema(&pool).await?;

        Ok(Self { pool })
    }

    async fn apply_schema(pool: &SqlitePool) -> Result<(), StoreError> {
        // Bootstrap the migrations table itself. Idempotent.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 id          TEXT PRIMARY KEY,
                 applied_at  TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
             )",
        )
        .execute(pool)
        .await?;

        // Backfill: if 0001's tables already exist (DB created before the
        // migrations table was introduced), record 0001 as applied so we
        // never try to re-run its idempotent CREATEs *and* don't trip
        // 0002's `ALTER TABLE events ADD COLUMN clip_id` twice.
        if Self::table_exists(pool, "cameras").await?
            && !Self::migration_applied(pool, "0001_initial").await?
        {
            sqlx::query("INSERT INTO schema_migrations (id) VALUES (?)")
                .bind("0001_initial")
                .execute(pool)
                .await?;
        }

        for (id, sql) in MIGRATIONS {
            if Self::migration_applied(pool, id).await? {
                continue;
            }
            // A migration may opt out of the wrapping transaction so it
            // can perform a parent-table rebuild safely. The official
            // SQLite recipe for that requires `PRAGMA foreign_keys=OFF`
            // OUTSIDE any transaction (the pragma is a no-op inside
            // one), and DROP TABLE under foreign_keys=ON does an
            // implicit `DELETE FROM` that fires every `ON DELETE
            // CASCADE` referencing the table — silently nuking child
            // rows on upgrade. To opt out, place the literal marker
            // `-- nexus:no-transaction` somewhere in the file. The
            // migration is still atomic on success because we record
            // it in `schema_migrations` only after every statement
            // succeeded; on partial failure the operator restarts the
            // engine and the migration retries from the top.
            let no_tx = sql.contains("-- nexus:no-transaction");
            // Strip `--` line comments BEFORE splitting on `;` — otherwise
            // a `;` inside a comment fragments the comment text and
            // sqlite then tries to parse the leftover prose as SQL.
            // (Migration files MUST NOT contain `;` inside string
            // literals or trigger bodies — keep them plain DDL.)
            let stripped = strip_sql_line_comments(sql);

            if no_tx {
                // Run all statements on the SAME connection so the
                // file's `PRAGMA foreign_keys=OFF` (must be issued
                // outside any transaction) actually applies to every
                // subsequent statement. The migration body is
                // responsible for its own BEGIN/COMMIT and the
                // matching `PRAGMA foreign_keys=ON`.
                let mut conn = pool.acquire().await?;
                for stmt in stripped.split(';') {
                    let s = stmt.trim();
                    if s.is_empty() {
                        continue;
                    }
                    sqlx::query(s).execute(&mut *conn).await?;
                }
                sqlx::query("INSERT INTO schema_migrations (id) VALUES (?)")
                    .bind(*id)
                    .execute(&mut *conn)
                    .await?;
            } else {
                let mut tx = pool.begin().await?;
                for stmt in stripped.split(';') {
                    let s = stmt.trim();
                    if s.is_empty() {
                        continue;
                    }
                    sqlx::query(s).execute(&mut *tx).await?;
                }
                sqlx::query("INSERT INTO schema_migrations (id) VALUES (?)")
                    .bind(*id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
            }
            info!(migration = %id, "applied schema migration");
        }
        Ok(())
    }

    async fn migration_applied(pool: &SqlitePool, id: &str) -> Result<bool, StoreError> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM schema_migrations WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
        Ok(row.is_some())
    }

    async fn table_exists(pool: &SqlitePool, name: &str) -> Result<bool, StoreError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
                .bind(name)
                .fetch_optional(pool)
                .await?;
        Ok(row.is_some())
    }

    /// Seed cameras + rules from the TOML config — exactly once. The flag
    /// lives in `engine_state.seeded_from_toml`.
    pub async fn seed_from_config_if_empty(&self, cfg: &Config) -> Result<(), StoreError> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM engine_state WHERE key = ?")
            .bind(SEEDED_KEY)
            .fetch_optional(&self.pool)
            .await?;

        if row.is_some() {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;
        for cam in &cfg.cameras {
            let json = serde_json::to_string(cam)?;
            sqlx::query(
                "INSERT OR REPLACE INTO cameras (id, name, url, enabled, config_json) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(cam.id)
            .bind(&cam.name)
            .bind(cam.ingest.url.to_string())
            .bind(cam.ingest.enabled as i64)
            .bind(&json)
            .execute(&mut *tx)
            .await?;
        }
        for rule in &cfg.rules.inline {
            let json = serde_json::to_string(rule)?;
            sqlx::query(
                "INSERT OR REPLACE INTO rules (id, name, enabled, config_json) VALUES (?, ?, ?, ?)",
            )
            .bind(&rule.id)
            .bind(&rule.name)
            .bind(rule.enabled as i64)
            .bind(&json)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("INSERT OR REPLACE INTO engine_state (key, value) VALUES (?, ?)")
            .bind(SEEDED_KEY)
            .bind(Utc::now().to_rfc3339())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        info!(
            cameras = cfg.cameras.len(),
            rules = cfg.rules.inline.len(),
            "store seeded"
        );
        Ok(())
    }

    pub async fn list_cameras(&self) -> Result<Vec<CameraConfig>, StoreError> {
        let rows = sqlx::query("SELECT config_json FROM cameras ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let s: String = r.get(0);
            out.push(serde_json::from_str(&s)?);
        }
        Ok(out)
    }

    pub async fn upsert_camera(&self, cam: &CameraConfig) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        self.upsert_camera_tx(&mut tx, cam).await?;
        tx.commit().await?;
        Ok(())
    }

    /// M6 Phase 4 Step 4.1 — tx-aware upsert. Runs the same
    /// INSERT-OR-UPDATE as [`Store::upsert_camera`] inside a
    /// caller-supplied transaction so the handler can pair the
    /// domain mutation with an `audit_log` insert and commit them
    /// atomically. A failed commit rolls back both halves.
    pub async fn upsert_camera_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        cam: &CameraConfig,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(cam)?;
        sqlx::query(
            "INSERT INTO cameras (id, name, url, enabled, config_json) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, url = excluded.url,
               enabled = excluded.enabled, config_json = excluded.config_json,
               updated_at = CURRENT_TIMESTAMP",
        )
        .bind(cam.id)
        .bind(&cam.name)
        .bind(cam.ingest.url.to_string())
        .bind(cam.ingest.enabled as i64)
        .bind(&json)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// INSERT a new camera, letting SQLite's `INTEGER PRIMARY
    /// KEY` rowid alias assign the id. Mutates `cam.id` to the
    /// assigned rowid and writes the updated JSON so the
    /// `config_json` blob carries the same id the row carries.
    /// Used by `POST /cameras` (operator-initiated create) where
    /// the caller has no way to invent a stable i64 id up front.
    pub async fn create_camera_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        cam: &mut CameraConfig,
    ) -> Result<(), StoreError> {
        // Insert with NULL id to trigger rowid auto-assignment.
        // `config_json` is provisionally serialised with whatever
        // id the caller passed in (usually 0) — we rewrite it
        // immediately after we learn the assigned rowid so the
        // JSON copy stays consistent with the column.
        let placeholder_json = serde_json::to_string(cam)?;
        let res = sqlx::query(
            "INSERT INTO cameras (id, name, url, enabled, config_json) \
             VALUES (NULL, ?, ?, ?, ?)",
        )
        .bind(&cam.name)
        .bind(cam.ingest.url.to_string())
        .bind(cam.ingest.enabled as i64)
        .bind(&placeholder_json)
        .execute(&mut **tx)
        .await?;
        let new_id = res.last_insert_rowid();
        cam.id = new_id;
        let json = serde_json::to_string(cam)?;
        sqlx::query("UPDATE cameras SET config_json = ? WHERE id = ?")
            .bind(&json)
            .bind(new_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub async fn delete_camera(&self, id: CameraId) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        self.delete_camera_tx(&mut tx, id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// M6 Phase 4 Step 4.1 — tx-aware delete. See
    /// [`Store::upsert_camera_tx`] for the tx-merge rationale.
    pub async fn delete_camera_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        id: CameraId,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM cameras WHERE id = ?")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub async fn list_rules(&self) -> Result<Vec<RuleConfig>, StoreError> {
        let rows = sqlx::query("SELECT config_json FROM rules ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let s: String = r.get(0);
            out.push(serde_json::from_str(&s)?);
        }
        Ok(out)
    }

    pub async fn upsert_rule(&self, rule: &RuleConfig) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        self.upsert_rule_tx(&mut tx, rule).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Allocate the next unused `rule-<N>` id within the same tx
    /// the caller will use for the INSERT. Scans for ids that
    /// match the canonical auto-id format (`rule-<positive int>`),
    /// returns `rule-{max+1}` (or `rule-1` if no auto-id rule
    /// exists yet). Custom string ids that don't match the
    /// pattern are ignored — they live alongside auto-ids without
    /// pushing the sequence forward.
    ///
    /// Running this inside the same tx that performs the INSERT
    /// closes the race window: SQLite's default serialised txs
    /// are mutually exclusive on writes, so two concurrent
    /// `POST /rules` calls can't both see the same max and pick
    /// the same id.
    pub async fn next_rule_id_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<String, StoreError> {
        // GLOB filters to numeric suffixes; CAST + ORDER BY DESC
        // picks the largest numerically (not lexicographically,
        // which would put `rule-9` after `rule-10`).
        let row = sqlx::query(
            "SELECT id FROM rules WHERE id GLOB 'rule-[0-9]*' \
             ORDER BY CAST(SUBSTR(id, 6) AS INTEGER) DESC LIMIT 1",
        )
        .fetch_optional(&mut **tx)
        .await?;
        let next: u64 = match row {
            Some(r) => {
                let id: String = r.get(0);
                let suffix = id.strip_prefix("rule-").unwrap_or("");
                suffix.parse::<u64>().unwrap_or(0).saturating_add(1).max(1)
            }
            None => 1,
        };
        Ok(format!("rule-{next}"))
    }

    /// M6 Phase 4 Step 4.1 — tx-aware upsert. See
    /// [`Store::upsert_camera_tx`] for the tx-merge rationale.
    pub async fn upsert_rule_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        rule: &RuleConfig,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(rule)?;
        sqlx::query(
            "INSERT INTO rules (id, name, enabled, config_json) VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name,
               enabled = excluded.enabled, config_json = excluded.config_json,
               updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&rule.id)
        .bind(&rule.name)
        .bind(rule.enabled as i64)
        .bind(&json)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn delete_rule(&self, id: &RuleId) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        self.delete_rule_tx(&mut tx, id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// M6 Phase 4 Step 4.1 — tx-aware delete. See
    /// [`Store::upsert_camera_tx`] for the tx-merge rationale.
    pub async fn delete_rule_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        id: &RuleId,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM rules WHERE id = ?")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Legacy 4-arg audit-write retained as a shim while we
    /// migrate every M2/M5/M7 call site to the M6 audit API.
    /// Translates the old `(actor, action, resource, diff)`
    /// shape into a [`NewAuditEntry`] and inserts via
    /// [`Store::record_audit_event_standalone`] so the row
    /// satisfies the M6 schema's CHECK constraints. Once
    /// Phase 2 Step 2.5 ships (`require_role` extractor that
    /// puts the session on the request), every handler will
    /// build its own `NewAuditEntry` with real `actor_kind` +
    /// `actor_id` + `ip` + `user_agent`, and this shim goes
    /// away.
    ///
    /// Compatibility choices baked in:
    ///
    /// * `actor` is denormalised into `actor_label`. The string
    ///   `"api"` / `"ui"` / `"discovery"` that today's callers
    ///   pass in is not a real user; it identifies the engine
    ///   subsystem that initiated the write. We tag it as
    ///   [`AuditActorKind::System`] so the M6 dashboards don't
    ///   surface these rows as if a human did them.
    /// * `resource` is split on the first `/` so the existing
    ///   `"admin/delivery"` and `"camera/abc"` strings populate
    ///   `(resource_kind, resource_id)` cleanly. A bare
    ///   `"runtime"` becomes `(resource_kind = "runtime",
    ///   resource_id = None)`.
    /// * `diff` lands in `after_json` (the legacy callers only
    ///   call this on the success path, post-write).
    /// * `outcome` is always `Success` — the legacy callers
    ///   bail with `?` on the audit insert as a non-fatal step
    ///   that runs after the actual mutation already committed.
    pub async fn write_audit(
        &self,
        actor: &str,
        action: &str,
        resource: &str,
        diff: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let (resource_kind, resource_id) = match resource.split_once('/') {
            Some((kind, id)) if !id.is_empty() => (Some(kind), Some(id)),
            _ => (Some(resource), None),
        };
        let diff_str = diff.to_string();
        let entry = crate::audit::NewAuditEntry {
            actor_kind: Some(crate::audit::AuditActorKind::System),
            actor_label: actor,
            action,
            resource_kind,
            resource_id,
            after_json: Some(diff_str.as_str()),
            outcome: crate::audit::AuditOutcome::Success,
            ..Default::default()
        };
        self.record_audit_event_standalone(&entry).await?;
        Ok(())
    }

    /// Stub — wired in M3. The intent is `ATTACH '...analytics.duckdb' AS analytics`
    /// for cross-engine queries from the events table.
    pub async fn attach_duckdb(&self, _cfg: &StoreConfig) -> Result<(), StoreError> {
        Err(StoreError::Unimplemented("duckdb attach (M3)"))
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[async_trait]
impl EventStore for Store {
    async fn record_event(&self, event: &AlertEvent) -> Result<(), StoreError> {
        let payload = serde_json::to_string(event)?;
        sqlx::query(
            "INSERT INTO events (event_id, camera_id, rule_id, track_id, label,
                                 severity, frame_id, captured_at, trace_id, payload_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(event.event_id.to_string())
        .bind(event.camera_id)
        .bind(&event.rule_id)
        .bind(event.track_id.map(|t| t as i64))
        .bind(&event.label)
        .bind(severity_str(event.severity))
        .bind(event.frame_id as i64)
        .bind(event.captured_at.to_rfc3339())
        .bind(&event.trace_id)
        .bind(&payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_recent_events(&self, limit: i64) -> Result<Vec<AlertEvent>, StoreError> {
        let rows = sqlx::query("SELECT payload_json FROM events ORDER BY captured_at DESC LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let s: String = r.get(0);
            out.push(serde_json::from_str(&s)?);
        }
        Ok(out)
    }
}

fn severity_str(s: nexus_types::Severity) -> &'static str {
    match s {
        nexus_types::Severity::Low => "low",
        nexus_types::Severity::Medium => "medium",
        nexus_types::Severity::High => "high",
        nexus_types::Severity::Critical => "critical",
    }
}

// ---------------------------------------------------------------------------
// M7 alert delivery — outbox writes.
//
// `record_event_and_enqueue` is the crash-safe enqueue API: the
// `events` row and every `alert_sink_outbox` row land in the same
// `sqlx::Transaction`, so a power loss between the two cannot leave
// a half-enqueued alert. The dispatcher uses the rest of the
// helpers in this block (drain + mark_*) to advance rows through
// the state machine described in `src/outbox.rs`.
// ---------------------------------------------------------------------------

impl Store {
    /// Re-hydrate an `AlertEvent` by id. Returns `None` when the
    /// event row is missing (most likely because a clip eviction
    /// cascaded through `events.clip_id`). The dispatcher uses
    /// this to load the alert payload referenced by each
    /// `alert_sink_outbox.event_id` row.
    pub async fn get_event(&self, event_id: &str) -> Result<Option<AlertEvent>, StoreError> {
        let row = sqlx::query("SELECT payload_json FROM events WHERE event_id = ?")
            .bind(event_id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            None => Ok(None),
            Some(r) => {
                let s: String = r.get(0);
                Ok(Some(serde_json::from_str(&s)?))
            }
        }
    }

    /// Insert an `AlertEvent` AND one `alert_sink_outbox` row per
    /// `sink_id` in a single transaction. If `sink_ids` is empty the
    /// behaviour is identical to `EventStore::record_event` (the
    /// event still lands; nothing is enqueued).
    ///
    /// On any failure (including a `UNIQUE (event_id, sink_id)`
    /// violation from a duplicate enqueue) the entire transaction
    /// rolls back — neither the event nor any outbox row survives.
    pub async fn record_event_and_enqueue(
        &self,
        event: &AlertEvent,
        sink_ids: &[&str],
    ) -> Result<(), StoreError> {
        let payload = serde_json::to_string(event)?;
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO events (event_id, camera_id, rule_id, track_id, label,
                                 severity, frame_id, captured_at, trace_id, payload_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(event.event_id.to_string())
        .bind(event.camera_id)
        .bind(&event.rule_id)
        .bind(event.track_id.map(|t| t as i64))
        .bind(&event.label)
        .bind(severity_str(event.severity))
        .bind(event.frame_id as i64)
        .bind(event.captured_at.to_rfc3339())
        .bind(&event.trace_id)
        .bind(&payload)
        .execute(&mut *tx)
        .await?;

        let event_id_str = event.event_id.to_string();
        for sink_id in sink_ids {
            sqlx::query(
                "INSERT INTO alert_sink_outbox (event_id, sink_id, status)
                 VALUES (?, ?, 'pending')",
            )
            .bind(&event_id_str)
            .bind(*sink_id)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Dispatcher drain query. Returns up to `limit` rows that are
    /// `pending` AND whose `next_attempt_at` is either NULL (fresh
    /// row, never tried) or `<= now`. Ordered by `id` ASC so the
    /// oldest enqueued row goes first.
    pub async fn outbox_pending(&self, limit: i64) -> Result<Vec<OutboxRow>, StoreError> {
        let now = Utc::now().to_rfc3339();
        let rows = sqlx::query(
            "SELECT id, event_id, sink_id, status, attempts, next_attempt_at,
                    last_error, suppression_reason, created_at, delivered_at
               FROM alert_sink_outbox
              WHERE status = 'pending'
                AND (next_attempt_at IS NULL OR next_attempt_at <= ?)
              ORDER BY id ASC
              LIMIT ?",
        )
        .bind(&now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(outbox_row_from_sql).collect()
    }

    /// Every outbox row for a given event — powers the per-event
    /// delivery badge view in the admin UI.
    pub async fn outbox_for_event(&self, event_id: &str) -> Result<Vec<OutboxRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, event_id, sink_id, status, attempts, next_attempt_at,
                    last_error, suppression_reason, created_at, delivered_at
               FROM alert_sink_outbox
              WHERE event_id = ?
              ORDER BY id ASC",
        )
        .bind(event_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(outbox_row_from_sql).collect()
    }

    /// Aggregate outbox status counts per `sink_id` over the rows
    /// whose `created_at >= since`. Powers
    /// `GET /api/v1/admin/sinks/health`: the admin handler calls
    /// this twice (1h + 24h windows) and zips the result with the
    /// `SinkRegistry::ids()` set so the UI gets a card per
    /// configured sink — including configured-but-quiet ones that
    /// don't appear in the outbox at all.
    ///
    /// Sink IDs that exist in the outbox but no longer in the
    /// registry (e.g. the operator deleted the sink) are still
    /// returned so the UI can show their historical state and
    /// surface the orphan rows for cleanup.
    pub async fn outbox_counts_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<OutboxSinkCounts>, StoreError> {
        let rows = sqlx::query(
            "SELECT sink_id,
                    SUM(CASE WHEN status = 'sent'       THEN 1 ELSE 0 END) AS sent,
                    SUM(CASE WHEN status = 'failed'     THEN 1 ELSE 0 END) AS failed,
                    SUM(CASE WHEN status = 'dead'       THEN 1 ELSE 0 END) AS dead,
                    SUM(CASE WHEN status = 'suppressed' THEN 1 ELSE 0 END) AS suppressed,
                    SUM(CASE WHEN status = 'pending'    THEN 1 ELSE 0 END) AS pending
               FROM alert_sink_outbox
              WHERE created_at >= ?
              GROUP BY sink_id
              ORDER BY sink_id ASC",
        )
        .bind(since.to_rfc3339())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| OutboxSinkCounts {
                sink_id: r.get("sink_id"),
                sent: r.try_get("sent").unwrap_or(0),
                failed: r.try_get("failed").unwrap_or(0),
                dead: r.try_get("dead").unwrap_or(0),
                suppressed: r.try_get("suppressed").unwrap_or(0),
                pending: r.try_get("pending").unwrap_or(0),
            })
            .collect())
    }

    /// Terminal success: `status='sent'`, stamp `delivered_at`,
    /// increment `attempts`.
    pub async fn outbox_mark_sent(&self, id: i64) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE alert_sink_outbox
                SET status = 'sent',
                    attempts = attempts + 1,
                    delivered_at = ?,
                    last_error = NULL
              WHERE id = ?",
        )
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Non-terminal failure: bump `attempts`, record `last_error`,
    /// schedule the next retry. Status flips back to `'pending'` so
    /// the next `outbox_pending` sweep that finds it past
    /// `next_attempt_at` picks it up.
    pub async fn outbox_mark_failed(
        &self,
        id: i64,
        last_error: &str,
        next_attempt_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE alert_sink_outbox
                SET status = 'pending',
                    attempts = attempts + 1,
                    last_error = ?,
                    next_attempt_at = ?
              WHERE id = ?",
        )
        .bind(last_error)
        .bind(next_attempt_at.to_rfc3339())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Terminal failure after exhausting retries.
    pub async fn outbox_mark_dead(&self, id: i64, last_error: &str) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE alert_sink_outbox
                SET status = 'dead',
                    attempts = attempts + 1,
                    last_error = ?,
                    next_attempt_at = NULL
              WHERE id = ?",
        )
        .bind(last_error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Terminal suppression — never attempted (or never re-attempted).
    /// `attempts` is NOT bumped because no `deliver()` call happened.
    pub async fn outbox_mark_suppressed(
        &self,
        id: i64,
        reason: SuppressionReason,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE alert_sink_outbox
                SET status = 'suppressed',
                    suppression_reason = ?,
                    next_attempt_at = NULL,
                    last_error = NULL
              WHERE id = ?",
        )
        .bind(reason.as_str())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- M7 Step 5: delivery_settings + per-rule policy ----

    /// Read the singleton `delivery_settings` row.
    ///
    /// The migration seeds row id=1 at install time so this always
    /// returns `Ok(DeliverySettings)` — callers never have to
    /// handle a "missing row" case. The schedule blob is parsed
    /// eagerly: a malformed `schedule_json` is reported as a
    /// `Decode` error so the engine refuses to boot rather than
    /// silently dropping the schedule and over-delivering.
    pub async fn delivery_settings_get(&self) -> Result<DeliverySettings, StoreError> {
        let row = sqlx::query(
            "SELECT enabled, schedule_json, timezone, updated_at
               FROM delivery_settings WHERE id = 1",
        )
        .fetch_one(&self.pool)
        .await?;
        let enabled: i64 = row.get("enabled");
        let schedule_json: Option<String> = row.try_get("schedule_json")?;
        let timezone: String = row.get("timezone");
        let updated_at_str: String = row.get("updated_at");
        let schedule = match schedule_json {
            Some(s) if !s.is_empty() => Some(serde_json::from_str::<DeliverySchedule>(&s)?),
            _ => None,
        };
        Ok(DeliverySettings {
            enabled: enabled != 0,
            schedule,
            timezone,
            updated_at: parse_sqlite_timestamp(&updated_at_str)?,
        })
    }

    /// Upsert the singleton `delivery_settings` row. The caller is
    /// expected to publish `delivery.settings.changed` on the bus
    /// after a successful write so the dispatcher's cached
    /// `ArcSwap<DeliverySettings>` picks up the change without a
    /// restart.
    ///
    /// Validation: the schedule shape (7 × 48) is checked here;
    /// the timezone string is NOT parsed at the store boundary —
    /// the policy layer resolves it via `chrono-tz` and falls back
    /// to UTC with a warn! on an unknown name (so a typo can't
    /// brick delivery globally).
    pub async fn delivery_settings_put(
        &self,
        settings: &DeliverySettings,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        self.delivery_settings_put_tx(&mut tx, settings).await?;
        tx.commit().await?;
        Ok(())
    }

    /// M6 Phase 4 Step 4.1 — tx-aware put. See
    /// [`Store::upsert_camera_tx`] for the tx-merge rationale.
    pub async fn delivery_settings_put_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        settings: &DeliverySettings,
    ) -> Result<(), StoreError> {
        if let Some(s) = &settings.schedule {
            s.validate()
                .map_err(|e| StoreError::Decode(e.to_string()))?;
        }
        let schedule_json = match &settings.schedule {
            Some(s) => Some(serde_json::to_string(s)?),
            None => None,
        };
        sqlx::query(
            "UPDATE delivery_settings
                SET enabled = ?,
                    schedule_json = ?,
                    timezone = ?,
                    updated_at = ?
              WHERE id = 1",
        )
        .bind(settings.enabled as i64)
        .bind(schedule_json)
        .bind(&settings.timezone)
        .bind(Utc::now().to_rfc3339())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Read the per-rule delivery policy override. Returns
    /// `Ok(None)` when the rule inherits the global settings
    /// (NULL `delivery_policy_json`), or when the rule id itself
    /// doesn't exist. The dispatcher treats both cases as
    /// "inherit global" so the missing-rule case is benign — an
    /// outbox row whose rule has since been deleted resolves
    /// against the global settings on its way to `dead`.
    pub async fn rule_delivery_policy_get(
        &self,
        rule_id: &RuleId,
    ) -> Result<Option<RuleDeliveryPolicy>, StoreError> {
        let row = sqlx::query("SELECT delivery_policy_json FROM rules WHERE id = ?")
            .bind(rule_id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let blob: Option<String> = row.try_get("delivery_policy_json")?;
        match blob {
            Some(s) if !s.is_empty() => Ok(Some(serde_json::from_str::<RuleDeliveryPolicy>(&s)?)),
            _ => Ok(None),
        }
    }

    /// Load every rule's policy override at once. Used by the
    /// dispatcher's cache hydration at boot and on each
    /// `rule.delivery_policy.changed` bus signal. Rules with a
    /// NULL `delivery_policy_json` are omitted from the map
    /// (inheriting global is the implicit default).
    pub async fn rule_delivery_policies_all(
        &self,
    ) -> Result<std::collections::HashMap<RuleId, RuleDeliveryPolicy>, StoreError> {
        let rows = sqlx::query("SELECT id, delivery_policy_json FROM rules")
            .fetch_all(&self.pool)
            .await?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for r in rows {
            let id: String = r.get("id");
            let blob: Option<String> = r.try_get("delivery_policy_json")?;
            if let Some(s) = blob {
                if !s.is_empty() {
                    let p: RuleDeliveryPolicy = serde_json::from_str(&s)?;
                    out.insert(id, p);
                }
            }
        }
        Ok(out)
    }

    /// Set or clear the per-rule policy. Passing `None` clears the
    /// override (the rule reverts to inheriting global). Caller is
    /// expected to publish `rule.delivery_policy.changed` on the
    /// bus after a successful write.
    ///
    /// Returns `NotFound` if the rule id doesn't exist — the API
    /// surfaces this as 404, never as a silent no-op.
    pub async fn rule_delivery_policy_put(
        &self,
        rule_id: &RuleId,
        policy: Option<&RuleDeliveryPolicy>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        self.rule_delivery_policy_put_tx(&mut tx, rule_id, policy)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// M6 Phase 4 Step 4.1 — tx-aware put. See
    /// [`Store::upsert_camera_tx`] for the tx-merge rationale.
    pub async fn rule_delivery_policy_put_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        rule_id: &RuleId,
        policy: Option<&RuleDeliveryPolicy>,
    ) -> Result<(), StoreError> {
        if let Some(p) = policy {
            if let Some(s) = &p.schedule {
                s.validate()
                    .map_err(|e| StoreError::Decode(e.to_string()))?;
            }
        }
        let blob = match policy {
            Some(p) => Some(serde_json::to_string(p)?),
            None => None,
        };
        let res = sqlx::query(
            "UPDATE rules
                SET delivery_policy_json = ?,
                    updated_at = CURRENT_TIMESTAMP
              WHERE id = ?",
        )
        .bind(blob)
        .bind(rule_id)
        .execute(&mut **tx)
        .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("rule {rule_id:?}")));
        }
        Ok(())
    }

    /// M6 Phase 4 Step 4.1 — open a new transaction on the
    /// underlying pool. Admin handlers use this to bracket a
    /// domain mutation + audit-log insert in one tx so a crash
    /// between the two cannot leave them out of sync.
    pub async fn begin_tx(&self) -> Result<sqlx::Transaction<'_, sqlx::Sqlite>, StoreError> {
        Ok(self.pool.begin().await?)
    }

    /// M6 Phase 4 Step 4.1 — commit a transaction opened via
    /// [`Store::begin_tx`]. Lifted out as an associated
    /// function so downstream crates (notably `nexus-engine`,
    /// which does not depend on `sqlx` outside dev-deps) can
    /// finalise a tx without having to name `sqlx::Error`.
    pub async fn commit_tx(tx: sqlx::Transaction<'_, sqlx::Sqlite>) -> Result<(), StoreError> {
        tx.commit().await?;
        Ok(())
    }
}

/// Hydrate an `alert_sink_outbox` row out of the generic SQL row.
/// Centralised so every helper above shares the same column-decode
/// logic — and so adding a column only requires one edit here.
fn outbox_row_from_sql(r: sqlx::sqlite::SqliteRow) -> Result<OutboxRow, StoreError> {
    let status_str: String = r.get("status");
    let status = OutboxStatus::try_from(status_str.as_str()).map_err(StoreError::Decode)?;

    let suppression_reason = match r.try_get::<Option<String>, _>("suppression_reason")? {
        Some(s) => Some(SuppressionReason::try_from(s.as_str()).map_err(StoreError::Decode)?),
        None => None,
    };

    let next_attempt_at =
        parse_optional_rfc3339(r.try_get::<Option<String>, _>("next_attempt_at")?)?;
    let delivered_at = parse_optional_rfc3339(r.try_get::<Option<String>, _>("delivered_at")?)?;
    let created_at_str: String = r.get("created_at");
    let created_at = parse_sqlite_timestamp(&created_at_str)?;

    Ok(OutboxRow {
        id: r.get::<i64, _>("id"),
        event_id: r.get("event_id"),
        sink_id: r.get("sink_id"),
        status,
        attempts: r.get::<i64, _>("attempts"),
        next_attempt_at,
        last_error: r.try_get::<Option<String>, _>("last_error")?,
        suppression_reason,
        created_at,
        delivered_at,
    })
}

fn parse_optional_rfc3339(s: Option<String>) -> Result<Option<DateTime<Utc>>, StoreError> {
    match s {
        Some(s) => Ok(Some(parse_sqlite_timestamp(&s)?)),
        None => Ok(None),
    }
}

/// Accept either RFC-3339 (our explicit binds) or the
/// `CURRENT_TIMESTAMP` shape SQLite emits for the `created_at`
/// default: `YYYY-MM-DD HH:MM:SS`.
fn parse_sqlite_timestamp(s: &str) -> Result<DateTime<Utc>, StoreError> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .map(|n| DateTime::<Utc>::from_naive_utc_and_offset(n, Utc))
        .map_err(|e| StoreError::Decode(format!("bad timestamp {s:?}: {e}")))
}

// ---------------------------------------------------------------------------
// (AuditEntry now lives in audit.rs and is re-exported above; see
//  audit::AuditEntry, audit::NewAuditEntry, etc.)
// ---------------------------------------------------------------------------

// Re-export the Arc-friendly handle type.
pub type StoreHandle = Arc<Store>;

/// Strip SQL `--` line comments from a string. Used by the migration
/// runner so that comments containing `;` don't fragment when we split
/// the file on `;`. We only strip line comments outside of single-quoted
/// string literals; block comments (`/* ... */`) and double-quoted
/// identifiers are left intact (we don't use either in our migrations).
fn strip_sql_line_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\'' {
                // Handle SQL escaped quotes ('').
                if chars.peek() == Some(&'\'') {
                    out.push(chars.next().unwrap());
                } else {
                    in_string = false;
                }
            }
            continue;
        }
        if c == '\'' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c == '-' && chars.peek() == Some(&'-') {
            // Skip the second '-' and everything up to (and including) the newline.
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::strip_sql_line_comments;

    #[test]
    fn strips_full_line_comment() {
        let input = "-- a comment\nSELECT 1;";
        assert_eq!(strip_sql_line_comments(input), "\nSELECT 1;");
    }

    #[test]
    fn strips_inline_comment() {
        let input = "SELECT 1; -- trailing\nSELECT 2;";
        assert_eq!(strip_sql_line_comments(input), "SELECT 1; \nSELECT 2;");
    }

    #[test]
    fn preserves_double_dash_inside_string() {
        let input = "INSERT INTO t VALUES ('-- not a comment');";
        assert_eq!(
            strip_sql_line_comments(input),
            "INSERT INTO t VALUES ('-- not a comment');"
        );
    }

    #[test]
    fn handles_semicolon_inside_comment() {
        // The actual M2 migration regression: comment text with a `;`
        // must be removed before split-on-semicolon runs.
        let input = "-- one; two; three.\nCREATE TABLE t (x INT);";
        let out = strip_sql_line_comments(input);
        assert!(!out.contains(';') || out.matches(';').count() == 1);
        assert!(out.trim_end().ends_with(");"));
    }

    #[test]
    fn handles_escaped_quote_inside_string() {
        let input = "INSERT INTO t VALUES ('it''s -- fine');";
        assert_eq!(
            strip_sql_line_comments(input),
            "INSERT INTO t VALUES ('it''s -- fine');"
        );
    }
}

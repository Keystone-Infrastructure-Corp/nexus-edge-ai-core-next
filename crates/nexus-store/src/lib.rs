//! SQLite-backed store for cameras, rules, events, and audit.
//!
//! Schema migrations live in `migrations/NNNN_*.sql` and are applied at
//! `open()` time. Each file is registered in `schema_migrations` so that
//! non-idempotent statements (e.g. `ALTER TABLE … ADD COLUMN`) only run
//! once per database. DuckDB attach for read-side analytics is wired
//! through `Store::attach_duckdb` (M0 stub: returns `Unimplemented`).

#![forbid(unsafe_code)]

pub mod motion;
pub use motion::{
    ClipClose, ClipId, ClipRow, MotionEventId, MotionEventKind, MotionEventRow, NewClip,
    NewMotionEvent,
};

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nexus_config::{CameraConfig, Config, RuleConfig, StoreConfig};
use nexus_types::{AlertEvent, CameraId, RuleId};
use serde::{Deserialize, Serialize};
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
            let mut tx = pool.begin().await?;
            // Strip `--` line comments BEFORE splitting on `;` — otherwise
            // a `;` inside a comment fragments the comment text and
            // sqlite then tries to parse the leftover prose as SQL.
            // (Migration files MUST NOT contain `;` inside string
            // literals or trigger bodies — keep them plain DDL.)
            let stripped = strip_sql_line_comments(sql);
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
            .bind(cam.url.to_string())
            .bind(cam.enabled as i64)
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
        let json = serde_json::to_string(cam)?;
        sqlx::query(
            "INSERT INTO cameras (id, name, url, enabled, config_json) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, url = excluded.url,
               enabled = excluded.enabled, config_json = excluded.config_json,
               updated_at = CURRENT_TIMESTAMP",
        )
        .bind(cam.id)
        .bind(&cam.name)
        .bind(cam.url.to_string())
        .bind(cam.enabled as i64)
        .bind(&json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_camera(&self, id: CameraId) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM cameras WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_rule(&self, id: &RuleId) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM rules WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn write_audit(
        &self,
        actor: &str,
        action: &str,
        resource: &str,
        diff: &serde_json::Value,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO audit_log (actor, action, resource, diff_json) VALUES (?, ?, ?, ?)",
        )
        .bind(actor)
        .bind(action)
        .bind(resource)
        .bind(diff.to_string())
        .execute(&self.pool)
        .await?;
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
// AuditEntry — exposed for the API layer.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub actor: String,
    pub action: String,
    pub resource: String,
    pub diff: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

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

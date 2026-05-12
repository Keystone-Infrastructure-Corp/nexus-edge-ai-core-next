//! SQLite-backed store for cameras, rules, events, and audit.
//!
//! Schema is in `migrations/0001_initial.sql` and is applied at `open()`
//! time. DuckDB attach for read-side analytics is wired through
//! `Store::attach_duckdb` (M0 stub: returns `Unimplemented`).

#![forbid(unsafe_code)]

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
const SCHEMA_SQL: &str = include_str!("../migrations/0001_initial.sql");

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
        // sqlx doesn't ship a multi-statement runner without `migrate!`; do it inline.
        for stmt in SCHEMA_SQL.split(';') {
            let s = stmt.trim();
            if s.is_empty() {
                continue;
            }
            sqlx::query(s).execute(pool).await?;
        }
        Ok(())
    }

    /// Seed cameras + rules from the TOML config — exactly once. The flag
    /// lives in `engine_state.seeded_from_toml`.
    pub async fn seed_from_config_if_empty(&self, cfg: &Config) -> Result<(), StoreError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM engine_state WHERE key = ?")
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
        info!(cameras = cfg.cameras.len(), rules = cfg.rules.inline.len(), "store seeded");
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

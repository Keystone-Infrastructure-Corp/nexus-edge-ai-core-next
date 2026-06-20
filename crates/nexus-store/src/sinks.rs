//! M7 cloud-managed alert sinks — persistent CRUD.
//!
//! The runtime sink set is the UNION of `nexus.toml` `[[sinks]]`
//! (file sinks, frozen at boot) and the rows in the `alert_sinks`
//! table (db sinks, mutable at runtime via the admin API / cloud
//! console). See migration `0021_alert_sinks.sql` for the merge
//! semantics (db wins on `sink_id` collision).
//!
//! This module stays agnostic of the [`nexus_config::SinkConfig`]
//! shape: `config_json` is an opaque blob the engine serialises on
//! the way in and deserialises on the way out. That keeps the store
//! decoupled from sink-schema evolution — adding a field to a sink
//! variant never touches this table.

use chrono::{DateTime, Utc};

use crate::{parse_sqlite_timestamp, Store, StoreError};

/// One persisted `alert_sinks` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertSinkRow {
    /// `"<kind>:<name>"` — matches the dispatcher's `SinkId`.
    pub sink_id: String,
    pub kind: String,
    pub name: String,
    /// Serialised `nexus_config::SinkConfig` (secrets included).
    pub config_json: String,
    pub updated_at: DateTime<Utc>,
}

impl Store {
    /// List every persisted sink, oldest-updated first. The engine
    /// calls this at boot (and on each `sink.config.changed` bus
    /// signal) to rebuild the live registry.
    pub async fn alert_sinks_list(&self) -> Result<Vec<AlertSinkRow>, StoreError> {
        let rows = sqlx::query_as::<_, (String, String, String, String, String)>(
            "SELECT sink_id, kind, name, config_json, updated_at
               FROM alert_sinks
              ORDER BY updated_at ASC, sink_id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(sink_id, kind, name, config_json, updated_at)| {
                Ok(AlertSinkRow {
                    sink_id,
                    kind,
                    name,
                    config_json,
                    updated_at: parse_sqlite_timestamp(&updated_at)?,
                })
            })
            .collect()
    }

    /// Fetch a single persisted sink by id. `Ok(None)` ⇔ the id is
    /// not in the db (it may still exist as a file sink).
    pub async fn alert_sink_get(&self, sink_id: &str) -> Result<Option<AlertSinkRow>, StoreError> {
        let row = sqlx::query_as::<_, (String, String, String, String, String)>(
            "SELECT sink_id, kind, name, config_json, updated_at
               FROM alert_sinks WHERE sink_id = ?",
        )
        .bind(sink_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some((sink_id, kind, name, config_json, updated_at)) => Ok(Some(AlertSinkRow {
                sink_id,
                kind,
                name,
                config_json,
                updated_at: parse_sqlite_timestamp(&updated_at)?,
            })),
            None => Ok(None),
        }
    }

    /// Insert or replace a persisted sink. The caller is expected to
    /// publish `sink.config.changed` on the bus after a successful
    /// write so the reload task rebuilds the registry without a
    /// restart.
    pub async fn alert_sink_upsert(
        &self,
        sink_id: &str,
        kind: &str,
        name: &str,
        config_json: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO alert_sinks (sink_id, kind, name, config_json, updated_at)
                  VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(sink_id) DO UPDATE SET
                  kind = excluded.kind,
                  name = excluded.name,
                  config_json = excluded.config_json,
                  updated_at = excluded.updated_at",
        )
        .bind(sink_id)
        .bind(kind)
        .bind(name)
        .bind(config_json)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete a persisted sink. Returns `true` iff a row was
    /// removed (so the API can 404 a delete of a sink that only
    /// exists as a file sink, which the db can't remove).
    pub async fn alert_sink_delete(&self, sink_id: &str) -> Result<bool, StoreError> {
        let res = sqlx::query("DELETE FROM alert_sinks WHERE sink_id = ?")
            .bind(sink_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

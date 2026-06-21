//! Phase 7.5 · Step 7.5.5 — fleet-managed provenance markers.
//!
//! Wraps the `fleet_managed_markers` table (migration
//! `0022_fleet_managed_markers.sql`). One row per fleet-settings
//! category records that the category is under cloud fleet management,
//! the scope it was applied from, and the canonical SHA-256 of the
//! effective payload last applied.
//!
//! Phase 7.5.5 changed the fleet-apply contract from overlay to
//! REPLACE (fleet overwrites local config); these markers are what let
//! the local admin UI badge a category as "Fleet-managed" and let the
//! edge short-circuit a no-op re-apply.

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{Store, StoreError};

/// One `fleet_managed_markers` row — a category currently under fleet
/// management on this core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetManagedMarker {
    /// Cloud `db_key` segment: `rules`, `text_prompts`,
    /// `visual_prompts`, `detector_config`, or `delivery_settings`.
    pub category: String,
    /// Apply scope type (`org`/`site`/`core`/`camera`), if recorded.
    pub scope_type: Option<String>,
    /// Apply scope id (uuid string), if recorded.
    pub scope_id: Option<String>,
    /// Lower-hex SHA-256 of the canonical JSON of the effective payload
    /// last applied, if recorded.
    pub effective_sha256: Option<String>,
    /// ISO-8601 timestamp of the last apply.
    pub applied_at: String,
}

impl Store {
    /// Tx-aware upsert of a fleet-managed marker. Called from the
    /// fleet-apply handler inside the same transaction that replaces
    /// the category's state, so the marker and the data move together.
    pub async fn fleet_marker_upsert_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        category: &str,
        scope_type: Option<&str>,
        scope_id: Option<&str>,
        effective_sha256: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO fleet_managed_markers
                 (category, scope_type, scope_id, effective_sha256, applied_at)
             VALUES (?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(category) DO UPDATE SET
                 scope_type       = excluded.scope_type,
                 scope_id         = excluded.scope_id,
                 effective_sha256 = excluded.effective_sha256,
                 applied_at       = excluded.applied_at",
        )
        .bind(category)
        .bind(scope_type)
        .bind(scope_id)
        .bind(effective_sha256)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Convenience wrapper that opens its own transaction. Used by the
    /// fleet-apply handler after the category's state has been replaced.
    pub async fn fleet_marker_upsert(
        &self,
        category: &str,
        scope_type: Option<&str>,
        scope_id: Option<&str>,
        effective_sha256: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        self.fleet_marker_upsert_tx(&mut tx, category, scope_type, scope_id, effective_sha256)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// List every fleet-managed category marker, ordered by category.
    /// Drives the local admin UI's "Fleet-managed" badges.
    pub async fn list_fleet_managed_markers(&self) -> Result<Vec<FleetManagedMarker>, StoreError> {
        let rows = sqlx::query(
            "SELECT category, scope_type, scope_id, effective_sha256, applied_at
             FROM fleet_managed_markers
             ORDER BY category",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| FleetManagedMarker {
                category: r.get(0),
                scope_type: r.get(1),
                scope_id: r.get(2),
                effective_sha256: r.get(3),
                applied_at: r.get(4),
            })
            .collect())
    }
}

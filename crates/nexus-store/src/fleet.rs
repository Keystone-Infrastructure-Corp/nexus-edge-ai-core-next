//! Phase 7.5 · Step 7.5.5 — fleet-managed provenance markers.
//!
//! Wraps the `fleet_managed_markers` table (migration
//! `0022_fleet_managed_markers.sql`, extended by
//! `0033_fleet_marker_apply_mode.sql`). One row per fleet-settings
//! category records that the category is under cloud fleet management,
//! the scope it was applied from, the mode it was applied with, the
//! identities the fleet currently owns, and the canonical SHA-256 of the
//! effective payload last applied.
//!
//! Step 7.5.5 changed the fleet-apply contract from overlay to REPLACE
//! (fleet overwrites local config). Step 7.5.11 made that conditional on
//! the cloud-supplied [`FleetApplyMode`]: `replace` keeps 7.5.5
//! semantics, while `merge` bounds deletion to `managed_keys` so purely
//! local entries survive a fleet push. These markers are what let the
//! local admin UI badge a category as "Fleet-managed", let the edge
//! short-circuit a no-op re-apply, and let `fleet_hash` restrict a
//! merge-managed category's digest to the fleet-owned subset.

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
    /// Apply mode of the last apply (`replace` / `merge`). `None` on rows
    /// written before migration `0032`, which are read as `replace`.
    pub mode: Option<String>,
    /// Per-category identities the last apply pushed (rule ids, prompt
    /// strings, prompt names, `<kind>:<name>` sink ids). `None` on rows
    /// written before migration `0032`.
    ///
    /// Under `merge` this is the exact set the fleet owns: the only keys
    /// an apply may delete, and the subset `fleet_hash` digests.
    pub managed_keys: Option<Vec<String>>,
    /// ISO-8601 timestamp of the last apply.
    pub applied_at: String,
}

/// Decode the stored `managed_keys` JSON array. A malformed value is read
/// as absent rather than failing the read — the column is provenance, and
/// a merge apply that cannot trust it simply deletes nothing.
fn decode_managed_keys(raw: Option<String>) -> Option<Vec<String>> {
    serde_json::from_str(raw.as_deref()?).ok()
}

/// The fields one fleet apply writes into a marker row. Grouped into a
/// struct so callers name each optional at the call site rather than
/// passing six positional `Option`s.
#[derive(Debug, Clone, Copy)]
pub struct FleetMarkerWrite<'a> {
    /// Cloud `db_key` segment the apply targeted.
    pub category: &'a str,
    /// Apply scope type (`org`/`site`/`core`/`camera`).
    pub scope_type: Option<&'a str>,
    /// Apply scope id (uuid string).
    pub scope_id: Option<&'a str>,
    /// Lower-hex SHA-256 of the canonical effective payload.
    pub effective_sha256: Option<&'a str>,
    /// `replace` or `merge`.
    pub mode: Option<&'a str>,
    /// Identities the fleet now owns for the category.
    pub managed_keys: Option<&'a [String]>,
}

impl Store {
    /// Tx-aware upsert of a fleet-managed marker. Called from the
    /// fleet-apply handler inside the same transaction that replaces
    /// the category's state, so the marker and the data move together.
    pub async fn fleet_marker_upsert_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        write: &FleetMarkerWrite<'_>,
    ) -> Result<(), StoreError> {
        let managed_keys_json = write.managed_keys.map(serde_json::to_string).transpose()?;
        sqlx::query(
            "INSERT INTO fleet_managed_markers
                 (category, scope_type, scope_id, effective_sha256, mode,
                  managed_keys, applied_at)
             VALUES (?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(category) DO UPDATE SET
                 scope_type       = excluded.scope_type,
                 scope_id         = excluded.scope_id,
                 effective_sha256 = excluded.effective_sha256,
                 mode             = excluded.mode,
                 managed_keys     = excluded.managed_keys,
                 applied_at       = excluded.applied_at",
        )
        .bind(write.category)
        .bind(write.scope_type)
        .bind(write.scope_id)
        .bind(write.effective_sha256)
        .bind(write.mode)
        .bind(managed_keys_json)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Convenience wrapper that opens its own transaction. Used by the
    /// fleet-apply handler after the category's state has been replaced.
    pub async fn fleet_marker_upsert(
        &self,
        write: &FleetMarkerWrite<'_>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        self.fleet_marker_upsert_tx(&mut tx, write).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Fetch one category's marker, if the category is fleet-managed.
    /// Drives the merge-mode delete bound in `fleet_apply` and the
    /// merge-mode digest restriction in `fleet_hash`.
    pub async fn fleet_marker_get(
        &self,
        category: &str,
    ) -> Result<Option<FleetManagedMarker>, StoreError> {
        let row = sqlx::query(
            "SELECT category, scope_type, scope_id, effective_sha256, mode,
                    managed_keys, applied_at
             FROM fleet_managed_markers
             WHERE category = ?",
        )
        .bind(category)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| FleetManagedMarker {
            category: r.get(0),
            scope_type: r.get(1),
            scope_id: r.get(2),
            effective_sha256: r.get(3),
            mode: r.get(4),
            managed_keys: decode_managed_keys(r.get(5)),
            applied_at: r.get(6),
        }))
    }

    /// List every fleet-managed category marker, ordered by category.
    /// Drives the local admin UI's "Fleet-managed" badges.
    pub async fn list_fleet_managed_markers(&self) -> Result<Vec<FleetManagedMarker>, StoreError> {
        let rows = sqlx::query(
            "SELECT category, scope_type, scope_id, effective_sha256, mode,
                    managed_keys, applied_at
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
                mode: r.get(4),
                managed_keys: decode_managed_keys(r.get(5)),
                applied_at: r.get(6),
            })
            .collect())
    }
}

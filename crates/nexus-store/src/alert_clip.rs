//! Alert-clip metadata store (M-Alert-Clip P2/P3).
//!
//! CRUD for the `alert_clips` table (migration 0026) and the
//! `events.alert_clip_id` link. An alert clip is the short, burned-in
//! MP4 that covers only the alert timeframe; it is built asynchronously
//! by the pipeline's `AlertClipBuilder` and reclaimed from hot storage
//! once every alert event that shares it has been delivered to all
//! sinks. See
//! `../../../nexus-cloud-console/docs/edge-core/M_ALERT_CLIP.md`.
//!
//! Lifecycle (`alert_clips.state`):
//! ```text
//!   building ──build ok──> ready ──all sinks delivered──> evicted
//!       └──build error──> failed
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use nexus_types::CameraId;

use crate::{Store, StoreError};

/// Primary key of an `alert_clips` row.
pub type AlertClipId = i64;

/// Args to open a fresh `alert_clips` row (inserted `state='building'`).
#[derive(Debug, Clone)]
pub struct NewAlertClip {
    pub camera_id: CameraId,
    /// Window start (`alert_ts - pre_secs`).
    pub started_at: DateTime<Utc>,
    /// Path relative to `clips.clips_dir` (same root as motion clips),
    /// under the `alert/` subdir. See `alert_clip::alert_clip_rel_path`
    /// in `nexus-pipeline`.
    pub path: String,
}

/// Hydrated `alert_clips` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertClipRow {
    pub id: AlertClipId,
    pub camera_id: CameraId,
    pub path: String,
    pub started_at: DateTime<Utc>,
    /// `NULL` until the MP4 is finalized (`state='ready'`).
    pub ready_at: Option<DateTime<Utc>>,
    pub state: String,
    pub duration_ms: i64,
    pub size_bytes: i64,
    /// 64-char lowercase hex of the finalized MP4, stamped when the
    /// builder marks the clip `ready`. `None` if hashing failed (the
    /// clip still serves local sinks but is skipped by the cold
    /// replicator, since the cloud `clip_replicated` envelope requires
    /// `sha256_hex`). Migration 0027.
    pub sha256: Option<String>,
    /// Active cold backend handle when the clip was replicated, or
    /// `None` while still hot-only. Migration 0027.
    pub cold_handle: Option<String>,
    /// Backend-relative path of the cold copy, or `None`. Migration 0027.
    pub cold_path: Option<String>,
    /// Wall-clock the clip was durably cold-replicated, or `None`.
    /// Gates the P6 evictor: a hot alert file is only reclaimed after
    /// this is set OR the grace window elapses. Migration 0027.
    pub cold_uploaded_at: Option<DateTime<Utc>>,
    /// Count of consecutive failed cold-upload attempts (diagnostics /
    /// retry gating). Migration 0027.
    pub cold_attempts: i64,
}

impl AlertClipRow {
    /// True once the MP4 is finalized and safe for a sink to read.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state == "ready"
    }
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| StoreError::Decode(format!("alert_clips timestamp `{s}`: {e}")))
}

fn alert_clip_row_from_row(row: sqlx::sqlite::SqliteRow) -> Result<AlertClipRow, StoreError> {
    let ready_at = row
        .get::<Option<String>, _>("ready_at")
        .map(|s| parse_dt(&s))
        .transpose()?;
    let cold_uploaded_at = row
        .get::<Option<String>, _>("cold_uploaded_at")
        .map(|s| parse_dt(&s))
        .transpose()?;
    Ok(AlertClipRow {
        id: row.get::<i64, _>("id"),
        camera_id: row.get::<i64, _>("camera_id") as CameraId,
        path: row.get::<String, _>("path"),
        started_at: parse_dt(&row.get::<String, _>("started_at"))?,
        ready_at,
        state: row.get::<String, _>("state"),
        duration_ms: row.get::<i64, _>("duration_ms"),
        size_bytes: row.get::<i64, _>("size_bytes"),
        sha256: row.get::<Option<String>, _>("sha256"),
        cold_handle: row.get::<Option<String>, _>("cold_handle"),
        cold_path: row.get::<Option<String>, _>("cold_path"),
        cold_uploaded_at,
        cold_attempts: row.get::<i64, _>("cold_attempts"),
    })
}

const ALERT_CLIP_COLUMNS: &str = "id, camera_id, path, started_at, ready_at, state, \
     duration_ms, size_bytes, sha256, cold_handle, cold_path, cold_uploaded_at, cold_attempts";

/// Args for [`Store::mark_alert_clip_cold_replicated`]. Mirrors
/// [`crate::ClipColdMark`] for motion clips.
#[derive(Debug, Clone)]
pub struct AlertClipColdMark {
    pub cold_handle: String,
    pub cold_path: String,
    pub cold_uploaded_at: DateTime<Utc>,
}

impl Store {
    /// Insert a fresh `building` alert clip and return its id. The
    /// builder later stamps it ready (or failed); alert events fired
    /// in the same motion burst link to this id via
    /// [`Self::link_event_alert_clip`].
    pub async fn insert_alert_clip(&self, new: &NewAlertClip) -> Result<AlertClipId, StoreError> {
        let row = sqlx::query(
            "INSERT INTO alert_clips (camera_id, started_at, path, state)
             VALUES (?, ?, ?, 'building')
             RETURNING id",
        )
        .bind(new.camera_id)
        .bind(new.started_at.to_rfc3339())
        .bind(&new.path)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>(0))
    }

    /// Stamp a built alert clip `ready`: record its final duration /
    /// size, `sha256`, and `ready_at = now`. Only transitions a
    /// `building` row, so a late/duplicate call after eviction is a
    /// no-op error rather than resurrecting a reclaimed clip. `sha256`
    /// is `None` when hashing the finalized MP4 failed — the clip still
    /// serves local sinks, but the cold replicator skips NULL-sha256
    /// rows (the cloud `clip_replicated` envelope requires `sha256_hex`).
    pub async fn mark_alert_clip_ready(
        &self,
        id: AlertClipId,
        duration_ms: i64,
        size_bytes: i64,
        sha256: Option<&str>,
    ) -> Result<(), StoreError> {
        let res = sqlx::query(
            "UPDATE alert_clips
                SET state = 'ready', duration_ms = ?, size_bytes = ?, sha256 = ?, ready_at = ?
              WHERE id = ? AND state = 'building'",
        )
        .bind(duration_ms)
        .bind(size_bytes)
        .bind(sha256)
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!(
                "alert_clip id={id} not in 'building' state"
            )));
        }
        Ok(())
    }

    /// Mark a `building` alert clip `failed` (build/encode error). The
    /// dispatcher then delivers the alarm clip-less once the build
    /// timeout elapses.
    pub async fn mark_alert_clip_failed(&self, id: AlertClipId) -> Result<(), StoreError> {
        sqlx::query("UPDATE alert_clips SET state = 'failed' WHERE id = ? AND state = 'building'")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Mark a `ready` alert clip `evicted` after its hot file has been
    /// reclaimed. Keeps the row (audit trail: this alert HAD a clip)
    /// but flips state so the dispatcher never re-attaches it.
    pub async fn mark_alert_clip_evicted(&self, id: AlertClipId) -> Result<(), StoreError> {
        sqlx::query("UPDATE alert_clips SET state = 'evicted' WHERE id = ? AND state = 'ready'")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Stamp the cold pointer after a successful cold-replication PUT
    /// (M-Alert-Clip cloud delivery). Records where the clip landed so
    /// the P6 evictor can reclaim the hot file. Mirrors
    /// [`Store::mark_cold_replicated`] for motion clips.
    pub async fn mark_alert_clip_cold_replicated(
        &self,
        id: AlertClipId,
        mark: &AlertClipColdMark,
    ) -> Result<(), StoreError> {
        let res = sqlx::query(
            "UPDATE alert_clips
                SET cold_handle = ?, cold_path = ?, cold_uploaded_at = ?
              WHERE id = ?",
        )
        .bind(&mark.cold_handle)
        .bind(&mark.cold_path)
        .bind(mark.cold_uploaded_at.to_rfc3339())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("alert_clip id={id}")));
        }
        Ok(())
    }

    /// Record a failed cold-upload attempt (increment `cold_attempts`,
    /// stamp the error + attempt time). The `cold_last_attempt_at`
    /// gate in [`Self::alert_clips_pending_cold_upload`] then holds the
    /// clip out of the working set for `retry_after`, so a persistently
    /// unreachable backend is retried at a bounded cadence rather than
    /// every replicator tick. Unlike motion clips there is no
    /// quarantine flag: the evictor's grace window is the ultimate
    /// backstop that removes a stuck alert clip from the hot tier.
    pub async fn record_alert_clip_cold_failure(
        &self,
        id: AlertClipId,
        now: DateTime<Utc>,
        error: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE alert_clips
                SET cold_attempts = cold_attempts + 1,
                    cold_last_attempt_at = ?,
                    cold_last_error = ?
              WHERE id = ?",
        )
        .bind(now.to_rfc3339())
        .bind(error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Alert clips eligible for cold replication: `ready`, hashed, not
    /// yet cold, and past the per-clip retry gate. `floor` mirrors the
    /// motion-clip enrollment floor (pre-enrollment clips stay
    /// local-only). `retry_cutoff` is `now - retry_after`: a clip whose
    /// last attempt is newer than this is held back so a failing
    /// backend isn't hammered every tick. Oldest-first, bounded by
    /// `limit`.
    pub async fn alert_clips_pending_cold_upload(
        &self,
        limit: i64,
        floor: Option<DateTime<Utc>>,
        retry_cutoff: DateTime<Utc>,
    ) -> Result<Vec<AlertClipRow>, StoreError> {
        let floor_str = floor.map(|f| f.to_rfc3339());
        let rows = sqlx::query(&format!(
            "SELECT {ALERT_CLIP_COLUMNS} FROM alert_clips
              WHERE state = 'ready'
                AND cold_uploaded_at IS NULL
                AND sha256 IS NOT NULL
                AND (cold_last_attempt_at IS NULL OR cold_last_attempt_at <= ?)
                AND (? IS NULL OR started_at >= ?)
              ORDER BY ready_at ASC
              LIMIT ?"
        ))
        .bind(retry_cutoff.to_rfc3339())
        .bind(&floor_str)
        .bind(&floor_str)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(alert_clip_row_from_row).collect()
    }

    /// Fetch one alert clip by id.
    pub async fn get_alert_clip(
        &self,
        id: AlertClipId,
    ) -> Result<Option<AlertClipRow>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT {ALERT_CLIP_COLUMNS} FROM alert_clips WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(alert_clip_row_from_row).transpose()
    }

    /// Resolve the alert clip linked to an event, if any. The sink
    /// dispatcher calls this for `wants_clip()` sinks instead of the
    /// motion-clip lookup: an alert clip is ready within ~`post_secs`,
    /// not after the up-to-5-minute motion clip closes. Returns `None`
    /// when the event has no linked alert clip (feature disabled, or
    /// the row was hard-deleted).
    pub async fn get_event_alert_clip(
        &self,
        event_id: &str,
    ) -> Result<Option<AlertClipRow>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT {} FROM alert_clips ac
               JOIN events e ON e.alert_clip_id = ac.id
              WHERE e.event_id = ?",
            ALERT_CLIP_COLUMNS
                .split(", ")
                .map(|c| format!("ac.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .bind(event_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(alert_clip_row_from_row).transpose()
    }

    /// Link an alert event to its burst alert clip
    /// (`events.alert_clip_id = ?`). Called for every event in a motion
    /// burst so they share one clip (burst coalescing).
    pub async fn link_event_alert_clip(
        &self,
        event_id: &str,
        alert_clip_id: AlertClipId,
    ) -> Result<(), StoreError> {
        let res = sqlx::query("UPDATE events SET alert_clip_id = ? WHERE event_id = ?")
            .bind(alert_clip_id)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("event_id={event_id}")));
        }
        Ok(())
    }

    /// Count the non-terminal (`pending` / `failed`) outbox deliveries
    /// still outstanding across **every** event linked to this alert
    /// clip. Zero means all sinks for all sharing events reached a
    /// terminal outcome, so the hot file can be reclaimed (P6). A clip
    /// with no linked outbox rows at all also returns 0.
    pub async fn alert_clip_pending_deliveries(
        &self,
        alert_clip_id: AlertClipId,
    ) -> Result<i64, StoreError> {
        let row = sqlx::query(
            "SELECT COUNT(*) FROM alert_sink_outbox o
               JOIN events e ON e.event_id = o.event_id
              WHERE e.alert_clip_id = ? AND o.status IN ('pending', 'failed')",
        )
        .bind(alert_clip_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>(0))
    }

    /// Alert clips that are `ready` with no outstanding deliveries AND
    /// either already cold-replicated or past the cold grace window —
    /// the P6 evictor unlinks their hot files then calls
    /// [`Self::mark_alert_clip_evicted`]. Bounded by `limit` so one
    /// sweep stays cheap.
    ///
    /// `cold_grace_cutoff` is `now - cold_grace`: a clip that has not
    /// cold-replicated is held in the hot tier until this cutoff so an
    /// enrolled core has time to ship it to the cloud. Past the cutoff
    /// (un-enrolled, cold disabled, or a persistently-down backend) the
    /// hot file is reclaimed anyway — the local experience never
    /// depends on cloud reachability (fail-open).
    pub async fn alert_clips_evictable(
        &self,
        limit: i64,
        cold_grace_cutoff: DateTime<Utc>,
    ) -> Result<Vec<AlertClipRow>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {ALERT_CLIP_COLUMNS} FROM alert_clips ac
              WHERE ac.state = 'ready'
                AND (ac.cold_uploaded_at IS NOT NULL OR ac.ready_at <= ?)
                AND NOT EXISTS (
                    SELECT 1 FROM alert_sink_outbox o
                      JOIN events e ON e.event_id = o.event_id
                     WHERE e.alert_clip_id = ac.id
                       AND o.status IN ('pending', 'failed')
                )
              ORDER BY ac.ready_at ASC
              LIMIT ?"
        ))
        .bind(cold_grace_cutoff.to_rfc3339())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(alert_clip_row_from_row).collect()
    }
}

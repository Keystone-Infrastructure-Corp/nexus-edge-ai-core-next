//! Motion timeline & clip recording — the M2.1 storage surface.
//!
//! Two tables back this module:
//!
//! * `motion_clips` — one row per recorded clip, identified by an
//!   integer id and a path relative to `RuntimeConfig.clips.clips_dir`.
//! * `motion_events` — many-rows-per-clip; the per-track lifecycle
//!   timeline (`born` / `updated` / `died`) keyed by `(camera_id,
//!   captured_at)`.
//!
//! All FKs are `ON DELETE CASCADE` (see `0002_motion_clips.sql`), so
//! eviction is a single `DELETE motion_clips WHERE id = ?`. The
//! `cascade_delete_clip_metadata` helper here is the explicit form
//! kept for the FK-disabled fallback path called out in
//! `docs/M2_STORAGE.md`.

use chrono::{DateTime, Utc};
use nexus_types::{BBox, CameraId, TrackId};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{Store, StoreError};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

pub type ClipId = i64;
pub type MotionEventId = i64;

/// Lifecycle event kind written into `motion_events.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotionEventKind {
    Born,
    Updated,
    Died,
}

impl MotionEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MotionEventKind::Born => "born",
            MotionEventKind::Updated => "updated",
            MotionEventKind::Died => "died",
        }
    }

    pub fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "born" => Ok(Self::Born),
            "updated" => Ok(Self::Updated),
            "died" => Ok(Self::Died),
            other => Err(StoreError::NotFound(format!("motion_event.kind={other}"))),
        }
    }
}

/// Args for opening a new `motion_clips` row.
#[derive(Debug, Clone)]
pub struct NewClip {
    pub camera_id: CameraId,
    pub started_at: DateTime<Utc>,
    pub path: String,
    pub codec: String,
    pub container: String,
    pub backend_id: String,
}

/// Args for closing an in-progress `motion_clips` row.
#[derive(Debug, Clone)]
pub struct ClipClose {
    pub ended_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub size_bytes: i64,
}

/// Hydrated `motion_clips` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipRow {
    pub id: ClipId,
    pub camera_id: CameraId,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub path: String,
    pub duration_ms: i64,
    pub size_bytes: i64,
    pub codec: String,
    pub container: String,
    pub backend_id: String,
}

/// Args for writing a single `motion_events` row.
#[derive(Debug, Clone)]
pub struct NewMotionEvent {
    pub camera_id: CameraId,
    pub clip_id: ClipId,
    pub track_id: TrackId,
    pub kind: MotionEventKind,
    pub captured_at: DateTime<Utc>,
    pub bbox: BBox,
    pub label: String,
    pub confidence: f32,
    /// Free-form annotator output (motion.speed_class, group.size, etc).
    pub attributes_json: String,
}

/// Hydrated `motion_events` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotionEventRow {
    pub id: MotionEventId,
    pub camera_id: CameraId,
    pub clip_id: ClipId,
    pub track_id: TrackId,
    pub kind: MotionEventKind,
    pub captured_at: DateTime<Utc>,
    pub bbox: BBox,
    pub label: String,
    pub confidence: f32,
    pub attributes_json: String,
}

// ---------------------------------------------------------------------------
// Store impl block — one new method per repo operation.
// ---------------------------------------------------------------------------

impl Store {
    /// Insert a new clip row in the `recording` state. `ended_at` /
    /// `duration_ms` / `size_bytes` are populated by `close_clip` when the
    /// recorder finalises the file.
    pub async fn open_clip(&self, new: &NewClip) -> Result<ClipId, StoreError> {
        let row = sqlx::query(
            "INSERT INTO motion_clips
                 (camera_id, started_at, path, codec, container, backend_id)
             VALUES (?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(new.camera_id)
        .bind(new.started_at.to_rfc3339())
        .bind(&new.path)
        .bind(&new.codec)
        .bind(&new.container)
        .bind(&new.backend_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>(0))
    }

    /// Stamp the close metadata on an in-progress clip.
    pub async fn close_clip(&self, clip_id: ClipId, close: &ClipClose) -> Result<(), StoreError> {
        let res = sqlx::query(
            "UPDATE motion_clips
                SET ended_at = ?, duration_ms = ?, size_bytes = ?
              WHERE id = ?",
        )
        .bind(close.ended_at.to_rfc3339())
        .bind(close.duration_ms)
        .bind(close.size_bytes)
        .bind(clip_id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("motion_clip id={clip_id}")));
        }
        Ok(())
    }

    pub async fn get_clip(&self, clip_id: ClipId) -> Result<Option<ClipRow>, StoreError> {
        let row = sqlx::query(
            "SELECT id, camera_id, started_at, ended_at, path, duration_ms,
                    size_bytes, codec, container, backend_id
               FROM motion_clips
              WHERE id = ?",
        )
        .bind(clip_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(clip_row_from_row).transpose()
    }

    /// Oldest (by `started_at`) finished or in-progress clip for a camera.
    /// Used by the per-camera-fair eviction round.
    pub async fn oldest_clip_for_camera(
        &self,
        camera_id: CameraId,
    ) -> Result<Option<ClipRow>, StoreError> {
        let row = sqlx::query(
            "SELECT id, camera_id, started_at, ended_at, path, duration_ms,
                    size_bytes, codec, container, backend_id
               FROM motion_clips
              WHERE camera_id = ?
              ORDER BY started_at ASC
              LIMIT 1",
        )
        .bind(camera_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(clip_row_from_row).transpose()
    }

    /// Distinct camera ids that currently own at least one clip. The
    /// eviction sampler iterates this set in `id` order so the round
    /// is deterministic across processes.
    pub async fn cameras_with_clips(&self) -> Result<Vec<CameraId>, StoreError> {
        let rows = sqlx::query("SELECT DISTINCT camera_id FROM motion_clips ORDER BY camera_id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|r| r.get::<i64, _>(0)).collect())
    }

    /// Single-DELETE eviction. With the `ON DELETE CASCADE` FKs in
    /// `0002_motion_clips.sql`, this also removes every linked
    /// `motion_events` row and NULL-outs `events.clip_id`.
    /// File unlink is the caller's responsibility (see
    /// `docs/M2_STORAGE.md` crash-safety section).
    pub async fn cascade_delete_clip_metadata(&self, clip_id: ClipId) -> Result<(), StoreError> {
        let res = sqlx::query("DELETE FROM motion_clips WHERE id = ?")
            .bind(clip_id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("motion_clip id={clip_id}")));
        }
        Ok(())
    }

    /// Clip rows older than the supplied cutoff, oldest-first. Used by
    /// the daily retention sweeper.
    pub async fn clips_older_than(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<ClipRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, camera_id, started_at, ended_at, path, duration_ms,
                    size_bytes, codec, container, backend_id
               FROM motion_clips
              WHERE started_at < ?
              ORDER BY started_at ASC
              LIMIT ?",
        )
        .bind(cutoff.to_rfc3339())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(clip_row_from_row).collect()
    }

    /// Insert a single motion-event row. The recorder owns the
    /// per-camera batching policy; this method writes one row per call
    /// so callers can pick their own transaction granularity.
    pub async fn insert_motion_event(
        &self,
        ev: &NewMotionEvent,
    ) -> Result<MotionEventId, StoreError> {
        let row = sqlx::query(
            "INSERT INTO motion_events
                 (camera_id, clip_id, track_id, kind, captured_at,
                  bbox_x1, bbox_y1, bbox_x2, bbox_y2,
                  label, confidence, attributes_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(ev.camera_id)
        .bind(ev.clip_id)
        .bind(ev.track_id as i64)
        .bind(ev.kind.as_str())
        .bind(ev.captured_at.to_rfc3339())
        .bind(ev.bbox.x1 as f64)
        .bind(ev.bbox.y1 as f64)
        .bind(ev.bbox.x2 as f64)
        .bind(ev.bbox.y2 as f64)
        .bind(&ev.label)
        .bind(ev.confidence as f64)
        .bind(&ev.attributes_json)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>(0))
    }

    /// Per-camera timeline lookup. Powers the M2.1 API endpoint
    /// `GET /api/v1/cameras/:id/motion?from=&to=`.
    pub async fn list_motion_events_for_camera(
        &self,
        camera_id: CameraId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<MotionEventRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, camera_id, clip_id, track_id, kind, captured_at,
                    bbox_x1, bbox_y1, bbox_x2, bbox_y2,
                    label, confidence, attributes_json
               FROM motion_events
              WHERE camera_id = ? AND captured_at BETWEEN ? AND ?
              ORDER BY captured_at ASC
              LIMIT ?",
        )
        .bind(camera_id)
        .bind(from.to_rfc3339())
        .bind(to.to_rfc3339())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(motion_event_row_from_row).collect()
    }

    /// Stamp `events.clip_id` for an alert. Called by the supervisor in
    /// the same per-frame transaction as `record_event` once the
    /// recorder hands back the open clip handle for the camera.
    pub async fn link_event_to_clip(
        &self,
        event_id: &str,
        clip_id: ClipId,
    ) -> Result<(), StoreError> {
        let res = sqlx::query("UPDATE events SET clip_id = ? WHERE event_id = ?")
            .bind(clip_id)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("event_id={event_id}")));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Row decoders
// ---------------------------------------------------------------------------

fn clip_row_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ClipRow, StoreError> {
    let started_at = parse_ts(row.get::<String, _>(2).as_str())?;
    let ended_at = row
        .try_get::<Option<String>, _>(3)?
        .map(|s| parse_ts(&s))
        .transpose()?;
    Ok(ClipRow {
        id: row.get::<i64, _>(0),
        camera_id: row.get::<i64, _>(1),
        started_at,
        ended_at,
        path: row.get::<String, _>(4),
        duration_ms: row.get::<i64, _>(5),
        size_bytes: row.get::<i64, _>(6),
        codec: row.get::<String, _>(7),
        container: row.get::<String, _>(8),
        backend_id: row.get::<String, _>(9),
    })
}

fn motion_event_row_from_row(row: sqlx::sqlite::SqliteRow) -> Result<MotionEventRow, StoreError> {
    let captured_at = parse_ts(row.get::<String, _>(5).as_str())?;
    let kind = MotionEventKind::parse(row.get::<String, _>(4).as_str())?;
    let bbox = BBox {
        x1: row.get::<f64, _>(6) as f32,
        y1: row.get::<f64, _>(7) as f32,
        x2: row.get::<f64, _>(8) as f32,
        y2: row.get::<f64, _>(9) as f32,
    };
    Ok(MotionEventRow {
        id: row.get::<i64, _>(0),
        camera_id: row.get::<i64, _>(1),
        clip_id: row.get::<i64, _>(2),
        track_id: row.get::<i64, _>(3) as u64,
        kind,
        captured_at,
        bbox,
        label: row.get::<String, _>(10),
        confidence: row.get::<f64, _>(11) as f32,
        attributes_json: row.get::<String, _>(12),
    })
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| StoreError::NotFound(format!("bad timestamp '{s}': {e}")))
}

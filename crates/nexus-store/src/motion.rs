//! Motion timeline & clip recording.
//!
//! Two tables back this module:
//!
//! * `motion_clips` — one row per recorded clip. M2.2 introduced the
//!   *dual-pointer* shape: every row tracks an optional **hot**
//!   pointer (`hot_handle` + `hot_path`, where `hot_handle` always
//!   resolves to a local-disk-backed [`storage_backends`] entry like
//!   `'local'` or `'usb-vault'`) and an optional **cold** pointer
//!   (`cold_handle` + `cold_path` + `cold_uploaded_at`, populated by
//!   the cold-mirror replicator). The schema CHECK guarantees at
//!   least one of the two is non-NULL — a clip row never points at
//!   nothing.
//! * `motion_events` — many-rows-per-clip; the per-track lifecycle
//!   timeline (`born` / `updated` / `died`) keyed by `(camera_id,
//!   captured_at)`.
//!
//! All FKs to `motion_clips(id)` are `ON DELETE CASCADE` (see
//! `0002_motion_clips.sql` + `0003_events_clip_cascade.sql`), so
//! hard-eviction is a single `DELETE motion_clips WHERE id = ?`.
//! Hard-eviction is only legal for clips that were NEVER replicated
//! to cold (M2.2 invariant) — soft-eviction
//! ([`Store::clear_hot_pointer`]) is the right call when a cold copy
//! exists and we just need the bytes off hot disk.
//!
//! The FKs from `motion_clips.hot_handle` and `motion_clips.cold_handle`
//! to `storage_backends.handle` are `ON DELETE RESTRICT` — operators
//! cannot delete a backend while any clip still references it. The
//! application's [`Store::delete_storage_backend`] helper enforces the
//! same rule explicitly so the error surfaces as `DeleteBackendError`
//! instead of a raw `sqlx::Error`.

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
///
/// The recorder is **unaware of cold storage**, so this struct only
/// carries the hot pointer. The cold-mirror replicator stamps the
/// cold pointer later via [`Store::mark_cold_replicated`].
#[derive(Debug, Clone)]
pub struct NewClip {
    pub camera_id: CameraId,
    pub started_at: DateTime<Utc>,
    /// Path relative to `RuntimeConfig.clips.clips_dir`. Required —
    /// every newly-opened clip is hot.
    pub hot_path: String,
    /// `storage_backends.handle` for the hot target. Defaults to
    /// `'local'` for the M2.1 single-disk recorder; multi-disk + USB
    /// recorders may pass `'usb-vault'` etc. once they exist.
    pub hot_handle: String,
    pub codec: String,
    pub container: String,
}

/// Args for closing an in-progress `motion_clips` row.
#[derive(Debug, Clone)]
pub struct ClipClose {
    pub ended_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub size_bytes: i64,
    /// Optional hot-pointer rename. M2.1 spec requires the on-disk
    /// filename to include `duration_ms` (e.g.
    /// `{start_unix_ms}_{duration_ms}.mp4`) which is only known at
    /// close time. The recorder renames the in-flight file then
    /// sets this so the DB pointer stays valid. `None` means "leave
    /// `hot_path` unchanged" (Stage A stub recorder when no rename
    /// is needed).
    pub hot_path: Option<String>,
    /// Streaming SHA-256 computed by the recorder over the final
    /// clip bytes. The cold replicator gates upload on this being
    /// `Some` — legacy/unhashed clips are skipped until an operator
    /// opts into a one-shot rehash sweep (Phase 2 work). `None`
    /// here leaves the column unchanged (stays NULL on a fresh row,
    /// keeps any prior value on re-close).
    pub sha256: Option<String>,
}

/// Hydrated `motion_clips` row.
///
/// `hot_*` columns are non-NULL while a clip is readable from a hot
/// backend; soft-eviction clears them while leaving the row intact
/// (with `cold_*` pointing at the surviving copy). `cold_*` columns
/// are non-NULL once the cold replicator finishes uploading and stay
/// set forever (replicator never deletes; hard-eviction is forbidden
/// on cold-replicated clips).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipRow {
    pub id: ClipId,
    pub camera_id: CameraId,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_ms: i64,
    pub size_bytes: i64,
    pub codec: String,
    pub container: String,
    pub hot_handle: Option<String>,
    pub hot_path: Option<String>,
    pub cold_handle: Option<String>,
    pub cold_path: Option<String>,
    pub cold_uploaded_at: Option<DateTime<Utc>>,
    pub sha256: Option<String>,
    /// Phase 2 Step 2.1c — cold-replicator priority lane. `0`
    /// (default) keeps the existing FIFO behaviour; the cloud
    /// Expedite RPC handler bumps this to `1` so the clip jumps to
    /// the head of the next replicator tick.
    pub priority: i64,
}

/// Args for [`Store::mark_cold_replicated`]. All three fields are
/// required because the schema CHECK enforces them as a unit.
#[derive(Debug, Clone)]
pub struct ClipColdMark {
    pub cold_handle: String,
    pub cold_path: String,
    pub cold_uploaded_at: DateTime<Utc>,
}

/// Hydrated `storage_backends` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageBackendRow {
    pub handle: String,
    pub kind: String,
    /// Opaque to the schema; each backend kind has its own settings
    /// shape (e.g. `{"root":"/mnt/lan-archive"}` for kind `'lan'`).
    pub config_json: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Hydrated singleton `storage_cold_replica` row. `backend_handle`
/// is `None` when cold replication is disabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdReplicaRow {
    pub backend_handle: Option<String>,
    pub throttle_bps: i64,
    pub updated_at: DateTime<Utc>,
}

/// Aggregate counts over `motion_clips` from the cold-mirror
/// perspective. Computed in a single SQL pass by
/// [`Store::cold_replica_stats`] so the API handler can render the
/// admin "replica" card + the storage tab "N clips cold-only"
/// subtitle without four round-trips.
///
/// Properties:
///
/// * `pending_count` — closed, hashed, hot-resident clips that
///   have not yet been uploaded to cold. The replicator drains
///   these on every tick; a persistent non-zero number with a
///   healthy backend is the signal to widen `BATCH_SIZE` or relax
///   the throttle.
/// * `replicated_count` — clips with a cold pointer set. Includes
///   both still-hot replicated clips and soft-evicted (cold-only)
///   clips. Strictly monotonic for a given cold backend (the
///   replicator never deletes from cold).
/// * `cold_only_count` — clips with `hot_path IS NULL AND
///   cold_handle IS NOT NULL`. These are the soft-evicted clips
///   served by the M2.2 Phase 4 cold-read cache. Drives the
///   storage tab subtitle and the playback latency expectation
///   (first request rehydrates from cold).
/// * `lifetime_uploaded_bytes` — `SUM(size_bytes)` over all
///   clips with a cold pointer. Lifetime-cumulative; matches the
///   bytes-on-cold-tier baseline because the replicator never
///   deletes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdReplicaStats {
    pub pending_count: i64,
    pub replicated_count: i64,
    pub cold_only_count: i64,
    pub lifetime_uploaded_bytes: i64,
}

/// Failure modes for [`Store::delete_storage_backend`]. The store
/// rejects deletion when removing the row would either violate the
/// `ON DELETE RESTRICT` FK on `motion_clips` or strand the
/// currently-active cold replica policy.
#[derive(Debug, thiserror::Error)]
pub enum DeleteBackendError {
    /// Backend is referenced by at least one `motion_clips` row
    /// (via `hot_handle` or `cold_handle`). The operator must
    /// migrate or evict those clips first.
    #[error("backend handle '{0}' is referenced by motion_clips and cannot be deleted")]
    InUse(String),
    /// Backend is the active cold replica. The operator must clear
    /// `storage_cold_replica` (point at NULL or another backend)
    /// first.
    #[error("backend handle '{0}' is the active cold replica; clear it first")]
    ActiveCold(String),
    /// The implicit `'local'` backend is owned by the engine and
    /// cannot be removed by an operator.
    #[error("backend handle '{0}' is the implicit local backend and cannot be deleted")]
    Local(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Per-camera clip-occupancy snapshot for the storage health endpoint.
///
/// Returned by [`Store::per_camera_clip_stats`]. Cameras with zero
/// clips are NOT in the result; the API handler may zero-fill if it
/// wants a row per known camera.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerCameraClipStats {
    pub camera_id: CameraId,
    /// Number of `motion_clips` rows owned by this camera.
    pub clip_count: i64,
    /// `SUM(size_bytes)` across those rows. In-flight clips
    /// (size_bytes still 0) contribute 0 — eventually correct
    /// once `close_clip` stamps the finalised file size.
    pub bytes: i64,
    /// `MIN(started_at)` across those rows. Drives "retention
    /// horizon for camera X" in the UI.
    pub oldest_started_at: DateTime<Utc>,
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

/// One time-bucket aggregation of motion activity for a camera.
/// Returned by [`Store::list_motion_histogram_for_camera`] and
/// surfaced to the UI as the per-hour Timeline grid bars.
///
/// Buckets are sparse — empty intervals are NOT included; the UI
/// fills zeros client-side. `bucket_start` is the inclusive lower
/// edge of the bucket, computed as `from + bucket_seconds * bucket`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotionHistogramBucket {
    /// Zero-based bucket index counted from `from`. The UI uses
    /// this for grid layout without re-deriving timestamps.
    pub bucket: i64,
    /// Inclusive start time of this bucket
    /// (`from + bucket_seconds * bucket`).
    pub bucket_start: DateTime<Utc>,
    /// Number of `motion_events` rows whose `captured_at` falls in
    /// the bucket. Drives the bar height.
    pub event_count: i64,
    /// Number of distinct `clip_id` values referenced by those
    /// events. Drives the secondary clip-count badge on hover.
    pub clip_count: i64,
}

// ---------------------------------------------------------------------------
// Store impl block — one new method per repo operation.
// ---------------------------------------------------------------------------

impl Store {
    /// Insert a new clip row in the `recording` state. The new row is
    /// hot-only — the cold pointer columns are NULL and the
    /// schema-level CHECK is satisfied by `hot_handle IS NOT NULL`.
    /// `ended_at` / `duration_ms` / `size_bytes` / `sha256` are
    /// populated by [`Self::close_clip`] when the recorder finalises
    /// the file.
    pub async fn open_clip(&self, new: &NewClip) -> Result<ClipId, StoreError> {
        let row = sqlx::query(
            "INSERT INTO motion_clips
                 (camera_id, started_at, codec, container, hot_handle, hot_path)
             VALUES (?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(new.camera_id)
        .bind(new.started_at.to_rfc3339())
        .bind(&new.codec)
        .bind(&new.container)
        .bind(&new.hot_handle)
        .bind(&new.hot_path)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>(0))
    }

    /// Stamp the close metadata on an in-progress clip. Optionally
    /// updates `hot_path` too — set when the recorder renamed the
    /// file from its in-flight name to the final
    /// `{start_ms}_{dur_ms}.mp4` shape per M2.1 spec. `sha256`, when
    /// `Some`, stamps the streaming hash the recorder computed over
    /// the final clip bytes; the cold replicator gates upload on
    /// this being non-NULL.
    pub async fn close_clip(&self, clip_id: ClipId, close: &ClipClose) -> Result<(), StoreError> {
        // The four bind parameters are always the same prefix
        // (ended_at, duration_ms, size_bytes, clip_id) regardless of
        // which optional columns we patch — keep the dynamic
        // SET-list short and bind in order.
        let res = match (&close.hot_path, &close.sha256) {
            (Some(hot_path), Some(sha)) => {
                sqlx::query(
                    "UPDATE motion_clips
                        SET ended_at = ?, duration_ms = ?, size_bytes = ?,
                            hot_path = ?, sha256 = ?
                      WHERE id = ?",
                )
                .bind(close.ended_at.to_rfc3339())
                .bind(close.duration_ms)
                .bind(close.size_bytes)
                .bind(hot_path)
                .bind(sha)
                .bind(clip_id)
                .execute(&self.pool)
                .await?
            }
            (Some(hot_path), None) => {
                sqlx::query(
                    "UPDATE motion_clips
                        SET ended_at = ?, duration_ms = ?, size_bytes = ?, hot_path = ?
                      WHERE id = ?",
                )
                .bind(close.ended_at.to_rfc3339())
                .bind(close.duration_ms)
                .bind(close.size_bytes)
                .bind(hot_path)
                .bind(clip_id)
                .execute(&self.pool)
                .await?
            }
            (None, Some(sha)) => {
                sqlx::query(
                    "UPDATE motion_clips
                        SET ended_at = ?, duration_ms = ?, size_bytes = ?, sha256 = ?
                      WHERE id = ?",
                )
                .bind(close.ended_at.to_rfc3339())
                .bind(close.duration_ms)
                .bind(close.size_bytes)
                .bind(sha)
                .bind(clip_id)
                .execute(&self.pool)
                .await?
            }
            (None, None) => {
                sqlx::query(
                    "UPDATE motion_clips
                        SET ended_at = ?, duration_ms = ?, size_bytes = ?
                      WHERE id = ?",
                )
                .bind(close.ended_at.to_rfc3339())
                .bind(close.duration_ms)
                .bind(close.size_bytes)
                .bind(clip_id)
                .execute(&self.pool)
                .await?
            }
        };
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("motion_clip id={clip_id}")));
        }
        Ok(())
    }

    pub async fn get_clip(&self, clip_id: ClipId) -> Result<Option<ClipRow>, StoreError> {
        let row = sqlx::query(CLIP_SELECT_COLUMNS_WHERE_ID)
            .bind(clip_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(clip_row_from_row).transpose()
    }

    /// Oldest (by `started_at`) finished or in-progress clip for a
    /// camera. M2.1 used this for round-robin oldest-first
    /// eviction. M2.2 splits the call into
    /// [`Self::find_soft_evict_candidate`] (cold-replicated, drop
    /// hot only) and [`Self::find_hard_evict_candidate`]
    /// (no-cold-yet, full cascade-delete) — but this method is kept
    /// for the legacy retention sweeper which doesn't care about
    /// the soft/hard distinction (it deletes everything past the
    /// retention horizon regardless of cold state).
    pub async fn oldest_clip_for_camera(
        &self,
        camera_id: CameraId,
    ) -> Result<Option<ClipRow>, StoreError> {
        let row = sqlx::query(&format!(
            "{CLIP_SELECT_COLUMNS_BASE}
              WHERE camera_id = ?
              ORDER BY started_at ASC
              LIMIT 1"
        ))
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

    /// Per-camera clip occupancy snapshot. One row per camera that
    /// currently owns at least one clip. Drives the
    /// `per_camera[]` array of `GET /api/v1/storage/local`.
    pub async fn per_camera_clip_stats(&self) -> Result<Vec<PerCameraClipStats>, StoreError> {
        let rows = sqlx::query(
            "SELECT camera_id,
                    COUNT(*)                AS clip_count,
                    COALESCE(SUM(size_bytes), 0) AS bytes,
                    MIN(started_at)         AS oldest_started_at
               FROM motion_clips
              GROUP BY camera_id
              ORDER BY camera_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let oldest_raw: String = r.get("oldest_started_at");
                let oldest_started_at = DateTime::parse_from_rfc3339(&oldest_raw)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                Ok(PerCameraClipStats {
                    camera_id: r.get::<i64, _>("camera_id"),
                    clip_count: r.get::<i64, _>("clip_count"),
                    bytes: r.get::<i64, _>("bytes"),
                    oldest_started_at,
                })
            })
            .collect()
    }

    /// Every `motion_clips.hot_path` currently in the DB whose
    /// `hot_handle` is the implicit `'local'` backend. Used by the
    /// orphan-file scanner to compute "files on disk under
    /// `clips_dir` that have no matching row" by set difference.
    /// Soft-evicted clips (no hot pointer) and clips whose hot
    /// backend is not `'local'` (e.g. USB-vault) are excluded — the
    /// scanner only owns the local `clips_dir` filesystem.
    pub async fn known_local_clip_paths(&self) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query(
            "SELECT hot_path FROM motion_clips
              WHERE hot_handle = 'local' AND hot_path IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>(0)).collect())
    }

    /// Single-DELETE eviction. With the `ON DELETE CASCADE` FKs from
    /// `0002_motion_clips.sql` (motion_events) and
    /// `0003_events_clip_cascade.sql` (events.clip_id, flipped from
    /// SET NULL to CASCADE), this also removes every linked
    /// `motion_events` row AND every alert `events` row that
    /// referenced the clip — leaving no half-deleted half-state the
    /// M2.1 schema invariant exists to prevent.
    /// File unlink is the caller's responsibility (see
    /// `docs/M2_STORAGE.md` crash-safety section).
    ///
    /// **M2.2 invariant**: callers MUST NOT invoke this on a
    /// cold-replicated clip — that would orphan the cold copy. Use
    /// [`Self::clear_hot_pointer`] (soft-eviction) instead. The
    /// storage-safety eviction loop enforces the rule by picking a
    /// soft candidate before falling back to a hard one
    /// (cf. [`Self::find_soft_evict_candidate`] /
    /// [`Self::find_hard_evict_candidate`]).
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
        let rows = sqlx::query(&format!(
            "{CLIP_SELECT_COLUMNS_BASE}
              WHERE started_at < ?
              ORDER BY started_at ASC
              LIMIT ?"
        ))
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

    /// Triggering track ids for a given clip — i.e. the
    /// `events.track_id` values of every alert row that was
    /// stamped against this `clip_id` by the supervisor's
    /// `link_event_to_clip` call.
    ///
    /// Powers the "show only the alerting object" filter on the
    /// per-clip overlay: the UI keeps the full event lifecycle
    /// (born/updated/died) on every track but skips drawing
    /// anything whose `track_id` isn't in this set. Empty result
    /// means the clip exists with no linked alerts (motion-only
    /// recording, or all alert rows have NULL track_id) — the UI
    /// then draws no overlay.
    ///
    /// Rows with `track_id IS NULL` are filtered out; result is
    /// de-duplicated since a single track can trigger multiple
    /// alerts (different rules) on the same clip.
    pub async fn list_event_track_ids_for_clip(
        &self,
        clip_id: ClipId,
    ) -> Result<Vec<i64>, StoreError> {
        let rows = sqlx::query(
            "SELECT DISTINCT track_id
               FROM events
              WHERE clip_id = ? AND track_id IS NOT NULL",
        )
        .bind(clip_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<i64, _>("track_id"))
            .collect())
    }

    /// Single-clip lookup. Powers the per-clip overlay endpoint
    /// `GET /api/v1/clips/:id/tracks` — the UI player draws bboxes
    /// on a canvas synced to `<video>.currentTime`, and each event
    /// row carries the bbox + label + lifecycle kind needed to do
    /// that drawing without re-querying. Rows are ordered by
    /// `captured_at ASC` so the UI can walk forward in time as the
    /// video plays without re-sorting.
    pub async fn list_motion_events_for_clip(
        &self,
        clip_id: ClipId,
    ) -> Result<Vec<MotionEventRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, camera_id, clip_id, track_id, kind, captured_at,
                    bbox_x1, bbox_y1, bbox_x2, bbox_y2,
                    label, confidence, attributes_json
               FROM motion_events
              WHERE clip_id = ?
              ORDER BY captured_at ASC",
        )
        .bind(clip_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(motion_event_row_from_row).collect()
    }

    /// Cross-camera timeline lookup. Powers the rule-preview endpoint
    /// (`POST /v1/admin/rules/preview`) — operators need to see "what
    /// past detections would my new rule have matched", regardless of
    /// camera. The per-camera variant above can't satisfy that without
    /// O(N) round-trips; this one fans out a single SQL query against
    /// an optional `IN (…)` filter.
    ///
    /// `camera_ids = None` ⇒ all cameras. Order is most-recent-first
    /// (DESC) so the preview UI shows what the operator is most likely
    /// to recognise; the per-camera variant uses ASC because it's
    /// scrolling through a fixed window. Limit is a hard cap, not a
    /// pagination cursor — the UI shows "stopped at N; widen the
    /// window to see more" rather than implementing pagination here.
    pub async fn list_motion_events_across_cameras(
        &self,
        camera_ids: Option<&[CameraId]>,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<MotionEventRow>, StoreError> {
        // sqlx doesn't bind a slice to `IN (?)`, so we either build
        // the placeholders inline or fall back to "no filter" when
        // the caller passes None. Building inline is safe here
        // because every value is a `CameraId` (i64) — never user
        // text — and we cap the list at 1000 to keep the SQL string
        // well under the default sqlite parser limit.
        let mut sql = String::from(
            "SELECT id, camera_id, clip_id, track_id, kind, captured_at,
                    bbox_x1, bbox_y1, bbox_x2, bbox_y2,
                    label, confidence, attributes_json
               FROM motion_events
              WHERE captured_at BETWEEN ? AND ?",
        );
        if let Some(ids) = camera_ids {
            if !ids.is_empty() {
                let truncated = if ids.len() > 1000 { &ids[..1000] } else { ids };
                sql.push_str(" AND camera_id IN (");
                for (i, id) in truncated.iter().enumerate() {
                    if i > 0 {
                        sql.push(',');
                    }
                    // i64 literal — no quoting / escaping needed.
                    sql.push_str(&id.to_string());
                }
                sql.push(')');
            }
        }
        sql.push_str(" ORDER BY captured_at DESC LIMIT ?");

        let rows = sqlx::query(&sql)
            .bind(from.to_rfc3339())
            .bind(to.to_rfc3339())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(motion_event_row_from_row).collect()
    }

    /// Bucketed motion-event counts for the per-camera Timeline UI
    /// (M2.1 Stage B B7). Powers
    /// `GET /api/v1/cameras/:id/motion/histogram`.
    ///
    /// Returns sparse buckets — only intervals containing events are
    /// included. The bucket index is computed against `from` so the
    /// UI can position bars deterministically without re-parsing
    /// timestamps. `bucket_seconds` must be > 0 (caller validates).
    pub async fn list_motion_histogram_for_camera(
        &self,
        camera_id: CameraId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        bucket_seconds: i64,
    ) -> Result<Vec<MotionHistogramBucket>, StoreError> {
        debug_assert!(bucket_seconds > 0);
        let from_unix = from.timestamp();
        let rows = sqlx::query(
            "SELECT
                 CAST((strftime('%s', captured_at) - ?) / ? AS INTEGER) AS bucket,
                 COUNT(*) AS event_count,
                 COUNT(DISTINCT clip_id) AS clip_count
               FROM motion_events
              WHERE camera_id = ? AND captured_at BETWEEN ? AND ?
              GROUP BY bucket
              ORDER BY bucket ASC",
        )
        .bind(from_unix)
        .bind(bucket_seconds)
        .bind(camera_id)
        .bind(from.to_rfc3339())
        .bind(to.to_rfc3339())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let bucket: i64 = row.get("bucket");
                let event_count: i64 = row.get("event_count");
                let clip_count: i64 = row.get("clip_count");
                let bucket_start = from + chrono::Duration::seconds(bucket * bucket_seconds);
                Ok(MotionHistogramBucket {
                    bucket,
                    bucket_start,
                    event_count,
                    clip_count,
                })
            })
            .collect()
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

    /// Read back the clip_id stamped on an event row. Returns
    /// `Ok(None)` if the row exists but has no clip cross-reference,
    /// and `Err(NotFound)` if the row itself is missing. Used by the
    /// alert detail UI to deep-link into the surrounding clip and by
    /// the supervisor's end-to-end test to verify the link wired up.
    pub async fn get_event_clip_id(&self, event_id: &str) -> Result<Option<ClipId>, StoreError> {
        let row = sqlx::query("SELECT clip_id FROM events WHERE event_id = ?")
            .bind(event_id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            None => Err(StoreError::NotFound(format!("event_id={event_id}"))),
            Some(r) => Ok(r.try_get::<Option<i64>, _>(0).ok().flatten()),
        }
    }

    // -----------------------------------------------------------------
    // M2.2 cold-mirror helpers.
    // -----------------------------------------------------------------

    /// Closed clips that are NOT yet replicated to cold and have a
    /// non-NULL `sha256` (the replicator gates upload on the hash).
    /// Highest `priority` first, then oldest-first within the same
    /// priority so backlogs drain in stream order. Phase 2 Step 2.1c
    /// added the priority lane so the cloud-side Expedite RPC can
    /// jump a single clip to the head of the queue without
    /// disrupting the rest of the backfill. The
    /// `idx_motion_clips_pending_cold` partial index makes the WHERE
    /// clause an O(log n) scan over just the pending subset; the
    /// `ORDER BY` is satisfied by an in-memory sort over that small
    /// result set (at `LIMIT 32` per tick, trivially fast).
    pub async fn clips_pending_cold_upload(&self, limit: i64) -> Result<Vec<ClipRow>, StoreError> {
        let rows = sqlx::query(&format!(
            "{CLIP_SELECT_COLUMNS_BASE}
              WHERE cold_handle IS NULL
                AND ended_at IS NOT NULL
                AND sha256 IS NOT NULL
                AND hot_handle IS NOT NULL
                AND hot_path IS NOT NULL
              ORDER BY priority DESC, ended_at ASC
              LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(clip_row_from_row).collect()
    }

    /// Position (1-indexed) of `clip_id` in the cold-upload pending
    /// queue if it is still pending, or `None` if it has already
    /// been replicated, was never closed, or does not exist.
    /// Used by the Phase 2 Step 2.1c Expedite RPC handler to return
    /// the post-expedite queue depth to the cloud caller. The query
    /// shape mirrors [`Self::clips_pending_cold_upload`] one-for-one
    /// (same WHERE + ORDER BY) so the position is computed against
    /// the same view the replicator will see on its next tick.
    pub async fn pending_cold_upload_position(
        &self,
        clip_id: ClipId,
    ) -> Result<Option<u32>, StoreError> {
        let rows = sqlx::query(
            "SELECT id FROM motion_clips
              WHERE cold_handle IS NULL
                AND ended_at IS NOT NULL
                AND sha256 IS NOT NULL
                AND hot_handle IS NOT NULL
                AND hot_path IS NOT NULL
              ORDER BY priority DESC, ended_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .position(|r| r.get::<i64, _>(0) == clip_id)
            .map(|i| (i + 1) as u32))
    }

    /// Phase 2 Step 2.1c — bump the cold-replicator priority on a
    /// single clip. Idempotent: the partial UPDATE only writes when
    /// the row exists AND the new `priority` is strictly greater
    /// than the current one, so repeated expedites from the cloud
    /// don't generate spurious writes. Returns `Ok(false)` if the
    /// clip id does not exist OR the current priority is already
    /// at least `new_priority` (caller can treat both as no-op).
    pub async fn bump_clip_priority(
        &self,
        clip_id: ClipId,
        new_priority: i64,
    ) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "UPDATE motion_clips
                SET priority = ?
              WHERE id = ?
                AND priority < ?",
        )
        .bind(new_priority)
        .bind(clip_id)
        .bind(new_priority)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Aggregate cold-mirror counters for the storage admin UI. See
    /// [`ColdReplicaStats`] for field semantics. Single SUM/COUNT
    /// pass with conditional-aggregate filters so it stays O(n) over
    /// the table (no per-bucket subqueries).
    pub async fn cold_replica_stats(&self) -> Result<ColdReplicaStats, StoreError> {
        let row = sqlx::query(
            "SELECT
                COUNT(*) FILTER (
                    WHERE cold_handle IS NULL
                      AND ended_at IS NOT NULL
                      AND sha256 IS NOT NULL
                      AND hot_handle IS NOT NULL
                      AND hot_path IS NOT NULL
                )                                                 AS pending_count,
                COUNT(*) FILTER (WHERE cold_handle IS NOT NULL)   AS replicated_count,
                COUNT(*) FILTER (
                    WHERE cold_handle IS NOT NULL
                      AND hot_path IS NULL
                )                                                 AS cold_only_count,
                COALESCE(
                    SUM(size_bytes) FILTER (WHERE cold_handle IS NOT NULL),
                    0
                )                                                 AS lifetime_uploaded_bytes
              FROM motion_clips",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(ColdReplicaStats {
            pending_count: row.get::<i64, _>("pending_count"),
            replicated_count: row.get::<i64, _>("replicated_count"),
            cold_only_count: row.get::<i64, _>("cold_only_count"),
            lifetime_uploaded_bytes: row.get::<i64, _>("lifetime_uploaded_bytes"),
        })
    }

    /// Stamp the cold pointer on a clip after the replicator finishes
    /// uploading. The three column updates land in a single UPDATE so
    /// the schema-level CHECK
    /// `cold_handle IS NULL OR (cold_path IS NOT NULL AND cold_uploaded_at IS NOT NULL)`
    /// can never see an intermediate inconsistent state.
    ///
    /// Idempotent for re-runs against the same `(clip_id, mark)` —
    /// last write wins.
    pub async fn mark_cold_replicated(
        &self,
        clip_id: ClipId,
        mark: &ClipColdMark,
    ) -> Result<(), StoreError> {
        let res = sqlx::query(
            "UPDATE motion_clips
                SET cold_handle = ?, cold_path = ?, cold_uploaded_at = ?
              WHERE id = ?",
        )
        .bind(&mark.cold_handle)
        .bind(&mark.cold_path)
        .bind(mark.cold_uploaded_at.to_rfc3339())
        .bind(clip_id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("motion_clip id={clip_id}")));
        }
        Ok(())
    }

    /// Soft-evict candidate: oldest clip per camera that is hot AND
    /// already cold-replicated. Soft-eviction drops the hot copy
    /// while leaving the row + cold pointer intact, so playback
    /// keeps working from cold and the row never goes through the
    /// (cascade-delete-the-event-rows) path.
    ///
    /// Returns `Ok(None)` if the camera has no soft-evict candidate
    /// (every clip is either hot-only-no-cold or already
    /// soft-evicted). The caller falls back to
    /// [`Self::find_hard_evict_candidate`].
    pub async fn find_soft_evict_candidate(
        &self,
        camera_id: CameraId,
    ) -> Result<Option<ClipRow>, StoreError> {
        let row = sqlx::query(&format!(
            "{CLIP_SELECT_COLUMNS_BASE}
              WHERE camera_id = ?
                AND hot_handle IS NOT NULL
                AND cold_handle IS NOT NULL
              ORDER BY started_at ASC
              LIMIT 1"
        ))
        .bind(camera_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(clip_row_from_row).transpose()
    }

    /// Hard-evict candidate: oldest clip per camera that has NEVER
    /// been replicated to cold. Hard-eviction goes through
    /// [`Self::cascade_delete_clip_metadata`] which removes
    /// `motion_events` + linked `events` via FK CASCADE. Replaces
    /// the M2.1 [`Self::oldest_clip_for_camera`] for the eviction
    /// loop's Pass 2.
    ///
    /// Returns `Ok(None)` when every clip for this camera has been
    /// cold-replicated — at that point the eviction loop must
    /// either fall back to soft-eviction (already failed in Pass 1
    /// for this camera) or refuse to evict (cold-only clips are
    /// undeletable until the operator clears cold).
    pub async fn find_hard_evict_candidate(
        &self,
        camera_id: CameraId,
    ) -> Result<Option<ClipRow>, StoreError> {
        let row = sqlx::query(&format!(
            "{CLIP_SELECT_COLUMNS_BASE}
              WHERE camera_id = ?
                AND cold_handle IS NULL
              ORDER BY started_at ASC
              LIMIT 1"
        ))
        .bind(camera_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(clip_row_from_row).transpose()
    }

    /// Soft-evict the hot pointer. Clears `hot_handle` + `hot_path`
    /// **only when a cold copy exists** — the WHERE-guard prevents
    /// the schema CHECK
    /// `hot_handle IS NOT NULL OR cold_handle IS NOT NULL` from
    /// firing at write time AND prevents racy double-evicts that
    /// could orphan a clip with no surviving pointer.
    ///
    /// Returns the number of rows actually updated. `Ok(0)` means
    /// the guard rejected the update (clip is hot-only or already
    /// soft-evicted) — callers should treat this as a soft "no-op,
    /// pick another candidate", NOT as an error.
    pub async fn clear_hot_pointer(&self, clip_id: ClipId) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "UPDATE motion_clips
                SET hot_handle = NULL, hot_path = NULL
              WHERE id = ? AND cold_handle IS NOT NULL",
        )
        .bind(clip_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Re-establish the hot pointer on a previously soft-evicted
    /// clip. Used by the future cold-to-hot warm-back path (Phase 4
    /// playback flow + operator "rehydrate clip" action). The
    /// WHERE-guard requires `hot_handle IS NULL` so we never
    /// silently overwrite a hot pointer that was independently
    /// re-established by another path.
    ///
    /// Returns the number of rows updated. `Ok(0)` means the clip
    /// already had a hot pointer; the caller should re-fetch and
    /// reconcile.
    pub async fn repopulate_hot_pointer(
        &self,
        clip_id: ClipId,
        hot_handle: &str,
        hot_path: &str,
    ) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "UPDATE motion_clips
                SET hot_handle = ?, hot_path = ?
              WHERE id = ? AND hot_handle IS NULL",
        )
        .bind(hot_handle)
        .bind(hot_path)
        .bind(clip_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Read the singleton `storage_cold_replica` row. The migration
    /// pre-seeds this with `id=1, backend_handle=NULL` so the helper
    /// never returns `NotFound` on an open store.
    pub async fn read_cold_replica(&self) -> Result<ColdReplicaRow, StoreError> {
        let row = sqlx::query(
            "SELECT backend_handle, throttle_bps, updated_at
               FROM storage_cold_replica WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        let row = row.ok_or_else(|| {
            StoreError::NotFound("storage_cold_replica singleton row missing".into())
        })?;
        let updated_at = parse_ts(row.get::<String, _>(2).as_str())?;
        Ok(ColdReplicaRow {
            backend_handle: row.try_get::<Option<String>, _>(0)?,
            throttle_bps: row.get::<i64, _>(1),
            updated_at,
        })
    }

    /// Update the active cold backend (or set to `None` to disable
    /// replication). The `ON DELETE RESTRICT` FK to
    /// `storage_backends` prevents pointing at a non-existent
    /// backend; an `Err(StoreError::Sqlx(_))` carrying SQLite's
    /// constraint message surfaces here for the API layer to map.
    pub async fn write_cold_replica(
        &self,
        backend_handle: Option<&str>,
        throttle_bps: i64,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        self.write_cold_replica_tx(&mut tx, backend_handle, throttle_bps)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// M6 Phase 4 Step 4.1 — tx-aware update. See
    /// [`Store::upsert_camera_tx`] for the tx-merge rationale.
    pub async fn write_cold_replica_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        backend_handle: Option<&str>,
        throttle_bps: i64,
    ) -> Result<(), StoreError> {
        let res = sqlx::query(
            "UPDATE storage_cold_replica
                SET backend_handle = ?, throttle_bps = ?,
                    updated_at = ?
              WHERE id = 1",
        )
        .bind(backend_handle)
        .bind(throttle_bps)
        .bind(Utc::now().to_rfc3339())
        .execute(&mut **tx)
        .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(
                "storage_cold_replica singleton row missing".into(),
            ));
        }
        Ok(())
    }

    /// Every registered storage backend, in handle order. Drives the
    /// `GET /api/v1/storage` cold-section listing AND the runtime
    /// `Registry<Handle, Arc<dyn ColdBackend>>` boot-time hydration
    /// in nexus-engine.
    pub async fn list_storage_backends(&self) -> Result<Vec<StorageBackendRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT handle, kind, config_json, created_at, updated_at
               FROM storage_backends
              ORDER BY handle",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(storage_backend_row_from_row).collect()
    }

    /// Insert-or-update a backend. The `kind` CHECK constraint in the
    /// schema rejects unknown kinds; SQLite's constraint message
    /// surfaces as `Err(StoreError::Sqlx(_))`. `config_json` is
    /// expected to be valid JSON; this layer doesn't parse it.
    pub async fn upsert_storage_backend(
        &self,
        handle: &str,
        kind: &str,
        config_json: &str,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        self.upsert_storage_backend_tx(&mut tx, handle, kind, config_json)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// M6 Phase 4 Step 4.1 — tx-aware upsert. See
    /// [`Store::upsert_camera_tx`] for the tx-merge rationale.
    pub async fn upsert_storage_backend_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        handle: &str,
        kind: &str,
        config_json: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO storage_backends (handle, kind, config_json, updated_at)
                  VALUES (?, ?, ?, ?)
             ON CONFLICT(handle) DO UPDATE
                  SET kind = excluded.kind,
                      config_json = excluded.config_json,
                      updated_at = excluded.updated_at",
        )
        .bind(handle)
        .bind(kind)
        .bind(config_json)
        .bind(Utc::now().to_rfc3339())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Delete a backend. Returns [`DeleteBackendError::InUse`] if any
    /// `motion_clips` row references it (the FK is `ON DELETE
    /// RESTRICT` so the DB would also reject the DELETE — we
    /// pre-check explicitly to give the API layer a typed error
    /// instead of an opaque SQLite constraint string),
    /// [`DeleteBackendError::ActiveCold`] if the singleton cold
    /// replica points at it, and [`DeleteBackendError::Local`] for
    /// the implicit `'local'` backend (engine-owned, never
    /// removable).
    pub async fn delete_storage_backend(&self, handle: &str) -> Result<(), DeleteBackendError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::from)?;
        self.delete_storage_backend_tx(&mut tx, handle).await?;
        tx.commit().await.map_err(StoreError::from)?;
        Ok(())
    }

    /// M6 Phase 4 Step 4.1 — tx-aware delete. Runs the same
    /// pre-checks ([`DeleteBackendError::Local`] / `ActiveCold` /
    /// `InUse`) and the DELETE inside the caller's transaction.
    /// The pre-check reads are issued through the same
    /// transaction so they observe any prior writes the caller
    /// has already issued in the same tx; in practice nothing
    /// here is the caller wrote, but doing it consistently keeps
    /// the implementation honest and serialises cleanly under
    /// SQLite's `BEGIN IMMEDIATE` semantics.
    pub async fn delete_storage_backend_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        handle: &str,
    ) -> Result<(), DeleteBackendError> {
        if handle == "local" {
            return Err(DeleteBackendError::Local(handle.to_string()));
        }

        // Pre-check: is this the active cold replica? Read inside
        // the tx so a concurrent write to storage_cold_replica
        // can't slip past us.
        let cold_row = sqlx::query("SELECT backend_handle FROM storage_cold_replica WHERE id = 1")
            .fetch_optional(&mut **tx)
            .await
            .map_err(StoreError::from)?;
        if let Some(row) = cold_row {
            let active: Option<String> = row
                .try_get::<Option<String>, _>(0)
                .map_err(StoreError::from)?;
            if active.as_deref() == Some(handle) {
                return Err(DeleteBackendError::ActiveCold(handle.to_string()));
            }
        }

        // Pre-check: any clip rows referencing it?
        let in_use: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM motion_clips
              WHERE hot_handle = ? OR cold_handle = ?",
        )
        .bind(handle)
        .bind(handle)
        .fetch_one(&mut **tx)
        .await
        .map_err(StoreError::from)?;
        if in_use.0 > 0 {
            return Err(DeleteBackendError::InUse(handle.to_string()));
        }

        let res = sqlx::query("DELETE FROM storage_backends WHERE handle = ?")
            .bind(handle)
            .execute(&mut **tx)
            .await
            .map_err(StoreError::from)?;
        if res.rows_affected() == 0 {
            return Err(DeleteBackendError::Store(StoreError::NotFound(format!(
                "storage_backend handle={handle}"
            ))));
        }
        Ok(())
    }

    /// Read a key from `engine_runtime_settings` (M2.2 Phase 3
    /// runtime-settings table). Returns:
    ///
    /// * `Ok(Some(Some(value)))` — row exists, value is set.
    /// * `Ok(Some(None))` — row exists, value is `NULL` (explicit
    ///   "clear" by the operator). The caller must distinguish this
    ///   from "no row" because the latter falls back to `nexus.toml`
    ///   while the former does NOT.
    /// * `Ok(None)` — no row. Fall back to TOML / hard-coded default.
    pub async fn read_runtime_setting(
        &self,
        key: &str,
    ) -> Result<Option<Option<String>>, StoreError> {
        let row = sqlx::query("SELECT value FROM engine_runtime_settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(r.try_get::<Option<String>, _>(0)?)),
        }
    }

    /// Insert-or-update a key in `engine_runtime_settings`. Passing
    /// `value = None` stores SQL `NULL` — semantically "operator
    /// explicitly cleared the setting"; the lookup-priority chain
    /// then short-circuits before consulting TOML.
    pub async fn write_runtime_setting(
        &self,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        self.write_runtime_setting_tx(&mut tx, key, value).await?;
        tx.commit().await?;
        Ok(())
    }

    /// M6 Phase 4 Step 4.1 — tx-aware upsert. See
    /// [`Store::upsert_camera_tx`] for the tx-merge rationale.
    pub async fn write_runtime_setting_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO engine_runtime_settings (key, value, updated_at)
                  VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE
                  SET value = excluded.value,
                      updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(Utc::now().to_rfc3339())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Drop a `engine_runtime_settings` row entirely. Semantically
    /// distinct from [`Self::write_runtime_setting_tx`] called with
    /// `value = None` — the latter persists SQL `NULL` (which the
    /// caller can read back as `Some(None)` to mean "operator
    /// explicitly cleared"); this removes the row so the caller's
    /// `Ok(None)` arm fires and they fall back to the on-disk
    /// `nexus.toml` default. First consumer is the `ui_bind` admin
    /// surface in `nexus-engine::admin_runtime` whose three-state
    /// model (`Reset` / `Clear` / `Set`) needs to distinguish the two.
    pub async fn delete_runtime_setting_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        key: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM engine_runtime_settings WHERE key = ?")
            .bind(key)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Row decoders
// ---------------------------------------------------------------------------

/// The 15 ClipRow columns in stable bind-index order. Centralised so
/// the row decoder below can index by position instead of carrying a
/// fragile column-name lookup. Every helper that returns
/// `Result<ClipRow, _>` must SELECT these in this exact order.
const CLIP_SELECT_COLUMNS_BASE: &str = "SELECT id, camera_id, started_at, ended_at, duration_ms,
            size_bytes, codec, container,
            hot_handle, hot_path, cold_handle, cold_path, cold_uploaded_at, sha256,
            priority
       FROM motion_clips";

const CLIP_SELECT_COLUMNS_WHERE_ID: &str =
    "SELECT id, camera_id, started_at, ended_at, duration_ms,
            size_bytes, codec, container,
            hot_handle, hot_path, cold_handle, cold_path, cold_uploaded_at, sha256,
            priority
       FROM motion_clips WHERE id = ?";

fn clip_row_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ClipRow, StoreError> {
    let started_at = parse_ts(row.get::<String, _>(2).as_str())?;
    let ended_at = row
        .try_get::<Option<String>, _>(3)?
        .map(|s| parse_ts(&s))
        .transpose()?;
    let cold_uploaded_at = row
        .try_get::<Option<String>, _>(12)?
        .map(|s| parse_ts(&s))
        .transpose()?;
    Ok(ClipRow {
        id: row.get::<i64, _>(0),
        camera_id: row.get::<i64, _>(1),
        started_at,
        ended_at,
        duration_ms: row.get::<i64, _>(4),
        size_bytes: row.get::<i64, _>(5),
        codec: row.get::<String, _>(6),
        container: row.get::<String, _>(7),
        hot_handle: row.try_get::<Option<String>, _>(8)?,
        hot_path: row.try_get::<Option<String>, _>(9)?,
        cold_handle: row.try_get::<Option<String>, _>(10)?,
        cold_path: row.try_get::<Option<String>, _>(11)?,
        cold_uploaded_at,
        sha256: row.try_get::<Option<String>, _>(13)?,
        priority: row.get::<i64, _>(14),
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
    // RFC3339 covers everything we explicitly write
    // (`Utc::now().to_rfc3339()`), but SQLite's `CURRENT_TIMESTAMP`
    // default emits `YYYY-MM-DD HH:MM:SS` (no `T`, no zone). The
    // `created_at` columns on `storage_backends` (and friends) use
    // that default, so we must accept both. Naive timestamps are
    // assumed UTC because that's what SQLite documents.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }
    Err(StoreError::NotFound(format!(
        "bad timestamp '{s}': not RFC3339 nor SQLite default"
    )))
}

fn storage_backend_row_from_row(
    row: sqlx::sqlite::SqliteRow,
) -> Result<StorageBackendRow, StoreError> {
    let created_at = parse_ts(row.get::<String, _>(3).as_str())?;
    let updated_at = parse_ts(row.get::<String, _>(4).as_str())?;
    Ok(StorageBackendRow {
        handle: row.get::<String, _>(0),
        kind: row.get::<String, _>(1),
        config_json: row.get::<String, _>(2),
        created_at,
        updated_at,
    })
}

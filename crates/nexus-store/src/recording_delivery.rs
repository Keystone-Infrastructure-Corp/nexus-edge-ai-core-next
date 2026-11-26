//! SPEC-090 — authoritative recording generation and delivery accounting.
//!
//! Two read models, both served by [`Store::recording_delivery_stats`]
//! from one database snapshot:
//!
//! * **Queue partitions.** Every finalized, not-yet-delivered row of an
//!   upload queue is in exactly one [`QueuePartition`], the first gate of
//!   the uploader's selection it fails. The gates are SQL fragments
//!   ([`QueueGates`]) that the uploader queries
//!   ([`Store::clips_pending_cold_upload_at`],
//!   [`Store::alert_clips_pending_cold_upload`]) are built from, so
//!   `Ready` cannot drift from what the uploader selects. Rows still being
//!   written are counted apart and never as queue bytes.
//! * **Minute buckets** (`recording_minute_buckets`, migration 0041),
//!   keyed `(minute_utc, camera_id, source_kind)`. Each counter is written
//!   in the transaction of the transition that caused it, guarded by an
//!   idempotent marker or a one-way state change, so a duplicate close or
//!   completion, a retry or a restart never counts twice. Rows that
//!   predate the migration are a baseline and never land in a bucket.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use nexus_types::CameraId;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection};

use crate::{Store, StoreError};

/// Minimum spacing between cold-upload attempts for one alert clip. Owned
/// here so the alert uploader's retry gate and its partition read use the
/// same value.
pub const ALERT_COLD_RETRY_SECS: u64 = 60;

/// Snapshot shape version; bump on a breaking change to its fields.
const SCHEMA_VERSION: u32 = 1;

/// Minute buckets older than this, relative to the newest write, are pruned.
const BUCKET_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

/// Aligned windows reported in the snapshot.
const WINDOW_SECONDS: [i64; 2] = [60 * 60, 24 * 60 * 60];

/// Which upload queue a clip belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingSource {
    /// `motion_clips`, uploaded by the cold replicator.
    Ordinary,
    /// `alert_clips`, uploaded by the separate alert-clip uploader.
    Alert,
}

impl RecordingSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ordinary => "ordinary",
            Self::Alert => "alert",
        }
    }

    fn gates(self) -> &'static QueueGates {
        match self {
            Self::Ordinary => &MOTION_GATES,
            Self::Alert => &ALERT_GATES,
        }
    }

    /// The value the queue's backoff gate compares against at `now`.
    fn gate_clock(self, now: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Self::Ordinary => now,
            Self::Alert => now - Duration::seconds(ALERT_COLD_RETRY_SECS as i64),
        }
    }
}

/// The mutually exclusive states of an undelivered, finalized clip,
/// evaluated in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuePartition {
    /// Excluded from upload for good (attempt or size ceiling).
    Quarantined,
    /// Captured before the enrollment's upload floor: local-only by policy.
    BeforeEnrollment,
    /// No hash or no hot pointer, so the uploader cannot send it.
    MissingLocalMedia,
    /// Waiting out a retry delay after a failed attempt.
    Backoff,
    /// Selected by the uploader now.
    Ready,
}

impl QueuePartition {
    fn parse(s: &str) -> Result<Self, StoreError> {
        Ok(match s {
            "quarantined" => Self::Quarantined,
            "before_enrollment" => Self::BeforeEnrollment,
            "missing_local_media" => Self::MissingLocalMedia,
            "backoff" => Self::Backoff,
            "ready" => Self::Ready,
            other => return Err(StoreError::Decode(format!("queue partition `{other}`"))),
        })
    }
}

/// One upload queue's selection, as SQL gates over its table. In every
/// fragment `?1` is the gate clock (see [`RecordingSource::gate_clock`])
/// and `?2` the enrollment floor (RFC3339, NULL for none).
pub(crate) struct QueueGates {
    table: &'static str,
    /// Finalized and not yet delivered; the rows partitions are drawn from.
    pub(crate) queue: &'static str,
    /// Still being written: counted apart, never as queue bytes.
    open: &'static str,
    not_quarantined: Option<&'static str>,
    pub(crate) after_floor: &'static str,
    has_local_media: &'static str,
    backoff_elapsed: &'static str,
}

pub(crate) const MOTION_GATES: QueueGates = QueueGates {
    table: "motion_clips",
    queue: "cold_handle IS NULL AND ended_at IS NOT NULL",
    open: "ended_at IS NULL",
    not_quarantined: Some("cold_quarantined = 0"),
    after_floor: "(?2 IS NULL OR started_at >= ?2)",
    has_local_media: "sha256 IS NOT NULL AND hot_handle IS NOT NULL AND hot_path IS NOT NULL",
    backoff_elapsed: "(cold_next_attempt_at IS NULL OR cold_next_attempt_at <= ?1)",
};

pub(crate) const ALERT_GATES: QueueGates = QueueGates {
    table: "alert_clips",
    queue: "state = 'ready' AND cold_uploaded_at IS NULL",
    open: "state = 'building'",
    not_quarantined: None,
    after_floor: "(?2 IS NULL OR started_at >= ?2)",
    has_local_media: "sha256 IS NOT NULL",
    backoff_elapsed: "(cold_last_attempt_at IS NULL OR cold_last_attempt_at <= ?1)",
};

impl QueueGates {
    /// The uploader's WHERE: queued rows that pass every gate. Terms stay
    /// top-level ANDs so the partial `*_pending_cold` indexes still apply.
    pub(crate) fn ready_where(&self) -> String {
        let quarantine = self
            .not_quarantined
            .map(|g| format!(" AND {g}"))
            .unwrap_or_default();
        format!(
            "{} AND {}{quarantine} AND {} AND {}",
            self.queue, self.has_local_media, self.backoff_elapsed, self.after_floor
        )
    }

    /// The first gate a queued row fails, in [`QueuePartition`] order; a
    /// row that fails none is exactly a [`Self::ready_where`] row.
    fn partition_case(&self) -> String {
        let quarantine = self
            .not_quarantined
            .map(|g| format!("WHEN NOT ({g}) THEN 'quarantined' "))
            .unwrap_or_default();
        format!(
            "CASE {quarantine}WHEN NOT {} THEN 'before_enrollment' \
             WHEN NOT ({}) THEN 'missing_local_media' \
             WHEN NOT {} THEN 'backoff' ELSE 'ready' END",
            self.after_floor, self.has_local_media, self.backoff_elapsed
        )
    }
}

/// Totals for one partition (or the open rows) of a queue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionTotals {
    pub count: i64,
    /// Sum of the sizes that are known (`size_bytes > 0`).
    pub known_bytes: i64,
    /// Rows whose size is not known; none of their bytes are in `known_bytes`.
    pub unknown_size_count: i64,
    /// Earliest capture start in the partition; `None` when it is empty.
    pub oldest_capture_at: Option<DateTime<Utc>>,
}

/// One upload queue at `sampled_at`. An in-flight transfer stays in the
/// partition it was selected from until its completion commits.
// shortcut: in-flight transfers are not marked, add a marker when the capacity view shows them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryQueue {
    /// `None` when the queue has no quarantine (alert clips).
    pub quarantined: Option<PartitionTotals>,
    pub before_enrollment: PartitionTotals,
    pub missing_local_media: PartitionTotals,
    pub backoff: PartitionTotals,
    pub ready: PartitionTotals,
    /// Rows still being written: outside every partition, sizes unknown.
    pub open: PartitionTotals,
}

/// Counters summed over a window. Payload bytes only: not network usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryCounters {
    /// Finalized clip bytes, counted at finalization.
    pub generated_bytes: i64,
    /// Finalized clips whose size was unknown (not in `generated_bytes`).
    pub generated_unknown_size_count: i64,
    /// Finalized clip duration.
    pub recorded_ms: i64,
    /// Bytes of clips whose cold copy was durably recorded.
    pub delivered_bytes: i64,
    /// Bytes handed to a transfer, retries included, whatever the outcome.
    pub attempted_bytes: i64,
    /// Transfers of a clip that had failed before.
    pub retry_count: i64,
    /// Failed upload attempts.
    pub failure_count: i64,
    /// Bytes of queued (not before-enrollment) clips evicted undelivered.
    pub evicted_before_upload_bytes: i64,
    /// Clips evicted undelivered, including any of unknown size.
    pub evicted_before_upload_count: i64,
}

impl DeliveryCounters {
    fn add(&mut self, o: &Self) {
        self.generated_bytes += o.generated_bytes;
        self.generated_unknown_size_count += o.generated_unknown_size_count;
        self.recorded_ms += o.recorded_ms;
        self.delivered_bytes += o.delivered_bytes;
        self.attempted_bytes += o.attempted_bytes;
        self.retry_count += o.retry_count;
        self.failure_count += o.failure_count;
        self.evicted_before_upload_bytes += o.evicted_before_upload_bytes;
        self.evicted_before_upload_count += o.evicted_before_upload_count;
    }

    pub(crate) fn generated(size_bytes: i64, duration_ms: i64) -> Self {
        Self {
            generated_bytes: size_bytes.max(0),
            generated_unknown_size_count: i64::from(size_bytes <= 0),
            recorded_ms: duration_ms.max(0),
            ..Self::default()
        }
    }

    pub(crate) fn evicted(size_bytes: i64) -> Self {
        Self {
            evicted_before_upload_bytes: size_bytes.max(0),
            evicted_before_upload_count: 1,
            ..Self::default()
        }
    }
}

/// Ordinary and alert counters, never summed together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCounters {
    pub ordinary: DeliveryCounters,
    pub alert: DeliveryCounters,
}

/// One camera's counters in a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraDelivery {
    pub camera_id: CameraId,
    #[serde(flatten)]
    pub counters: SourceCounters,
}

/// Counters over `[start, end)`, whole UTC minutes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryWindow {
    pub window_seconds: i64,
    pub start: DateTime<Utc>,
    /// Start of the minute holding `sampled_at`; that minute is excluded.
    pub end: DateTime<Utc>,
    /// False when the window starts before accounting began on this core,
    /// so its counters cover only part of it.
    pub complete: bool,
    /// The whole core.
    pub core: SourceCounters,
    /// Cameras with any counter in the window, by id.
    pub cameras: Vec<CameraDelivery>,
}

/// The versioned `recording_delivery` snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingDeliverySnapshot {
    pub schema_version: u32,
    pub sampled_at: DateTime<Utc>,
    /// When accounting began on this core (migration 0041).
    pub accounting_since: DateTime<Utc>,
    pub ordinary: DeliveryQueue,
    pub alert: DeliveryQueue,
    /// The aligned 1 h and 24 h windows, in that order.
    pub windows: Vec<DeliveryWindow>,
    /// The whole core from the windows' `end` to `sampled_at` (the minute
    /// holding the sample), so a change the queue already shows is never
    /// outside every counter.
    pub since_window_end: SourceCounters,
}

/// Add `d` to the `(minute of at, camera, source)` bucket and prune buckets
/// more than [`BUCKET_RETENTION_SECS`] older than it. Callers run it in
/// the transaction of the transition being counted.
pub(crate) async fn add_to_bucket(
    conn: &mut SqliteConnection,
    camera_id: CameraId,
    source: RecordingSource,
    at: DateTime<Utc>,
    d: &DeliveryCounters,
) -> Result<(), StoreError> {
    let minute = at.timestamp().div_euclid(60) * 60;
    sqlx::query(
        "INSERT INTO recording_minute_buckets
             (minute_utc, camera_id, source_kind, generated_bytes,
              generated_unknown_size_count, recorded_ms, delivered_bytes,
              attempted_bytes, retry_count, failure_count,
              evicted_before_upload_bytes, evicted_before_upload_count)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (minute_utc, camera_id, source_kind) DO UPDATE SET
             generated_bytes = generated_bytes + excluded.generated_bytes,
             generated_unknown_size_count =
                 generated_unknown_size_count + excluded.generated_unknown_size_count,
             recorded_ms = recorded_ms + excluded.recorded_ms,
             delivered_bytes = delivered_bytes + excluded.delivered_bytes,
             attempted_bytes = attempted_bytes + excluded.attempted_bytes,
             retry_count = retry_count + excluded.retry_count,
             failure_count = failure_count + excluded.failure_count,
             evicted_before_upload_bytes =
                 evicted_before_upload_bytes + excluded.evicted_before_upload_bytes,
             evicted_before_upload_count =
                 evicted_before_upload_count + excluded.evicted_before_upload_count",
    )
    .bind(minute)
    .bind(camera_id)
    .bind(source.as_str())
    .bind(d.generated_bytes)
    .bind(d.generated_unknown_size_count)
    .bind(d.recorded_ms)
    .bind(d.delivered_bytes)
    .bind(d.attempted_bytes)
    .bind(d.retry_count)
    .bind(d.failure_count)
    .bind(d.evicted_before_upload_bytes)
    .bind(d.evicted_before_upload_count)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM recording_minute_buckets WHERE minute_utc < ?")
        .bind(minute - BUCKET_RETENTION_SECS)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

const COUNTER_SUMS: &str = "SUM(generated_bytes), SUM(generated_unknown_size_count),
    SUM(recorded_ms), SUM(delivered_bytes), SUM(attempted_bytes), SUM(retry_count),
    SUM(failure_count), SUM(evicted_before_upload_bytes), SUM(evicted_before_upload_count)";

impl Store {
    /// Every finalized, undelivered row of `source`'s queue with its
    /// partition at `now` and enrollment `floor`, by id. The snapshot
    /// aggregates the same classification.
    pub async fn upload_queue_partitions(
        &self,
        source: RecordingSource,
        now: DateTime<Utc>,
        floor: Option<DateTime<Utc>>,
    ) -> Result<Vec<(i64, QueuePartition)>, StoreError> {
        let g = source.gates();
        let rows = sqlx::query(&format!(
            "SELECT id, {} FROM {} WHERE {} ORDER BY id",
            g.partition_case(),
            g.table,
            g.queue
        ))
        .bind(source.gate_clock(now).to_rfc3339())
        .bind(floor.map(|f| f.to_rfc3339()))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| Ok((r.get::<i64, _>(0), QueuePartition::parse(r.get(1))?)))
            .collect()
    }

    /// Count one transfer of `bytes` for clip `id` of `source`, as it
    /// starts: attempted bytes include retries and transfers that never
    /// finish. A clip with an earlier failed attempt counts as a retry.
    pub async fn record_upload_attempt(
        &self,
        source: RecordingSource,
        id: i64,
        at: DateTime<Utc>,
        bytes: i64,
    ) -> Result<(), StoreError> {
        let table = source.gates().table;
        let mut conn = self.pool.acquire().await?;
        let row = sqlx::query(&format!(
            "SELECT camera_id, cold_attempts FROM {table} WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("{table} id={id}")))?;
        let counters = DeliveryCounters {
            attempted_bytes: bytes.max(0),
            retry_count: i64::from(row.get::<i64, _>(1) > 0),
            ..DeliveryCounters::default()
        };
        add_to_bucket(&mut conn, row.get(0), source, at, &counters).await
    }

    /// The enrolled core's upload floor, for eviction accounting. A failed
    /// read applies no floor: a loss is over-reported, never hidden.
    pub(crate) async fn eviction_floor(&self) -> Option<String> {
        match self.get_cloud_enrollment().await {
            Ok(e) => e.map(|e| e.upload_floor().to_rfc3339()),
            Err(e) => {
                tracing::warn!(error = %e, "recording delivery: enrollment read failed; no floor");
                None
            }
        }
    }

    /// Count `camera_id`'s undelivered queue (at or after the enrollment
    /// floor) as evicted before upload, in the caller's transaction: a
    /// camera delete's FK cascade removes those clips without the evictors.
    pub(crate) async fn record_camera_loss(
        &self,
        conn: &mut SqliteConnection,
        camera_id: CameraId,
    ) -> Result<(), StoreError> {
        let floor = self.eviction_floor().await;
        for source in [RecordingSource::Ordinary, RecordingSource::Alert] {
            let g = source.gates();
            let (bytes, count): (i64, i64) = sqlx::query_as(&format!(
                "SELECT COALESCE(SUM(CASE WHEN size_bytes > 0 THEN size_bytes END), 0), COUNT(*)
                   FROM {} WHERE camera_id = ?1 AND {} AND {}",
                g.table, g.queue, g.after_floor
            ))
            .bind(camera_id)
            .bind(floor.as_deref())
            .fetch_one(&mut *conn)
            .await?;
            if count > 0 {
                let lost = DeliveryCounters {
                    evicted_before_upload_bytes: bytes,
                    evicted_before_upload_count: count,
                    ..DeliveryCounters::default()
                };
                add_to_bucket(conn, camera_id, source, Utc::now(), &lost).await?;
            }
        }
        Ok(())
    }

    /// SPEC-090 — the versioned `recording_delivery` snapshot at `now`
    /// for enrollment `floor`, read in one transaction.
    pub async fn recording_delivery_stats(
        &self,
        now: DateTime<Utc>,
        floor: Option<DateTime<Utc>>,
    ) -> Result<RecordingDeliverySnapshot, StoreError> {
        let floor = floor.map(|f| f.to_rfc3339());
        let mut tx = self.pool.begin().await?;
        let since: String = sqlx::query_scalar(
            "SELECT accounting_since FROM recording_delivery_baseline WHERE id = 1",
        )
        .fetch_one(&mut *tx)
        .await?;
        let accounting_since = crate::motion::parse_ts(&since)?;
        let ordinary =
            queue_totals(&mut tx, RecordingSource::Ordinary, now, floor.as_deref()).await?;
        let alert = queue_totals(&mut tx, RecordingSource::Alert, now, floor.as_deref()).await?;
        let end = now
            - Duration::seconds(now.timestamp().rem_euclid(60))
            - Duration::nanoseconds(i64::from(now.timestamp_subsec_nanos()));
        let mut windows = Vec::with_capacity(WINDOW_SECONDS.len());
        for secs in WINDOW_SECONDS {
            windows.push(window(&mut tx, end, secs, accounting_since).await?);
        }
        let (since_window_end, _) = bucket_sums(&mut tx, end, end + Duration::minutes(1)).await?;
        tx.commit().await?;
        Ok(RecordingDeliverySnapshot {
            schema_version: SCHEMA_VERSION,
            sampled_at: now,
            accounting_since,
            ordinary,
            alert,
            windows,
            since_window_end,
        })
    }
}

async fn queue_totals(
    conn: &mut SqliteConnection,
    source: RecordingSource,
    now: DateTime<Utc>,
    floor: Option<&str>,
) -> Result<DeliveryQueue, StoreError> {
    let g = source.gates();
    let rows = sqlx::query(&format!(
        "SELECT CASE WHEN {open} THEN 'open' ELSE {case} END AS part,
                COUNT(*),
                COALESCE(SUM(CASE WHEN size_bytes > 0 THEN size_bytes END), 0),
                SUM(size_bytes <= 0),
                MIN(started_at)
           FROM {table}
          WHERE ({open}) OR ({queue})
          GROUP BY part",
        open = g.open,
        case = g.partition_case(),
        table = g.table,
        queue = g.queue,
    ))
    .bind(source.gate_clock(now).to_rfc3339())
    .bind(floor)
    .fetch_all(&mut *conn)
    .await?;
    let mut q = DeliveryQueue {
        quarantined: g.not_quarantined.map(|_| PartitionTotals::default()),
        ..DeliveryQueue::default()
    };
    for r in rows {
        let totals = PartitionTotals {
            count: r.get(1),
            known_bytes: r.get(2),
            unknown_size_count: r.get(3),
            oldest_capture_at: Some(crate::motion::parse_ts(r.get(4))?),
        };
        let slot = match r.get::<&str, _>(0) {
            "open" => &mut q.open,
            p => match QueuePartition::parse(p)? {
                QueuePartition::Quarantined => q.quarantined.get_or_insert_with(Default::default),
                QueuePartition::BeforeEnrollment => &mut q.before_enrollment,
                QueuePartition::MissingLocalMedia => &mut q.missing_local_media,
                QueuePartition::Backoff => &mut q.backoff,
                QueuePartition::Ready => &mut q.ready,
            },
        };
        *slot = totals;
    }
    Ok(q)
}

async fn window(
    conn: &mut SqliteConnection,
    end: DateTime<Utc>,
    secs: i64,
    accounting_since: DateTime<Utc>,
) -> Result<DeliveryWindow, StoreError> {
    let start = end - Duration::seconds(secs);
    let (core, cameras) = bucket_sums(conn, start, end).await?;
    Ok(DeliveryWindow {
        window_seconds: secs,
        start,
        end,
        complete: start >= accounting_since,
        core,
        cameras,
    })
}

/// Core and per-camera sums of the buckets in `[start, end)`.
async fn bucket_sums(
    conn: &mut SqliteConnection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<(SourceCounters, Vec<CameraDelivery>), StoreError> {
    let rows = sqlx::query(&format!(
        "SELECT camera_id, source_kind, {COUNTER_SUMS}
           FROM recording_minute_buckets
          WHERE minute_utc >= ? AND minute_utc < ?
          GROUP BY camera_id, source_kind"
    ))
    .bind(start.timestamp())
    .bind(end.timestamp())
    .fetch_all(&mut *conn)
    .await?;
    let mut core = SourceCounters::default();
    let mut cameras: BTreeMap<CameraId, SourceCounters> = BTreeMap::new();
    for r in rows {
        let c = DeliveryCounters {
            generated_bytes: r.get(2),
            generated_unknown_size_count: r.get(3),
            recorded_ms: r.get(4),
            delivered_bytes: r.get(5),
            attempted_bytes: r.get(6),
            retry_count: r.get(7),
            failure_count: r.get(8),
            evicted_before_upload_bytes: r.get(9),
            evicted_before_upload_count: r.get(10),
        };
        let cam = cameras.entry(r.get(0)).or_default();
        let (cam_slot, core_slot) = match r.get::<&str, _>(1) {
            "alert" => (&mut cam.alert, &mut core.alert),
            _ => (&mut cam.ordinary, &mut core.ordinary),
        };
        cam_slot.add(&c);
        core_slot.add(&c);
    }
    let cameras = cameras
        .into_iter()
        .map(|(camera_id, counters)| CameraDelivery {
            camera_id,
            counters,
        })
        .collect();
    Ok((core, cameras))
}

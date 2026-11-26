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
//! * **Clock discontinuities.** Buckets are placed and pruned by the wall
//!   clock. When it jumps (see [`Store::check_clock`]), `accounting_since`
//!   moves past the jump so no window overlapping it reads complete, and
//!   pruning never runs ahead of monotonic time in this process, so a
//!   forward jump while running deletes no history.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
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

/// How far the wall clock may move apart from monotonic time between two
/// accounting reads or writes, or fall behind a written bucket, before it
/// counts as a discontinuity.
const CLOCK_TOLERANCE_SECS: i64 = 60;

/// Aligned windows reported in the snapshot.
const WINDOW_SECONDS: [i64; 2] = [60 * 60, 24 * 60 * 60];

/// SPEC-090 state that lives only in this process, shared by every clone
/// of a [`Store`].
#[derive(Debug, Default)]
pub(crate) struct DeliveryState {
    clock: Mutex<DeliveryClock>,
    transfers: Mutex<Transfers>,
}

/// Clip ids an uploader is transferring now, per queue; `None` until that
/// queue's uploader is tracked, so the snapshot cannot claim zero.
#[derive(Debug, Default)]
struct Transfers {
    ordinary: Option<BTreeSet<i64>>,
    alert: Option<BTreeSet<i64>>,
}

impl Transfers {
    fn of(&mut self, source: RecordingSource) -> &mut Option<BTreeSet<i64>> {
        match source {
            RecordingSource::Ordinary => &mut self.ordinary,
            RecordingSource::Alert => &mut self.alert,
        }
    }
}

impl DeliveryState {
    fn transfers(&self) -> std::sync::MutexGuard<'_, Transfers> {
        self.transfers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Marks one clip as being transferred until dropped; see
/// [`Store::begin_transfer`].
#[must_use = "the clip is in flight only while the guard lives"]
pub struct TransferGuard {
    state: Arc<DeliveryState>,
    source: RecordingSource,
    id: i64,
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        if let Some(ids) = self.state.transfers().of(self.source) {
            ids.remove(&self.id);
        }
    }
}

/// Wall-clock readings paired with the monotonic clock, to notice a
/// wall-clock discontinuity.
#[derive(Debug, Default)]
struct DeliveryClock {
    /// At this process's first accounting read or write.
    first: Option<(DateTime<Utc>, Instant)>,
    /// At the previous one.
    last: Option<(DateTime<Utc>, Instant)>,
    /// The latest accounting restart detected in this process, re-applied
    /// by every check: the transaction that first wrote it may roll back
    /// (a no-op once it committed).
    restart: Option<DateTime<Utc>>,
    /// Test seam: the wall clock read instead of `Utc::now()`.
    #[cfg(test)]
    test_wall: Option<DateTime<Utc>>,
}

impl DeliveryState {
    /// Read the clocks: the wall time, whether it moved more than
    /// [`CLOCK_TOLERANCE_SECS`] apart from monotonic time since the
    /// previous reading, and the latest wall time monotonic time vouches
    /// for since the first reading (the prune horizon).
    fn observe_clock(&self) -> (DateTime<Utc>, bool, DateTime<Utc>) {
        let mut c = self.clock.lock().unwrap_or_else(PoisonError::into_inner);
        #[cfg(test)]
        let wall = c.test_wall.unwrap_or_else(Utc::now);
        #[cfg(not(test))]
        let wall = Utc::now();
        let mono = Instant::now();
        let advanced = |(w, m): (DateTime<Utc>, Instant)| {
            w + Duration::from_std(mono.duration_since(m)).unwrap_or(Duration::zero())
        };
        let jumped = c
            .last
            .is_some_and(|l| (wall - advanced(l)).num_seconds().abs() > CLOCK_TOLERANCE_SECS);
        let first = *c.first.get_or_insert((wall, mono));
        c.last = Some((wall, mono));
        // shortcut: after a forward jump the horizon trails the wall clock by
        // the jump until the engine restarts (extra rows, never lost ones);
        // re-anchor once a jump is confirmed if that row growth matters.
        (wall, jumped, wall.min(advanced(first)))
    }

    /// Keep `detected` pending with any earlier restart; the latest one.
    fn pending_restart(&self, detected: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
        let mut c = self.clock.lock().unwrap_or_else(PoisonError::into_inner);
        c.restart = c.restart.max(detected);
        c.restart
    }
}

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

/// Rows an uploader is transferring now: a marked subset of the queue's
/// partitions, never an addition to them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlight {
    pub count: i64,
    pub known_bytes: i64,
    pub unknown_size_count: i64,
}

/// One upload queue at `sampled_at`. An in-flight transfer stays in the
/// partition it was selected from until its completion commits.
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
    /// The partitions' rows being transferred now, already counted in
    /// them. `None` when this process does not track the queue's uploader.
    pub in_flight: Option<InFlight>,
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
    /// or before it restarted after a clock discontinuity, so its counters
    /// cover only part of it.
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
    /// When accounting began on this core (migration 0041), moved forward
    /// past each wall-clock discontinuity.
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
/// more than [`BUCKET_RETENTION_SECS`] older than it, or than the prune
/// horizon of [`Store::check_clock`] if that is earlier. Callers run it in
/// the transaction of the transition being counted.
pub(crate) async fn add_to_bucket(
    store: &Store,
    conn: &mut SqliteConnection,
    camera_id: CameraId,
    source: RecordingSource,
    at: DateTime<Utc>,
    d: &DeliveryCounters,
) -> Result<(), StoreError> {
    let horizon = store.check_clock(conn).await?;
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
        .bind(minute.min(horizon.timestamp().div_euclid(60) * 60) - BUCKET_RETENTION_SECS)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// A clip counted evicted before upload at `evicted_at` was delivered after
/// all: remove that loss (one clip, `size_bytes`) from the bucket that
/// counted it. Callers run it once, in the completion's transaction.
pub(crate) async fn take_back_loss(
    conn: &mut SqliteConnection,
    camera_id: CameraId,
    source: RecordingSource,
    evicted_at: &str,
    size_bytes: i64,
) -> Result<(), StoreError> {
    let lost = DeliveryCounters::evicted(size_bytes);
    let minute = crate::motion::parse_ts(evicted_at)?
        .timestamp()
        .div_euclid(60)
        * 60;
    // Never inserts: a pruned bucket stays gone rather than going negative,
    // and one before `accounting_since` is left alone, since a clock jump
    // may have pruned and re-used its minute.
    sqlx::query(
        "UPDATE recording_minute_buckets
            SET evicted_before_upload_bytes = evicted_before_upload_bytes - ?1,
                evicted_before_upload_count = evicted_before_upload_count - ?2
          WHERE minute_utc = ?3 AND camera_id = ?4 AND source_kind = ?5
            AND ?6 >= (SELECT accounting_since FROM recording_delivery_baseline WHERE id = 1)",
    )
    .bind(lost.evicted_before_upload_bytes)
    .bind(lost.evicted_before_upload_count)
    .bind(minute)
    .bind(camera_id)
    .bind(source.as_str())
    .bind(evicted_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

const COUNTER_SUMS: &str = "SUM(generated_bytes), SUM(generated_unknown_size_count),
    SUM(recorded_ms), SUM(delivered_bytes), SUM(attempted_bytes), SUM(retry_count),
    SUM(failure_count), SUM(evicted_before_upload_bytes), SUM(evicted_before_upload_count)";

impl Store {
    /// SPEC-090 — notice a wall-clock discontinuity: the wall clock moved
    /// apart from monotonic time since this process's previous accounting
    /// read or write, or it is behind a bucket already written. Either
    /// moves `accounting_since` (persisted, never backward) past the
    /// detection time and every written bucket, so no window overlapping
    /// the discontinuity reads complete. Every later check re-applies the
    /// restart, so a write that detects it and rolls back cannot consume
    /// it. Returns the prune horizon.
    async fn check_clock(&self, conn: &mut SqliteConnection) -> Result<DateTime<Utc>, StoreError> {
        let (wall, jumped, horizon) = self.delivery.observe_clock();
        let newest: Option<i64> =
            sqlx::query_scalar("SELECT MAX(minute_utc) FROM recording_minute_buckets")
                .fetch_one(&mut *conn)
                .await?;
        let behind = newest.is_some_and(|m| wall.timestamp() + CLOCK_TOLERANCE_SECS < m);
        let detected = (jumped || behind).then(|| {
            let past_buckets = newest
                .and_then(|m| DateTime::from_timestamp(m + 60, 0))
                .unwrap_or(wall);
            wall.max(past_buckets)
        });
        if let Some(restart) = self.delivery.pending_restart(detected) {
            // Same text shape as the migration's, so the comparison is by time.
            let moved = sqlx::query(
                "UPDATE recording_delivery_baseline SET accounting_since = ?1
                  WHERE id = 1 AND accounting_since < ?1",
            )
            .bind(restart.to_rfc3339_opts(SecondsFormat::Millis, true))
            .execute(&mut *conn)
            .await?;
            if moved.rows_affected() > 0 {
                tracing::warn!(%wall, %restart, "recording delivery: wall clock discontinuity; accounting restarts");
            }
        }
        Ok(horizon)
    }

    /// SPEC-090 — an accounting write failed at `at`: the counters now
    /// understate what happened, so treat it as a discontinuity. The gap is
    /// kept pending (as a detected clock jump is) and applied now if the
    /// store allows; every later check re-applies it until it commits.
    // shortcut: the pending gap lives in memory, so a process restart before
    // it commits forgets it; persist it if accounting writes fail that long.
    pub async fn record_accounting_gap(&self, at: DateTime<Utc>) {
        self.delivery.pending_restart(Some(at));
        let applied = async {
            let mut conn = self.pool.acquire().await?;
            self.check_clock(&mut conn).await
        }
        .await;
        if let Err(e) = applied {
            tracing::warn!(error = %e, "recording delivery: accounting gap stays pending");
        }
    }

    /// `source`'s uploader runs in this process: report its transfers
    /// (`in_flight`) from now on instead of null.
    pub fn track_transfers(&self, source: RecordingSource) {
        self.delivery
            .transfers()
            .of(source)
            .get_or_insert_with(BTreeSet::new);
    }

    /// Mark clip `id` of `source` as in flight until the guard drops; hold
    /// it from selection until the completion commits or the attempt fails.
    pub fn begin_transfer(&self, source: RecordingSource, id: i64) -> TransferGuard {
        self.delivery
            .transfers()
            .of(source)
            .get_or_insert_with(BTreeSet::new)
            .insert(id);
        TransferGuard {
            state: self.delivery.clone(),
            source,
            id,
        }
    }

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
        add_to_bucket(self, &mut conn, row.get(0), source, at, &counters).await
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
                add_to_bucket(self, conn, camera_id, source, Utc::now(), &lost).await?;
            }
        }
        Ok(())
    }

    /// SPEC-090 — the versioned `recording_delivery` snapshot for
    /// enrollment `floor`, read in one transaction and sampled at the
    /// time `clock` returns. `clock` is read once the transaction has
    /// pinned its snapshot, so every change the snapshot shows (an
    /// eviction emptying the queue) is counted at or before `sampled_at`.
    pub async fn recording_delivery_stats(
        &self,
        clock: impl FnOnce() -> DateTime<Utc>,
        floor: Option<DateTime<Utc>>,
    ) -> Result<RecordingDeliverySnapshot, StoreError> {
        let floor = floor.map(|f| f.to_rfc3339());
        // A jump with no write since must not leave its windows complete;
        // checked outside the read transaction, which then never writes.
        self.check_clock(&mut *self.pool.acquire().await?).await?;
        // Read before the snapshot: a transfer that completes in between is
        // no longer queued there, so the marked rows stay a subset.
        let (ordinary_ids, alert_ids) = {
            let t = self.delivery.transfers();
            (t.ordinary.clone(), t.alert.clone())
        };
        let mut tx = self.pool.begin().await?;
        let since: String = sqlx::query_scalar(
            "SELECT accounting_since FROM recording_delivery_baseline WHERE id = 1",
        )
        .fetch_one(&mut *tx)
        .await?;
        // WAL pins the read snapshot at the first read, just above.
        let now = clock();
        let accounting_since = crate::motion::parse_ts(&since)?;
        let ordinary = queue_totals(
            &mut tx,
            RecordingSource::Ordinary,
            now,
            floor.as_deref(),
            ordinary_ids,
        )
        .await?;
        let alert = queue_totals(
            &mut tx,
            RecordingSource::Alert,
            now,
            floor.as_deref(),
            alert_ids,
        )
        .await?;
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
    in_flight: Option<BTreeSet<i64>>,
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
    if let Some(ids) = in_flight {
        // Integer ids, so formatting them into the statement is safe.
        let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
        let (count, known_bytes, unknown_size_count) = sqlx::query_as(&format!(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN size_bytes > 0 THEN size_bytes END), 0),
                    COALESCE(SUM(size_bytes <= 0), 0)
               FROM {} WHERE ({}) AND id IN ({})",
            g.table,
            g.queue,
            ids.join(",")
        ))
        .fetch_one(&mut *conn)
        .await?;
        q.in_flight = Some(InFlight {
            count,
            known_bytes,
            unknown_size_count,
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClipClose, NewClip};
    use chrono::SecondsFormat;
    use nexus_config::StoreConfig;

    async fn open(dir: &std::path::Path) -> Store {
        Store::open(&StoreConfig {
            url: format!("sqlite://{}/store.db?mode=rwc", dir.display()),
            ..StoreConfig::default()
        })
        .await
        .unwrap()
    }

    /// Camera 1, with accounting begun two days before `t0`.
    async fn store_since(dir: &std::path::Path, t0: DateTime<Utc>) -> Store {
        let store = open(dir).await;
        sqlx::query(
            "INSERT INTO cameras (id, name, url, config_json) VALUES (1, 'c', 'rtsp://h/s', '{}')",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        sqlx::query("UPDATE recording_delivery_baseline SET accounting_since = ?")
            .bind((t0 - Duration::days(2)).to_rfc3339_opts(SecondsFormat::Millis, true))
            .execute(&store.pool)
            .await
            .unwrap();
        store
    }

    async fn closed(store: &Store, started: DateTime<Utc>, size_bytes: i64) -> i64 {
        let id = store
            .open_clip(&NewClip {
                camera_id: 1,
                started_at: started,
                hot_path: format!("1/{}.mp4", started.timestamp_millis()),
                codec: "h264".into(),
                container: "mp4".into(),
                hot_handle: "local".into(),
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();
        let close = ClipClose {
            ended_at: started + Duration::seconds(30),
            duration_ms: 30_000,
            size_bytes,
            hot_path: None,
            sha256: Some("a".repeat(64)),
        };
        store
            .close_clip_at(id, &close, close.ended_at)
            .await
            .unwrap();
        id
    }

    fn set_wall(store: &Store, wall: DateTime<Utc>) {
        store.delivery.clock.lock().unwrap().test_wall = Some(wall);
    }

    /// The review's reproduction: an hour holding 3.6 MB generated and an
    /// eviction, then the wall clock jumps ten days forward, a write lands
    /// there, and the clock is corrected. No window overlapping the jump
    /// reads complete, the hour's history is not pruned, and the moved
    /// `accounting_since` survives a restart.
    #[tokio::test]
    async fn recording_delivery_clock_jump_forward_then_back_leaves_no_complete_window() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = Utc::now();
        let store = store_since(dir.path(), t0).await;
        let generated = closed(&store, t0 - Duration::minutes(30), 3_600_000).await;
        let lost = closed(&store, t0 - Duration::minutes(20), 500).await;
        store.cascade_delete_clip_metadata(lost).await.unwrap();
        let sample = t0 + Duration::minutes(2);
        let before = store
            .recording_delivery_stats(|| sample, None)
            .await
            .unwrap();
        assert!(before.windows.iter().all(|w| w.complete));
        assert_eq!(before.windows[0].core.ordinary.generated_bytes, 3_600_500);

        let jumped = t0 + Duration::days(10);
        set_wall(&store, jumped);
        store
            .record_upload_attempt(RecordingSource::Ordinary, generated, jumped, 1)
            .await
            .unwrap();
        let during = store
            .recording_delivery_stats(|| jumped + Duration::minutes(2), None)
            .await
            .unwrap();
        assert!(
            during.windows.iter().all(|w| !w.complete),
            "on the jumped clock"
        );

        let corrected = t0 + Duration::minutes(1);
        set_wall(&store, corrected);
        store
            .record_upload_attempt(RecordingSource::Ordinary, generated, corrected, 1)
            .await
            .unwrap();
        let after = store
            .recording_delivery_stats(|| sample, None)
            .await
            .unwrap();
        for w in &after.windows {
            assert!(
                !w.complete,
                "the {} s window overlaps the jump",
                w.window_seconds
            );
        }
        let o = after.windows[0].core.ordinary;
        assert_eq!(
            (o.generated_bytes, o.evicted_before_upload_count),
            (3_600_500, 1),
            "the jump did not prune the hour away"
        );
        assert!(after.accounting_since > jumped, "past every jumped bucket");

        store.pool.close().await;
        let store = open(dir.path()).await;
        let reopened = store
            .recording_delivery_stats(|| sample, None)
            .await
            .unwrap();
        assert_eq!(reopened.accounting_since, after.accounting_since);
    }

    /// A restart after twenty minutes down is not a discontinuity: the new
    /// process has no earlier reading to drift from, and the wall clock is
    /// past every bucket.
    #[tokio::test]
    async fn recording_delivery_restart_without_clock_jump_keeps_windows_complete() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = Utc::now();
        let store = store_since(dir.path(), t0).await;
        set_wall(&store, t0 - Duration::minutes(30));
        closed(&store, t0 - Duration::minutes(31), 1_000).await;
        let since = store
            .recording_delivery_stats(|| t0, None)
            .await
            .unwrap()
            .accounting_since;
        store.pool.close().await;

        let store = open(dir.path()).await;
        set_wall(&store, t0 - Duration::minutes(10));
        closed(&store, t0 - Duration::minutes(11), 2_000).await;
        let snap = store.recording_delivery_stats(|| t0, None).await.unwrap();
        assert_eq!(snap.accounting_since, since);
        assert!(snap.windows.iter().all(|w| w.complete));
        assert_eq!(snap.windows[0].core.ordinary.generated_bytes, 3_000);
    }

    /// A forward jump detected by a write whose transaction then rolls
    /// back (failed or cancelled), with the clock left jumped. Returns the
    /// jumped wall time and the queued clip.
    async fn jump_in_rolled_back_write(store: &Store, t0: DateTime<Utc>) -> (DateTime<Utc>, i64) {
        set_wall(store, t0);
        let clip = closed(store, t0 - Duration::minutes(30), 1_000).await;
        let jumped = t0 + Duration::days(10);
        set_wall(store, jumped);
        let mut tx = store.pool.begin().await.unwrap();
        let none = DeliveryCounters::default();
        add_to_bucket(store, &mut tx, 1, RecordingSource::Ordinary, jumped, &none)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        (jumped, clip)
    }

    /// The rolled-back write does not consume the detection: the next
    /// read still moves `accounting_since`, so no window reads complete
    /// against the old baseline.
    #[tokio::test]
    async fn recording_delivery_clock_jump_survives_a_rolled_back_write_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = Utc::now();
        let store = store_since(dir.path(), t0).await;
        let (jumped, _) = jump_in_rolled_back_write(&store, t0).await;
        let snap = store
            .recording_delivery_stats(|| jumped + Duration::minutes(2), None)
            .await
            .unwrap();
        // Persisted at millisecond precision.
        assert_eq!(
            snap.accounting_since.timestamp_millis(),
            jumped.timestamp_millis()
        );
        assert!(snap.windows.iter().all(|w| !w.complete));
    }

    /// ...and so does the next committed write, before any read.
    #[tokio::test]
    async fn recording_delivery_clock_jump_survives_a_rolled_back_write_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = Utc::now();
        let store = store_since(dir.path(), t0).await;
        let (jumped, clip) = jump_in_rolled_back_write(&store, t0).await;
        store
            .record_upload_attempt(RecordingSource::Ordinary, clip, jumped, 1)
            .await
            .unwrap();
        let since: String =
            sqlx::query_scalar("SELECT accounting_since FROM recording_delivery_baseline")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        let since = crate::motion::parse_ts(&since).unwrap();
        assert_eq!(since.timestamp_millis(), jumped.timestamp_millis());
    }

    /// A transient write failure on an attempt counter, then a delivery:
    /// the windows overlapping the failure read incomplete (never complete
    /// with attempted 0 / delivered 1000), and a gap whose own write also
    /// fails stays pending until the store recovers.
    #[tokio::test]
    async fn recording_delivery_failed_accounting_write_marks_windows_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = Utc::now();
        let store = store_since(dir.path(), t0 - Duration::hours(2)).await;
        set_wall(&store, t0);
        let clip = closed(&store, t0 - Duration::minutes(30), 1_000).await;
        for t in ["recording_minute_buckets", "recording_delivery_baseline"] {
            sqlx::query(&format!(
                "CREATE TRIGGER fail_{t} BEFORE {} ON {t}
                 BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
                if t == "recording_minute_buckets" {
                    "INSERT"
                } else {
                    "UPDATE"
                }
            ))
            .execute(&store.pool)
            .await
            .unwrap();
        }
        store
            .record_upload_attempt(RecordingSource::Ordinary, clip, t0, 1_000)
            .await
            .unwrap_err();
        let failed_at = t0 + Duration::seconds(5);
        store.record_accounting_gap(failed_at).await;
        // The baseline is still unmoved: the gap is only pending.
        sqlx::query("DROP TRIGGER fail_recording_minute_buckets")
            .execute(&store.pool)
            .await
            .unwrap();
        set_wall(&store, failed_at);
        store
            .mark_cold_uploaded(
                clip,
                &crate::ClipColdMark {
                    cold_handle: "local".into(),
                    cold_path: "p".into(),
                    cold_uploaded_at: failed_at,
                },
                Some(1_000),
            )
            .await
            .unwrap_err();
        sqlx::query("DROP TRIGGER fail_recording_delivery_baseline")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .mark_cold_uploaded(
                clip,
                &crate::ClipColdMark {
                    cold_handle: "local".into(),
                    cold_path: "p".into(),
                    cold_uploaded_at: failed_at,
                },
                Some(1_000),
            )
            .await
            .unwrap();
        let snap = store
            .recording_delivery_stats(|| failed_at + Duration::minutes(2), None)
            .await
            .unwrap();
        assert_eq!(snap.windows[0].core.ordinary.delivered_bytes, 1_000);
        assert_eq!(snap.windows[0].core.ordinary.attempted_bytes, 0);
        for w in &snap.windows {
            assert!(
                !w.complete,
                "the {} s window overlaps the gap",
                w.window_seconds
            );
        }
        assert_eq!(
            snap.accounting_since.timestamp_millis(),
            failed_at.timestamp_millis()
        );
    }
}

//! Rolling 24-hour buffer of host [`SystemMetrics`] snapshots
//! (migration `0024_metrics_samples.sql`).
//!
//! The engine samples `nexus-engine`'s `system_metrics::render()` every
//! 5 seconds and calls [`Store::insert_metrics_sample`] with the full
//! JSON blob. The cloud console reads it back via the admin
//! metrics-history endpoint to draw a "last 24 hours" trend view for a
//! core while that core is online. Nothing here ever crosses the tunnel
//! on its own — the pull is on-demand and viewer-gated.
//!
//! ## Two-tier retention
//!
//! [`Store::prune_metrics_samples`] runs on a wall-clock tick and keeps
//! the table bounded:
//!
//! * **< 60 min:** full 5-second resolution (powers the console's
//!   "full resolution" toggle and its live tail).
//! * **60 min .. 24 h:** coarsened to one sample per 5-minute boundary.
//! * **> 24 h:** dropped.
//!
//! The row key `captured_at_ms` is floored to the 5-second sample grid
//! at insert time (the true capture instant is preserved inside
//! `payload.captured_at`). Because `300_000` is a whole multiple of
//! `5_000`, "on a 5-minute boundary" is exactly `captured_at_ms %
//! 300_000 = 0`, so the coarsening prune is a pure modulo and the
//! `PRIMARY KEY` dedups jittered ticks that land in the same slot.
//!
//! ## No PII
//!
//! `payload` is the verbatim `SystemMetrics` JSON — host / CPU / memory
//! / GPU / NPU / disk counters plus the engine process RSS. No camera
//! credentials, no identity data, no operator secrets. See the migration
//! header for the diagnostics-snapshot interaction.

use sqlx::Row as _;

use crate::{Store, StoreError};

/// The 5-second sampling grid the engine writes on. Row keys are floored
/// to this so pruning and dedup are exact.
const FINE_INTERVAL_MS: i64 = 5_000;
/// The boundary spacing retained for samples older than the full-res
/// window: one row every 5 minutes.
const COARSE_BUCKET_MS: i64 = 300_000;
/// Keep full 5-second resolution for the most recent 60 minutes.
const FINE_WINDOW_MS: i64 = 60 * 60 * 1000;
/// Hard cap — nothing older than 24 hours survives a prune.
const MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;

impl Store {
    /// Insert one metrics sample. `captured_at_ms` is the wall-clock
    /// capture instant in Unix epoch milliseconds; it is floored to the
    /// 5-second grid to form the row key (the un-floored instant stays
    /// inside `payload`). `payload` is the compact `SystemMetrics` JSON
    /// blob, stored verbatim. Two ticks that land in the same 5-second
    /// slot collapse to one row (last write wins).
    pub async fn insert_metrics_sample(
        &self,
        captured_at_ms: i64,
        payload: &str,
    ) -> Result<(), StoreError> {
        let slot = captured_at_ms / FINE_INTERVAL_MS * FINE_INTERVAL_MS;
        sqlx::query(
            "INSERT INTO metrics_samples (captured_at_ms, payload)
             VALUES (?, ?)
             ON CONFLICT (captured_at_ms) DO UPDATE SET payload = excluded.payload",
        )
        .bind(slot)
        .bind(payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Return sample payloads within a window, one representative per
    /// bucket, oldest → newest.
    ///
    /// * `now_ms` — caller's "now" in Unix epoch ms (the window anchor).
    /// * `window_secs` — how far back from `now_ms` to include.
    /// * `bucket_secs` — downsample width. `5` returns every stored
    ///   sample (full resolution); `300` returns one per 5-minute
    ///   bucket. Within a bucket the earliest sample's payload is
    ///   returned (SQLite's documented bare-column-with-`MIN()` rule).
    /// * `since_ms` — optional cursor for delta tailing. Floored to the
    ///   sample grid and clamped to the window floor; the comparison is
    ///   inclusive so the caller's newest slot is always re-returned
    ///   (never skipped) and the client dedups by `captured_at`.
    pub async fn list_metrics_samples(
        &self,
        now_ms: i64,
        window_secs: i64,
        bucket_secs: i64,
        since_ms: Option<i64>,
    ) -> Result<Vec<String>, StoreError> {
        let bucket_ms = bucket_secs.max(1).saturating_mul(1000);
        let window_ms = window_secs.max(0).saturating_mul(1000);
        let floor_ms = now_ms.saturating_sub(window_ms);
        let start_ms = match since_ms {
            Some(s) => (s / FINE_INTERVAL_MS * FINE_INTERVAL_MS).max(floor_ms),
            None => floor_ms,
        };
        let rows = sqlx::query(
            "SELECT payload, MIN(captured_at_ms) AS ts
               FROM metrics_samples
              WHERE captured_at_ms >= ?
              GROUP BY captured_at_ms / ?
              ORDER BY ts ASC",
        )
        .bind(start_ms)
        .bind(bucket_ms)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(row.try_get::<String, _>("payload")?);
        }
        Ok(out)
    }

    /// Enforce the two-tier retention policy relative to `now_ms`.
    /// Returns the total number of rows deleted. Idempotent — safe to
    /// call on every sweeper tick.
    pub async fn prune_metrics_samples(&self, now_ms: i64) -> Result<u64, StoreError> {
        // Tier 1 — hard 24h cap.
        let day_floor = now_ms.saturating_sub(MAX_AGE_MS);
        let r1 = sqlx::query("DELETE FROM metrics_samples WHERE captured_at_ms < ?")
            .bind(day_floor)
            .execute(&self.pool)
            .await?;
        // Tier 2 — coarsen everything older than the full-res window to
        // 5-minute boundaries. Rows sit on the 5s grid and 300000 is a
        // multiple of 5000, so "on a boundary" is exactly `% 300000 = 0`.
        let fine_floor = now_ms.saturating_sub(FINE_WINDOW_MS);
        let r2 = sqlx::query(
            "DELETE FROM metrics_samples
              WHERE captured_at_ms < ?
                AND captured_at_ms % ? <> 0",
        )
        .bind(fine_floor)
        .bind(COARSE_BUCKET_MS)
        .execute(&self.pool)
        .await?;
        Ok(r1.rows_affected() + r2.rows_affected())
    }

    /// Aggressive pressure prune: drop every metrics sample older than
    /// `cutoff_ms` outright, with no coarsening tier. Called by the
    /// storage-safety ladder when the disk tips into Low/Panic to
    /// reclaim the rolling host-metrics buffer — the cloud console
    /// keeps its own copy, so shedding local history under disk
    /// pressure is safe. Returns the number of rows deleted.
    pub async fn prune_metrics_samples_older_than(
        &self,
        cutoff_ms: i64,
    ) -> Result<u64, StoreError> {
        let res = sqlx::query("DELETE FROM metrics_samples WHERE captured_at_ms < ?")
            .bind(cutoff_ms)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::StoreConfig;
    use tempfile::TempDir;

    async fn fresh_store() -> (Store, TempDir) {
        let tmp = TempDir::new().expect("tempdir");
        let cfg = StoreConfig {
            url: format!("sqlite://{}/store.db?mode=rwc", tmp.path().display()),
            ..StoreConfig::default()
        };
        let store = Store::open(&cfg).await.expect("store open");
        (store, tmp)
    }

    /// Parse the `captured_at_ms` echoed into a test payload blob.
    fn ts_of(payload: &str) -> i64 {
        let v: serde_json::Value = serde_json::from_str(payload).expect("payload json");
        v["captured_at_ms"].as_i64().expect("captured_at_ms")
    }

    /// Insert 90 minutes of 5-second samples ending exactly on a
    /// 5-minute boundary anchored `now_ms`. Returns `(now_ms, inserted)`.
    async fn seed_90min(store: &Store) -> (i64, Vec<i64>) {
        // Anchor `now_ms` on a 5-minute boundary so the modulo math in
        // the assertions is exact.
        let now_ms = 1_000_000_000_000i64 / COARSE_BUCKET_MS * COARSE_BUCKET_MS;
        let span_ms = 90 * 60 * 1000;
        let mut inserted = Vec::new();
        let mut t = now_ms - span_ms;
        while t <= now_ms {
            let payload = format!("{{\"captured_at_ms\":{t}}}");
            store.insert_metrics_sample(t, &payload).await.unwrap();
            inserted.push(t);
            t += FINE_INTERVAL_MS;
        }
        (now_ms, inserted)
    }

    #[tokio::test]
    async fn insert_floors_to_grid_and_dedups() {
        let (store, _tmp) = fresh_store().await;
        // Two off-grid instants in the same 5s slot collapse to one row.
        let base = 1_000_000_000_000i64 / FINE_INTERVAL_MS * FINE_INTERVAL_MS;
        store
            .insert_metrics_sample(base + 1234, "{\"captured_at_ms\":1}")
            .await
            .unwrap();
        store
            .insert_metrics_sample(base + 4999, "{\"captured_at_ms\":2}")
            .await
            .unwrap();
        let rows = store
            .list_metrics_samples(base + FINE_INTERVAL_MS, 86_400, 5, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "same-slot inserts must collapse to one row");
        assert_eq!(ts_of(&rows[0]), 2, "last write wins");
    }

    #[tokio::test]
    async fn full_resolution_returns_every_recent_sample() {
        let (store, _tmp) = fresh_store().await;
        let (now_ms, inserted) = seed_90min(&store).await;
        // Full-res, last 60 minutes: every 5s sample present, in order.
        let rows = store
            .list_metrics_samples(now_ms, 3_600, 5, None)
            .await
            .unwrap();
        let got: Vec<i64> = rows.iter().map(|p| ts_of(p)).collect();
        let want: Vec<i64> = inserted
            .iter()
            .copied()
            .filter(|t| *t >= now_ms - 3_600_000)
            .collect();
        assert_eq!(got, want, "full-res tail must return every grid sample");
    }

    #[tokio::test]
    async fn bucketing_picks_earliest_per_bucket_in_order() {
        let (store, _tmp) = fresh_store().await;
        let (now_ms, _inserted) = seed_90min(&store).await;
        let rows = store
            .list_metrics_samples(now_ms, 86_400, 300, None)
            .await
            .unwrap();
        let ts: Vec<i64> = rows.iter().map(|p| ts_of(p)).collect();
        assert!(!ts.is_empty(), "expected some buckets");
        // Strictly increasing, and each row is the earliest of its bucket.
        for w in ts.windows(2) {
            assert!(w[0] < w[1], "bucketed samples must be strictly increasing");
        }
        let buckets: std::collections::BTreeSet<i64> =
            ts.iter().map(|t| t / COARSE_BUCKET_MS).collect();
        assert_eq!(buckets.len(), ts.len(), "one representative per bucket");
    }

    #[tokio::test]
    async fn prune_keeps_fine_recent_and_coarsens_old() {
        let (store, _tmp) = fresh_store().await;
        let (now_ms, _inserted) = seed_90min(&store).await;
        store.prune_metrics_samples(now_ms).await.unwrap();

        // Enumerate every surviving row (bucket=5 => one bucket per slot).
        let rows = store
            .list_metrics_samples(now_ms, 86_400, 5, None)
            .await
            .unwrap();
        let survivors: Vec<i64> = rows.iter().map(|p| ts_of(p)).collect();

        let fine_floor = now_ms - FINE_WINDOW_MS;
        // Everything within the full-res window is kept at 5s cadence.
        let fine: Vec<i64> = survivors
            .iter()
            .copied()
            .filter(|t| *t >= fine_floor)
            .collect();
        let mut want_fine = Vec::new();
        let mut t = fine_floor;
        while t <= now_ms {
            want_fine.push(t);
            t += FINE_INTERVAL_MS;
        }
        assert_eq!(fine, want_fine, "full-res window must be untouched");

        // Older survivors are all on 5-minute boundaries.
        for t in survivors.iter().copied().filter(|t| *t < fine_floor) {
            assert_eq!(
                t % COARSE_BUCKET_MS,
                0,
                "coarse survivors must be 5-min boundaries"
            );
        }
        // And every 5-minute boundary in the coarse region survived.
        let coarse: std::collections::BTreeSet<i64> = survivors
            .iter()
            .copied()
            .filter(|t| *t < fine_floor)
            .collect();
        let mut b = now_ms - 90 * 60 * 1000;
        while b < fine_floor {
            if b % COARSE_BUCKET_MS == 0 {
                assert!(coarse.contains(&b), "5-min boundary {b} must survive");
            }
            b += FINE_INTERVAL_MS;
        }
    }

    #[tokio::test]
    async fn since_ms_tails_forward_without_gaps() {
        let (store, _tmp) = fresh_store().await;
        let (now_ms, _inserted) = seed_90min(&store).await;
        // Cursor a few slots below now; expect only at-or-after rows.
        let cursor = now_ms - 3 * FINE_INTERVAL_MS + 1234; // off-grid on purpose
        let rows = store
            .list_metrics_samples(now_ms, 3_600, 5, Some(cursor))
            .await
            .unwrap();
        let ts: Vec<i64> = rows.iter().map(|p| ts_of(p)).collect();
        // Floors to now-3 slots (inclusive) => 4 rows: now-3,-2,-1,now.
        assert_eq!(
            ts,
            vec![
                now_ms - 3 * FINE_INTERVAL_MS,
                now_ms - 2 * FINE_INTERVAL_MS,
                now_ms - FINE_INTERVAL_MS,
                now_ms,
            ],
        );
    }

    #[tokio::test]
    async fn prune_older_than_drops_only_older() {
        let (store, _tmp) = fresh_store().await;
        let now_ms = 1_000_000_000_000i64 / FINE_INTERVAL_MS * FINE_INTERVAL_MS;
        // Three samples: 3h old, 1h old, and "now".
        for age_ms in [3 * 3_600_000i64, 3_600_000, 0] {
            let t = now_ms - age_ms;
            store
                .insert_metrics_sample(t, &format!("{{\"captured_at_ms\":{t}}}"))
                .await
                .unwrap();
        }
        // Keep only the last 2h: the 3h-old sample is dropped.
        let deleted = store
            .prune_metrics_samples_older_than(now_ms - 2 * 3_600_000)
            .await
            .unwrap();
        assert_eq!(deleted, 1, "only the 3h-old sample precedes the 2h cutoff");
        let remaining = store
            .list_metrics_samples(now_ms, 86_400, 5, None)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 2, "the 1h-old and current samples survive");
    }

    #[tokio::test]
    async fn wal_checkpoint_truncate_is_ok() {
        let (store, _tmp) = fresh_store().await;
        store
            .insert_metrics_sample(5_000, "{\"captured_at_ms\":5000}")
            .await
            .unwrap();
        // Best-effort; a fresh store must at least not error.
        store.checkpoint_wal_truncate().await.unwrap();
    }
}

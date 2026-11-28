//! SPEC-089 Task 2 — bounded summaries of the shadow recording-activity
//! policy (migration `0042_recording_activity_diagnostics.sql`): one row per
//! camera-minute of counters and one per finalized legacy clip, kept seven
//! days. The pipeline serializes the counters; this module stores and prunes.

use nexus_types::CameraId;

use crate::{ClipId, Store, StoreError};

/// Summaries older than this, before the newest write, are dropped.
pub const RECORDING_ACTIVITY_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// One camera-minute of one supervisor generation's shadow counters, under
/// the policy revision that counted them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingActivityMinute {
    pub camera_id: CameraId,
    pub generation: i64,
    pub policy_revision: String,
    /// Index of the generation's monotonic minute.
    pub minute: i64,
    /// Wall clock of the minute's first frame, Unix epoch ms.
    pub started_at_ms: i64,
    pub counters_json: String,
}

/// One finalized legacy clip as the shadow policy saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingActivityEpisode {
    pub camera_id: CameraId,
    pub generation: i64,
    pub clip_id: ClipId,
    pub policy_revision: String,
    pub opened_at_ms: i64,
    pub closed_at_ms: i64,
    pub open_reason: &'static str,
    pub close_reason: &'static str,
    pub elapsed_json: String,
}

impl Store {
    /// Write one camera-minute, then prune summaries past retention relative
    /// to it. A minute already written is refused, never replaced, so the
    /// caller reports the gap instead of losing the counters already there.
    pub async fn insert_recording_activity_minute(
        &self,
        row: &RecordingActivityMinute,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO recording_activity_minutes
                 (camera_id, generation, policy_revision, minute, started_at_ms, counters)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(row.camera_id)
        .bind(row.generation)
        .bind(&row.policy_revision)
        .bind(row.minute)
        .bind(row.started_at_ms)
        .bind(&row.counters_json)
        .execute(&self.pool)
        .await?;
        self.prune_recording_activity(row.started_at_ms).await?;
        Ok(())
    }

    /// Write one clip's episode summary, then prune past retention.
    pub async fn insert_recording_activity_episode(
        &self,
        row: &RecordingActivityEpisode,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO recording_activity_episodes
                 (camera_id, generation, clip_id, policy_revision, opened_at_ms,
                  closed_at_ms, open_reason, close_reason, elapsed)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(row.camera_id)
        .bind(row.generation)
        .bind(row.clip_id)
        .bind(&row.policy_revision)
        .bind(row.opened_at_ms)
        .bind(row.closed_at_ms)
        .bind(row.open_reason)
        .bind(row.close_reason)
        .bind(&row.elapsed_json)
        .execute(&self.pool)
        .await?;
        self.prune_recording_activity(row.closed_at_ms).await?;
        Ok(())
    }

    /// Drop every summary older than [`RECORDING_ACTIVITY_RETENTION_MS`]
    /// before `now_ms`. Returns the rows deleted.
    pub async fn prune_recording_activity(&self, now_ms: i64) -> Result<u64, StoreError> {
        let floor = now_ms.saturating_sub(RECORDING_ACTIVITY_RETENTION_MS);
        let minutes = sqlx::query("DELETE FROM recording_activity_minutes WHERE started_at_ms < ?")
            .bind(floor)
            .execute(&self.pool)
            .await?;
        let episodes =
            sqlx::query("DELETE FROM recording_activity_episodes WHERE closed_at_ms < ?")
                .bind(floor)
                .execute(&self.pool)
                .await?;
        Ok(minutes.rows_affected() + episodes.rows_affected())
    }

    /// A camera's minutes, oldest first.
    pub async fn list_recording_activity_minutes(
        &self,
        camera_id: CameraId,
    ) -> Result<Vec<RecordingActivityMinute>, StoreError> {
        let rows: Vec<(i64, String, i64, i64, String)> = sqlx::query_as(
            "SELECT generation, policy_revision, minute, started_at_ms, counters
               FROM recording_activity_minutes
              WHERE camera_id = ?
              ORDER BY generation, started_at_ms, minute",
        )
        .bind(camera_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(generation, policy_revision, minute, started_at_ms, counters_json)| {
                    RecordingActivityMinute {
                        camera_id,
                        generation,
                        policy_revision,
                        minute,
                        started_at_ms,
                        counters_json,
                    }
                },
            )
            .collect())
    }

    /// A camera's episodes, in the order they were written.
    pub async fn list_recording_activity_episodes(
        &self,
        camera_id: CameraId,
    ) -> Result<Vec<RecordingActivityEpisode>, StoreError> {
        type Row = (i64, ClipId, String, i64, i64, String, String, String);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT generation, clip_id, policy_revision, opened_at_ms, closed_at_ms,
                    open_reason, close_reason, elapsed
               FROM recording_activity_episodes
              WHERE camera_id = ?
              ORDER BY id",
        )
        .bind(camera_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(
                |(generation, clip_id, policy_revision, opened, closed, open, close, elapsed)| {
                    Ok(RecordingActivityEpisode {
                        camera_id,
                        generation,
                        clip_id,
                        policy_revision,
                        opened_at_ms: opened,
                        closed_at_ms: closed,
                        open_reason: reason(&open)?,
                        close_reason: reason(&close)?,
                        elapsed_json: elapsed,
                    })
                },
            )
            .collect()
    }
}

/// The migration's CHECK admits exactly these.
fn reason(stored: &str) -> Result<&'static str, StoreError> {
    [
        "activity",
        "post_roll",
        "degraded_coverage",
        "quiet",
        "minimum_duration",
    ]
    .into_iter()
    .find(|r| *r == stored)
    .ok_or_else(|| StoreError::Decode(format!("recording reason {stored:?}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use nexus_config::StoreConfig;

    async fn open(dir: &std::path::Path) -> Store {
        Store::open(&StoreConfig {
            url: format!("sqlite://{}/store.db?mode=rwc", dir.display()),
            ..StoreConfig::default()
        })
        .await
        .expect("store open")
    }

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    const NOW: i64 = 1_790_000_000_000;

    fn minute(camera_id: CameraId, minute: i64, started_at_ms: i64) -> RecordingActivityMinute {
        RecordingActivityMinute {
            camera_id,
            generation: 4,
            policy_revision: "rev-a".into(),
            minute,
            started_at_ms,
            counters_json: format!("{{\"minute\":{minute}}}"),
        }
    }

    fn episode(clip_id: ClipId, closed_at_ms: i64) -> RecordingActivityEpisode {
        RecordingActivityEpisode {
            camera_id: 1,
            generation: 4,
            clip_id,
            policy_revision: "sha256:abc".into(),
            opened_at_ms: closed_at_ms - 12_000,
            closed_at_ms,
            open_reason: "activity",
            close_reason: "quiet",
            elapsed_json: "{\"activity\":2000,\"post_roll\":10000}".into(),
        }
    }

    /// The migration applies once, rows survive a restart, a minute written
    /// again is refused and keeps what it had, and nothing reads across cameras.
    #[tokio::test]
    async fn summaries_survive_a_restart_and_a_minute_is_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).await;
        store
            .insert_recording_activity_minute(&minute(1, 0, NOW))
            .await
            .unwrap();
        store
            .insert_recording_activity_minute(&minute(2, 0, NOW))
            .await
            .unwrap();
        store
            .insert_recording_activity_episode(&episode(9, NOW))
            .await
            .unwrap();
        drop(store);

        let store = open(dir.path()).await;
        let mut again = minute(1, 0, NOW);
        again.counters_json = "{\"minute\":0,\"again\":true}".into();
        assert!(store
            .insert_recording_activity_minute(&again)
            .await
            .is_err());
        store
            .insert_recording_activity_minute(&minute(1, 1, NOW + 60_000))
            .await
            .unwrap();
        assert_eq!(
            store.list_recording_activity_minutes(1).await.unwrap(),
            vec![minute(1, 0, NOW), minute(1, 1, NOW + 60_000)]
        );
        assert_eq!(
            store.list_recording_activity_episodes(1).await.unwrap(),
            vec![episode(9, NOW)]
        );
        assert!(store
            .list_recording_activity_episodes(2)
            .await
            .unwrap()
            .is_empty());
    }

    /// A write keeps exactly the last seven days before it, of both kinds.
    #[tokio::test]
    async fn writes_prune_summaries_older_than_seven_days() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).await;
        let oldest_kept = NOW - 7 * DAY_MS;
        for (m, at) in [(0, oldest_kept - 1), (1, oldest_kept), (2, NOW - DAY_MS)] {
            store
                .insert_recording_activity_minute(&minute(1, m, at))
                .await
                .unwrap();
        }
        store
            .insert_recording_activity_episode(&episode(1, oldest_kept - 1))
            .await
            .unwrap();
        store
            .insert_recording_activity_episode(&episode(2, oldest_kept))
            .await
            .unwrap();
        // Nothing is past retention relative to any of these writes.
        assert_eq!(
            store
                .list_recording_activity_minutes(1)
                .await
                .unwrap()
                .len(),
            3
        );

        store
            .insert_recording_activity_minute(&minute(1, 3, NOW))
            .await
            .unwrap();
        let minutes: Vec<i64> = store
            .list_recording_activity_minutes(1)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.minute)
            .collect();
        assert_eq!(minutes, [1, 2, 3]);
        let clips: Vec<ClipId> = store
            .list_recording_activity_episodes(1)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.clip_id)
            .collect();
        assert_eq!(clips, [2]);
        assert_eq!(
            store
                .prune_recording_activity(NOW + 8 * DAY_MS)
                .await
                .unwrap(),
            4
        );
    }

    /// The real migrations, in an in-memory database.
    async fn in_memory() -> Store {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        Store::apply_schema(&pool).await.unwrap();
        Store {
            pool,
            delivery: Default::default(),
        }
    }

    /// A live policy change mid-minute: the earlier revision's counters for
    /// that minute stay beside the new one's.
    #[tokio::test]
    async fn each_policy_revision_keeps_its_own_minute() {
        let store = in_memory().await;
        let earlier = minute(1, 0, NOW);
        let later = RecordingActivityMinute {
            policy_revision: "rev-b".into(),
            started_at_ms: NOW + 30_000,
            counters_json: "{\"rev\":\"b\"}".into(),
            ..minute(1, 0, NOW)
        };
        store
            .insert_recording_activity_minute(&earlier)
            .await
            .unwrap();
        store
            .insert_recording_activity_minute(&later)
            .await
            .unwrap();
        assert_eq!(
            store.list_recording_activity_minutes(1).await.unwrap(),
            vec![earlier, later]
        );
    }

    /// Observed mode holds a short clip for the recorder's minimum
    /// duration: its episode keeps that reason.
    #[tokio::test]
    async fn a_minimum_duration_close_is_kept() {
        let store = in_memory().await;
        let mut held = episode(1, NOW);
        held.close_reason = "minimum_duration";
        store
            .insert_recording_activity_episode(&held)
            .await
            .unwrap();
        assert_eq!(
            store.list_recording_activity_episodes(1).await.unwrap(),
            vec![held]
        );
    }

    #[tokio::test]
    async fn an_unknown_reason_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).await;
        let mut bad = episode(1, NOW);
        bad.open_reason = "tracker";
        assert!(store.insert_recording_activity_episode(&bad).await.is_err());
    }
}

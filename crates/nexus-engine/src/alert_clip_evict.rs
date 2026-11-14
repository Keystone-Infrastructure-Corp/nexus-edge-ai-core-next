//! M-Alert-Clip P6 — hot-storage reclaim for delivered alert clips.
//!
//! An alert clip is a transient artifact: once every alert event that
//! shares it has been delivered to all of its sinks (no `pending` /
//! `failed` outbox rows remain), the on-disk MP4 is no longer needed and
//! is reclaimed here. This periodic sweeper deletes the file and marks
//! the `alert_clips` row `evicted` (the row is kept for audit — "this
//! alert HAD a clip that was delivered then reclaimed").
//!
//! Distinct from the daily motion-clip retention sweep: alert clips are
//! reclaimed on *delivery completion*, not on age, so this runs on a
//! short interval. It is a cheap indexed query when the feature is off
//! (no `ready` rows exist), so it always runs regardless of config.
//!
//! M-Alert-Clip cloud delivery adds a second gate: a `ready` clip is
//! only reclaimed once it has been cold-replicated to the cloud OR its
//! [`COLD_GRACE`] window has elapsed. That keeps the burned-in evidence
//! on the hot tier long enough for an enrolled core to ship it to the
//! console, while still guaranteeing hot-space reclaim on un-enrolled /
//! cold-disabled boxes (fail-open — the local experience never depends
//! on cloud reachability).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use nexus_store::Store;
use tracing::{debug, info, warn};

/// How many evictable clips to reclaim per sweep. Bounds one pass so a
/// large backlog (e.g. after a delivery outage clears) doesn't monopolise
/// the task; the next tick picks up the remainder.
const EVICT_BATCH: i64 = 64;

/// Grace window a `ready` alert clip is held on the hot tier waiting for
/// cloud cold-replication before it is reclaimed regardless. Sized well
/// above the cold replicator's retry cadence so an enrolled core reliably
/// ships the clip first, yet short enough that hot space is always
/// reclaimed promptly when there is no cloud (un-enrolled / cold
/// disabled / backend down past the window).
const COLD_GRACE: Duration = Duration::from_secs(15 * 60);

/// Tunables for [`run_alert_clip_evictor`].
pub struct AlertClipEvictorConfig {
    /// Root the `alert_clips.path` values are relative to (same
    /// `clips_dir` as motion clips).
    pub clips_dir: PathBuf,
    /// Time between sweeps.
    pub interval: Duration,
}

/// Run the evictor until `shutdown` resolves. Mirrors the retention /
/// dispatcher task shape: a `select!` over the interval and the
/// shutdown future.
pub async fn run_alert_clip_evictor(
    cfg: AlertClipEvictorConfig,
    store: Arc<Store>,
    shutdown: impl std::future::Future<Output = ()>,
) {
    info!(
        interval_secs = cfg.interval.as_secs(),
        "alert-clip evictor starting"
    );
    let mut interval = tokio::time::interval(cfg.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate t=0 tick so the first real sweep waits one
    // interval (nothing is deliverable at boot).
    interval.tick().await;

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!("alert-clip evictor: shutdown requested");
                return;
            }
            _ = interval.tick() => {
                sweep(&cfg, &store).await;
            }
        }
    }
}

async fn sweep(cfg: &AlertClipEvictorConfig, store: &Arc<Store>) {
    let cold_grace_cutoff = Utc::now()
        - chrono::Duration::from_std(COLD_GRACE).unwrap_or_else(|_| chrono::Duration::minutes(15));
    reclaim_evictable_alert_clips(store, &cfg.clips_dir, cold_grace_cutoff, EVICT_BATCH).await;
}

/// Reclaim up to `batch` evictable alert clips: unlink each hot MP4
/// and flip its row to `evicted`. `cold_grace_cutoff` is passed
/// straight through to [`Store::alert_clips_evictable`] — the periodic
/// sweeper passes `now - COLD_GRACE` (hold un-replicated clips long
/// enough for an enrolled core to ship them to the cloud), while the
/// storage-safety pressure ladder passes `now` to drop the grace wait
/// and reclaim delivered clips immediately. The delivery-drained gate
/// lives inside the query, so this NEVER destroys an alert clip whose
/// alarm is still being delivered. Returns the number reclaimed.
pub async fn reclaim_evictable_alert_clips(
    store: &Arc<Store>,
    clips_dir: &Path,
    cold_grace_cutoff: DateTime<Utc>,
    batch: i64,
) -> usize {
    let rows = match store.alert_clips_evictable(batch, cold_grace_cutoff).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "alert-clip evictor: alert_clips_evictable failed");
            return 0;
        }
    };
    let mut reclaimed = 0;
    for ac in rows {
        let path = clips_dir.join(&ac.path);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => debug!(id = ac.id, path = %path.display(), "reclaimed alert clip"),
            // Already gone (double sweep, or never written): fine.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(id = ac.id, path = %path.display(), error = %e,
                      "alert-clip evictor: remove_file failed");
                // Don't mark evicted if we couldn't remove the file —
                // retry next sweep.
                continue;
            }
        }
        if let Err(e) = store.mark_alert_clip_evicted(ac.id).await {
            warn!(id = ac.id, error = %e, "alert-clip evictor: mark_alert_clip_evicted failed");
        } else {
            reclaimed += 1;
        }
    }
    reclaimed
}

/// Boot-time: resolve alert clips a previous process left `building`.
/// Run before the recorder exists, so no build of this process can be
/// swept. Marks them `failed` (the build never finished) and unlinks the
/// partial and any final-path file; a file that cannot be unlinked leaves
/// its row `building` to retry next boot.
pub async fn fail_interrupted_alert_clips(store: &Store, clips_dir: &Path) {
    let rows = match store.building_alert_clips().await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "alert-clip boot reconcile: building_alert_clips failed");
            return;
        }
    };
    for (id, rel) in rows {
        let fin = clips_dir.join(&rel);
        let partial = fin.with_extension("partial.mp4");
        let mut removed = true;
        for p in [&partial, &fin] {
            match tokio::fs::remove_file(p).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(id, path = %p.display(), error = %e, "alert-clip boot reconcile: remove_file failed");
                    removed = false;
                }
            }
        }
        if removed {
            match store.mark_alert_clip_failed(id).await {
                Ok(()) => info!(
                    id,
                    "alert-clip boot reconcile: interrupted build marked failed"
                ),
                Err(e) => warn!(id, error = %e, "alert-clip boot reconcile: mark failed errored"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retention::tests::fixture;

    /// BUG-343: a build interrupted by an engine restart stayed
    /// `building` forever.
    #[tokio::test]
    async fn interrupted_building_clip_is_failed_and_its_files_removed() {
        let (store, _dir, clips_dir) = fixture().await;
        let now = Utc::now();
        let rel = format!(
            "alert/1/{}/{}.mp4",
            now.format("%Y-%m-%d"),
            now.timestamp_millis()
        );
        let id = store
            .insert_alert_clip(&nexus_store::NewAlertClip {
                camera_id: 1,
                started_at: now,
                path: rel.clone(),
            })
            .await
            .unwrap();
        let partial = clips_dir.join(&rel).with_extension("partial.mp4");
        std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
        std::fs::write(&partial, b"half").unwrap();

        fail_interrupted_alert_clips(&store, &clips_dir).await;

        assert!(!partial.exists(), "partial must be removed");
        assert!(store.building_alert_clips().await.unwrap().is_empty());
        // Terminal: a late ready stamp must not resurrect it.
        assert!(store.mark_alert_clip_ready(id, 1, 1, None).await.is_err());
    }
}

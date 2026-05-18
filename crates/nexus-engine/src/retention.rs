//! Retention sweeper + orphan-file scan — M2.1 Stage A (PR 6).
//!
//! Two background jobs that share one tokio task:
//!
//! 1. **Retention sweeper.** Once a day, deletes every
//!    `motion_clips` row whose `started_at` is older than
//!    `motion_clips_retention_days`, then unlinks the file on disk.
//!    This is the polite, configurable counterpart to the watermark
//!    eviction loop in `storage_safety.rs` — retention runs slowly
//!    in steady state; eviction is the "drop everything, save the
//!    device" panic floor.
//!
//! 2. **Orphan-file scan.** Same cadence. Walks every file under
//!    `clips_dir` and compares to `store.known_clip_paths()`.
//!    * Files on disk with no DB row -> deleted (file leaked because
//!      a previous process crashed mid-recorder.open before the
//!      `motion_clips` insert committed, or mid-eviction after the
//!      DELETE but before the unlink).
//!    * DB rows with no file -> logged at warn but NOT deleted, so
//!      operators can investigate (a dropped LUN, manual rm, etc).
//!
//! Both jobs honour `tokio::select!` against the engine's shutdown
//! signal so a Ctrl-C between sweep ticks doesn't have to wait the
//! full `interval`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use nexus_store::Store;
use tokio::time::interval;
use tracing::{debug, info, warn};

/// Cap for one sweep tick. Keeps a single retention round bounded so
/// a long-stopped engine restarting against months of clips can't
/// monopolise the runtime — the next tick picks up if more is owed.
pub const RETENTION_BATCH_SIZE: i64 = 500;

#[derive(Debug, Clone)]
pub struct RetentionConfig {
    pub clips_dir: PathBuf,
    pub retention_days: u32,
    /// How often to sweep. In production this is 24h; tests pass
    /// shorter intervals.
    pub interval: Duration,
}

/// Run the retention sweeper + orphan-file scan until cancelled.
/// Returns when the shutdown future resolves.
pub async fn run_retention(
    cfg: RetentionConfig,
    store: Arc<Store>,
    shutdown: impl std::future::Future<Output = ()>,
) {
    info!(
        clips_dir = %cfg.clips_dir.display(),
        retention_days = cfg.retention_days,
        interval_secs = cfg.interval.as_secs(),
        "retention sweeper starting"
    );

    tokio::pin!(shutdown);
    let mut tick = interval(cfg.interval);
    // First tick fires immediately so a freshly-booted engine
    // catches up on overdue retention without a 24h wait.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!("retention sweeper shutting down");
                return;
            }
            _ = tick.tick() => {}
        }

        let cutoff = Utc::now() - chrono::Duration::days(cfg.retention_days as i64);
        match sweep_once(&store, &cfg.clips_dir, cutoff).await {
            Ok(SweepResult {
                evicted,
                orphans,
                missing,
            }) => {
                if evicted == 0 && orphans == 0 && missing == 0 {
                    debug!("retention sweep idle");
                } else {
                    info!(evicted, orphans, missing, "retention sweep complete");
                }
            }
            Err(e) => warn!(error = %e, "retention sweep failed"),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SweepResult {
    /// Number of `motion_clips` rows + files deleted by retention.
    pub evicted: usize,
    /// Number of orphan files on disk deleted (had no DB row).
    pub orphans: usize,
    /// Number of DB rows whose file was missing on disk (NOT
    /// deleted; logged so operators can investigate).
    pub missing: usize,
}

/// One full sweep cycle. Public for tests + future ad-hoc API
/// invocation.
pub async fn sweep_once(
    store: &Arc<Store>,
    clips_dir: &Path,
    cutoff: DateTime<Utc>,
) -> anyhow::Result<SweepResult> {
    let mut out = SweepResult::default();

    // ---- 1. Retention ----
    let stale = store.clips_older_than(cutoff, RETENTION_BATCH_SIZE).await?;
    for clip in &stale {
        // Best-effort unlink the hot file. Soft-evicted clips have no
        // hot pointer; the cascade-delete below still tears down the
        // metadata. Cold-replicated rows are NOT special-cased here
        // because retention is a deliberate horizon eviction —
        // operators set the horizon precisely to discard everything
        // past it, including cold copies (the cold backend is then
        // responsible for its own retention; the replicator never
        // deletes from cold). Phase 4 may revisit if customers want
        // "keep cold forever" semantics.
        if let Some(hot_path) = clip.hot_path.as_deref() {
            let abs = clips_dir.join(hot_path);
            match tokio::fs::remove_file(&abs).await {
                Ok(()) => {
                    debug!(clip_id = clip.id, path = %abs.display(), "retention unlinked file")
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    debug!(clip_id = clip.id, "retention: file already gone");
                }
                Err(e) => warn!(
                    clip_id = clip.id,
                    error = %e,
                    "retention: remove_file failed; deleting metadata anyway"
                ),
            }
        }
        store.cascade_delete_clip_metadata(clip.id).await?;
        out.evicted += 1;
    }

    // ---- 2. Orphan-file scan ----
    let known: HashSet<PathBuf> = store
        .known_local_clip_paths()
        .await?
        .into_iter()
        .map(|p| clips_dir.join(p))
        .collect();
    let on_disk = walk_clip_files(clips_dir).await?;
    for path in &on_disk {
        if !known.contains(path) {
            match tokio::fs::remove_file(path).await {
                Ok(()) => {
                    info!(path = %path.display(), "orphan-file scan removed unreferenced file");
                    out.orphans += 1;
                }
                Err(e) => warn!(path = %path.display(), error = %e, "orphan-file unlink failed"),
            }
        }
    }
    let on_disk_set: HashSet<PathBuf> = on_disk.into_iter().collect();
    for path in &known {
        if !on_disk_set.contains(path) {
            warn!(
                path = %path.display(),
                "DB references clip file that does not exist on disk; row LEFT in place for operator review"
            );
            out.missing += 1;
        }
    }

    Ok(out)
}

/// Recursively collect every regular-file path under `root`.
/// Tolerates a missing root (returns empty).
async fn walk_clip_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        while let Some(entry) = rd.next_entry().await? {
            let p = entry.path();
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                out.push(p);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::{CameraConfig, StoreConfig};
    use nexus_store::NewClip;
    use std::path::PathBuf;
    use url::Url;

    async fn fixture() -> (Arc<Store>, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let db_path = dir.path().join("nexus.db");
        let store = Arc::new(
            Store::open(&StoreConfig {
                url: format!("sqlite:{}?mode=rwc", db_path.display()),
                seed_from_config: false,
                duckdb_attach: false,
                duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
            })
            .await
            .unwrap(),
        );
        store
            .upsert_camera(&CameraConfig {
                id: 1,
                name: "front".into(),
                ingest: nexus_config::CameraIngest {
                    url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                    enabled: true,
                    max_fps: 0,
                },
                detector: nexus_config::CameraDetector {
                    prompts: vec![],
                    model_override: None,
                },
                behavior: nexus_config::CameraBehavior {
                    parking_lot_mode: false,
                },
                zones: vec![],
            })
            .await
            .unwrap();
        let clips_dir = dir.path().join("clips");
        tokio::fs::create_dir_all(&clips_dir).await.unwrap();
        (store, dir, clips_dir)
    }

    /// Helper: create a `motion_clips` row + the matching on-disk
    /// file under `clips_dir`. Returns the clip_id and the absolute
    /// path written.
    async fn seed_clip(
        store: &Arc<Store>,
        clips_dir: &Path,
        camera_id: i64,
        started: DateTime<Utc>,
        rel_name: &str,
    ) -> (i64, PathBuf) {
        let abs = clips_dir.join(rel_name);
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&abs, b"stub-payload").await.unwrap();
        let clip_id = store
            .open_clip(&NewClip {
                camera_id,
                started_at: started,
                hot_path: rel_name.into(),
                codec: "stub".into(),
                container: "mp4".into(),
                hot_handle: "local".into(),
            })
            .await
            .unwrap();
        (clip_id, abs)
    }

    #[tokio::test]
    async fn retention_evicts_only_clips_older_than_cutoff() {
        let (store, _dir, clips_dir) = fixture().await;
        let now = Utc::now();
        let (old_id, old_path) = seed_clip(
            &store,
            &clips_dir,
            1,
            now - chrono::Duration::days(60),
            "cam1/old.mp4",
        )
        .await;
        let (recent_id, recent_path) = seed_clip(
            &store,
            &clips_dir,
            1,
            now - chrono::Duration::days(1),
            "cam1/recent.mp4",
        )
        .await;

        let cutoff = now - chrono::Duration::days(30);
        let res = sweep_once(&store, &clips_dir, cutoff).await.unwrap();
        assert_eq!(res.evicted, 1);
        assert_eq!(res.orphans, 0);
        assert_eq!(res.missing, 0);

        // Old gone, recent still here.
        assert!(!old_path.exists(), "old file should have been unlinked");
        assert!(recent_path.exists(), "recent file should remain");
        assert!(store.get_clip(old_id).await.unwrap().is_none());
        assert!(store.get_clip(recent_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn orphan_file_without_row_is_deleted() {
        let (store, _dir, clips_dir) = fixture().await;
        let now = Utc::now();

        // One legit clip (row + file).
        let (_id, legit) = seed_clip(&store, &clips_dir, 1, now, "cam1/legit.mp4").await;
        // One orphan: a file on disk with no DB row. Simulates a
        // crash mid-open BEFORE the insert committed.
        let orphan = clips_dir.join("cam1").join("orphan.mp4");
        tokio::fs::write(&orphan, b"crash-leftover").await.unwrap();

        let cutoff = now - chrono::Duration::days(30); // doesn't trigger retention
        let res = sweep_once(&store, &clips_dir, cutoff).await.unwrap();
        assert_eq!(res.evicted, 0);
        assert_eq!(res.orphans, 1, "orphan must be unlinked");
        assert_eq!(res.missing, 0);
        assert!(!orphan.exists(), "orphan file should be gone");
        assert!(legit.exists(), "legit file must NOT be touched");
    }

    #[tokio::test]
    async fn missing_file_with_row_is_logged_but_kept() {
        let (store, _dir, clips_dir) = fixture().await;
        let now = Utc::now();

        // Insert a row, then delete the file out from under it.
        let (clip_id, path) = seed_clip(&store, &clips_dir, 1, now, "cam1/gone.mp4").await;
        tokio::fs::remove_file(&path).await.unwrap();

        let cutoff = now - chrono::Duration::days(30);
        let res = sweep_once(&store, &clips_dir, cutoff).await.unwrap();
        assert_eq!(res.evicted, 0);
        assert_eq!(res.orphans, 0);
        assert_eq!(res.missing, 1, "missing-file count must increment");
        // Row MUST still exist — operator decides whether to delete.
        assert!(
            store.get_clip(clip_id).await.unwrap().is_some(),
            "row must NOT be auto-deleted just because the file is gone"
        );
    }

    #[tokio::test]
    async fn run_retention_runs_first_tick_then_shuts_down_on_signal() {
        let (store, _dir, clips_dir) = fixture().await;
        let now = Utc::now();
        let (_id, old_path) = seed_clip(
            &store,
            &clips_dir,
            1,
            now - chrono::Duration::days(60),
            "cam1/old.mp4",
        )
        .await;

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let cfg = RetentionConfig {
            clips_dir: clips_dir.clone(),
            retention_days: 30,
            interval: Duration::from_secs(3600), // long; we only want the first tick
        };
        let store2 = store.clone();
        let handle = tokio::spawn(async move {
            run_retention(cfg, store2, async {
                let _ = rx.await;
            })
            .await;
        });

        // Wait long enough for the immediate first tick to land.
        for _ in 0..50 {
            if !old_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !old_path.exists(),
            "first tick should have evicted the stale file"
        );

        let _ = tx.send(());
        // Shutdown should be fast — no waiting for the next interval.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("retention task did not shut down promptly")
            .expect("retention task panicked");
    }
}

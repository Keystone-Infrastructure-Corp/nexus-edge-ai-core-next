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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

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
    /// How long terminal `alert_sink_outbox` rows are kept. `0`
    /// disables outbox trimming entirely (retain the full ledger).
    pub outbox_retention_days: u32,
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
        match sweep_once(&store, &cfg.clips_dir, cutoff, ORPHAN_MIN_AGE).await {
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

        match sweep_outbox_once(&store, cfg.outbox_retention_days).await {
            Ok(0) => debug!("outbox retention idle"),
            Ok(deleted) => info!(deleted, "outbox retention sweep complete"),
            Err(e) => warn!(error = %e, "outbox retention sweep failed"),
        }
    }
}

/// Trim terminal `alert_sink_outbox` rows past the horizon.
///
/// Split out from [`sweep_once`] because it touches a different table
/// with different safety rules: clip retention unlinks files and
/// cascades metadata, whereas this is a pure row delete that must
/// never touch `pending` work.
///
/// `retention_days == 0` disables trimming — the same "retain forever"
/// escape hatch the audit sweeper offers, for operators who treat the
/// delivery ledger as a compliance artifact.
pub async fn sweep_outbox_once(store: &Arc<Store>, retention_days: u32) -> anyhow::Result<u64> {
    if retention_days == 0 {
        return Ok(0);
    }
    let cutoff = Utc::now() - chrono::Duration::days(retention_days as i64);
    let mut total = 0u64;
    // Loop in bounded batches so a first sweep against a long-neglected
    // table doesn't issue one enormous DELETE and stall the writer lock
    // that the dispatcher needs every second.
    loop {
        let deleted = store
            .prune_outbox_terminal_older_than(cutoff, RETENTION_BATCH_SIZE)
            .await?;
        total += deleted;
        if deleted < RETENTION_BATCH_SIZE as u64 {
            return Ok(total);
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
    orphan_min_age: Duration,
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
    // The known set must cover EVERY table that puts a file under
    // `clips_dir`, because the walk below owns the whole tree. It used
    // to be motion clips on the `local` backend only, which silently
    // made alert clips and the USB vault deletable (BUG-211).
    let mut known: HashSet<PathBuf> = store
        .known_clip_paths()
        .await?
        .into_iter()
        .map(|p| clips_dir.join(p))
        .collect();
    // "Spared from deletion" and "expected to exist" are different
    // sets. A `building` alert clip is spared — the file appears at the
    // final path the moment the builder renames, before the row flips
    // to `ready` — but it is not yet expected, so it must not be
    // reported missing while the encoder is still working.
    let mut expected: HashSet<PathBuf> = known.clone();
    for (rel, state) in store.known_alert_clip_paths().await? {
        let abs = clips_dir.join(rel);
        if state == "ready" {
            expected.insert(abs.clone());
        }
        known.insert(abs);
    }
    let on_disk = walk_clip_files(clips_dir).await?;
    // Files younger than this are never orphans.
    //
    // The case that needs it is the ALERT clip: `insert_alert_clip`
    // registers the FINAL path while the builder writes
    // `<name>.partial.mp4` and renames on completion, so the partial is
    // in no known set by construction and the sweep would unlink it
    // mid-write. Motion clips are NOT in this state — both recorders
    // insert their row with the in-flight path (`clip_rel_path` over
    // `inflight_clip_path`), so a motion partial was always known; its
    // only unknown window is the few ms between the rename and
    // `close_clip`'s UPDATE, when the unknown file is the finished one.
    //
    // The floor covers both, and is the backstop for the class: any
    // future writer under `clips_dir` gets this window to register its
    // row before the scanner may claim the file.
    let young_cutoff = SystemTime::now() - orphan_min_age;
    for path in &on_disk {
        if !known.contains(path) {
            if is_younger_than(path, young_cutoff).await {
                debug!(
                    path = %path.display(),
                    "orphan-file scan skipped a file younger than the grace window"
                );
                continue;
            }
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
    // A missing file is worth one warn line; a missing *medium* is not
    // worth one per clip. The USB vault mounts inside `clips_dir` and is
    // hot-pluggable by design, so a detached stick makes every one of
    // its rows absent at once — on a 20k-clip vault that is 20k lines a
    // sweep, drowning the signal this counter exists to carry. Skip a
    // row whose containing directory is gone (the medium went away) and
    // keep warning when the directory is there but the file is not
    // (the file was removed). Directory existence is memoised because
    // clips cluster by camera and day.
    let mut dir_exists: HashMap<PathBuf, bool> = HashMap::new();
    for path in &expected {
        if on_disk_set.contains(path) {
            continue;
        }
        let parent = path.parent().unwrap_or(clips_dir).to_path_buf();
        let present = match dir_exists.get(&parent) {
            Some(v) => *v,
            None => {
                let v = tokio::fs::metadata(&parent)
                    .await
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
                dir_exists.insert(parent.clone(), v);
                v
            }
        };
        if !present {
            debug!(
                path = %path.display(),
                "clip row's directory is absent (detached medium?); not counted as missing"
            );
            continue;
        }
        warn!(
            path = %path.display(),
            "DB references clip file that does not exist on disk; row LEFT in place for operator review"
        );
        out.missing += 1;
    }

    Ok(out)
}

/// Grace window before an unreferenced file may be treated as an
/// orphan. Sized well above the longest clip finalisation (the alert
/// builder's own ceiling is minutes, not hours) and well below the 24 h
/// sweep interval, so a genuine orphan is still reclaimed on the next
/// pass rather than lingering.
const ORPHAN_MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// `true` when `path`'s mtime is newer than `cutoff`. An unreadable
/// mtime returns `true` — fail safe, because the cost of sparing a real
/// orphan for one more sweep is a stale file, and the cost of deleting
/// a live one is lost footage.
async fn is_younger_than(path: &Path, cutoff: SystemTime) -> bool {
    match tokio::fs::metadata(path).await.and_then(|m| m.modified()) {
        Ok(mtime) => mtime > cutoff,
        Err(_) => true,
    }
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
                    analysis_url: None,
                    enabled: true,
                    max_fps: 0,
                    codec: None,
                },
                detector: nexus_config::CameraDetector {
                    prompts: vec![],
                    visual_prompts: vec![],
                    model_override: None,
                },
                behavior: nexus_config::CameraBehavior {
                    parking_lot_mode: false,
                    anchor_ttl_secs: None,
                    ..Default::default()
                },
                onvif: Default::default(),
                talk_down: Default::default(),
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
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();
        (clip_id, abs)
    }

    /// Helper: insert an `events` row plus one `alert_sink_outbox` row
    /// in a given status, backdated by `age_days`. Raw SQL because the
    /// outbox is normally only written through the alert path, and
    /// this test cares purely about the retention predicate.
    async fn seed_outbox(
        store: &Arc<Store>,
        event_id: &str,
        status: &str,
        age_days: i64,
    ) -> anyhow::Result<()> {
        let ts = (Utc::now() - chrono::Duration::days(age_days))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        sqlx::query(
            "INSERT INTO events (event_id, camera_id, rule_id, label, severity,
                                 frame_id, captured_at, trace_id, payload_json)
             VALUES (?, 1, 'rule.x', 'person', 'info', 0, ?, 'trace', '{}')",
        )
        .bind(event_id)
        .bind(&ts)
        .execute(store.pool())
        .await?;
        let reason = if status == "suppressed" {
            Some("global_disabled")
        } else {
            None
        };
        sqlx::query(
            "INSERT INTO alert_sink_outbox
                (event_id, sink_id, status, suppression_reason, created_at)
             VALUES (?, 'webhook:x', ?, ?, ?)",
        )
        .bind(event_id)
        .bind(status)
        .bind(reason)
        .bind(&ts)
        .execute(store.pool())
        .await?;
        Ok(())
    }

    async fn outbox_ids(store: &Arc<Store>) -> Vec<String> {
        sqlx::query_scalar::<_, String>("SELECT event_id FROM alert_sink_outbox ORDER BY event_id")
            .fetch_all(store.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn outbox_retention_trims_only_old_terminal_rows() {
        let (store, _dir, _clips_dir) = fixture().await;

        seed_outbox(&store, "old-sent", "sent", 60).await.unwrap();
        seed_outbox(&store, "old-suppressed", "suppressed", 60)
            .await
            .unwrap();
        seed_outbox(&store, "old-dead", "dead", 60).await.unwrap();
        // Live work, however stale it looks — must survive.
        seed_outbox(&store, "old-pending", "pending", 60)
            .await
            .unwrap();
        // Inside the horizon.
        seed_outbox(&store, "new-sent", "sent", 1).await.unwrap();

        let deleted = sweep_outbox_once(&store, 30).await.unwrap();

        assert_eq!(deleted, 3, "the three old terminal rows");
        assert_eq!(
            outbox_ids(&store).await,
            vec!["new-sent".to_string(), "old-pending".to_string()],
            "pending work and in-horizon rows must survive"
        );
    }

    #[tokio::test]
    async fn outbox_retention_disabled_by_zero() {
        let (store, _dir, _clips_dir) = fixture().await;
        seed_outbox(&store, "ancient", "sent", 5_000).await.unwrap();

        let deleted = sweep_outbox_once(&store, 0).await.unwrap();

        assert_eq!(deleted, 0);
        assert_eq!(outbox_ids(&store).await, vec!["ancient".to_string()]);
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
        let res = sweep_once(&store, &clips_dir, cutoff, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(res.evicted, 1);
        assert_eq!(res.orphans, 0);
        assert_eq!(res.missing, 0);

        // Old gone, recent still here.
        assert!(!old_path.exists(), "old file should have been unlinked");
        assert!(recent_path.exists(), "recent file should remain");
        assert!(store.get_clip(old_id).await.unwrap().is_none());
        assert!(store.get_clip(recent_id).await.unwrap().is_some());
    }

    /// The grace window must not become a way to never collect
    /// anything: a young orphan is spared, and the SAME file is
    /// collected once its mtime ages past the window.
    #[tokio::test]
    async fn a_young_orphan_is_spared_then_collected_once_it_ages() {
        let (store, _dir, clips_dir) = fixture().await;
        let now = Utc::now();
        let cutoff = now - chrono::Duration::days(30);

        let orphan = clips_dir.join("cam1").join("fresh-orphan.mp4");
        tokio::fs::create_dir_all(orphan.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&orphan, b"just-written").await.unwrap();

        let spared = sweep_once(&store, &clips_dir, cutoff, ORPHAN_MIN_AGE)
            .await
            .unwrap();
        assert_eq!(
            spared.orphans, 0,
            "a just-written file is not yet an orphan"
        );
        assert!(orphan.exists());

        // Age the file itself rather than shrinking the window, so this
        // exercises `is_younger_than` returning false under a real window.
        let old = SystemTime::now() - (ORPHAN_MIN_AGE * 2);
        std::fs::File::options()
            .write(true)
            .open(&orphan)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();

        let collected = sweep_once(&store, &clips_dir, cutoff, ORPHAN_MIN_AGE)
            .await
            .unwrap();
        assert_eq!(collected.orphans, 1, "it must be reclaimed once it ages");
        assert!(!orphan.exists());
    }

    /// A `failed` alert clip's final path never existed, and nothing
    /// ever deletes the row -- so counting it as known would report it
    /// missing on every sweep, forever.
    #[tokio::test]
    async fn a_failed_alert_clip_is_not_reported_missing() {
        let (store, _dir, clips_dir) = fixture().await;
        let now = Utc::now();

        let rel = format!(
            "alert/2/{}/{}.mp4",
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
        store.mark_alert_clip_failed(id).await.unwrap();

        let cutoff = now - chrono::Duration::days(30);
        let res = sweep_once(&store, &clips_dir, cutoff, Duration::ZERO)
            .await
            .unwrap();

        assert_eq!(
            res.missing, 0,
            "a failed alert clip has no file by construction and must not be \
             warned about on every sweep"
        );

        // Assert the query's own contract too. The sweep result above is
        // also satisfied by the ready/expected split, so without this the
        // test would pass even if `failed` rows leaked back into the
        // spared set.
        let spared = store.known_alert_clip_paths().await.unwrap();
        assert!(
            !spared.iter().any(|(p, _)| p == &rel),
            "a failed alert clip must not be in the spared set: {spared:?}"
        );
    }

    /// A detached USB vault makes every one of its rows absent at once.
    /// That is one event, not one per clip.
    #[tokio::test]
    async fn a_detached_vault_does_not_warn_once_per_clip() {
        let (store, _dir, clips_dir) = fixture().await;
        let now = Utc::now();

        sqlx::query(
            "INSERT INTO storage_backends (handle, kind, config_json)
             VALUES ('usb-GONE', 'usb', '{}')",
        )
        .execute(store.pool())
        .await
        .unwrap();

        // Rows exist; the mount never does.
        for n in 0..3 {
            store
                .open_clip(&NewClip {
                    camera_id: 1,
                    started_at: now,
                    hot_path: format!("usb/GONE/cam1/clip{n}.mp4"),
                    codec: "stub".into(),
                    container: "mp4".into(),
                    hot_handle: "usb-GONE".into(),
                    frame_width: 960,
                    frame_height: 540,
                })
                .await
                .unwrap();
        }

        let cutoff = now - chrono::Duration::days(30);
        let res = sweep_once(&store, &clips_dir, cutoff, Duration::ZERO)
            .await
            .unwrap();

        assert_eq!(
            res.missing, 0,
            "a detached medium must not produce one missing warning per clip"
        );
    }

    /// The alert builder registers the FINAL path but writes
    /// `<name>.partial.mp4`, so the partial is in no known set by
    /// construction. Without the grace window the sweep unlinks it
    /// mid-write and the builder's rename then fails.
    ///
    /// Motion clips are deliberately not exercised here: both recorders
    /// insert their row with the in-flight path, so a motion partial
    /// was always in the known set.
    #[tokio::test]
    async fn an_in_flight_alert_partial_survives_the_orphan_sweep() {
        let (store, _dir, clips_dir) = fixture().await;
        let now = Utc::now();

        let rel = format!(
            "alert/1/{}/{}.mp4",
            now.format("%Y-%m-%d"),
            now.timestamp_millis()
        );
        store
            .insert_alert_clip(&nexus_store::NewAlertClip {
                camera_id: 1,
                started_at: now,
                path: rel.clone(),
            })
            .await
            .unwrap();

        // On disk there is only the partial; the row names the final path.
        let partial = clips_dir.join(&rel).with_extension("partial.mp4");
        tokio::fs::create_dir_all(partial.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&partial, b"still-being-encoded")
            .await
            .unwrap();

        let cutoff = now - chrono::Duration::days(30);
        let res = sweep_once(&store, &clips_dir, cutoff, ORPHAN_MIN_AGE)
            .await
            .unwrap();

        assert!(
            partial.exists(),
            "an in-flight alert partial was unlinked mid-write (orphans={}); \
             the rename to the final path would then fail",
            res.orphans
        );
        assert_eq!(
            res.missing, 0,
            "a building row whose file is still at the partial path must not \
             be reported missing"
        );
    }

    /// USB-vault clips live at `clips_dir/usb/<label>/...` -- inside the swept
    /// tree -- but their rows carry `hot_handle = "usb-<label>"`, which
    /// `known_clip_paths()` used to filter out.
    #[tokio::test]
    async fn a_usb_vault_clip_survives_the_orphan_sweep() {
        let (store, _dir, clips_dir) = fixture().await;
        let now = Utc::now();

        // motion_clips.hot_handle REFERENCES storage_backends(handle),
        // so a vault clip can only exist once its backend is registered.
        sqlx::query(
            "INSERT INTO storage_backends (handle, kind, config_json)
             VALUES ('usb-NEXUS_VAULT', 'usb', '{}')",
        )
        .execute(store.pool())
        .await
        .unwrap();

        let rel = "usb/NEXUS_VAULT/cam1/vaulted.mp4";
        let abs = clips_dir.join(rel);
        tokio::fs::create_dir_all(abs.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&abs, b"vaulted-evidence").await.unwrap();
        store
            .open_clip(&NewClip {
                camera_id: 1,
                started_at: now,
                hot_path: rel.into(),
                codec: "stub".into(),
                container: "mp4".into(),
                hot_handle: "usb-NEXUS_VAULT".into(),
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();

        let cutoff = now - chrono::Duration::days(30);
        let res = sweep_once(&store, &clips_dir, cutoff, Duration::ZERO)
            .await
            .unwrap();

        assert!(
            abs.exists(),
            "a USB-vault clip with a live row was deleted by the orphan sweep \
             (orphans={}); its files are under clips_dir but its hot_handle is \
             not 'local'",
            res.orphans
        );
    }

    /// DIAGNOSTIC (issue: orphan sweep vs alert clips). An alert clip
    /// with a live `alert_clips` row must survive the sweep.
    #[tokio::test]
    async fn alert_clip_with_a_row_survives_the_orphan_sweep() {
        let (store, _dir, clips_dir) = fixture().await;
        let now = Utc::now();

        // A real alert clip, laid out exactly as alert_clip_rel_path does:
        // alert/{camera_id}/{YYYY-MM-DD}/{start_unix_ms}.mp4
        let rel = format!(
            "alert/1/{}/{}.mp4",
            now.format("%Y-%m-%d"),
            now.timestamp_millis()
        );
        let abs = clips_dir.join(&rel);
        tokio::fs::create_dir_all(abs.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&abs, b"alert-clip-payload").await.unwrap();

        let id = store
            .insert_alert_clip(&nexus_store::NewAlertClip {
                camera_id: 1,
                started_at: now,
                path: rel.clone(),
            })
            .await
            .expect("insert alert clip row");
        assert!(id > 0, "row must exist");

        let cutoff = now - chrono::Duration::days(30);
        let res = sweep_once(&store, &clips_dir, cutoff, Duration::ZERO)
            .await
            .unwrap();

        assert!(
            abs.exists(),
            "alert clip with a live alert_clips row was deleted by the orphan sweep \
             (orphans={}); the known set did not include alert_clips",
            res.orphans
        );
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
        let res = sweep_once(&store, &clips_dir, cutoff, Duration::ZERO)
            .await
            .unwrap();
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
        let res = sweep_once(&store, &clips_dir, cutoff, Duration::ZERO)
            .await
            .unwrap();
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
            outbox_retention_days: 30,
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

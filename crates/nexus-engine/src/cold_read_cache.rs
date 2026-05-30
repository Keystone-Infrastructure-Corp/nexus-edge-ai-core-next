//! M2.2 Phase 4 — cold-read transient cache.
//!
//! When the M2.2 soft-eviction sweeper drops a clip's hot file
//! while keeping the row + cold pointer, subsequent reads through
//! `GET /api/v1/clips/:id` would otherwise be permanently served
//! from cold (LAN / cloud) — every request paying the cold-tier
//! latency and bandwidth cost. This module keeps that experience
//! tolerable by streaming the cold copy back to local hot storage
//! the *first* time the clip is requested, so the second request
//! hits the M2.1 fast path again.
//!
//! Properties (load-bearing — keep them true at every refactor):
//!
//! 1. **Dedup by clip_id.** Concurrent reads for the same
//!    soft-evicted clip share ONE cache job. Implemented as
//!    `HashMap<ClipId, Arc<Notify>>`.
//!
//! 2. **Bounded global concurrency.** [`CACHE_MAX_INFLIGHT`] caps
//!    the number of simultaneous cold-to-hot downloads so a viewer
//!    refreshing the timeline rapidly can't saturate the cold
//!    backend or thrash local disk. Backed by
//!    `tokio::sync::Semaphore`.
//!
//! 3. **Watermark-priority.** A cache job MUST NOT start when the
//!    storage safety FSM is at Low or Panic — rehydrating cold
//!    clips back to hot would actively work against the eviction
//!    sweeper trying to free space. An in-flight job is cancelled
//!    on the *next* Ok→Low / Ok→Panic / Low→Panic transition;
//!    its partial file is deleted.
//!
//! 4. **Crash-safe.** All writes go to `<clip_id>.cache.partial`
//!    next to the canonical hot path. Successful jobs verify
//!    sha256 against [`nexus_store::ClipRow::sha256`] and
//!    `tokio::fs::rename` the partial onto the canonical path
//!    (atomic on POSIX). A crash between download and rename
//!    leaves only the partial, which the next attempt overwrites.
//!
//! 5. **DB write is a conditional UPDATE.** The job calls
//!    [`nexus_store::Store::repopulate_hot_pointer`] which has a
//!    `WHERE hot_handle IS NULL` guard. If the recorder or another
//!    rehydrate raced ahead, the UPDATE returns `Ok(0)` and the
//!    job leaves the on-disk file in place — the row already
//!    points at it (or at a different hot copy that also exists).
//!
//! 6. **Replicator NEVER deletes from source.** This module only
//!    *reads* from the cold backend via
//!    [`nexus_storage::ColdBackend::get_range`]; it never touches
//!    `delete()`. The cold copy stays intact for a future
//!    re-eviction.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use nexus_storage::Registry;
use nexus_store::{ClipId, Store};
use parking_lot::Mutex;
use tokio::sync::{Notify, Semaphore};
use tracing::{debug, info, warn};

use crate::storage_safety::WatermarkSignal;

/// Maximum number of cold-to-hot rehydrate jobs that may run
/// simultaneously. Picked low on purpose: a typical tier-10 box
/// has a single LAN NIC + a single SSD, and 4 parallel `get_range`
/// + write streams already saturate both. Increase only on tier-
///   24+ hardware after measuring.
pub const CACHE_MAX_INFLIGHT: usize = 4;

/// Suffix used for in-progress cache files so an orphaned partial
/// from a crash is trivially recognisable + deleteable. The
/// retention sweeper [`crate::retention`] could be taught to mop
/// these up; for now they are just overwritten by the next
/// rehydrate attempt.
const PARTIAL_SUFFIX: &str = ".cache.partial";

/// Shared cache-jobs coordinator. Cheap to clone (it's an `Arc`
/// internally). Constructed once at engine boot and stored on
/// [`crate::api::ApiState`].
#[derive(Clone)]
pub struct CacheJobs {
    inner: Arc<CacheJobsInner>,
}

struct CacheJobsInner {
    inflight: Mutex<HashMap<ClipId, Arc<Notify>>>,
    semaphore: Arc<Semaphore>,
    watermark: WatermarkSignal,
    store: Arc<Store>,
    registry: Registry,
    clips_dir: PathBuf,
}

impl CacheJobs {
    pub fn new(
        store: Arc<Store>,
        registry: Registry,
        clips_dir: PathBuf,
        watermark: WatermarkSignal,
    ) -> Self {
        Self {
            inner: Arc::new(CacheJobsInner {
                inflight: Mutex::new(HashMap::new()),
                semaphore: Arc::new(Semaphore::new(CACHE_MAX_INFLIGHT)),
                watermark,
                store,
                registry,
                clips_dir,
            }),
        }
    }

    /// Number of jobs currently running OR waiting on a permit.
    /// Surfaced for tests + the operator UI.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn inflight_count(&self) -> usize {
        self.inner.inflight.lock().len()
    }

    /// Spawn a rehydrate job for `clip_id`. Returns:
    ///
    /// * `Some(Arc<Notify>)` when a job is in flight (newly
    ///   spawned OR already in flight from a previous call). The
    ///   caller can `await notify.notified()` to be woken when the
    ///   job completes — handy for tests; production callers fire
    ///   and forget.
    /// * `None` when no job was started because the watermark is
    ///   not `Ok`. In that case the caller MUST stream from cold
    ///   directly without expecting hot rehydrate this round.
    pub fn spawn(&self, clip_id: ClipId) -> Option<Arc<Notify>> {
        // Watermark check FIRST — the cheapest possible reject.
        // (We re-check inside the spawned task too, after the
        // semaphore acquire, to catch a transition that happens
        // between this check and the permit grant.)
        if !self.inner.watermark.is_ok() {
            debug!(
                clip_id,
                level = ?self.inner.watermark.level(),
                "cold_read_cache: skipping rehydrate (watermark not Ok)"
            );
            return None;
        }

        // Dedup: if a job is already in flight for this clip_id,
        // hand back the same Notify so concurrent waiters all wake
        // on the same event.
        let mut inflight = self.inner.inflight.lock();
        if let Some(existing) = inflight.get(&clip_id) {
            return Some(existing.clone());
        }
        let notify = Arc::new(Notify::new());
        inflight.insert(clip_id, notify.clone());
        drop(inflight);

        let inner = self.inner.clone();
        let notify_done = notify.clone();
        tokio::spawn(async move {
            let outcome = run_one(&inner, clip_id).await;
            // Always remove from inflight and wake waiters,
            // regardless of outcome.
            inner.inflight.lock().remove(&clip_id);
            notify_done.notify_waiters();
            match outcome {
                Ok(CacheOutcome::Rehydrated { bytes }) => {
                    info!(clip_id, bytes, "cold_read_cache: clip rehydrated");
                }
                Ok(CacheOutcome::AlreadyHot) => {
                    debug!(
                        clip_id,
                        "cold_read_cache: clip was already hot; nothing to do"
                    );
                }
                Ok(CacheOutcome::WatermarkAborted) => {
                    debug!(clip_id, "cold_read_cache: aborted by watermark pressure");
                }
                Ok(CacheOutcome::Skipped { reason }) => {
                    debug!(clip_id, reason, "cold_read_cache: skipped");
                }
                Err(e) => warn!(clip_id, error = %e, "cold_read_cache: job failed"),
            }
        });
        Some(notify)
    }
}

/// Job outcome — surfaced via tracing only. The caller's
/// `Notify::notified()` is the only signal the API path waits on.
#[derive(Debug)]
enum CacheOutcome {
    Rehydrated { bytes: u64 },
    AlreadyHot,
    WatermarkAborted,
    Skipped { reason: &'static str },
}

#[derive(Debug, thiserror::Error)]
enum CacheError {
    #[error("clip {0} not found")]
    NotFound(ClipId),
    #[error("registry has no backend with handle '{0}'")]
    NoBackend(String),
    #[error("backend {handle}: {source}")]
    Backend {
        handle: String,
        #[source]
        source: nexus_storage::BackendError,
    },
    #[error("write partial {path}: {source}")]
    WritePartial {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("rename partial → final {dst}: {source}")]
    Rename {
        dst: String,
        #[source]
        source: std::io::Error,
    },
    #[error("sha256 mismatch on rehydrated clip: cold yielded {actual}, expected {expected}")]
    Sha256Mismatch { expected: String, actual: String },
    #[error("store: {0}")]
    Store(#[from] nexus_store::StoreError),
}

/// Body of a single cache job. Returns one of [`CacheOutcome`] on
/// success — `Err` is reserved for genuine failures we want to
/// surface in tracing.
async fn run_one(inner: &Arc<CacheJobsInner>, clip_id: ClipId) -> Result<CacheOutcome, CacheError> {
    // ---------------------------------------------------------------
    // Permit + re-check watermark
    // ---------------------------------------------------------------
    // Wait for a slot. The semaphore is global, so under heavy
    // load this naturally back-pressures cold reads.
    let _permit = inner
        .semaphore
        .clone()
        .acquire_owned()
        .await
        .expect("semaphore should never close while CacheJobs is alive");
    if !inner.watermark.is_ok() {
        return Ok(CacheOutcome::WatermarkAborted);
    }

    // ---------------------------------------------------------------
    // Read the row
    // ---------------------------------------------------------------
    let clip = inner
        .store
        .get_clip(clip_id)
        .await?
        .ok_or(CacheError::NotFound(clip_id))?;

    // Already hot? Nothing to do — the API path will read the
    // local file. This is the idempotent path for "two viewers
    // requested at once and one already finished rehydrating".
    if clip.hot_path.is_some() {
        return Ok(CacheOutcome::AlreadyHot);
    }

    // Soft-evicted but missing the metadata we need to rehydrate
    // safely. Skip silently — the cold-only path still works.
    let Some(cold_handle) = clip.cold_handle.as_deref() else {
        return Ok(CacheOutcome::Skipped {
            reason: "no cold pointer",
        });
    };
    let Some(cold_path) = clip.cold_path.as_deref() else {
        return Ok(CacheOutcome::Skipped {
            reason: "no cold path",
        });
    };
    let Some(expected_sha256) = clip.sha256.as_deref() else {
        // Legacy row pre-streaming-hash. Without sha256 we can't
        // verify the rehydrate, and a silent torn write would
        // corrupt the hot copy forever. Skip until an operator
        // opts into a one-shot rehash sweep (Phase 2 work).
        return Ok(CacheOutcome::Skipped {
            reason: "no sha256",
        });
    };
    if clip.size_bytes <= 0 {
        return Ok(CacheOutcome::Skipped {
            reason: "zero size",
        });
    }
    let size = clip.size_bytes as u64;

    let backend = inner
        .registry
        .get(cold_handle)
        .ok_or_else(|| CacheError::NoBackend(cold_handle.to_string()))?;

    // ---------------------------------------------------------------
    // Resolve target paths.
    //
    // The canonical hot path mirrors the cold path verbatim (the
    // replicator chose `cold_path = hot_path_rel` precisely so we
    // can reverse-map here). The partial file lives next to it so
    // a successful rename is a single inode-rename op on the same
    // filesystem.
    // ---------------------------------------------------------------
    let rel = std::path::PathBuf::from(cold_path);
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Ok(CacheOutcome::Skipped {
            reason: "cold_path contains '..'",
        });
    }
    let final_abs = inner.clips_dir.join(&rel);
    if let Some(parent) = final_abs.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| CacheError::WritePartial {
                path: parent.display().to_string(),
                source: e,
            })?;
    }
    let partial_abs = {
        let mut p = final_abs.clone();
        let fname = format!(
            "{}{PARTIAL_SUFFIX}",
            p.file_name().and_then(|o| o.to_str()).unwrap_or("clip.mp4")
        );
        p.set_file_name(fname);
        p
    };

    // ---------------------------------------------------------------
    // Download with watermark cancellation.
    //
    // ColdBackend::get_range returns Vec<u8> for the full byte
    // range. For a typical 30s-clip-at-2Mbps that's ~7.5 MB, well
    // inside what we want to hold in memory briefly. If a future
    // backend wants to stream we can introduce a streaming variant
    // of get_range; the current shape is fine for Phase 4.
    // ---------------------------------------------------------------
    let pressure = inner.watermark.pressure_notified();
    tokio::pin!(pressure);
    let download = backend.get_range(cold_path, 0, size - 1);
    tokio::pin!(download);
    let bytes = tokio::select! {
        biased;
        _ = &mut pressure => {
            // Don't even bother writing what we got — reject early.
            return Ok(CacheOutcome::WatermarkAborted);
        }
        r = &mut download => {
            r.map_err(|e| CacheError::Backend {
                handle: cold_handle.to_string(),
                source: e,
            })?
        }
    };

    // ---------------------------------------------------------------
    // sha256 verify BEFORE writing to disk so a torn / wrong-bytes
    // payload never reaches the canonical filename.
    // ---------------------------------------------------------------
    use sha2::{Digest, Sha256};
    let actual = {
        let mut h = Sha256::new();
        h.update(&bytes);
        format!("{:x}", h.finalize())
    };
    if actual != expected_sha256 {
        return Err(CacheError::Sha256Mismatch {
            expected: expected_sha256.to_string(),
            actual,
        });
    }

    // ---------------------------------------------------------------
    // Write the partial, fsync, atomic-rename.
    //
    // We accept a single allocation hit (write_all) over chunked
    // writes because the verified bytes are already in memory.
    // ---------------------------------------------------------------
    {
        use tokio::io::AsyncWriteExt;
        let mut f =
            tokio::fs::File::create(&partial_abs)
                .await
                .map_err(|e| CacheError::WritePartial {
                    path: partial_abs.display().to_string(),
                    source: e,
                })?;
        f.write_all(&bytes)
            .await
            .map_err(|e| CacheError::WritePartial {
                path: partial_abs.display().to_string(),
                source: e,
            })?;
        f.sync_all().await.map_err(|e| CacheError::WritePartial {
            path: partial_abs.display().to_string(),
            source: e,
        })?;
    }
    tokio::fs::rename(&partial_abs, &final_abs)
        .await
        .map_err(|e| CacheError::Rename {
            dst: final_abs.display().to_string(),
            source: e,
        })?;

    // ---------------------------------------------------------------
    // Conditional UPDATE: re-establish the hot pointer ONLY if it
    // is still NULL. If it isn't (race with another rehydrate or
    // the recorder reopened a clip with the same id — unlikely
    // because clip_ids are AUTOINCREMENT), the on-disk file we
    // just wrote is a harmless duplicate of whatever's already
    // canonical.
    // ---------------------------------------------------------------
    let updated = inner
        .store
        .repopulate_hot_pointer(clip_id, "local", cold_path)
        .await?;
    if updated == 0 {
        debug!(
            clip_id,
            "cold_read_cache: hot pointer raced ahead; UPDATE was a no-op"
        );
    }

    Ok(CacheOutcome::Rehydrated { bytes: size })
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use nexus_config::{CameraConfig, StoreConfig};
    use nexus_storage::{BackendError, ColdBackend, HealthStatus, PutReceipt, VolumeInfo};
    use nexus_store::{ClipClose, ClipColdMark, NewClip};
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    use url::Url;

    /// Mock cold backend that returns pre-loaded bytes from `get_range`.
    struct MockColdBackend {
        handle: String,
        store: parking_lot::Mutex<HashMap<String, Vec<u8>>>,
        get_calls: AtomicU32,
        // Optional override that returns a different payload than
        // expected so we can drive the sha256-mismatch test path.
        corrupt: parking_lot::Mutex<Option<Vec<u8>>>,
    }
    impl MockColdBackend {
        fn new(handle: &str) -> Arc<Self> {
            Arc::new(Self {
                handle: handle.into(),
                store: parking_lot::Mutex::new(HashMap::new()),
                get_calls: AtomicU32::new(0),
                corrupt: parking_lot::Mutex::new(None),
            })
        }
        fn put_bytes(&self, path: &str, bytes: Vec<u8>) {
            self.store.lock().insert(path.to_string(), bytes);
        }
        fn corrupt_with(&self, bytes: Vec<u8>) {
            *self.corrupt.lock() = Some(bytes);
        }
        fn get_call_count(&self) -> u32 {
            self.get_calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl ColdBackend for MockColdBackend {
        fn handle(&self) -> &str {
            &self.handle
        }
        fn kind(&self) -> &str {
            "lan"
        }
        async fn put(
            &self,
            _path: &str,
            _bytes: &[u8],
            _expected_sha256: &str,
        ) -> Result<PutReceipt, BackendError> {
            unreachable!("cache jobs never call put")
        }
        async fn get_range(
            &self,
            path: &str,
            start: u64,
            end_inclusive: u64,
        ) -> Result<Vec<u8>, BackendError> {
            self.get_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(c) = self.corrupt.lock().clone() {
                return Ok(c);
            }
            let bytes = self
                .store
                .lock()
                .get(path)
                .cloned()
                .ok_or_else(|| BackendError::Other(format!("mock has no path {path}")))?;
            let s = start as usize;
            let e = (end_inclusive as usize + 1).min(bytes.len());
            Ok(bytes[s..e].to_vec())
        }
        async fn delete(&self, _path: &str) -> Result<bool, BackendError> {
            unreachable!("cache jobs never call delete")
        }
        async fn exists(&self, _path: &str, _expected_sha256: &str) -> Result<bool, BackendError> {
            Ok(true)
        }
        async fn volume_info(&self) -> Result<VolumeInfo, BackendError> {
            Ok(VolumeInfo {
                free_bytes: Some(1 << 30),
                total_bytes: Some(1 << 31),
                used_bytes: Some(1 << 30),
            })
        }
        async fn health(&self) -> HealthStatus {
            HealthStatus::Ok
        }
    }

    /// Seed: one camera, one clip that's ALREADY soft-evicted —
    /// row + cold pointer present, hot pointer NULL, file gone.
    /// Returns the in-process pieces tests need.
    async fn seed_soft_evicted(
        bytes: Vec<u8>,
    ) -> (
        Arc<Store>,
        Registry,
        PathBuf,
        ClipId,
        Arc<MockColdBackend>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
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
        let clips_dir = dir.path().join("clips");
        tokio::fs::create_dir_all(clips_dir.join("9"))
            .await
            .unwrap();
        store
            .upsert_camera(&CameraConfig {
                id: 9,
                name: "cam9".into(),
                ingest: nexus_config::CameraIngest {
                    url: Url::parse("rtsp://127.0.0.1/stream9").unwrap(),
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
                },
                zones: vec![],
            })
            .await
            .unwrap();
        store
            .upsert_storage_backend("mock", "lan", "{\"root\":\"/tmp/mock\"}")
            .await
            .unwrap();
        let now = Utc::now();
        let rel = "9/clip_0007.mp4".to_string();
        let clip_id = store
            .open_clip(&NewClip {
                camera_id: 9,
                started_at: now - chrono::Duration::seconds(60),
                hot_path: rel.clone(),
                codec: "stub".into(),
                container: "mp4".into(),
                hot_handle: "local".into(),
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();
        let sha256 = {
            let mut h = Sha256::new();
            h.update(&bytes);
            format!("{:x}", h.finalize())
        };
        store
            .close_clip(
                clip_id,
                &ClipClose {
                    ended_at: now,
                    duration_ms: 1000,
                    size_bytes: bytes.len() as i64,
                    hot_path: Some(rel.clone()),
                    sha256: Some(sha256),
                },
            )
            .await
            .unwrap();
        // Mark cold-replicated so soft-evict is allowed.
        store
            .mark_cold_replicated(
                clip_id,
                &ClipColdMark {
                    cold_handle: "mock".into(),
                    cold_path: rel.clone(),
                    cold_uploaded_at: now,
                },
            )
            .await
            .unwrap();
        // Soft-evict: drop hot pointer (no file to delete since we
        // never wrote one in the fixture).
        let cleared = store.clear_hot_pointer(clip_id).await.unwrap();
        assert_eq!(cleared, 1);

        let backend = MockColdBackend::new("mock");
        backend.put_bytes(&rel, bytes);
        let registry = Registry::new();
        registry.replace_all([backend.clone() as Arc<dyn ColdBackend>]);

        (store, registry, clips_dir, clip_id, backend, dir)
    }

    /// Happy path: a soft-evicted clip is rehydrated. Row gets
    /// hot_path repopulated, file lands on disk, sha256 matches.
    #[tokio::test]
    async fn rehydrates_soft_evicted_clip_on_first_request() {
        let (store, registry, clips_dir, clip_id, backend, _tmp) =
            seed_soft_evicted(b"hello-cold-world".to_vec()).await;
        let watermark = WatermarkSignal::new();
        watermark.set(super::super::storage_safety::WatermarkLevel::Ok);
        let jobs = CacheJobs::new(
            store.clone(),
            registry,
            clips_dir.clone(),
            watermark.clone(),
        );
        let notify = jobs
            .spawn(clip_id)
            .expect("Ok watermark must allow rehydrate");
        tokio::time::timeout(Duration::from_secs(2), notify.notified())
            .await
            .expect("rehydrate should complete within 2s");

        // Hot pointer is back.
        let row = store.get_clip(clip_id).await.unwrap().unwrap();
        assert_eq!(row.hot_handle.as_deref(), Some("local"));
        assert_eq!(row.hot_path.as_deref(), Some("9/clip_0007.mp4"));

        // File exists on disk with the correct bytes.
        let on_disk = tokio::fs::read(clips_dir.join("9/clip_0007.mp4"))
            .await
            .unwrap();
        assert_eq!(on_disk, b"hello-cold-world");

        // Backend was hit exactly once.
        assert_eq!(backend.get_call_count(), 1);
        assert_eq!(jobs.inflight_count(), 0, "inflight map drains on success");
    }

    /// `spawn` is a no-op when the watermark is not Ok — the
    /// caller still gets to serve the cold-only stream, but no
    /// hot rehydrate fires.
    #[tokio::test]
    async fn skips_rehydrate_when_watermark_low() {
        let (store, registry, clips_dir, clip_id, backend, _tmp) =
            seed_soft_evicted(b"xxx".to_vec()).await;
        let watermark = WatermarkSignal::new();
        watermark.set(super::super::storage_safety::WatermarkLevel::Low);
        let jobs = CacheJobs::new(store, registry, clips_dir, watermark);

        let res = jobs.spawn(clip_id);
        assert!(res.is_none(), "Low watermark must reject spawn");
        // Give the (non-existent) job a moment to NOT happen.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(backend.get_call_count(), 0);
    }

    /// Two concurrent `spawn(clip_id)` calls share ONE job — they
    /// get the same Notify handle and the backend is hit once.
    #[tokio::test]
    async fn dedupes_concurrent_spawns_for_same_clip() {
        let (store, registry, clips_dir, clip_id, backend, _tmp) =
            seed_soft_evicted(b"share-the-job".to_vec()).await;
        let watermark = WatermarkSignal::new();
        watermark.set(super::super::storage_safety::WatermarkLevel::Ok);
        let jobs = CacheJobs::new(store, registry, clips_dir, watermark);

        let n1 = jobs.spawn(clip_id).expect("first spawn must succeed");
        let n2 = jobs.spawn(clip_id).expect("second spawn must dedupe");
        assert!(
            Arc::ptr_eq(&n1, &n2),
            "dedup MUST hand back the same Notify handle"
        );

        tokio::time::timeout(Duration::from_secs(2), n1.notified())
            .await
            .expect("rehydrate completes");
        assert_eq!(
            backend.get_call_count(),
            1,
            "deduped spawns must hit the backend exactly once"
        );
    }

    /// Sha256 mismatch is a hard error: the canonical file MUST
    /// NOT appear on disk and the hot pointer stays NULL.
    #[tokio::test]
    async fn sha256_mismatch_does_not_publish_file_or_update_row() {
        let (store, registry, clips_dir, clip_id, backend, _tmp) =
            seed_soft_evicted(b"correct-bytes".to_vec()).await;
        // Force the backend to return DIFFERENT bytes so the
        // verify step fails.
        backend.corrupt_with(b"WRONG-bytes!!".to_vec());
        let watermark = WatermarkSignal::new();
        watermark.set(super::super::storage_safety::WatermarkLevel::Ok);
        let jobs = CacheJobs::new(store.clone(), registry, clips_dir.clone(), watermark);

        let notify = jobs.spawn(clip_id).unwrap();
        tokio::time::timeout(Duration::from_secs(2), notify.notified())
            .await
            .unwrap();

        // Final file MUST NOT exist.
        let final_abs = clips_dir.join("9/clip_0007.mp4");
        assert!(
            !final_abs.exists(),
            "sha256-failed rehydrate must not publish a file"
        );
        // Hot pointer stays NULL.
        let row = store.get_clip(clip_id).await.unwrap().unwrap();
        assert!(
            row.hot_path.is_none(),
            "row hot_path MUST stay NULL on sha256 mismatch"
        );
    }

    /// If the row already has a hot pointer (e.g. recorder raced
    /// ahead), the job is a fast no-op and never touches the
    /// backend.
    #[tokio::test]
    async fn already_hot_clip_is_no_op() {
        let (store, registry, clips_dir, clip_id, backend, _tmp) =
            seed_soft_evicted(b"ok".to_vec()).await;
        // Race-ahead: simulate another path repopulating the hot
        // pointer between request and job acquire.
        store
            .repopulate_hot_pointer(clip_id, "local", "9/clip_0007.mp4")
            .await
            .unwrap();
        let watermark = WatermarkSignal::new();
        watermark.set(super::super::storage_safety::WatermarkLevel::Ok);
        let jobs = CacheJobs::new(store, registry, clips_dir, watermark);

        let notify = jobs.spawn(clip_id).unwrap();
        tokio::time::timeout(Duration::from_secs(2), notify.notified())
            .await
            .unwrap();
        assert_eq!(
            backend.get_call_count(),
            0,
            "already-hot clip must NOT hit the backend"
        );
    }
}

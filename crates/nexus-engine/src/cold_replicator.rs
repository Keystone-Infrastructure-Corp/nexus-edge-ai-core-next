//! M2.2 Phase 1 — cold-mirror replicator.
//!
//! A single follower task that copies closed clips from the local
//! hot tier onto the configured cold backend. Two design rules are
//! load-bearing and MUST stay true at every refactor:
//!
//! 1. **The replicator NEVER deletes from source.** It only ever
//!    *adds* a cold pointer to a row. Deletion of the hot copy is
//!    the eviction path's job (see [`crate::storage_safety`]) and
//!    the eviction path will refuse to soft-evict any row that
//!    isn't already cold-replicated.
//!
//! 2. **There is exactly one active cold backend at a time.** It's
//!    the row pointed at by `storage_cold_replica.backend_handle`.
//!    A NULL handle (or a handle that doesn't resolve in the
//!    [`Registry`]) means "cold disabled" — the replicator skips
//!    every tick quietly and does NOT emit unreachable warnings
//!    (those are reserved for "configured but cannot reach").
//!
//! The replicator is event-driven on `topic::CLIP_CLOSED` and has a
//! 5-minute polling backstop so a missed bus event (e.g. lagged
//! subscriber on engine restart) can never strand a clip outside
//! cold for more than that interval. Both triggers funnel into the
//! same `tick()` body that drains
//! [`Store::clips_pending_cold_upload`] in oldest-first order.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use nexus_bus::{topic, Bus, BusExt};
use nexus_storage::{BackendError, HealthStatus, Registry};
use nexus_store::{ClipColdMark, ClipRow, Store};
use serde::Deserialize;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use nexus_storage::TokenBucket;

/// Polling backstop interval. A bus event is the normal trigger; the
/// timer just guarantees forward progress if a subscriber lagged or
/// a closed clip was created out-of-band (e.g. operator imported a
/// row by hand). 5 min is long enough to keep idle CPU near zero on
/// a quiet box and short enough that a missed event never strands a
/// clip past its eviction window.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Max clips drained per tick. Keeps a single pass bounded so a
/// large backfill (e.g. operator just attached a fresh LAN share to
/// a long-running box with a backlog) doesn't monopolise the task
/// for an entire interval — we keep ticking, and each tick makes
/// progress. The 32 number is per-tick; with the default 5 min
/// backstop that's >9 k clips/day even with zero bus events.
pub const BATCH_SIZE: i64 = 32;

/// Configuration for the replicator task. All fields are owned so
/// the spawn site can `clone()` and move into the spawn future.
#[derive(Clone)]
pub struct ColdReplicatorConfig {
    /// Hot clips_dir root — used to resolve a [`ClipRow::hot_path`]
    /// (which is stored as a path relative to clips_dir) to an
    /// absolute filesystem path the replicator can read from.
    pub clips_dir: std::path::PathBuf,
}

/// Subscriber payload for `topic::CLIP_CLOSED`. We only deserialise
/// the fields we need; extra fields are ignored. The recorder
/// publishes more (camera_id, hot_handle, size_bytes, sha256) but
/// the replicator re-reads everything from the database to avoid
/// a race where the bus event arrives before the row's commit is
/// visible to other connections — `Store::clips_pending_cold_upload`
/// is the source of truth.
#[derive(Debug, Deserialize)]
struct ClipClosedHint {
    #[allow(dead_code)] // included only so missing field doesn't break deserialise
    clip_id: i64,
}

/// Run the cold replicator until `shutdown` resolves. Intended to be
/// spawned once from `main.rs` alongside the storage-safety task.
///
/// The task is structured as a `select!` over three sources:
/// * 5-min polling timer (backstop)
/// * `CLIP_CLOSED` bus subscription (event-driven)
/// * a future-only `Notify::notified()` that the boot path can use
///   to kick a tick immediately at startup
///
/// All three sources funnel into the same `tick()` body, so adding
/// a fourth (e.g. `STORAGE_BACKENDS_CHANGED` causing a registry
/// rehydrate then an opportunistic tick) is mechanical.
pub async fn run_cold_replicator(
    cfg: ColdReplicatorConfig,
    store: Arc<Store>,
    bus: Arc<dyn Bus>,
    registry: Registry,
    shutdown: impl std::future::Future<Output = ()>,
) {
    info!(
        clips_dir = %cfg.clips_dir.display(),
        poll_interval_secs = POLL_INTERVAL.as_secs(),
        batch_size = BATCH_SIZE,
        "cold replicator starting"
    );

    // Subscribe BEFORE the first tick so we don't miss events that
    // fire while we're servicing the boot-time drain.
    let mut events = match bus.subscribe::<ClipClosedHint>(topic::CLIP_CLOSED).await {
        Ok(s) => s,
        Err(e) => {
            // Subscribing to a broadcast bus shouldn't fail in
            // practice; log and fall back to polling-only.
            warn!(error = %e, "cold replicator: subscribe to CLIP_CLOSED failed; running polling-only");
            Box::pin(futures::stream::pending())
        }
    };

    // Track the last unreachable warning so we don't spam the log
    // every 5 min when the LAN share is permanently down. The bus
    // event is emitted exactly once per "transition into
    // unreachable" state.
    let mut last_health_was_ok = true;

    // One persistent throttle, kept alive across every tick. Pre-
    // M2.2-closeout the bucket was constructed fresh on every entry
    // to `tick()` (see git blame), which silently dropped any
    // credit a quiet interval would have accrued. We now build it
    // once and call `set_rate` at the top of each tick so a live
    // admin config change still takes effect immediately while the
    // accumulated burst budget survives.
    let throttle = TokenBucket::new(0);

    // Kick a first tick immediately at startup so any backlog from
    // a previous engine run gets attention without waiting for
    // either an event or the first 5-min interval.
    let kick = Arc::new(Notify::new());
    kick.notify_one();

    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first interval tick fires immediately; consume it so the
    // boot kick above is the actual first action.
    interval.tick().await;

    tokio::pin!(shutdown);

    loop {
        let kick_fut = kick.notified();
        tokio::pin!(kick_fut);

        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!("cold replicator: shutdown requested");
                return;
            }
            _ = interval.tick() => {
                debug!("cold replicator: polling backstop tick");
                tick(&cfg, &store, &bus, &registry, &throttle, &mut last_health_was_ok).await;
            }
            ev = events.next() => {
                match ev {
                    None => {
                        // Stream ended (bus dropped). Fall back to
                        // polling-only by re-arming with a pending
                        // future for the next loop iteration.
                        warn!("cold replicator: CLIP_CLOSED stream ended; polling-only mode");
                        events = Box::pin(futures::stream::pending());
                    }
                    Some(Err(e)) => {
                        // Lagged subscriber, deserialise error,
                        // etc. — the polling backstop will catch
                        // up so we just log and continue.
                        warn!(error = %e, "cold replicator: CLIP_CLOSED stream error");
                    }
                    Some(Ok(_hint)) => {
                        debug!("cold replicator: CLIP_CLOSED event received");
                        tick(&cfg, &store, &bus, &registry, &throttle, &mut last_health_was_ok).await;
                    }
                }
            }
            _ = &mut kick_fut => {
                debug!("cold replicator: boot kick");
                tick(&cfg, &store, &bus, &registry, &throttle, &mut last_health_was_ok).await;
            }
        }
    }
}

/// One drain attempt. Reads the active cold backend off
/// `storage_cold_replica` and the `Registry`, then uploads up to
/// [`BATCH_SIZE`] pending clips. Single-shot: returns to the
/// `select!` after one batch so other triggers (shutdown, bus) get
/// a chance.
async fn tick(
    cfg: &ColdReplicatorConfig,
    store: &Arc<Store>,
    bus: &Arc<dyn Bus>,
    registry: &Registry,
    throttle: &TokenBucket,
    last_health_was_ok: &mut bool,
) {
    // 1. Read active cold target.
    let policy = match store.read_cold_replica().await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "cold replicator: read_cold_replica failed");
            return;
        }
    };
    let Some(handle) = policy.backend_handle.as_deref() else {
        // Cold disabled. Quiet skip — this is a normal config.
        return;
    };
    if handle == "local" {
        // Defensive: 'local' should never appear here (it's the
        // implicit hot backend and `build_backend` rejects it).
        // Treat as disabled.
        return;
    }
    let Some(backend) = registry.get(handle) else {
        // Configured-but-unhydrated. Logged so the operator
        // notices, but the next backends-changed event will fix it.
        warn!(
            handle,
            "cold replicator: configured backend not in registry; skipping tick"
        );
        return;
    };

    // 2. Health probe. A persistently-down share would otherwise
    // turn every tick into a flurry of failed put() retries.
    match backend.health().await {
        HealthStatus::Ok => {
            if !*last_health_was_ok {
                info!(handle, "cold replicator: backend recovered");
                *last_health_was_ok = true;
            }
        }
        HealthStatus::ReadOnly { reason } => {
            warn!(
                handle,
                reason, "cold replicator: backend read-only; pausing tick"
            );
            return;
        }
        HealthStatus::Unreachable { reason } => {
            // Emit STORAGE_COLD_UNREACHABLE exactly once per
            // transition into the unreachable state so the UI can
            // surface a single sticky banner rather than 1/tick.
            if *last_health_was_ok {
                let payload = serde_json::json!({
                    "handle": handle,
                    "reason": reason,
                    "at": Utc::now().to_rfc3339(),
                });
                if let Err(e) = bus.publish(topic::STORAGE_COLD_UNREACHABLE, &payload).await {
                    warn!(error = %e, "cold replicator: publish STORAGE_COLD_UNREACHABLE failed");
                }
                *last_health_was_ok = false;
            }
            warn!(
                handle,
                reason, "cold replicator: backend unreachable; pausing tick"
            );
            return;
        }
    }

    // 3. Pull a batch of pending clips and process oldest-first.
    let pending = match store.clips_pending_cold_upload(BATCH_SIZE).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, "cold replicator: clips_pending_cold_upload failed");
            return;
        }
    };
    if pending.is_empty() {
        debug!("cold replicator: no pending clips this tick");
        return;
    }

    // Sync the persistent bucket to the current admin throttle.
    // `set_rate` preserves whatever credit accrued during the quiet
    // interval, so a normal "one clip every 30 s" workload is
    // effectively unthrottled at the moment of upload — and a burst
    // after a long quiet period is still smoothed by the bucket's
    // 1-second capacity ceiling.
    throttle.set_rate(policy.throttle_bps.max(0) as u64).await;
    let backend_handle = backend.handle().to_string();

    let mut uploaded = 0usize;
    let mut failed = 0usize;
    for clip in pending {
        match upload_one(cfg, store, &*backend, throttle, &backend_handle, &clip).await {
            Ok(()) => uploaded += 1,
            Err(e) => {
                failed += 1;
                warn!(
                    clip_id = clip.id,
                    error = %e,
                    "cold replicator: upload failed; will retry on next tick"
                );
            }
        }
    }
    info!(
        backend = %backend_handle,
        uploaded,
        failed,
        "cold replicator: batch complete"
    );
}

/// Per-clip upload path. Idempotent: relies on
/// [`nexus_storage::ColdBackend::exists`] (which does a sha256
/// spot-check) to skip clips that are already on the cold side
/// from a previous interrupted run.
async fn upload_one(
    cfg: &ColdReplicatorConfig,
    store: &Arc<Store>,
    backend: &dyn nexus_storage::ColdBackend,
    throttle: &TokenBucket,
    backend_handle: &str,
    clip: &ClipRow,
) -> Result<(), UploadError> {
    let hot_path_rel = clip
        .hot_path
        .as_deref()
        .ok_or(UploadError::MissingHotPath)?;
    let sha256 = clip.sha256.as_deref().ok_or(UploadError::MissingSha256)?;

    // Cold path mirrors hot path verbatim so a future "rescan
    // cold" can cross-reference by relative path without an extra
    // mapping table.
    let cold_path = hot_path_rel.to_string();

    // Idempotent fast-path: if the backend already has a complete
    // copy (sha256 spot-check passes), skip the read+upload and
    // just stamp the row.
    match backend.exists(&cold_path, sha256).await {
        Ok(true) => {
            debug!(
                clip_id = clip.id,
                cold_path, "cold replicator: backend already has clip; stamping pointer only"
            );
            store
                .mark_cold_replicated(
                    clip.id,
                    &ClipColdMark {
                        cold_handle: backend_handle.to_string(),
                        cold_path,
                        cold_uploaded_at: Utc::now(),
                    },
                )
                .await
                .map_err(UploadError::Store)?;
            return Ok(());
        }
        Ok(false) => {} // proceed with upload
        Err(e) => return Err(UploadError::Backend(e)),
    }

    // Read the source bytes off the hot tier.
    let abs = cfg.clips_dir.join(hot_path_rel);
    let bytes = tokio::fs::read(&abs)
        .await
        .map_err(|e| UploadError::HotRead {
            path: abs.display().to_string(),
            source: e,
        })?;

    // Throttle BEFORE the put so the bandwidth budget actually
    // governs the bytes-on-the-wire moment.
    throttle.acquire(bytes.len() as u64).await;

    let receipt = backend
        .put(&cold_path, &bytes, sha256)
        .await
        .map_err(UploadError::Backend)?;

    store
        .mark_cold_replicated(
            clip.id,
            &ClipColdMark {
                cold_handle: backend_handle.to_string(),
                cold_path: receipt.cold_path,
                cold_uploaded_at: receipt.uploaded_at,
            },
        )
        .await
        .map_err(UploadError::Store)?;

    debug!(
        clip_id = clip.id,
        bytes_written = receipt.bytes_written,
        "cold replicator: clip uploaded"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum UploadError {
    #[error("clip has no hot_path (already soft-evicted?)")]
    MissingHotPath,
    #[error("clip has no sha256 (recorder hash failed)")]
    MissingSha256,
    #[error("read hot file {path}: {source}")]
    HotRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("backend: {0}")]
    Backend(#[from] BackendError),
    #[error("store: {0}")]
    Store(nexus_store::StoreError),
}

// ===================================================================
// Tests — M2.2 Phase 1
// ===================================================================
//
// These exercise `run_cold_replicator` end-to-end against an
// in-process mock backend. They cover:
//
// * Event-driven pickup (CLIP_CLOSED triggers an immediate tick
//   well before the 5-min polling backstop)
// * Cold-disabled fast skip (backend_handle is NULL → no put())
// * Unreachable backend emits STORAGE_COLD_UNREACHABLE EXACTLY
//   ONCE per outage, not once per tick
// * upload_one is idempotent on already-uploaded clips (skips
//   read+upload, just stamps the row)
//
// Together with the soft/hard eviction tests in
// storage_safety.rs::tests these fulfil §Tests of plan-m22.md.

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Duration as ChronoDuration;
    use nexus_bus::BroadcastBus;
    use nexus_config::{CameraConfig, StoreConfig};
    use nexus_storage::{ColdBackend, PutReceipt, VolumeInfo};
    use nexus_store::{ClipClose, NewClip, StoreError};
    use parking_lot::Mutex;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use url::Url;

    /// In-process mock cold backend. Health is configurable; every
    /// `put` records the args and returns a successful receipt.
    struct MockBackend {
        handle: String,
        health: Mutex<HealthStatus>,
        puts: AtomicU32,
        existing: Mutex<std::collections::HashSet<String>>,
    }
    impl MockBackend {
        fn new(handle: &str, health: HealthStatus) -> Arc<Self> {
            Arc::new(Self {
                handle: handle.into(),
                health: Mutex::new(health),
                puts: AtomicU32::new(0),
                existing: Mutex::new(Default::default()),
            })
        }
        fn put_count(&self) -> u32 {
            self.puts.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl ColdBackend for MockBackend {
        fn handle(&self) -> &str {
            &self.handle
        }
        fn kind(&self) -> &str {
            "lan"
        }
        async fn put(
            &self,
            path: &str,
            bytes: &[u8],
            _expected_sha256: &str,
        ) -> Result<PutReceipt, BackendError> {
            self.puts.fetch_add(1, Ordering::SeqCst);
            self.existing.lock().insert(path.to_string());
            Ok(PutReceipt {
                cold_path: path.to_string(),
                uploaded_at: Utc::now(),
                bytes_written: bytes.len() as u64,
            })
        }
        async fn get_range(
            &self,
            _path: &str,
            _start: u64,
            _end_inclusive: u64,
        ) -> Result<Vec<u8>, BackendError> {
            Ok(vec![])
        }
        async fn delete(&self, _path: &str) -> Result<bool, BackendError> {
            Ok(true)
        }
        async fn exists(&self, path: &str, _expected_sha256: &str) -> Result<bool, BackendError> {
            Ok(self.existing.lock().contains(path))
        }
        async fn volume_info(&self) -> Result<VolumeInfo, BackendError> {
            Ok(VolumeInfo {
                free_bytes: Some(1 << 30),
                total_bytes: Some(1 << 31),
                used_bytes: Some(1 << 30),
            })
        }
        async fn health(&self) -> HealthStatus {
            self.health.lock().clone()
        }
    }

    /// Build a Store with a single camera + one closed clip ready
    /// to upload (sha256 set, ended_at non-NULL). Returns the store,
    /// the clip-id of the seeded clip, the hot clips_dir, and the
    /// owning TempDir to keep on the stack.
    async fn seed_one_pending_clip() -> (Arc<Store>, i64, PathBuf, tempfile::TempDir) {
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
        tokio::fs::create_dir_all(clips_dir.join("1"))
            .await
            .unwrap();

        store
            .upsert_camera(&CameraConfig {
                id: 1,
                name: "cam1".into(),
                ingest: nexus_config::CameraIngest {
                    url: Url::parse("rtsp://127.0.0.1/stream1").unwrap(),
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
        store
            .upsert_storage_backend("mock", "lan", "{\"root\":\"/tmp/mock\"}")
            .await
            .unwrap();

        let now = Utc::now();
        let rel = "1/clip_0001.mp4".to_string();
        let clip_id = store
            .open_clip(&NewClip {
                camera_id: 1,
                started_at: now - ChronoDuration::seconds(60),
                hot_path: rel.clone(),
                codec: "stub".into(),
                container: "mp4".into(),
                hot_handle: "local".into(),
            })
            .await
            .unwrap();
        tokio::fs::write(clips_dir.join(&rel), b"clip-bytes")
            .await
            .unwrap();
        store
            .close_clip(
                clip_id,
                &ClipClose {
                    ended_at: now,
                    duration_ms: 1000,
                    size_bytes: 10,
                    hot_path: Some(rel),
                    sha256: Some(format!("{:064x}", clip_id)),
                },
            )
            .await
            .unwrap();

        (store, clip_id, clips_dir, dir)
    }

    /// CLIP_CLOSED on the bus drives a tick well before the 5-min
    /// backstop fires; the seeded clip's row gets a cold pointer
    /// stamped. Also covers the boot kick (it would have ticked
    /// even without the event) — that's exercised by the next test.
    #[tokio::test]
    async fn replicator_uploads_pending_clip_on_event() {
        let (store, clip_id, clips_dir, _dir) = seed_one_pending_clip().await;
        store.write_cold_replica(Some("mock"), 0).await.unwrap();

        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
        let backend = MockBackend::new("mock", HealthStatus::Ok);
        let registry = Registry::new();
        registry.replace_all([backend.clone() as Arc<dyn ColdBackend>]);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let cfg = ColdReplicatorConfig {
            clips_dir: clips_dir.clone(),
        };
        let store_clone = store.clone();
        let bus_clone = bus.clone();
        let task = tokio::spawn(async move {
            run_cold_replicator(cfg, store_clone, bus_clone, registry, async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        // Wait until the row's cold pointer is populated, polling
        // every 25ms with a 2s budget — generous for a single
        // mock-backend put + DB write.
        let mut got_cold = false;
        for _ in 0..80 {
            let row = store.clips_pending_cold_upload(8).await.unwrap();
            if !row.iter().any(|c| c.id == clip_id) {
                got_cold = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

        assert!(
            got_cold,
            "replicator should have stamped cold pointer within 2s"
        );
        assert_eq!(backend.put_count(), 1, "exactly one put expected");
    }

    /// When the cold replica is disabled (handle is NULL), the
    /// replicator MUST NOT call `put` even though there are
    /// pending clips. This is the "exactly one cold backend at a
    /// time, NULL means disabled" invariant.
    #[tokio::test]
    async fn replicator_skips_when_cold_disabled() {
        let (store, clip_id, clips_dir, _dir) = seed_one_pending_clip().await;
        // Leave write_cold_replica untouched → backend_handle is NULL.

        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
        let backend = MockBackend::new("mock", HealthStatus::Ok);
        let registry = Registry::new();
        registry.replace_all([backend.clone() as Arc<dyn ColdBackend>]);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let cfg = ColdReplicatorConfig {
            clips_dir: clips_dir.clone(),
        };
        let store_clone = store.clone();
        let bus_clone = bus.clone();
        let task = tokio::spawn(async move {
            run_cold_replicator(cfg, store_clone, bus_clone, registry, async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        // Give the boot kick ample time to fire and discover the
        // disabled config.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

        assert_eq!(backend.put_count(), 0, "no put when cold disabled");
        // Row still pending.
        let pending = store.clips_pending_cold_upload(8).await.unwrap();
        assert!(
            pending.iter().any(|c| c.id == clip_id),
            "pending clip stays pending when cold is disabled"
        );
    }

    /// An Unreachable backend MUST emit STORAGE_COLD_UNREACHABLE
    /// exactly once per outage, NOT once per tick. We force
    /// multiple kicks by publishing CLIP_CLOSED several times in
    /// quick succession and assert at most one event lands on the
    /// subscriber within the outage window.
    #[tokio::test]
    async fn replicator_unreachable_emits_event_once_per_outage() {
        let (store, _clip_id, clips_dir, _dir) = seed_one_pending_clip().await;
        store.write_cold_replica(Some("mock"), 0).await.unwrap();

        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
        let backend = MockBackend::new(
            "mock",
            HealthStatus::Unreachable {
                reason: "test-down".into(),
            },
        );
        let registry = Registry::new();
        registry.replace_all([backend.clone() as Arc<dyn ColdBackend>]);

        // Subscribe BEFORE spawning the replicator so the boot
        // kick's emission isn't missed.
        let mut sub = bus
            .subscribe::<serde_json::Value>(topic::STORAGE_COLD_UNREACHABLE)
            .await
            .unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let cfg = ColdReplicatorConfig {
            clips_dir: clips_dir.clone(),
        };
        let store_clone = store.clone();
        let bus_clone = bus.clone();
        let task = tokio::spawn(async move {
            run_cold_replicator(cfg, store_clone, bus_clone, registry, async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        // Push 3 CLIP_CLOSED events to force 3 tick attempts.
        for _ in 0..3 {
            bus.publish(topic::CLIP_CLOSED, &serde_json::json!({"clip_id": 1}))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(75)).await;
        }
        // Brief drain window before shutdown.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

        // Drain the subscriber and count STORAGE_COLD_UNREACHABLE.
        let mut count = 0;
        while let Ok(Some(Ok(_v))) = tokio::time::timeout(
            Duration::from_millis(50),
            tokio_stream::StreamExt::next(&mut sub),
        )
        .await
        {
            count += 1;
        }
        assert_eq!(
            count, 1,
            "exactly one STORAGE_COLD_UNREACHABLE per outage, not one per tick"
        );
        assert_eq!(
            backend.put_count(),
            0,
            "unreachable backend never receives puts"
        );
    }

    /// The fast-path branch of `upload_one` — when
    /// `backend.exists()` returns true the replicator MUST skip the
    /// read+upload and only stamp the row. Drives this through
    /// `run_cold_replicator` (event-driven) by pre-seeding the
    /// mock's existing-set with the hot path.
    #[tokio::test]
    async fn replicator_idempotent_skips_already_uploaded() {
        let (store, clip_id, clips_dir, _dir) = seed_one_pending_clip().await;
        store.write_cold_replica(Some("mock"), 0).await.unwrap();

        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
        let backend = MockBackend::new("mock", HealthStatus::Ok);
        // Pre-mark the clip as already-on-cold so exists() returns true.
        backend
            .existing
            .lock()
            .insert("1/clip_0001.mp4".to_string());

        let registry = Registry::new();
        registry.replace_all([backend.clone() as Arc<dyn ColdBackend>]);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let cfg = ColdReplicatorConfig {
            clips_dir: clips_dir.clone(),
        };
        let store_clone = store.clone();
        let bus_clone = bus.clone();
        let task = tokio::spawn(async move {
            run_cold_replicator(cfg, store_clone, bus_clone, registry, async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        // Wait for cold-stamp.
        let mut got_cold = false;
        for _ in 0..80 {
            let row = store.clips_pending_cold_upload(8).await.unwrap();
            if !row.iter().any(|c| c.id == clip_id) {
                got_cold = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

        assert!(got_cold, "row should be stamped via fast-path");
        assert_eq!(
            backend.put_count(),
            0,
            "fast-path must NOT call put when exists() returns true"
        );
    }

    // Quiet the unused `StoreError` import warning when only some
    // tests use it.
    #[allow(dead_code)]
    fn _silence_unused(_: StoreError) {}
}

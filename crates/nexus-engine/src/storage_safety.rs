//! Storage safety floor — M2.1 Stage A (PR 4).
//!
//! Periodically samples free space under `clips_dir` and, when it
//! drops below the operator-set thresholds, takes two actions:
//!
//! 1. Flips `recorder.set_panic(true)` so no NEW clips are opened
//!    (in-flight clips still close cleanly — see
//!    [`crate::recorder::ClipRecorder`] in nexus-pipeline).
//! 2. Walks `cameras_with_clips` round-robin and deletes their
//!    oldest clip until either free space recovers past
//!    `low_watermark_pct + HYSTERESIS_PCT` or there are no clips
//!    left to evict.
//!
//! This is the *safety floor*. It is intentionally aggressive: when
//! the disk is hot, retention semantics get suspended in favour of
//! "do not lock the device". The retention sweeper (PR 6) is the
//! polite, configurable cousin that runs once a day in steady state.
//!
//! Design split for testability:
//!
//! * [`FreeSpaceProbe`] — trait. Real impl is [`StatvfsProbe`]
//!   under `cfg(unix)`. Tests use the inline `MockProbe`.
//! * [`WatermarkController`] — pure FSM. No I/O. All hysteresis
//!   logic lives here so it can be exhaustively tested.
//! * [`run_storage_safety`] — the background loop. Glues probe →
//!   controller → recorder + store + bus.
//!
//! The whole thing lives in nexus-engine (binary crate) because it
//! needs the wired Store / Recorder / Bus and nothing else
//! consumes it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use nexus_bus::{topic, Bus, BusExt};
use nexus_pipeline::ClipRecorder;
use nexus_store::Store;
use nexus_types::CameraId;
use serde::{Deserialize, Serialize};
use tokio::time::interval;
use tracing::{debug, error, info, warn};

/// How far above `low_watermark_pct` we have to climb before clearing
/// panic mode. Prevents flapping when eviction frees just enough to
/// dip back under the threshold.
pub const HYSTERESIS_PCT: u8 = 5;

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(unix)]
    #[error("statvfs: {0}")]
    Statvfs(#[from] nix::errno::Errno),
}

/// What the watermark sampler asks the OS for. Returns the percent
/// of bytes FREE under the path (0..=100).
#[async_trait]
pub trait FreeSpaceProbe: Send + Sync {
    async fn free_pct(&self) -> Result<f32, ProbeError>;
}

/// Real implementation. Reads `statvfs(path)` and returns
/// `bavail * frsize / (blocks * frsize) * 100` — i.e. the percent
/// of *user-available* bytes free, NOT raw free-bytes (those count
/// reserved-for-root space).
#[cfg(unix)]
pub struct StatvfsProbe {
    pub path: PathBuf,
}

#[cfg(unix)]
#[async_trait]
impl FreeSpaceProbe for StatvfsProbe {
    async fn free_pct(&self) -> Result<f32, ProbeError> {
        // statvfs is blocking (single syscall, but blocking). Run on
        // the blocking pool so we don't stall the runtime.
        let path = self.path.clone();
        let stat = tokio::task::spawn_blocking(move || nix::sys::statvfs::statvfs(path.as_path()))
            .await
            .map_err(|e| ProbeError::Io(std::io::Error::other(format!("spawn_blocking: {e}"))))??;
        let blocks = stat.blocks() as f64;
        let avail = stat.blocks_available() as f64;
        if blocks <= 0.0 {
            return Ok(0.0);
        }
        Ok(((avail / blocks) * 100.0) as f32)
    }
}

/// Non-unix fallback: never reports low space. Logs once at
/// construction so operators see why no eviction is happening.
#[cfg(not(unix))]
pub struct StatvfsProbe {
    pub path: PathBuf,
}

#[cfg(not(unix))]
#[async_trait]
impl FreeSpaceProbe for StatvfsProbe {
    async fn free_pct(&self) -> Result<f32, ProbeError> {
        Ok(100.0)
    }
}

#[cfg(not(unix))]
impl StatvfsProbe {
    pub fn warn_once_about_platform(&self) {
        warn!(
            path = %self.path.display(),
            "watermark sampler is a no-op on this platform; clips_dir will fill until manually cleaned"
        );
    }
}

// ---------------------------------------------------------------------------
// Watermark FSM
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatermarkLevel {
    Ok,
    Low,
    Panic,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transition {
    /// No level change this tick.
    Stable,
    /// Crossed into a more-severe level.
    Entered(WatermarkLevel),
    /// Recovered to a less-severe level.
    Exited(WatermarkLevel),
}

/// Pure, deterministic FSM. Inputs: free%, low%, panic%. Output:
/// the current level + whether this tick changed it.
///
/// Hysteresis rules (the WHY for ops):
/// * Ok → Low when free <= low_pct.
/// * Low → Panic when free <= panic_pct.
/// * Panic → Low only when free >= low_pct + HYSTERESIS_PCT.
/// * Low → Ok only when free >= low_pct + HYSTERESIS_PCT.
///
/// The two recoveries use the SAME hysteresis floor on purpose:
/// the controller never re-enters a less-severe level just because
/// free pct nudged a hair above the threshold, which would make the
/// recorder-panic flag flap and the bus event re-fire.
#[derive(Debug, Clone)]
pub struct WatermarkController {
    low_pct: u8,
    panic_pct: u8,
    level: WatermarkLevel,
}

impl WatermarkController {
    pub fn new(low_pct: u8, panic_pct: u8) -> Self {
        debug_assert!(panic_pct <= low_pct, "panic_pct must be <= low_pct");
        Self {
            low_pct,
            panic_pct,
            level: WatermarkLevel::Ok,
        }
    }

    pub fn level(&self) -> WatermarkLevel {
        self.level
    }

    pub fn observe(&mut self, free_pct: f32) -> Transition {
        let recovery_floor = (self.low_pct + HYSTERESIS_PCT) as f32;
        let new_level = match self.level {
            WatermarkLevel::Ok => {
                if free_pct <= self.panic_pct as f32 {
                    WatermarkLevel::Panic
                } else if free_pct <= self.low_pct as f32 {
                    WatermarkLevel::Low
                } else {
                    WatermarkLevel::Ok
                }
            }
            WatermarkLevel::Low => {
                if free_pct <= self.panic_pct as f32 {
                    WatermarkLevel::Panic
                } else if free_pct >= recovery_floor {
                    WatermarkLevel::Ok
                } else {
                    WatermarkLevel::Low
                }
            }
            WatermarkLevel::Panic => {
                if free_pct >= recovery_floor {
                    // Skip past Low and go straight to Ok if we have
                    // climbed far enough. Otherwise drop one notch.
                    WatermarkLevel::Ok
                } else if free_pct > self.panic_pct as f32 {
                    WatermarkLevel::Low
                } else {
                    WatermarkLevel::Panic
                }
            }
        };

        if new_level == self.level {
            Transition::Stable
        } else {
            let trans = if Self::severity(new_level) > Self::severity(self.level) {
                Transition::Entered(new_level)
            } else {
                Transition::Exited(new_level)
            };
            self.level = new_level;
            trans
        }
    }

    fn severity(l: WatermarkLevel) -> u8 {
        match l {
            WatermarkLevel::Ok => 0,
            WatermarkLevel::Low => 1,
            WatermarkLevel::Panic => 2,
        }
    }
}

// ---------------------------------------------------------------------------
// Bus payload
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoragePanicEvent {
    pub level: WatermarkLevel,
    pub free_pct: f32,
    pub low_pct: u8,
    pub panic_pct: u8,
    pub clips_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Background task
// ---------------------------------------------------------------------------

/// Configuration plumbed in from `RuntimeConfig.clips`.
#[derive(Debug, Clone)]
pub struct StorageSafetyConfig {
    pub clips_dir: PathBuf,
    pub low_watermark_pct: u8,
    pub panic_watermark_pct: u8,
    pub sample_interval: Duration,
}

/// Run the watermark sampler + eviction loop until the runtime
/// shuts down. Spawned by `nexus-engine::main` once the store +
/// recorder + bus are wired. Returns only on probe error or when
/// the runtime is dropped.
pub async fn run_storage_safety(
    cfg: StorageSafetyConfig,
    probe: Arc<dyn FreeSpaceProbe>,
    recorder: Arc<dyn ClipRecorder>,
    store: Arc<Store>,
    bus: Arc<dyn Bus>,
) -> Result<()> {
    info!(
        clips_dir = %cfg.clips_dir.display(),
        low_pct = cfg.low_watermark_pct,
        panic_pct = cfg.panic_watermark_pct,
        interval_secs = cfg.sample_interval.as_secs(),
        "storage safety loop starting"
    );

    let mut controller = WatermarkController::new(cfg.low_watermark_pct, cfg.panic_watermark_pct);
    let mut tick = interval(cfg.sample_interval);
    // First tick fires immediately so we don't wait a whole interval
    // before noticing a disk that booted already-full.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut rr_cursor: usize = 0;
    loop {
        tick.tick().await;
        let free_pct = match probe.free_pct().await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "free-space probe failed; assuming Ok this tick");
                continue;
            }
        };
        let trans = controller.observe(free_pct);
        debug!(free_pct, level = ?controller.level(), ?trans, "watermark tick");

        match trans {
            Transition::Entered(WatermarkLevel::Panic) => {
                recorder.set_panic(true);
                warn!(
                    free_pct,
                    low_pct = cfg.low_watermark_pct,
                    panic_pct = cfg.panic_watermark_pct,
                    "PANIC: storage below panic_watermark_pct — refusing new clips + evicting"
                );
                publish_storage_event(&bus, &cfg, controller.level(), free_pct).await;
            }
            Transition::Exited(WatermarkLevel::Ok) => {
                recorder.set_panic(false);
                info!(free_pct, "storage recovered to Ok — clearing panic flag");
                publish_storage_event(&bus, &cfg, controller.level(), free_pct).await;
            }
            Transition::Entered(WatermarkLevel::Low) | Transition::Exited(WatermarkLevel::Low) => {
                publish_storage_event(&bus, &cfg, controller.level(), free_pct).await;
            }
            _ => {}
        }

        // Eviction: any time we are at Low or Panic, do round-robin
        // per-camera oldest-clip eviction. One clip per tick keeps
        // the loop bounded; the next tick will pick up if we still
        // need to free more.
        if matches!(
            controller.level(),
            WatermarkLevel::Low | WatermarkLevel::Panic
        ) {
            if let Err(e) = evict_one(&store, &cfg.clips_dir, &mut rr_cursor).await {
                warn!(error = %e, "eviction step failed");
            }
        }
    }
}

async fn publish_storage_event(
    bus: &Arc<dyn Bus>,
    cfg: &StorageSafetyConfig,
    level: WatermarkLevel,
    free_pct: f32,
) {
    let payload = StoragePanicEvent {
        level,
        free_pct,
        low_pct: cfg.low_watermark_pct,
        panic_pct: cfg.panic_watermark_pct,
        clips_dir: cfg.clips_dir.clone(),
    };
    if let Err(e) = bus.publish(topic::STORAGE_PANIC, &payload).await {
        warn!(error = %e, "failed to publish storage.panic event");
    }
}

/// Per-camera round-robin: walk the camera list once, picking the
/// next camera that still has at least one clip, and evict its
/// oldest. `rr_cursor` is mutated to advance the round-robin so we
/// don't always hammer camera 0.
///
/// Returns Ok(()) whether or not a clip was actually evicted; the
/// caller logs and tries again next tick.
async fn evict_one(
    store: &Arc<Store>,
    clips_dir: &Path,
    rr_cursor: &mut usize,
) -> anyhow::Result<()> {
    let cams: Vec<CameraId> = store.cameras_with_clips().await?;
    if cams.is_empty() {
        return Ok(());
    }
    let n = cams.len();
    for offset in 0..n {
        let idx = (*rr_cursor + offset) % n;
        let cam = cams[idx];
        if let Some(clip) = store.oldest_clip_for_camera(cam).await? {
            // Resolve absolute path. ClipRow.path is stored relative
            // to clips_dir (per StubClipRecorder; PR 5 will document
            // this for any Stage B real recorder too).
            let abs = clips_dir.join(&clip.path);
            match tokio::fs::remove_file(&abs).await {
                Ok(()) => {
                    debug!(camera_id = cam, clip_id = clip.id, path = %abs.display(), "evicted clip file");
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    debug!(camera_id = cam, clip_id = clip.id, "clip file already gone");
                }
                Err(e) => {
                    warn!(camera_id = cam, clip_id = clip.id, error = %e, "remove_file failed; deleting metadata anyway");
                }
            }
            store.cascade_delete_clip_metadata(clip.id).await?;
            *rr_cursor = idx + 1;
            return Ok(());
        }
    }
    // Nothing to evict (every camera had 0 clips). Advance cursor
    // to avoid recomputing the same scan immediately.
    *rr_cursor = rr_cursor.wrapping_add(1);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- WatermarkController ---

    #[test]
    fn ok_to_low_at_low_pct() {
        let mut c = WatermarkController::new(15, 5);
        assert_eq!(c.observe(50.0), Transition::Stable);
        assert_eq!(c.observe(15.0), Transition::Entered(WatermarkLevel::Low));
        assert_eq!(c.level(), WatermarkLevel::Low);
    }

    #[test]
    fn low_to_panic_at_panic_pct() {
        let mut c = WatermarkController::new(15, 5);
        let _ = c.observe(15.0); // -> Low
        assert_eq!(c.observe(5.0), Transition::Entered(WatermarkLevel::Panic));
        assert_eq!(c.level(), WatermarkLevel::Panic);
    }

    #[test]
    fn panic_does_not_recover_until_above_low_plus_hysteresis() {
        let mut c = WatermarkController::new(15, 5);
        let _ = c.observe(2.0); // straight to Panic from Ok (free < panic)
        assert_eq!(c.level(), WatermarkLevel::Panic);
        // Bouncing right back to 6% (just above panic) shouldn't
        // fully recover — has to stay at Low at most.
        assert_eq!(c.observe(6.0), Transition::Exited(WatermarkLevel::Low));
        assert_eq!(c.level(), WatermarkLevel::Low);
        // Right at low_pct still stays in Low (recovery needs >= 20%).
        assert_eq!(c.observe(15.0), Transition::Stable);
        assert_eq!(c.level(), WatermarkLevel::Low);
        // Cross hysteresis floor -> Ok.
        assert_eq!(c.observe(20.0), Transition::Exited(WatermarkLevel::Ok));
    }

    #[test]
    fn ok_jumps_straight_to_panic_when_free_below_panic() {
        let mut c = WatermarkController::new(15, 5);
        assert_eq!(c.observe(3.0), Transition::Entered(WatermarkLevel::Panic));
    }

    #[test]
    fn panic_jumps_straight_to_ok_when_recovery_is_dramatic() {
        let mut c = WatermarkController::new(15, 5);
        let _ = c.observe(2.0); // Panic
        assert_eq!(c.observe(80.0), Transition::Exited(WatermarkLevel::Ok));
    }

    #[test]
    fn flapping_around_low_pct_stays_in_low_after_first_entry() {
        let mut c = WatermarkController::new(15, 5);
        assert_eq!(c.observe(15.0), Transition::Entered(WatermarkLevel::Low));
        // Bobbing in 16-19 stays Low (below recovery floor of 20).
        assert_eq!(c.observe(18.0), Transition::Stable);
        assert_eq!(c.observe(14.0), Transition::Stable);
        assert_eq!(c.observe(19.0), Transition::Stable);
    }

    // --- Probe ---

    #[cfg(unix)]
    #[tokio::test]
    async fn statvfs_probe_returns_sane_value_for_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let probe = StatvfsProbe {
            path: dir.path().to_path_buf(),
        };
        let pct = probe.free_pct().await.unwrap();
        assert!(
            (0.0..=100.0).contains(&pct),
            "free pct should be in 0..=100, got {pct}"
        );
    }

    // --- End-to-end: panic -> evict -> recover ---

    use nexus_bus::BroadcastBus;
    use nexus_config::{CameraConfig, StoreConfig};
    use nexus_pipeline::{ClipRecorder, StubClipRecorder};
    use nexus_store::NewClip;
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use url::Url;

    /// Returns the next pre-programmed value each call. When the
    /// queue runs dry, repeats the last value forever — keeps the
    /// loop deterministic without panicking.
    struct MockProbe(Mutex<VecDeque<f32>>, Mutex<f32>);

    impl MockProbe {
        fn new(values: Vec<f32>) -> Self {
            let last = *values.last().unwrap_or(&50.0);
            Self(Mutex::new(values.into()), Mutex::new(last))
        }
    }

    #[async_trait]
    impl FreeSpaceProbe for MockProbe {
        async fn free_pct(&self) -> Result<f32, ProbeError> {
            let mut q = self.0.lock();
            if let Some(v) = q.pop_front() {
                *self.1.lock() = v;
                Ok(v)
            } else {
                Ok(*self.1.lock())
            }
        }
    }

    async fn build_store_with_camera() -> (Arc<Store>, tempfile::TempDir, PathBuf) {
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
        store
            .upsert_camera(&CameraConfig {
                id: 1,
                name: "front".into(),
                url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                enabled: true,
                prompts: vec![],
                model_override: None,
                zones: vec![],
                max_fps: 0,
                parking_lot_mode: false,
            })
            .await
            .unwrap();
        let clips_dir = dir.path().join("clips");
        tokio::fs::create_dir_all(&clips_dir).await.unwrap();
        (store, dir, clips_dir)
    }

    /// Drives the full FSM: starts at 50% (Ok), drops to 2% (Panic
    /// + evict), recovers to 80% (back to Ok). Verifies recorder
    /// panic flag flips at each transition AND that an oldest clip
    /// got evicted in the panic tick.
    #[tokio::test]
    async fn watermark_panic_evict_recover_cycle() {
        let (store, _dir, clips_dir) = build_store_with_camera().await;
        let recorder: Arc<dyn ClipRecorder> =
            Arc::new(StubClipRecorder::new(store.clone(), clips_dir.clone()));
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));

        // Seed two clips so eviction has something to chew on.
        let _id1 = store
            .open_clip(&NewClip {
                camera_id: 1,
                started_at: chrono::Utc::now() - chrono::Duration::minutes(10),
                path: "cam1/oldest.mp4".into(),
                codec: "stub".into(),
                container: "mp4".into(),
                backend_id: "local".into(),
            })
            .await
            .unwrap();
        let _id2 = store
            .open_clip(&NewClip {
                camera_id: 1,
                started_at: chrono::Utc::now() - chrono::Duration::minutes(5),
                path: "cam1/newer.mp4".into(),
                codec: "stub".into(),
                container: "mp4".into(),
                backend_id: "local".into(),
            })
            .await
            .unwrap();
        // Materialise the on-disk files so the eviction's
        // remove_file actually has a target.
        tokio::fs::create_dir_all(clips_dir.join("cam1"))
            .await
            .unwrap();
        tokio::fs::write(clips_dir.join("cam1/oldest.mp4"), b"x")
            .await
            .unwrap();
        tokio::fs::write(clips_dir.join("cam1/newer.mp4"), b"x")
            .await
            .unwrap();

        let cfg = StorageSafetyConfig {
            clips_dir: clips_dir.clone(),
            low_watermark_pct: 15,
            panic_watermark_pct: 5,
            sample_interval: Duration::from_millis(20),
        };
        // Tick 1: 50% -> Ok. Tick 2: 2% -> Panic + evict.
        // Tick 3: 80% -> Ok (clears panic). The MockProbe will
        // repeat 80.0 forever after that, so the loop stays Ok.
        let probe = Arc::new(MockProbe::new(vec![50.0, 2.0, 80.0])) as Arc<dyn FreeSpaceProbe>;

        let recorder_for_loop = recorder.clone();
        let store_for_loop = store.clone();
        let bus_for_loop = bus.clone();
        let handle = tokio::spawn(async move {
            let _ = run_storage_safety(cfg, probe, recorder_for_loop, store_for_loop, bus_for_loop)
                .await;
        });

        // Wait for at least the panic tick to land. Recorder panic
        // flag goes true, then false again.
        let mut saw_panic = false;
        for _ in 0..200 {
            if recorder.is_panic() {
                saw_panic = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            saw_panic,
            "recorder.is_panic should have flipped true on the 2% tick"
        );

        // Then it should clear once the 80% tick lands.
        let mut saw_recover = false;
        for _ in 0..200 {
            if !recorder.is_panic() {
                saw_recover = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            saw_recover,
            "recorder.is_panic should have cleared on the 80% tick"
        );

        // Oldest clip must be gone (file + row). The newer clip
        // may or may not be — depends on timing; we only assert
        // the deterministic invariant.
        // Wait briefly for the eviction's DB delete to commit.
        let mut evicted = false;
        for _ in 0..200 {
            if !clips_dir.join("cam1/oldest.mp4").exists() {
                evicted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            evicted,
            "oldest clip file should have been evicted in the panic tick"
        );

        handle.abort();
    }

    // ---------------------------------------------------------------
    // B6 acceptance test — drive the *real* StatvfsProbe against an
    // operator-prepared low-disk filesystem (tmpfs on Linux,
    // hdiutil-backed RAM disk on macOS, or any small partition).
    //
    // Skipped by default. To run:
    //
    //   # Linux
    //   sudo mkdir -p /mnt/nexus-test-tmpfs
    //   sudo mount -t tmpfs -o size=64M tmpfs /mnt/nexus-test-tmpfs
    //   sudo chown $USER /mnt/nexus-test-tmpfs
    //   NEXUS_LOW_DISK_TEST_DIR=/mnt/nexus-test-tmpfs \
    //     cargo test -p nexus-engine -- --ignored \
    //       tmpfs_acceptance_panic_then_recover --nocapture
    //
    //   # macOS (creates an in-memory 64MB HFS+ disk)
    //   DEV=$(hdiutil attach -nomount ram://131072 | xargs)
    //   diskutil erasevolume HFS+ nexus-test "$DEV"
    //   NEXUS_LOW_DISK_TEST_DIR=/Volumes/nexus-test \
    //     cargo test -p nexus-engine -- --ignored \
    //       tmpfs_acceptance_panic_then_recover --nocapture
    //
    // The test refuses to run on a filesystem ≥ 1 GiB so a misset
    // env var can never fill someone's real disk.
    // ---------------------------------------------------------------

    /// Wall-clock budget for the panic / recovery transitions. The
    /// loop's sample interval is 200ms, so 3s gives us ~15 ticks of
    /// slack — more than enough on a busy CI runner without
    /// masking real bugs.
    const TMPFS_TRANSITION_BUDGET: Duration = Duration::from_secs(3);

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires NEXUS_LOW_DISK_TEST_DIR pointing at a small (<1 GiB) tmpfs / ramdisk"]
    async fn tmpfs_acceptance_panic_then_recover() {
        let Ok(raw) = std::env::var("NEXUS_LOW_DISK_TEST_DIR") else {
            eprintln!(
                "skip: set NEXUS_LOW_DISK_TEST_DIR=<small-tmpfs-mountpoint> to run this acceptance test"
            );
            return;
        };
        let test_dir = PathBuf::from(raw);
        assert!(
            test_dir.is_dir(),
            "{} must exist and be a directory",
            test_dir.display()
        );

        // Hard refuse on anything that looks like a real disk. tmpfs
        // / ramdisk for this test should be tens of MiB, not gigs.
        let stat0 = nix::sys::statvfs::statvfs(test_dir.as_path()).unwrap();
        let frag = stat0.fragment_size() as u64;
        let blocks = stat0.blocks() as u64;
        let avail_blocks = stat0.blocks_available() as u64;
        let total_bytes = blocks * frag;
        let avail_bytes = avail_blocks * frag;
        assert!(
            total_bytes < 1024 * 1024 * 1024,
            "NEXUS_LOW_DISK_TEST_DIR points at a {} byte fs; expected < 1 GiB so we don't fill a real disk",
            total_bytes
        );
        assert!(
            avail_bytes > 64 * 1024,
            "test fs needs at least 64 KiB free to seed the sentinel file; only {} bytes free",
            avail_bytes
        );

        // Spin up store + recorder + bus rooted at the test fs so
        // sqlite's WAL also lives on the small mount (matches the
        // production layout — clips_dir + db share a partition).
        let db_path = test_dir.join("nexus-acceptance.db");
        let _ = tokio::fs::remove_file(&db_path).await;
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
        let clips_dir = test_dir.join("acceptance-clips");
        let _ = tokio::fs::remove_dir_all(&clips_dir).await;
        tokio::fs::create_dir_all(&clips_dir).await.unwrap();

        let recorder: Arc<dyn ClipRecorder> =
            Arc::new(StubClipRecorder::new(store.clone(), clips_dir.clone()));
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
        let probe: Arc<dyn FreeSpaceProbe> = Arc::new(StatvfsProbe {
            path: test_dir.clone(),
        });

        // Sample fast so the test runs end-to-end in seconds.
        let cfg = StorageSafetyConfig {
            clips_dir: clips_dir.clone(),
            low_watermark_pct: 15,
            panic_watermark_pct: 5,
            sample_interval: Duration::from_millis(200),
        };

        let recorder_for_loop = recorder.clone();
        let store_for_loop = store.clone();
        let bus_for_loop = bus.clone();
        let cfg_for_loop = cfg.clone();
        let probe_for_loop = probe.clone();
        let handle = tokio::spawn(async move {
            let _ = run_storage_safety(
                cfg_for_loop,
                probe_for_loop,
                recorder_for_loop,
                store_for_loop,
                bus_for_loop,
            )
            .await;
        });

        // Baseline: tmpfs is fresh, free pct should be > panic
        // threshold, and the recorder must not be in panic mode
        // after the first sample tick.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !recorder.is_panic(),
            "recorder should NOT be panicked at baseline (free fs just mounted)"
        );

        // Flood the fs to <= 2% free. Reserve a small headroom
        // below total so the kernel's tmpfs accounting doesn't
        // refuse the write outright (we want the *engine* to
        // observe panic via statvfs, not get an ENOSPC mid-write).
        let sentinel_path = test_dir.join("nexus-acceptance.fill");
        let target_consume = avail_bytes.saturating_sub(total_bytes / 100); // leave ~1% headroom
        write_sentinel(&sentinel_path, target_consume).await;

        // Within one sample interval (with slack), the loop must
        // observe the new free% and flip the recorder.
        let panicked = wait_for(TMPFS_TRANSITION_BUDGET, || recorder.is_panic()).await;
        assert!(
            panicked,
            "recorder.is_panic() never became true after filling fs to <{}% within {:?}",
            cfg.panic_watermark_pct, TMPFS_TRANSITION_BUDGET
        );

        // Drop the sentinel — free space jumps back. Eviction may
        // also be running in the background and removing test
        // clips, but with no clips seeded it's a no-op.
        tokio::fs::remove_file(&sentinel_path).await.unwrap();
        let recovered = wait_for(TMPFS_TRANSITION_BUDGET, || !recorder.is_panic()).await;
        assert!(
            recovered,
            "recorder.is_panic() never cleared after freeing the sentinel within {:?}",
            TMPFS_TRANSITION_BUDGET
        );

        handle.abort();

        // Best-effort cleanup so the operator's tmpfs is left tidy
        // for the next run.
        let _ = tokio::fs::remove_dir_all(&clips_dir).await;
        let _ = tokio::fs::remove_file(&db_path).await;
        let _ = tokio::fs::remove_file(test_dir.join("nexus-acceptance.db-wal")).await;
        let _ = tokio::fs::remove_file(test_dir.join("nexus-acceptance.db-shm")).await;
    }

    /// Write `bytes` zeroes to `path`. Uses 1 MiB chunks so we
    /// don't allocate a giant buffer for multi-MiB sentinels.
    async fn write_sentinel(path: &Path, bytes: u64) {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::File::create(path).await.unwrap();
        let chunk = vec![0u8; 1024 * 1024];
        let mut remaining = bytes;
        while remaining > 0 {
            let n = remaining.min(chunk.len() as u64) as usize;
            f.write_all(&chunk[..n]).await.unwrap();
            remaining -= n as u64;
        }
        f.flush().await.unwrap();
        // sync_all so statvfs sees the new occupancy on the next
        // sample. Without this the kernel may still be lazily
        // accounting the writes when the loop ticks.
        f.sync_all().await.unwrap();
    }

    /// Polls `pred` every 50ms up to `budget`. Returns whether the
    /// predicate ever returned true.
    async fn wait_for<F: Fn() -> bool>(budget: Duration, pred: F) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if pred() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

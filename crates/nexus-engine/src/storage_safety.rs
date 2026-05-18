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

impl WatermarkLevel {
    fn as_u8(self) -> u8 {
        match self {
            WatermarkLevel::Ok => 0,
            WatermarkLevel::Low => 1,
            WatermarkLevel::Panic => 2,
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            0 => WatermarkLevel::Ok,
            1 => WatermarkLevel::Low,
            _ => WatermarkLevel::Panic,
        }
    }
}

/// Cheap-to-clone shared signal of the current watermark level,
/// surfaced for the M2.2 Phase 4 cold-read cache: cache jobs read
/// [`WatermarkSignal::level`] before starting and `select!` on
/// [`WatermarkSignal::pressure_notified`] mid-stream so an in-flight
/// rehydrate is cancelled when the disk tips into Low or Panic.
///
/// Internally an `AtomicU8` (lock-free read on the hot path) and a
/// `tokio::sync::Notify` that is `notify_waiters()`-pinged ONLY on
/// transitions INTO Low or Panic. Recovery transitions are silent —
/// readers re-check `level()` next time.
#[derive(Clone, Default)]
pub struct WatermarkSignal {
    inner: Arc<WatermarkSignalInner>,
}

#[derive(Default)]
struct WatermarkSignalInner {
    level: std::sync::atomic::AtomicU8,
    pressure: tokio::sync::Notify,
}

impl WatermarkSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current level. Lock-free.
    pub fn level(&self) -> WatermarkLevel {
        WatermarkLevel::from_u8(self.inner.level.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// `true` iff the disk is healthy enough to start new cold-read
    /// cache work. Convenience wrapper.
    pub fn is_ok(&self) -> bool {
        matches!(self.level(), WatermarkLevel::Ok)
    }

    /// Future that resolves the next time the level transitions
    /// into Low or Panic. Use in `select!` arms to cancel
    /// long-running cache jobs.
    pub async fn pressure_notified(&self) {
        self.inner.pressure.notified().await
    }

    /// Update the stored level and wake any pressure waiters when
    /// the new level is Low or Panic. Called by
    /// [`run_storage_safety`] after every `observe()`. Public
    /// only because tests construct + drive the signal directly.
    pub fn set(&self, level: WatermarkLevel) {
        let prev = self
            .inner
            .level
            .swap(level.as_u8(), std::sync::atomic::Ordering::Release);
        if level != WatermarkLevel::Ok && WatermarkLevel::from_u8(prev) != level {
            // Edge-trigger waiters only on Ok→Low, Ok→Panic, or
            // Low→Panic — repeating the same non-Ok level is a
            // steady state and shouldn't re-wake.
            self.inner.pressure.notify_waiters();
        }
    }
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
///
/// `signal` is the shared [`WatermarkSignal`] read by the M2.2
/// cold-read cache to gate rehydrate jobs. The loop updates it
/// after every `observe()`; tests that don't care about the signal
/// can pass `WatermarkSignal::new()` (a fresh, ignored handle).
pub async fn run_storage_safety(
    cfg: StorageSafetyConfig,
    probe: Arc<dyn FreeSpaceProbe>,
    recorder: Arc<dyn ClipRecorder>,
    store: Arc<Store>,
    bus: Arc<dyn Bus>,
    signal: WatermarkSignal,
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

        // Mirror the new level into the shared signal for the cold-
        // read cache. Cheap atomic store; only wakes pressure
        // waiters on Ok→Low / Ok→Panic / Low→Panic transitions.
        signal.set(controller.level());

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
            if let Err(e) = evict_one(&store, &cfg.clips_dir, &bus, &mut rr_cursor).await {
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
/// next camera that still has an evictable clip. M2.2 splits this
/// into a two-pass strategy:
///
/// **Pass 1 (soft-evict).** For the chosen camera, look up
/// [`Store::find_soft_evict_candidate`] — the oldest clip that has
/// BOTH a hot pointer AND a cold pointer. If found, drop the hot
/// copy: file unlink FIRST (so a crash can never leave the row
/// pointing at a freed inode that's been reallocated), then
/// [`Store::clear_hot_pointer`] which uses a WHERE-guard to atomic
/// update only if `cold_handle IS NOT NULL`. Emit
/// [`topic::CLIP_HOT_EVICTED`]. The row + cold pointer + linked
/// motion_events all stay intact — playback keeps working from
/// cold via the future Phase 4 streaming path.
///
/// **Pass 2 (hard-evict, fallback).** If no soft candidate exists
/// for that camera, fall back to
/// [`Store::find_hard_evict_candidate`] which yields the oldest
/// clip that was NEVER cold-replicated. This goes through the M2.1
/// metadata-FIRST cascade-delete path so motion_events + linked
/// `events` rows tear down with the clip. Emit
/// [`topic::CLIP_HARD_EVICTED`].
///
/// **Cold-replicated clips are undeletable.** The hard-evict
/// candidate's `cold_handle IS NULL` guard makes this structural:
/// once a clip lives on cold, only the soft path can touch it from
/// the safety floor, and the soft path leaves cold alone. Operator
/// intervention via the admin API is the only way to reclaim cold.
///
/// Returns `Ok(())` whether or not a clip was actually evicted; the
/// caller logs and tries again next tick.
async fn evict_one(
    store: &Arc<Store>,
    clips_dir: &Path,
    bus: &Arc<dyn Bus>,
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

        // ----- Pass 1: soft-evict (hot drop, cold preserved) -----
        if let Some(clip) = store.find_soft_evict_candidate(cam).await? {
            let hot_path = match clip.hot_path.as_deref() {
                Some(p) => p,
                None => {
                    // Defensive: the soft-candidate query already
                    // filters on hot_handle IS NOT NULL, so this
                    // arm shouldn't fire. Fall through to Pass 2
                    // for the same camera.
                    debug!(
                        camera_id = cam,
                        clip_id = clip.id,
                        "soft candidate had no hot_path; skipping"
                    );
                    return Ok(());
                }
            };
            let abs = clips_dir.join(hot_path);

            // **File FIRST, DB SECOND.** Inverts the M2.1 ordering
            // because the row must stay intact (cold pointer + the
            // motion_events that hang off it). If the row update
            // fails after the file unlink the next tick re-picks
            // the same row and the unlink is a no-op (NotFound is
            // not an error here); the row eventually clears.
            match tokio::fs::remove_file(&abs).await {
                Ok(()) => {
                    debug!(
                        camera_id = cam,
                        clip_id = clip.id,
                        path = %abs.display(),
                        "soft-evict: hot file removed"
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    debug!(
                        camera_id = cam,
                        clip_id = clip.id,
                        "soft-evict: hot file already gone"
                    );
                }
                Err(e) => {
                    warn!(
                        camera_id = cam,
                        clip_id = clip.id,
                        error = %e,
                        path = %abs.display(),
                        "soft-evict: remove_file failed; aborting this round (orphan-file scan will reap)"
                    );
                    *rr_cursor = idx + 1;
                    return Ok(());
                }
            }

            let n_cleared = store.clear_hot_pointer(clip.id).await?;
            if n_cleared == 0 {
                // WHERE-guard rejected (cold pointer disappeared
                // between the find and the clear, or another
                // soft-evict already ran). The unlink already
                // happened so the orphan scan is the safety net.
                warn!(
                    camera_id = cam,
                    clip_id = clip.id,
                    "soft-evict: clear_hot_pointer rejected (cold pointer missing?); not emitting event"
                );
            } else {
                let payload = serde_json::json!({
                    "clip_id": clip.id,
                    "camera_id": cam,
                    "cold_handle": clip.cold_handle,
                    "cold_path": clip.cold_path,
                    "freed_bytes": clip.size_bytes,
                });
                if let Err(e) = bus.publish(topic::CLIP_HOT_EVICTED, &payload).await {
                    warn!(error = %e, "publish CLIP_HOT_EVICTED failed");
                }
            }
            *rr_cursor = idx + 1;
            return Ok(());
        }

        // ----- Pass 2: hard-evict (cascade-delete, no cold copy) -----
        if let Some(clip) = store.find_hard_evict_candidate(cam).await? {
            // Metadata FIRST (M2.1 ordering preserved for hard path).
            store.cascade_delete_clip_metadata(clip.id).await?;

            // Then unlink the hot file. Hard-evict candidates always
            // have a hot pointer because cold_handle IS NULL → they
            // were never replicated → they must still be hot (the
            // schema CHECK forbids "neither pointer set").
            if let Some(hot_path) = clip.hot_path.as_deref() {
                let abs = clips_dir.join(hot_path);
                match tokio::fs::remove_file(&abs).await {
                    Ok(()) => {
                        debug!(
                            camera_id = cam,
                            clip_id = clip.id,
                            path = %abs.display(),
                            "hard-evict: clip file removed"
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        debug!(
                            camera_id = cam,
                            clip_id = clip.id,
                            "hard-evict: clip file already gone"
                        );
                    }
                    Err(e) => {
                        warn!(
                            camera_id = cam,
                            clip_id = clip.id,
                            error = %e,
                            path = %abs.display(),
                            "hard-evict: remove_file failed; orphan-file scan will reap"
                        );
                    }
                }
            } else {
                debug!(
                    camera_id = cam,
                    clip_id = clip.id,
                    "hard-evict: clip had no hot_path (already soft-evicted but cold gone?)"
                );
            }

            let payload = serde_json::json!({
                "clip_id": clip.id,
                "camera_id": cam,
                "freed_bytes": clip.size_bytes,
            });
            if let Err(e) = bus.publish(topic::CLIP_HARD_EVICTED, &payload).await {
                warn!(error = %e, "publish CLIP_HARD_EVICTED failed");
            }
            *rr_cursor = idx + 1;
            return Ok(());
        }

        // This camera has no soft AND no hard candidate — every
        // remaining clip is cold-only (undeletable from the safety
        // floor). Move on to the next camera in the round.
        debug!(
            camera_id = cam,
            "evict_one: camera has no evictable clip (all remaining are cold-only)"
        );
    }
    // Nothing to evict (every camera was either empty or cold-only).
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
                hot_path: "cam1/oldest.mp4".into(),
                codec: "stub".into(),
                container: "mp4".into(),
                hot_handle: "local".into(),
            })
            .await
            .unwrap();
        let _id2 = store
            .open_clip(&NewClip {
                camera_id: 1,
                started_at: chrono::Utc::now() - chrono::Duration::minutes(5),
                hot_path: "cam1/newer.mp4".into(),
                codec: "stub".into(),
                container: "mp4".into(),
                hot_handle: "local".into(),
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
            let _ = run_storage_safety(
                cfg,
                probe,
                recorder_for_loop,
                store_for_loop,
                bus_for_loop,
                WatermarkSignal::new(),
            )
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

    /// Helper for the round-robin + crash-mid-eviction tests:
    /// build a store with three cameras and seed `(cam_id, count)`
    /// clips per camera, oldest-first. Returns a function that maps
    /// camera_id -> remaining clip count.
    async fn build_store_with_three_cams_and_clips(
        seeds: [(CameraId, usize); 3],
    ) -> (Arc<Store>, tempfile::TempDir, PathBuf) {
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
        tokio::fs::create_dir_all(&clips_dir).await.unwrap();
        let now = chrono::Utc::now();
        for (cam_id, count) in seeds {
            store
                .upsert_camera(&CameraConfig {
                    id: cam_id,
                    name: format!("cam{cam_id}"),
                    ingest: nexus_config::CameraIngest {
                        url: Url::parse(&format!("rtsp://127.0.0.1/stream{cam_id}")).unwrap(),
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
            tokio::fs::create_dir_all(clips_dir.join(format!("{cam_id}")))
                .await
                .unwrap();
            for i in 0..count {
                let path_rel = format!("{cam_id}/clip_{i:04}.mp4");
                store
                    .open_clip(&NewClip {
                        camera_id: cam_id,
                        // Older i = older started_at; oldest_clip_for_camera
                        // returns the smallest started_at first.
                        started_at: now - chrono::Duration::seconds((count - i) as i64 * 60),
                        hot_path: path_rel.clone(),
                        codec: "stub".into(),
                        container: "mp4".into(),
                        hot_handle: "local".into(),
                    })
                    .await
                    .unwrap();
                tokio::fs::write(clips_dir.join(&path_rel), b"x")
                    .await
                    .unwrap();
            }
        }
        (store, dir, clips_dir)
    }

    /// Audit fix #8: per-camera round-robin must be FAIR. Three
    /// cameras seeded with skewed clip counts (10, 5, 20) all lose
    /// exactly one clip per round of three eviction calls, no
    /// matter how lopsided the on-disk distribution is.
    #[tokio::test]
    async fn evict_one_round_robin_is_fair() {
        let (store, _dir, clips_dir) =
            build_store_with_three_cams_and_clips([(1, 10), (2, 5), (3, 20)]).await;

        async fn count_for(store: &Arc<Store>, cam: CameraId) -> i64 {
            store
                .per_camera_clip_stats()
                .await
                .unwrap()
                .into_iter()
                .find(|s| s.camera_id == cam)
                .map(|s| s.clip_count)
                .unwrap_or(0)
        }

        // Snapshot pre-eviction counts.
        let before = [
            count_for(&store, 1).await,
            count_for(&store, 2).await,
            count_for(&store, 3).await,
        ];
        assert_eq!(before, [10, 5, 20]);

        // Three eviction calls = one full round.
        let bus: Arc<dyn Bus> = Arc::new(nexus_bus::BroadcastBus::new(64));
        let mut cursor = 0usize;
        for _ in 0..3 {
            evict_one(&store, &clips_dir, &bus, &mut cursor)
                .await
                .unwrap();
        }
        let after = [
            count_for(&store, 1).await,
            count_for(&store, 2).await,
            count_for(&store, 3).await,
        ];
        assert_eq!(
            after,
            [9, 4, 19],
            "round-robin must drop exactly one clip per camera per round; got {after:?} from {before:?}"
        );

        // Run two more full rounds to confirm fairness holds even
        // after the cursor wraps.
        for _ in 0..6 {
            evict_one(&store, &clips_dir, &bus, &mut cursor)
                .await
                .unwrap();
        }
        let after_three = [
            count_for(&store, 1).await,
            count_for(&store, 2).await,
            count_for(&store, 3).await,
        ];
        assert_eq!(
            after_three,
            [7, 2, 17],
            "after 3 full rounds each camera should have lost exactly 3 clips; got {after_three:?}"
        );
    }

    /// Audit fix #1 + #8: the eviction is metadata-FIRST. Even if
    /// the file-unlink step "fails" (here we simulate by pointing
    /// the metadata at a nonexistent file), the metadata row MUST
    /// still be cascade-deleted. The orphan-file scanner reaps any
    /// genuinely-orphaned files on a later sweep.
    #[tokio::test]
    async fn evict_one_metadata_first_survives_missing_file() {
        let (store, _dir, clips_dir) =
            build_store_with_three_cams_and_clips([(1, 1), (2, 0), (3, 0)]).await;
        // Cam 1 has 1 clip; nuke its file BEFORE eviction to
        // simulate an externally-deleted clip whose row outlived
        // the bytes.
        let oldest = store.oldest_clip_for_camera(1).await.unwrap().unwrap();
        let abs = clips_dir.join(oldest.hot_path.as_deref().unwrap());
        assert!(abs.exists(), "fixture clip file must exist before nuke");
        tokio::fs::remove_file(&abs).await.unwrap();
        assert!(!abs.exists());

        // Run one eviction. The file-unlink will return NotFound
        // (logged at debug, swallowed). The metadata MUST be gone
        // regardless.
        let bus: Arc<dyn Bus> = Arc::new(nexus_bus::BroadcastBus::new(64));
        let mut cursor = 0usize;
        evict_one(&store, &clips_dir, &bus, &mut cursor)
            .await
            .unwrap();

        // Row is GONE.
        assert!(
            store.get_clip(oldest.id).await.unwrap().is_none(),
            "metadata-first eviction must cascade-delete the row even when the file unlink is a no-op"
        );
        // No leftover row for cam 1.
        let remaining = store.per_camera_clip_stats().await.unwrap();
        assert!(
            remaining.iter().find(|s| s.camera_id == 1).is_none()
                || remaining
                    .iter()
                    .find(|s| s.camera_id == 1)
                    .unwrap()
                    .clip_count
                    == 0,
            "cam 1 must have zero remaining clips after eviction; got {remaining:?}"
        );
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
                WatermarkSignal::new(),
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

    // ===================================================================
    // M2.2 — soft / hard eviction fork
    // ===================================================================

    use nexus_store::ClipColdMark;
    use tokio_stream::StreamExt as _;

    /// Helper: build a store + clips_dir, register a `lan-test`
    /// backend in `storage_backends`, then for `cam` open `count`
    /// clips. The first `cold_count` of them get a cold pointer
    /// stamped via `mark_cold_replicated` so they are eligible for
    /// soft-evict; the rest are hot-only and only the hard-evict
    /// path can touch them.
    async fn build_store_with_mixed_clips(
        cam: CameraId,
        cold_count: usize,
        hot_only_count: usize,
    ) -> (Arc<Store>, tempfile::TempDir, PathBuf) {
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
        tokio::fs::create_dir_all(&clips_dir).await.unwrap();
        store
            .upsert_camera(&CameraConfig {
                id: cam,
                name: format!("cam{cam}"),
                ingest: nexus_config::CameraIngest {
                    url: Url::parse(&format!("rtsp://127.0.0.1/stream{cam}")).unwrap(),
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
            .upsert_storage_backend("lan-test", "lan", "{\"root\":\"/tmp/unused\"}")
            .await
            .unwrap();
        tokio::fs::create_dir_all(clips_dir.join(format!("{cam}")))
            .await
            .unwrap();
        let now = chrono::Utc::now();
        let total = cold_count + hot_only_count;
        // Order them oldest-first so the cold-replicated ones are
        // the OLDEST — matching the natural replicator ordering
        // where older clips get cold-stamped first.
        for i in 0..total {
            let path_rel = format!("{cam}/clip_{i:04}.mp4");
            let clip_id = store
                .open_clip(&NewClip {
                    camera_id: cam,
                    started_at: now - chrono::Duration::seconds((total - i) as i64 * 60),
                    hot_path: path_rel.clone(),
                    codec: "stub".into(),
                    container: "mp4".into(),
                    hot_handle: "local".into(),
                })
                .await
                .unwrap();
            tokio::fs::write(clips_dir.join(&path_rel), b"x")
                .await
                .unwrap();
            // Close so ended_at is set (clips_pending_cold_upload
            // filters on ended_at IS NOT NULL — same gate the
            // replicator uses).
            store
                .close_clip(
                    clip_id,
                    &nexus_store::ClipClose {
                        ended_at: now,
                        duration_ms: 1000,
                        size_bytes: 1,
                        hot_path: Some(path_rel.clone()),
                        sha256: Some(format!("{:064x}", i)),
                    },
                )
                .await
                .unwrap();
            if i < cold_count {
                store
                    .mark_cold_replicated(
                        clip_id,
                        &ClipColdMark {
                            cold_handle: "lan-test".into(),
                            cold_path: path_rel.clone(),
                            cold_uploaded_at: now,
                        },
                    )
                    .await
                    .unwrap();
            }
        }
        (store, dir, clips_dir)
    }

    /// Soft-evict prefers cold-replicated clips: file is removed,
    /// the row + its motion_events stay (none here) and `hot_path`
    /// is cleared. The matching CLIP_HOT_EVICTED bus event fires
    /// exactly once.
    #[tokio::test]
    async fn evict_one_soft_evicts_cold_replicated_first() {
        let (store, _dir, clips_dir) = build_store_with_mixed_clips(1, 2, 1).await;
        let bus: Arc<dyn Bus> = Arc::new(nexus_bus::BroadcastBus::new(64));
        let mut sub = bus
            .subscribe::<serde_json::Value>(topic::CLIP_HOT_EVICTED)
            .await
            .unwrap();

        // Snapshot file paths and ids BEFORE the evict.
        let stats_before = store.per_camera_clip_stats().await.unwrap();
        let total_before = stats_before
            .iter()
            .find(|s| s.camera_id == 1)
            .unwrap()
            .clip_count;
        assert_eq!(total_before, 3, "fixture must seed 3 clips total");

        let mut cursor = 0usize;
        evict_one(&store, &clips_dir, &bus, &mut cursor)
            .await
            .unwrap();

        // The OLDEST cold-replicated clip (0) had its file removed.
        let oldest_path = clips_dir.join("1/clip_0000.mp4");
        assert!(
            !oldest_path.exists(),
            "soft-evict should have unlinked the oldest cold-replicated clip's file"
        );

        // Row count UNCHANGED — soft-evict preserves the row.
        let stats_after = store.per_camera_clip_stats().await.unwrap();
        let total_after = stats_after
            .iter()
            .find(|s| s.camera_id == 1)
            .unwrap()
            .clip_count;
        assert_eq!(
            total_after, total_before,
            "soft-evict must NOT delete the row"
        );

        // The row's hot_path is now NULL.
        let clip_after = store
            .find_hard_evict_candidate(1)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("the hot-only clip should still be hard-evict eligible"));
        // The hard-evict candidate is the one we did NOT cold-replicate
        // (clip index 2 — the newest hot-only one), NOT the
        // soft-evicted clip.
        assert_eq!(
            clip_after.hot_path.as_deref(),
            Some("1/clip_0002.mp4"),
            "soft-evict shouldn't touch hot-only candidates"
        );

        // CLIP_HOT_EVICTED fired exactly once.
        let evt = tokio::time::timeout(Duration::from_millis(500), sub.next())
            .await
            .expect("CLIP_HOT_EVICTED should fire within 500ms")
            .expect("subscriber stream returned None")
            .expect("subscriber stream returned an Err");
        assert_eq!(evt.get("camera_id").and_then(|v| v.as_i64()), Some(1));
    }

    /// When NO cold-replicated clips remain for a camera, the
    /// safety floor falls back to hard-evict (cascade-delete the
    /// metadata + unlink the file). CLIP_HARD_EVICTED fires.
    #[tokio::test]
    async fn evict_one_falls_back_to_hard_evict_when_no_soft_candidate() {
        // 0 cold, 2 hot-only — Pass 1 finds nothing, Pass 2 fires.
        let (store, _dir, clips_dir) = build_store_with_mixed_clips(2, 0, 2).await;
        let bus: Arc<dyn Bus> = Arc::new(nexus_bus::BroadcastBus::new(64));
        let mut sub = bus
            .subscribe::<serde_json::Value>(topic::CLIP_HARD_EVICTED)
            .await
            .unwrap();

        let mut cursor = 0usize;
        evict_one(&store, &clips_dir, &bus, &mut cursor)
            .await
            .unwrap();

        // The hard-evicted clip's file is gone.
        let oldest_path = clips_dir.join("2/clip_0000.mp4");
        assert!(
            !oldest_path.exists(),
            "hard-evict should have unlinked the oldest hot-only clip's file"
        );

        // Row count DROPS by exactly 1 (cascade-delete).
        let stats_after = store.per_camera_clip_stats().await.unwrap();
        let count_after = stats_after
            .iter()
            .find(|s| s.camera_id == 2)
            .map(|s| s.clip_count)
            .unwrap_or(0);
        assert_eq!(count_after, 1, "hard-evict must cascade-delete the row");

        let evt = tokio::time::timeout(Duration::from_millis(500), sub.next())
            .await
            .expect("CLIP_HARD_EVICTED should fire within 500ms")
            .expect("subscriber stream returned None")
            .expect("subscriber stream returned an Err");
        assert_eq!(evt.get("camera_id").and_then(|v| v.as_i64()), Some(2));
    }

    /// Cold-only clips (hot pointer cleared, cold pointer set) MUST
    /// NOT be touched by either eviction pass. This is the
    /// "hard-evicting a cold-replicated clip is forbidden" invariant
    /// from the M2.2 plan.
    #[tokio::test]
    async fn evict_one_refuses_cold_only_clips() {
        let (store, _dir, clips_dir) = build_store_with_mixed_clips(3, 1, 0).await;
        let bus: Arc<dyn Bus> = Arc::new(nexus_bus::BroadcastBus::new(64));
        let mut cursor = 0usize;

        // First pass: soft-evicts the one cold-replicated clip
        // (drops hot pointer; row stays).
        evict_one(&store, &clips_dir, &bus, &mut cursor)
            .await
            .unwrap();
        // Row still there.
        let count_after_first = store
            .per_camera_clip_stats()
            .await
            .unwrap()
            .iter()
            .find(|s| s.camera_id == 3)
            .map(|s| s.clip_count)
            .unwrap_or(0);
        assert_eq!(count_after_first, 1, "soft-evict must not delete the row");

        // Second pass: the only remaining clip is cold-only (no
        // hot pointer). Pass 1 (soft) finds nothing because
        // hot_handle is now NULL; Pass 2 (hard) finds nothing
        // because cold_handle is NOT NULL. evict_one returns Ok
        // without touching anything.
        evict_one(&store, &clips_dir, &bus, &mut cursor)
            .await
            .unwrap();
        let count_after_second = store
            .per_camera_clip_stats()
            .await
            .unwrap()
            .iter()
            .find(|s| s.camera_id == 3)
            .map(|s| s.clip_count)
            .unwrap_or(0);
        assert_eq!(
            count_after_second, 1,
            "cold-only clip MUST survive a second eviction pass"
        );
    }
}

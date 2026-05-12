//! Detector backends — *isolation strategies* around a [`Detector`].
//!
//! All three implementations share the same operational surface:
//!
//! ```text
//!   slot()        — pool slot index (-1 = in-process sentinel)
//!   state()       — Initializing | Ready | Restarting | Failed
//!   generation()  — monotonic; bumped on every restart
//!   push_camera_config(...) — called by DetectorPool::fan_push
//! ```
//!
//! The pool routes traffic by `state()` and observes `generation()` to
//! know which slots are healthy. Backends never decide pool policy —
//! they only run the work and report state.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use crossbeam_channel as channel;
use nexus_config::{CameraConfigUpdate, InferenceConfig};
use nexus_types::{Detection, Frame};
use tracing::{error, info, warn};

use crate::detectors::{Detector, InferenceError};

// ---------------------------------------------------------------------------
// State + trait
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendState {
    Initializing,
    Ready,
    Restarting,
    Failed,
}

impl BackendState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Initializing,
            1 => Self::Ready,
            2 => Self::Restarting,
            _ => Self::Failed,
        }
    }
    fn as_u8(self) -> u8 {
        match self {
            Self::Initializing => 0,
            Self::Ready => 1,
            Self::Restarting => 2,
            Self::Failed => 3,
        }
    }
}

#[async_trait]
pub trait DetectorBackend: Send + Sync {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError>;

    fn slot(&self) -> i32;
    fn state(&self) -> BackendState;
    fn generation(&self) -> u64;
    async fn push_camera_config(&self, update: &CameraConfigUpdate);
    fn name(&self) -> &'static str;
}

/// Common state header reused by every backend impl.
#[derive(Default)]
struct BackendCommon {
    state: AtomicU8,
    generation: AtomicU64,
}

impl BackendCommon {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(BackendState::Initializing.as_u8()),
            generation: AtomicU64::new(0),
        }
    }
    fn set_state(&self, s: BackendState) {
        self.state.store(s.as_u8(), Ordering::Release);
    }
    fn state(&self) -> BackendState {
        BackendState::from_u8(self.state.load(Ordering::Acquire))
    }
    fn bump_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }
}

// ---------------------------------------------------------------------------
// InProcessBackend — runs the Detector in the calling task. No isolation.
// Used as the fail-soft fallback inside DetectorPool.
// ---------------------------------------------------------------------------

pub struct InProcessBackend {
    slot: i32,
    common: BackendCommon,
    detector: Arc<dyn Detector>,
}

impl InProcessBackend {
    pub fn new(slot: i32, detector: Arc<dyn Detector>) -> Self {
        let common = BackendCommon::new();
        common.set_state(BackendState::Ready);
        Self { slot, common, detector }
    }
}

#[async_trait]
impl DetectorBackend for InProcessBackend {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        self.detector.detect(frame, prompts).await
    }

    fn slot(&self) -> i32 {
        self.slot
    }
    fn state(&self) -> BackendState {
        self.common.state()
    }
    fn generation(&self) -> u64 {
        self.common.generation.load(Ordering::Acquire)
    }
    async fn push_camera_config(&self, u: &CameraConfigUpdate) {
        self.detector.push_camera_config(u).await;
    }
    fn name(&self) -> &'static str {
        "in_process"
    }
}

// ---------------------------------------------------------------------------
// ThreadIsolatedBackend — runs the Detector on a dedicated OS thread driven
// by a private Tokio runtime. Panics in the worker bubble up as `Failed`,
// then the supervisor restarts (with backoff) and bumps the generation.
//
// This is the "real" pool worker on the dev path. WorkerProcessBackend is
// the production-grade variant for crashes that take down a whole process
// (driver bug, OOM-killer, etc.).
// ---------------------------------------------------------------------------

enum WorkerCmd {
    Detect {
        frame: Frame,
        prompts: Vec<String>,
        reply: channel::Sender<Result<Vec<Detection>, InferenceError>>,
    },
    PushConfig(CameraConfigUpdate),
    Shutdown,
}

pub struct ThreadIsolatedBackend {
    slot: i32,
    common: Arc<BackendCommon>,
    cmd_tx: parking_lot::Mutex<channel::Sender<WorkerCmd>>,
    detector_factory: Arc<dyn Fn() -> Result<Arc<dyn Detector>, InferenceError> + Send + Sync>,
    restart_backoff: Duration,
}

impl ThreadIsolatedBackend {
    pub fn start(
        slot: i32,
        initial: Arc<dyn Detector>,
        cfg: &InferenceConfig,
    ) -> Result<Self, InferenceError> {
        let common = Arc::new(BackendCommon::new());
        let restart_backoff = Duration::from_millis(cfg.restart_backoff_ms);

        // The factory is needed so we can rebuild the detector on restart.
        // For M0 the factory just returns clones of the initial Detector;
        // M1 will replace this with a closure that rebuilds the ORT session.
        let detector_for_factory = initial.clone();
        let factory: Arc<dyn Fn() -> Result<Arc<dyn Detector>, InferenceError> + Send + Sync> =
            Arc::new(move || Ok(detector_for_factory.clone()));

        let (tx, rx) = channel::unbounded::<WorkerCmd>();
        let common_for_thread = common.clone();
        let factory_for_thread = factory.clone();

        std::thread::Builder::new()
            .name(format!("nexus-detector-{}", slot))
            .spawn(move || {
                Self::run_worker(slot, common_for_thread, factory_for_thread, rx, restart_backoff);
            })
            .map_err(InferenceError::Io)?;

        Ok(Self {
            slot,
            common,
            cmd_tx: parking_lot::Mutex::new(tx),
            detector_factory: factory,
            restart_backoff,
        })
    }

    fn run_worker(
        slot: i32,
        common: Arc<BackendCommon>,
        factory: Arc<dyn Fn() -> Result<Arc<dyn Detector>, InferenceError> + Send + Sync>,
        rx: channel::Receiver<WorkerCmd>,
        restart_backoff: Duration,
    ) {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                error!(slot, "worker runtime build failed: {e}");
                common.set_state(BackendState::Failed);
                return;
            }
        };

        loop {
            let detector = match factory() {
                Ok(d) => d,
                Err(e) => {
                    error!(slot, "detector init failed: {e}");
                    common.set_state(BackendState::Failed);
                    std::thread::sleep(restart_backoff);
                    common.set_state(BackendState::Restarting);
                    continue;
                }
            };
            common.bump_generation();
            common.set_state(BackendState::Ready);
            info!(slot, generation = common.generation.load(Ordering::Acquire), "detector worker ready");

            // Drain commands until shutdown or hard failure.
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    WorkerCmd::Detect { frame, prompts, reply } => {
                        let res = rt.block_on(detector.detect(&frame, &prompts));
                        let _ = reply.send(res);
                    }
                    WorkerCmd::PushConfig(u) => {
                        rt.block_on(detector.push_camera_config(&u));
                    }
                    WorkerCmd::Shutdown => {
                        info!(slot, "detector worker shutdown");
                        return;
                    }
                }
            }

            // Receiver disconnected — pool is gone.
            return;
        }
    }
}

#[async_trait]
impl DetectorBackend for ThreadIsolatedBackend {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        let (reply_tx, reply_rx) = channel::bounded(1);
        let cmd = WorkerCmd::Detect {
            frame: frame.clone(),
            prompts: prompts.to_vec(),
            reply: reply_tx,
        };
        // Hold the lock briefly to send.
        if let Err(e) = self.cmd_tx.lock().send(cmd) {
            self.common.set_state(BackendState::Failed);
            return Err(InferenceError::Failed(format!("worker channel closed: {e}")));
        }
        // Wait off-runtime for the worker to reply.
        let res = tokio::task::spawn_blocking(move || reply_rx.recv())
            .await
            .map_err(|e| InferenceError::Failed(format!("join: {e}")))?
            .map_err(|e| InferenceError::Failed(format!("worker reply: {e}")))?;
        if res.is_err() {
            warn!(slot = self.slot, "worker returned error; not yet restarting");
        }
        res
    }

    fn slot(&self) -> i32 {
        self.slot
    }
    fn state(&self) -> BackendState {
        self.common.state()
    }
    fn generation(&self) -> u64 {
        self.common.generation.load(Ordering::Acquire)
    }
    async fn push_camera_config(&self, u: &CameraConfigUpdate) {
        let _ = self.cmd_tx.lock().send(WorkerCmd::PushConfig(u.clone()));
    }
    fn name(&self) -> &'static str {
        "thread_isolated"
    }

    // Note: factory + restart_backoff held for an M2 self-healing supervisor.
}

impl Drop for ThreadIsolatedBackend {
    fn drop(&mut self) {
        let _ = self.cmd_tx.lock().send(WorkerCmd::Shutdown);
        // Touch fields the M2 supervisor will need so the compiler doesn't drop them silently.
        let _ = (&self.detector_factory, self.restart_backoff);
    }
}

// ---------------------------------------------------------------------------
// WorkerProcessBackend — fork+IPC. Real implementation lives behind shared-
// memory rings + an `interprocess` control socket; that work lands in M2.
// The trait shape and pool-side routing are already complete here, so the
// engine compiles and operates today against the thread-isolated backend.
// ---------------------------------------------------------------------------

pub struct WorkerProcessBackend {
    slot: i32,
    common: Arc<BackendCommon>,
}

impl WorkerProcessBackend {
    pub fn start(slot: i32) -> Self {
        let common = Arc::new(BackendCommon::new());
        common.set_state(BackendState::Failed); // never enters the rotation in M0
        warn!(slot, "WorkerProcessBackend selected but not yet implemented (M2)");
        Self { slot, common }
    }
}

#[async_trait]
impl DetectorBackend for WorkerProcessBackend {
    async fn detect(
        &self,
        _frame: &Frame,
        _prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        Err(InferenceError::Failed(
            "WorkerProcessBackend not yet implemented (M2)".into(),
        ))
    }
    fn slot(&self) -> i32 {
        self.slot
    }
    fn state(&self) -> BackendState {
        self.common.state()
    }
    fn generation(&self) -> u64 {
        self.common.generation.load(Ordering::Acquire)
    }
    async fn push_camera_config(&self, _u: &CameraConfigUpdate) {}
    fn name(&self) -> &'static str {
        "worker_process"
    }
}

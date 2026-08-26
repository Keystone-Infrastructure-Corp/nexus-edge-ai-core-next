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

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
        Self {
            slot,
            common,
            detector,
        }
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

/// Consecutive `detect` failures that demote a slot to `Failed`.
///
/// Paired with [`DEVICE_FAILURE_WINDOW`]: both must be satisfied. A frame count
/// alone means something different on every deployment — one worker serving ten
/// cameras reaches 32 in well under a second, a single low-fps camera takes
/// half a minute.
const DEVICE_FAILURE_STREAK: u32 = 32;

/// How long the failures must also have been going on.
///
/// Demotion is permanent until the process restarts, so the evidence has to
/// outlast a recoverable hang. The i915 reset that preceded BUG-133 recovered
/// on its own; a count-only rule would have demoted a healthy accelerator for
/// the life of the process over it.
const DEVICE_FAILURE_WINDOW: Duration = Duration::from_secs(15);

pub struct ThreadIsolatedBackend {
    slot: i32,
    common: Arc<BackendCommon>,
    cmd_tx: parking_lot::Mutex<channel::Sender<WorkerCmd>>,
    detector_factory: Arc<dyn Fn() -> Result<Arc<dyn Detector>, InferenceError> + Send + Sync>,
    restart_backoff: Duration,
    /// Consecutive `detect` failures; the first success clears it.
    error_streak: AtomicU32,
    /// When the current streak began, set as it goes 0 -> 1.
    streak_started: parking_lot::Mutex<Option<Instant>>,
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
                Self::run_worker(
                    slot,
                    common_for_thread,
                    factory_for_thread,
                    rx,
                    restart_backoff,
                );
            })
            .map_err(InferenceError::Io)?;

        Ok(Self {
            slot,
            common,
            cmd_tx: parking_lot::Mutex::new(tx),
            detector_factory: factory,
            restart_backoff,
            error_streak: AtomicU32::new(0),
            streak_started: parking_lot::Mutex::new(None),
        })
    }

    fn run_worker(
        slot: i32,
        common: Arc<BackendCommon>,
        factory: Arc<dyn Fn() -> Result<Arc<dyn Detector>, InferenceError> + Send + Sync>,
        rx: channel::Receiver<WorkerCmd>,
        restart_backoff: Duration,
    ) {
        // Must be a multi-thread runtime: the `Detector::detect` impls
        // for YOLO / YOLO-World / YOLOE all call
        // `tokio::task::block_in_place(...)` to drive the synchronous
        // `ort::Session::run` while holding the session mutex.
        // `block_in_place` panics on `new_current_thread` runtimes
        // ("can call blocking only when running on the multi-threaded
        // runtime") — that exact panic shipped in v0.1.1 → v0.1.4 and
        // killed every detector worker on the first real frame.
        //
        // `worker_threads(1)` keeps the resource footprint identical
        // to the original `current_thread` design (one tokio worker
        // per detector slot); the only thing it adds is the blocking-
        // pool wiring that `block_in_place` requires.
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name(format!("nexus-detect-{slot}-rt"))
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
            info!(
                slot,
                generation = common.generation.load(Ordering::Acquire),
                "detector worker ready"
            );

            // Drain commands until shutdown or hard failure.
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    WorkerCmd::Detect {
                        frame,
                        prompts,
                        reply,
                    } => {
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
            return Err(InferenceError::Failed(format!(
                "worker channel closed: {e}"
            )));
        }
        // Wait off-runtime for the worker to reply.
        let res = tokio::task::spawn_blocking(move || reply_rx.recv())
            .await
            .map_err(|e| InferenceError::Failed(format!("join: {e}")))?
            .map_err(|e| InferenceError::Failed(format!("worker reply: {e}")))?;
        match &res {
            Ok(_) => {
                self.error_streak.store(0, Ordering::Relaxed);
            }
            Err(e) => {
                let streak = self.error_streak.fetch_add(1, Ordering::Relaxed) + 1;
                if streak == 1 {
                    *self.streak_started.lock() = Some(Instant::now());
                }
                let sustained = streak >= DEVICE_FAILURE_STREAK
                    && self
                        .streak_started
                        .lock()
                        .is_some_and(|t| t.elapsed() >= DEVICE_FAILURE_WINDOW);
                if sustained {
                    // Cleared so a slot that is restored one day can demote
                    // again; `>=` alone would latch past the threshold forever.
                    self.error_streak.store(0, Ordering::Relaxed);
                    if self.common.state() != BackendState::Failed {
                        // The worker thread is alive, so nothing else demotes
                        // it; without this the pool keeps picking a slot whose
                        // device is gone and never reaches the fallback.
                        self.common.set_state(BackendState::Failed);
                        error!(
                            slot = self.slot,
                            streak,
                            error = %e,
                            "detector has failed every frame for {}s; marking the slot Failed \
                             so the pool routes to the fail-soft fallback",
                            DEVICE_FAILURE_WINDOW.as_secs()
                        );
                    }
                } else {
                    warn!(slot = self.slot, streak, error = %e, "detector returned an error");
                }
            }
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
// WorkerProcessBackend — out-of-process detector worker driven over a stdio
// pipe with length-prefixed bincode messages.
//
// The wire protocol + worker binary live in `worker_proto.rs` and
// `bin/nexus-inference-worker.rs`. The parent here owns:
//
//   1. Spawning the child via `tokio::process::Command`.
//   2. A reader task that pulls `WorkerResponse`s off the child stdout
//      and dispatches them to per-request `oneshot` reply channels.
//   3. A writer side (held under a mutex on the stdin handle) that
//      length-prefixes each request and flushes.
//   4. A monotonic `req_id` so multiple concurrent `detect` calls can
//      multiplex over one child pipe.
//   5. Mark `Failed` on child death so the pool routes around us. M2 will
//      add an in-place restart supervisor; today the pool fail-soft path
//      covers the brief gap.
//
// Shared-memory rings + an `interprocess` control socket are the M3 evolution
// for zero-copy frames — the trait surface is intentionally identical.
// ---------------------------------------------------------------------------

pub struct WorkerProcessBackend {
    slot: i32,
    common: Arc<BackendCommon>,
    inner: Arc<WorkerProcessInner>,
}

type DetectReply = tokio::sync::oneshot::Sender<Result<Vec<Detection>, InferenceError>>;

struct WorkerProcessInner {
    /// `req_id -> oneshot reply`. The reader task drains this on every
    /// inbound `WorkerResponse`.
    pending: parking_lot::Mutex<std::collections::HashMap<u64, DetectReply>>,
    /// Monotonic request identifier.
    next_id: std::sync::atomic::AtomicU64,
    /// Child stdin held behind a mutex so writes never interleave on the
    /// length prefix. Wrapped in Option so we can take it during Drop.
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    /// The child process itself, kept alive for Drop.
    child: parking_lot::Mutex<Option<tokio::process::Child>>,
}

impl WorkerProcessBackend {
    /// Spawn the worker child located at `$NEXUS_INFERENCE_WORKER_BIN`, or
    /// (when unset) a sibling of the current executable named
    /// `nexus-inference-worker`. The model kind is forwarded via the
    /// `NEXUS_WORKER_MODEL_KIND` env var, and `ep_priority` (CSV) via
    /// `NEXUS_WORKER_EP_PRIORITY` so the worker registers the same
    /// ORT execution providers the engine config requested.
    pub fn start(
        slot: i32,
        model_kind: &str,
        ep_priority: &[String],
    ) -> Result<Self, InferenceError> {
        let program = resolve_worker_program()?;
        Self::start_with_program(slot, &program, model_kind, ep_priority)
    }

    /// Like [`start`](Self::start) but with an explicit binary path. The
    /// integration tests use this with `CARGO_BIN_EXE_nexus-inference-worker`
    /// so they don't depend on the worker living next to the test runner.
    pub fn start_with_program(
        slot: i32,
        program: &std::path::Path,
        model_kind: &str,
        ep_priority: &[String],
    ) -> Result<Self, InferenceError> {
        use std::process::Stdio;
        use tokio::process::Command;

        let common = Arc::new(BackendCommon::new());

        let mut cmd = Command::new(program);
        cmd.env("NEXUS_WORKER_MODEL_KIND", model_kind)
            .env("NEXUS_WORKER_EP_PRIORITY", ep_priority.join(","))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| InferenceError::Failed(format!("spawn {}: {e}", program.display())))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| InferenceError::Failed("child stdin missing".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| InferenceError::Failed("child stdout missing".into()))?;
        let stderr = child.stderr.take();

        let inner = Arc::new(WorkerProcessInner {
            pending: parking_lot::Mutex::new(std::collections::HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            child: parking_lot::Mutex::new(Some(child)),
        });

        // Reader task — drains responses and dispatches them. Owns
        // `stdout` for the lifetime of the worker.
        let reader_common = common.clone();
        let reader_inner = inner.clone();
        let slot_for_reader = slot;
        tokio::spawn(async move {
            run_reader(slot_for_reader, stdout, reader_common, reader_inner).await;
        });

        // Stderr forwarder — pumps worker diagnostics onto our tracing
        // subscriber under the worker slot. We also re-emit on the
        // parent's own stderr at debug volume so test runs without a
        // subscriber still surface a crashing worker.
        if let Some(stderr) = stderr {
            let slot_for_err = slot;
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    info!(target: "worker_process_stderr", slot = slot_for_err, "{}", line);
                    if std::env::var("NEXUS_WORKER_LOG_STDERR").is_ok() {
                        eprintln!("[worker slot={slot_for_err}] {line}");
                    }
                }
            });
        }

        common.bump_generation();
        common.set_state(BackendState::Ready);
        info!(
            slot,
            program = %program.display(),
            model_kind,
            "WorkerProcessBackend spawned"
        );

        Ok(Self {
            slot,
            common,
            inner,
        })
    }

    /// PID of the spawned worker, or `None` if the child has already been
    /// reaped by Drop. Exposed primarily so chaos tests can `kill -9` the
    /// child and exercise the fail-soft path; the engine itself never
    /// uses this.
    pub fn child_pid(&self) -> Option<u32> {
        self.inner.child.lock().as_ref().and_then(|c| c.id())
    }
}

async fn run_reader(
    slot: i32,
    stdout: tokio::process::ChildStdout,
    common: Arc<BackendCommon>,
    inner: Arc<WorkerProcessInner>,
) {
    let mut stdout = stdout;
    loop {
        let msg: crate::worker_proto::WorkerResponse =
            match crate::worker_proto::read_msg(&mut stdout).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(slot, "worker stdout closed: {e}");
                    break;
                }
            };
        let req_id = msg.req_id();
        let reply = inner.pending.lock().remove(&req_id);
        match (msg, reply) {
            (crate::worker_proto::WorkerResponse::DetectOk { detections, .. }, Some(tx)) => {
                let _ = tx.send(Ok(detections));
            }
            (crate::worker_proto::WorkerResponse::DetectErr { message, .. }, Some(tx)) => {
                let _ = tx.send(Err(InferenceError::Failed(message)));
            }
            (m, None) => {
                warn!(slot, "worker response for unknown req_id={}", m.req_id());
            }
        }
    }
    // Worker is gone. Fail every outstanding request and mark the backend
    // unhealthy so the pool routes around us.
    common.set_state(BackendState::Failed);
    let mut pending = inner.pending.lock();
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(InferenceError::Failed(
            "worker process exited before reply".into(),
        )));
    }
}

fn resolve_worker_program() -> Result<std::path::PathBuf, InferenceError> {
    if let Ok(p) = std::env::var("NEXUS_INFERENCE_WORKER_BIN") {
        return Ok(std::path::PathBuf::from(p));
    }
    // Default: sibling of the current executable. That covers the
    // standard cargo layout (`target/{debug,release}/nexus-engine` and
    // `.../nexus-inference-worker`) and the Docker image, which copies
    // both binaries into the same dir.
    let me = std::env::current_exe().map_err(InferenceError::Io)?;
    let dir = me
        .parent()
        .ok_or_else(|| InferenceError::Failed("current_exe has no parent dir".into()))?;
    let name = if cfg!(windows) {
        "nexus-inference-worker.exe"
    } else {
        "nexus-inference-worker"
    };
    Ok(dir.join(name))
}

#[async_trait]
impl DetectorBackend for WorkerProcessBackend {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        if self.common.state() == BackendState::Failed {
            return Err(InferenceError::Failed(
                "worker_process backend is Failed".into(),
            ));
        }

        let req_id = self
            .inner
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.inner.pending.lock().insert(req_id, reply_tx);

        let req = crate::worker_proto::WorkerRequest::Detect {
            req_id,
            frame: crate::worker_proto::WireFrame::from_frame(frame),
            prompts: prompts.to_vec(),
        };

        {
            let mut guard = self.inner.stdin.lock().await;
            let stdin = guard.as_mut().ok_or_else(|| {
                self.common.set_state(BackendState::Failed);
                InferenceError::Failed("worker stdin gone".into())
            })?;
            if let Err(e) = crate::worker_proto::write_msg(stdin, &req).await {
                self.common.set_state(BackendState::Failed);
                self.inner.pending.lock().remove(&req_id);
                return Err(InferenceError::Failed(format!("write_msg: {e}")));
            }
        }

        match reply_rx.await {
            Ok(res) => res,
            Err(_) => {
                self.common.set_state(BackendState::Failed);
                Err(InferenceError::Failed(
                    "worker reply channel dropped".into(),
                ))
            }
        }
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
    async fn push_camera_config(&self, _u: &CameraConfigUpdate) {
        // M2: forward over the wire as a `WorkerRequest::PushConfig`. For
        // M1 the worker is stateless w.r.t. per-camera config (MockDetector
        // doesn't use it), so we just no-op rather than thrash the protocol.
    }
    fn name(&self) -> &'static str {
        "worker_process"
    }
}

impl Drop for WorkerProcessBackend {
    fn drop(&mut self) {
        // Best-effort soft shutdown then hard kill via kill_on_drop.
        let stdin = self.inner.stdin.try_lock();
        if let Ok(mut g) = stdin {
            if let Some(mut s) = g.take() {
                // Fire-and-forget — we can't await in Drop. The pipe close
                // alone will tip the worker into a clean EOF exit.
                let _ = (&mut s,);
                drop(s);
            }
        }
        if let Some(mut child) = self.inner.child.lock().take() {
            let _ = child.start_kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use nexus_types::{Frame, PixelFormat};
    use std::sync::Arc;

    /// Probe detector that mirrors what every real YOLO* detector does:
    /// call `tokio::task::block_in_place(...)` inside `detect`. With the
    /// pre-v0.1.5 `Builder::new_current_thread()` worker runtime this
    /// would panic with:
    ///   "can call blocking only when running on the multi-threaded runtime"
    /// the first time a frame reached the detector — which is exactly the
    /// crash loop that took down the v0.1.1..v0.1.4 Docker images.
    struct BlockInPlaceProbe;

    #[async_trait]
    impl Detector for BlockInPlaceProbe {
        async fn detect(
            &self,
            _f: &Frame,
            _p: &[String],
        ) -> Result<Vec<Detection>, InferenceError> {
            tokio::task::block_in_place(|| Ok(vec![]))
        }

        fn name(&self) -> &'static str {
            "block_in_place_probe"
        }
    }

    fn tiny_frame() -> Frame {
        Frame {
            camera_id: 0,
            frame_id: 0,
            captured_at: Utc::now(),
            width: 2,
            height: 2,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![0u8; 2 * 2 * 3]),
            trace_id: "regression-block-in-place".into(),
        }
    }

    /// Regression: detector workers must support `tokio::task::block_in_place`
    /// in their `Detector::detect` impl. The fix swapped the worker's
    /// `Builder::new_current_thread()` runtime for
    /// `new_multi_thread().worker_threads(1)`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn thread_isolated_worker_supports_block_in_place() {
        let cfg = InferenceConfig::default();
        let detector: Arc<dyn Detector> = Arc::new(BlockInPlaceProbe);
        let backend = ThreadIsolatedBackend::start(0, detector, &cfg).expect("worker spawn");

        // Wait briefly for the worker to swap into Ready.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while backend.state() != BackendState::Ready && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(backend.state(), BackendState::Ready);

        let frame = tiny_frame();
        let res = backend.detect(&frame, &[]).await;
        assert!(res.is_ok(), "detect returned {res:?}");
        assert!(res.unwrap().is_empty());
    }

    /// Detector standing in for a session whose device is gone: the thread is
    /// healthy, every frame fails. `fail` flips so a test can interleave a
    /// success.
    struct WedgedDeviceProbe {
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl Detector for WedgedDeviceProbe {
        async fn detect(
            &self,
            _f: &Frame,
            _p: &[String],
        ) -> Result<Vec<Detection>, InferenceError> {
            if self.fail.load(Ordering::Relaxed) {
                Err(InferenceError::Failed("CL_OUT_OF_RESOURCES".into()))
            } else {
                Ok(vec![])
            }
        }

        fn name(&self) -> &'static str {
            "wedged_device_probe"
        }
    }

    async fn ready_backend(fail: Arc<std::sync::atomic::AtomicBool>) -> ThreadIsolatedBackend {
        let cfg = InferenceConfig::default();
        let detector: Arc<dyn Detector> = Arc::new(WedgedDeviceProbe { fail });
        let backend = ThreadIsolatedBackend::start(0, detector, &cfg).expect("worker spawn");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while backend.state() != BackendState::Ready && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(backend.state(), BackendState::Ready);
        backend
    }

    /// Regression for BUG-133.
    ///
    /// A wedged accelerator leaves the worker *thread* healthy, so the only
    /// thing that used to set `Failed` — a closed command channel — never
    /// fires. The slot stayed `Ready`, `DetectorPool::pick_ready` kept handing
    /// it every frame, and the fail-soft fallback was never consulted: 36
    /// minutes of zero detections until systemd restarted the process.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_persistently_failing_slot_demotes_itself() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let backend = ready_backend(fail).await;
        let frame = tiny_frame();

        // The first failure starts the clock; backdate it so this test spends
        // its time on the count rather than on DEVICE_FAILURE_WINDOW.
        assert!(backend.detect(&frame, &[]).await.is_err());
        backdate_streak(&backend);

        for i in 2..DEVICE_FAILURE_STREAK {
            assert!(backend.detect(&frame, &[]).await.is_err());
            assert_eq!(
                backend.state(),
                BackendState::Ready,
                "demoted after only {i} failures"
            );
        }

        assert!(backend.detect(&frame, &[]).await.is_err());
        assert_eq!(
            backend.state(),
            BackendState::Failed,
            "a slot that failed {DEVICE_FAILURE_STREAK} frames in a row over \
             {}s is still being offered to the pool",
            DEVICE_FAILURE_WINDOW.as_secs()
        );
    }

    /// Demotion is permanent until the process restarts, so a hang the driver
    /// recovers from must not trigger it. The i915 reset before BUG-133 did
    /// recover; with one worker serving ten cameras, 32 frames is under a
    /// second, so a count-only rule would have demoted a healthy accelerator.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_recoverable_hang_does_not_demote() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let backend = ready_backend(fail.clone()).await;
        let frame = tiny_frame();

        for _ in 0..DEVICE_FAILURE_STREAK * 3 {
            assert!(backend.detect(&frame, &[]).await.is_err());
        }

        assert_eq!(
            backend.state(),
            BackendState::Ready,
            "failures well inside DEVICE_FAILURE_WINDOW permanently demoted the accelerator"
        );
    }

    /// The streak must measure *consecutive* failures. Without the reset the
    /// threshold would just be a lifetime error budget, and any long-running
    /// box would eventually demote its accelerator over unrelated hiccups.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_success_clears_the_error_streak() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let backend = ready_backend(fail.clone()).await;
        let frame = tiny_frame();

        assert!(backend.detect(&frame, &[]).await.is_err());
        backdate_streak(&backend);
        for _ in 2..DEVICE_FAILURE_STREAK {
            assert!(backend.detect(&frame, &[]).await.is_err());
        }
        assert_eq!(backend.state(), BackendState::Ready);

        fail.store(false, Ordering::Relaxed);
        assert!(backend.detect(&frame, &[]).await.is_ok());
        fail.store(true, Ordering::Relaxed);

        // Backdated again, so only the count can decide. If the success did
        // not clear it, the very first failure here lands on 32 and demotes.
        assert!(backend.detect(&frame, &[]).await.is_err());
        backdate_streak(&backend);
        for _ in 2..DEVICE_FAILURE_STREAK {
            assert!(backend.detect(&frame, &[]).await.is_err());
            assert_eq!(
                backend.state(),
                BackendState::Ready,
                "the success did not clear the streak"
            );
        }
    }

    /// Push the current streak's start further into the past than the window,
    /// so a test can exercise the count without sleeping.
    fn backdate_streak(backend: &ThreadIsolatedBackend) {
        *backend.streak_started.lock() =
            Some(Instant::now() - DEVICE_FAILURE_WINDOW - Duration::from_secs(1));
    }
}

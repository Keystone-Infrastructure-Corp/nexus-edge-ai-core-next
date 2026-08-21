//! One place where every ORT [`Session`] in the engine is built.
//!
//! Why this module exists
//! ----------------------
//! Each detector / encoder / extractor used to call
//! `Session::builder()` itself and set only the optimization level and
//! the EP list. Everything else took ORT's defaults, and two of those
//! defaults are actively hostile to a multi-camera edge box:
//!
//! 1. **Intra-op thread count defaults to the logical core count** —
//!    *per session*. The engine opens several sessions in one process
//!    (`[inference] workers = N` builds N detector sessions plus one
//!    more for the `fail_soft` fallback, and `[reid]` adds another).
//!    On an 8-core box with `workers = 2` that is 8 × 4 = 32 ORT
//!    compute threads, on top of ~5 GStreamer threads per camera.
//!
//! 2. **Thread-pool spinning is enabled.** ORT's pool busy-waits with
//!    `SpinPause()` at fork-join barriers rather than blocking. That is
//!    a good trade when a session owns the machine and runs
//!    back-to-back inferences. It is a catastrophic one when the pool
//!    is oversubscribed: the shard that hasn't finished gets preempted
//!    by an unrelated pipeline thread, and its N-1 peers burn a full
//!    scheduler timeslice spinning on a thread that isn't even
//!    running — while occupying the cores it needs to be rescheduled
//!    onto. Observed in production as `SpinPause()` accounting for 56%
//!    of all CPU cycles on a saturated box, versus 15% for the actual
//!    GEMM kernel.
//!
//! Both are now set explicitly for every session, on every hardware
//! profile. This is deliberately **not** NPU-specific: a box that runs
//! the detector on an Intel iGPU, a CUDA GPU, or a Hailo-8 still opens
//! CPU-EP sessions for whatever the accelerator didn't take (re-ID, in
//! particular), and those sessions oversubscribe exactly the same way.
//!
//! Auto-sizing
//! -----------
//! [`SessionTuning::intra_threads`] left as `None` resolves via
//! [`auto_intra_threads`]:
//!
//! * **An accelerator EP attached** → [`ACCELERATED_INTRA_THREADS`].
//!   The heavy math is off-CPU; the intra-op pool only services nodes
//!   the accelerator rejected, so a large pool is pure contention.
//! * **CPU-only** → `cores / concurrent_sessions`, floored at 1. The
//!   sessions in a process share the box rather than each assuming
//!   they own it.

#![cfg(feature = "ort")]

use std::path::Path;
use std::time::Duration;

use ort::session::{builder::GraphOptimizationLevel, Session};
use tracing::{debug, error};

use crate::execution_providers;

/// How long an accelerator-backed session build may run before the
/// chain is abandoned and rebuilt on the CPU EP.
///
/// A provider that cannot serve the graph is expected to *fail*, and a
/// healthy one compiles in seconds. Neither needs this long; the budget
/// exists only so a provider that never returns cannot hold the engine
/// at startup forever. Observed in the field: OpenVINO's GPU plugin on
/// Intel Gen9 LP hangs `commit_from_file` indefinitely, and because the
/// whole chain is handed to ORT as one provider list, the trailing
/// `"cpu"` entry is never reached (BUG-120).
pub const ACCELERATED_BUILD_TIMEOUT: Duration = Duration::from_secs(90);

/// True iff the chain asks for anything other than the CPU EP.
fn requests_accelerator(ep_priority: &[String]) -> bool {
    ep_priority.iter().any(|e| !e.starts_with("cpu"))
}

/// Run `f` on a throwaway thread, yielding `None` if it has not
/// finished within `timeout`.
///
/// The thread is **abandoned, not cancelled**. A provider wedged inside
/// an FFI call cannot be interrupted, so the choice is between leaking
/// one parked thread for the life of the process and never starting the
/// engine at all.
fn run_with_deadline<T, F>(timeout: Duration, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("ort-session-build".to_owned())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    rx.recv_timeout(timeout).ok()
}

/// Intra-op pool size for a session that got a real accelerator EP.
///
/// Not 1: ORT still runs any node the accelerator's plugin refused on
/// the CPU EP, and forcing those onto a single thread can serialise a
/// surprisingly large fallback subgraph. Not `cores` either, for the
/// oversubscription reasons in the module docs.
pub const ACCELERATED_INTRA_THREADS: usize = 2;

/// How an ORT session's thread pools should be configured.
///
/// `Default` is "one session, auto-size, no spinning" — correct for
/// tests and one-shot admin-side sessions.
#[derive(Debug, Clone, Copy)]
pub struct SessionTuning {
    /// Explicit intra-op pool size, or `None` to auto-size.
    pub intra_threads: Option<usize>,
    /// Whether pool workers may busy-wait before blocking.
    pub allow_spinning: bool,
    /// How many ORT sessions share this process, used as the divisor
    /// when auto-sizing.
    pub concurrent_sessions: usize,
}

impl Default for SessionTuning {
    fn default() -> Self {
        Self {
            intra_threads: None,
            allow_spinning: false,
            concurrent_sessions: 1,
        }
    }
}

impl SessionTuning {
    /// Build from the operator-facing config triple.
    pub fn new(
        intra_threads: Option<usize>,
        allow_spinning: bool,
        concurrent_sessions: usize,
    ) -> Self {
        Self {
            intra_threads,
            allow_spinning,
            concurrent_sessions: concurrent_sessions.max(1),
        }
    }
}

/// Logical core count, or a conservative guess when the platform
/// won't say.
fn available_cores() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4)
}

/// True iff any EP that attached is something other than the CPU EP.
///
/// `names` comes from [`execution_providers::selected_for_priority`],
/// which labels the implicit terminal fallback `"cpu(fallback)"` and an
/// operator-requested CPU entry `"cpu"`. Anything else — `openvino`,
/// `npu`, `gpu`, `cuda`, `tensorrt`, `coreml`, `vulkan(webgpu)`, … —
/// means real work should be leaving the CPU.
fn has_accelerator(names: &[String]) -> bool {
    names.iter().any(|n| !n.starts_with("cpu"))
}

/// Resolve the intra-op pool size for one session.
pub fn auto_intra_threads(concurrent_sessions: usize, accelerated: bool) -> usize {
    if accelerated {
        return ACCELERATED_INTRA_THREADS;
    }
    (available_cores() / concurrent_sessions.max(1)).max(1)
}

/// A committed session plus the diagnostics its caller wants to log.
pub struct BuiltSession {
    pub session: Session,
    /// EP labels that were registered, in priority order.
    pub ep_names: Vec<String>,
    /// The intra-op pool size actually applied.
    pub intra_threads: usize,
}

/// Build and commit an ORT session for `model_path`.
///
/// Every ORT session in the workspace goes through here so the
/// threading policy is applied uniformly. Errors are returned as
/// `String` because callers wrap them in their own crate-local error
/// types (`InferenceError::ModelLoad`, `ExtractorError::ModelLoad`).
///
/// A chain that names an accelerator is built under
/// [`ACCELERATED_BUILD_TIMEOUT`] and demoted to the CPU EP if the
/// provider never returns, so a wedged provider degrades the box
/// instead of bricking it (BUG-120).
pub fn build_session(
    model_path: &Path,
    ep_priority: &[String],
    tuning: &SessionTuning,
) -> Result<BuiltSession, String> {
    let path = model_path.to_path_buf();
    let cfg = *tuning;
    build_or_demote_to_cpu(
        model_path,
        ep_priority,
        ACCELERATED_BUILD_TIMEOUT,
        move |chain| commit(&path, &chain, &cfg),
    )
}

/// Run `build` against `ep_priority` under `timeout`, retrying on the
/// CPU EP alone if it never returns.
///
/// Generic over the build step so the demotion policy is exercisable
/// without ORT — the failure this exists for cannot be reproduced from
/// a test on any hardware we own.
///
/// A build that *fails* is returned as-is, not demoted: an EP that says
/// no has already let ORT resolve the rest of the chain. Only silence
/// is treated as a wedge.
fn build_or_demote_to_cpu<T, F>(
    model_path: &Path,
    ep_priority: &[String],
    timeout: Duration,
    build: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: Fn(Vec<String>) -> Result<T, String> + Send + Sync + 'static,
{
    if !requests_accelerator(ep_priority) {
        return build(ep_priority.to_vec());
    }

    let build = std::sync::Arc::new(build);
    let requested = ep_priority.to_vec();
    let attempt = std::sync::Arc::clone(&build);
    if let Some(res) = run_with_deadline(timeout, move || attempt(requested)) {
        return res;
    }

    error!(
        model = %model_path.display(),
        ep_priority = ?ep_priority,
        timeout_secs = timeout.as_secs(),
        "execution provider never returned while building the session; abandoning that \
         thread and rebuilding on the CPU EP"
    );
    // Bounded too: the abandoned thread may still hold ORT-internal state.
    run_with_deadline(timeout, move || build(vec!["cpu".to_owned()])).unwrap_or_else(|| {
        Err(format!(
            "load {}: accelerator EP timed out and the CPU fallback did not return within {}s",
            model_path.display(),
            timeout.as_secs()
        ))
    })
}

/// Configure and commit one session against `ep_priority`, with no
/// time bound of its own.
fn commit(
    model_path: &Path,
    ep_priority: &[String],
    tuning: &SessionTuning,
) -> Result<BuiltSession, String> {
    let (eps, ep_names) = execution_providers::selected_for_priority(ep_priority);
    let accelerated = has_accelerator(&ep_names);
    let intra_threads = tuning
        .intra_threads
        .filter(|n| *n > 0)
        .unwrap_or_else(|| auto_intra_threads(tuning.concurrent_sessions, accelerated));

    debug!(
        model = %model_path.display(),
        intra_threads,
        allow_spinning = tuning.allow_spinning,
        concurrent_sessions = tuning.concurrent_sessions,
        accelerated,
        explicit = tuning.intra_threads.is_some(),
        "configuring ORT session thread pools"
    );

    let session = Session::builder()
        .map_err(|e| format!("session builder: {e}"))?
        // ORT_ENABLE_ALL (99) — valid on every ONNX Runtime ABI.
        // NOT `Level3`: in ort 2.0-rc that maps to ORT_ENABLE_LAYOUT (3),
        // a level introduced in ONNX Runtime 1.22 that the ROCm 1.21
        // runtime rejects with "graph_optimization_level is not valid".
        .with_optimization_level(GraphOptimizationLevel::All)
        .map_err(|e| format!("opt level: {e}"))?
        .with_intra_threads(intra_threads)
        .map_err(|e| format!("intra threads: {e}"))?
        // Inter-op only applies with parallel execution mode, which we
        // never enable — pin it to 1 so ORT doesn't stand up a second
        // idle pool per session.
        .with_inter_threads(1)
        .map_err(|e| format!("inter threads: {e}"))?
        .with_intra_op_spinning(tuning.allow_spinning)
        .map_err(|e| format!("intra spin: {e}"))?
        .with_inter_op_spinning(tuning.allow_spinning)
        .map_err(|e| format!("inter spin: {e}"))?
        .with_execution_providers(eps)
        .map_err(|e| format!("EP register: {e}"))?
        .commit_from_file(model_path)
        .map_err(|e| format!("load {}: {e}", model_path.display()))?;

    Ok(BuiltSession {
        session,
        ep_names,
        intra_threads,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect BUG-120 was filed for: a provider that never returns
    /// must still leave the caller with a CPU-backed session.
    #[test]
    fn a_wedged_accelerator_still_yields_a_cpu_backed_session() {
        let attempts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&attempts);
        let started = std::time::Instant::now();

        let built = build_or_demote_to_cpu(
            Path::new("/models/detector.onnx"),
            &["gpu".to_owned(), "cpu".to_owned()],
            Duration::from_millis(150),
            move |chain| {
                seen.lock().expect("attempt log").push(chain.join(","));
                if chain.iter().any(|e| e == "gpu") {
                    // Stands in for OpenVINO's GPU plugin wedging inside
                    // `commit_from_file` — it never returns at all.
                    std::thread::sleep(Duration::from_secs(60));
                }
                Ok(chain.join(","))
            },
        );

        assert_eq!(
            built,
            Ok("cpu".to_owned()),
            "a wedged accelerator must still produce a CPU-backed session"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}, so the build is not bounded",
            started.elapsed()
        );
        assert_eq!(
            *attempts.lock().expect("attempt log"),
            vec!["gpu,cpu".to_owned(), "cpu".to_owned()],
            "the retry must drop the accelerator, not re-offer the same chain"
        );
    }

    /// A provider that *fails* has already let ORT resolve the rest of
    /// the chain, so the error is the answer. Widening the demotion to
    /// cover errors would silently mask a broken accelerator.
    #[test]
    fn a_provider_that_fails_fast_is_not_demoted() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&calls);

        let built: Result<String, String> = build_or_demote_to_cpu(
            Path::new("/models/detector.onnx"),
            &["gpu".to_owned(), "cpu".to_owned()],
            Duration::from_secs(30),
            move |_| {
                counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Err("EP register: no device".to_owned())
            },
        );

        assert_eq!(built, Err("EP register: no device".to_owned()));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a failure is not a wedge and must not trigger a second build"
        );
    }

    #[test]
    fn a_cpu_only_chain_is_built_on_the_calling_thread() {
        let on = std::thread::current().id();
        let built = build_or_demote_to_cpu(
            Path::new("/models/detector.onnx"),
            &["cpu".to_owned()],
            Duration::from_millis(1),
            move |_| Ok(std::thread::current().id()),
        );
        assert_eq!(
            built,
            Ok(on),
            "a chain with no accelerator must not pay for a thread or a deadline"
        );
    }

    #[test]
    fn a_build_that_never_returns_is_abandoned_at_the_deadline() {
        let started = std::time::Instant::now();
        let got = run_with_deadline(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(60));
            "never delivered"
        });
        assert!(
            got.is_none(),
            "a wedged build must not be waited on forever"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "gave up after {:?}, so the deadline is not bounding the wait",
            started.elapsed()
        );
    }

    #[test]
    fn a_build_that_finishes_returns_its_value() {
        assert_eq!(
            run_with_deadline(Duration::from_secs(30), || 7u8),
            Some(7),
            "a healthy build must pass its result through unchanged"
        );
    }

    #[test]
    fn only_chains_naming_an_accelerator_are_time_bounded() {
        assert!(!requests_accelerator(&["cpu".into()]));
        assert!(requests_accelerator(&["gpu".into(), "cpu".into()]));
        assert!(requests_accelerator(&["npu".into(), "cpu".into()]));
        assert!(requests_accelerator(&["hailo".into(), "cpu".into()]));
    }

    #[test]
    fn accelerator_detection_ignores_cpu_entries() {
        assert!(!has_accelerator(&["cpu(fallback)".into()]));
        assert!(!has_accelerator(&["cpu".into()]));
        assert!(has_accelerator(&["openvino(NPU)".into(), "cpu".into()]));
        assert!(has_accelerator(&["cuda".into(), "cpu(fallback)".into()]));
        assert!(has_accelerator(&["vulkan(webgpu)".into()]));
    }

    #[test]
    fn accelerated_sessions_get_a_small_pool() {
        assert_eq!(auto_intra_threads(1, true), ACCELERATED_INTRA_THREADS);
        assert_eq!(auto_intra_threads(8, true), ACCELERATED_INTRA_THREADS);
    }

    #[test]
    fn cpu_sessions_split_the_box_and_never_hit_zero() {
        let cores = available_cores();
        assert_eq!(auto_intra_threads(1, false), cores);
        // More sessions than cores must still leave each one a thread.
        assert_eq!(auto_intra_threads(cores * 4, false), 1);
    }

    #[test]
    fn concurrent_sessions_is_floored_at_one() {
        assert_eq!(SessionTuning::new(None, false, 0).concurrent_sessions, 1);
    }
}

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

use ort::session::{builder::GraphOptimizationLevel, Session};
use tracing::debug;

use crate::execution_providers;

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
pub fn build_session(
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

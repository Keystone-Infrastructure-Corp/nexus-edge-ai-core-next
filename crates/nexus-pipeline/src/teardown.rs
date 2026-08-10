//! Off-runtime teardown for live-source GStreamer pipelines.
//!
//! `Element::set_state(Null)` is a *synchronous* downward state change: it
//! takes the element's STATE_LOCK and waits for every streaming thread to
//! stop. When a source element is parked in a blocking network read — an RTSP
//! camera whose publisher just died, a PoE switch that dropped the link — that
//! wait has no bound. Performing it on a tokio worker parks the worker;
//! performing it on the single task that owns camera lifecycle parks *every*
//! subsequent camera mutation, which is how the engine ends up accepting
//! camera creates that silently never start (BUG-036).
//!
//! Every teardown of a pipeline that owns a live network source therefore goes
//! through [`null_pipeline_detached`], which hands the pipeline to a dedicated
//! pool of OS threads and returns immediately. A pipeline that never reaches
//! NULL then costs one sacrificed thread instead of the reconciler, and the
//! pool holds the pipeline's strong reference until the transition completes so
//! nothing is ever disposed while still PLAYING.
//!
//! Short-lived pipelines that own no network source — clip muxing, thumbnail
//! extraction — keep their inline `set_state` calls: they run on a blocking
//! thread already and their completion is load-bearing (the file must be
//! finalised before the caller reads it).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use nexus_types::CameraId;
use tracing::{debug, error, warn};

/// A NULL transition slower than this is pathological, and is exactly the
/// event that used to be invisible. Logged at WARN with the call site.
const SLOW_TEARDOWN: Duration = Duration::from_secs(5);

/// How long an idle teardown thread waits for more work before exiting, so a
/// burst of camera churn does not leave threads parked for the process
/// lifetime.
const WORKER_IDLE_LINGER: Duration = Duration::from_secs(30);

/// Ceiling on teardown threads. Reaching it means that many pipelines are
/// simultaneously stuck in NULL, which is a loud fault, not a workload.
const MAX_WORKERS: usize = 64;

static SUBMITTED: AtomicU64 = AtomicU64::new(0);
static COMPLETED: AtomicU64 = AtomicU64::new(0);
static SLOW: AtomicU64 = AtomicU64::new(0);

struct Job {
    pipeline: gst::Pipeline,
    site: &'static str,
    camera_id: Option<CameraId>,
}

struct Pool {
    tx: Sender<Job>,
    rx: Arc<Mutex<Receiver<Job>>>,
    idle: Arc<AtomicUsize>,
    workers: Arc<AtomicUsize>,
}

static POOL: OnceLock<Pool> = OnceLock::new();

fn pool() -> &'static Pool {
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        Pool {
            tx,
            rx: Arc::new(Mutex::new(rx)),
            idle: Arc::new(AtomicUsize::new(0)),
            workers: Arc::new(AtomicUsize::new(0)),
        }
    })
}

/// Transition `pipeline` to NULL on a teardown thread and return immediately.
///
/// `site` names the caller (`"preroll_ingester::shutdown"`) so a stuck
/// transition is attributable from the log alone. Safe to call from an async
/// context, from a `Drop` impl, and from a thread already holding a lock.
pub fn null_pipeline_detached(
    pipeline: gst::Pipeline,
    site: &'static str,
    camera_id: Option<CameraId>,
) {
    let pool = pool();
    SUBMITTED.fetch_add(1, Ordering::Relaxed);
    if pool
        .tx
        .send(Job {
            pipeline,
            site,
            camera_id,
        })
        .is_err()
    {
        error!(
            site,
            "gst teardown pool receiver is gone; pipeline abandoned"
        );
        return;
    }
    ensure_worker(pool);
}

fn ensure_worker(pool: &'static Pool) {
    if pool.idle.load(Ordering::Acquire) > 0 {
        return;
    }
    let mut n = pool.workers.load(Ordering::Acquire);
    loop {
        if n >= MAX_WORKERS {
            warn!(
                max_workers = MAX_WORKERS,
                "gst teardown pool saturated; NULL transitions are queueing"
            );
            return;
        }
        match pool
            .workers
            .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => break,
            Err(cur) => n = cur,
        }
    }
    let rx = Arc::clone(&pool.rx);
    let idle = Arc::clone(&pool.idle);
    let workers = Arc::clone(&pool.workers);
    let spawned = std::thread::Builder::new()
        .name("gst-teardown".to_owned())
        .spawn(move || {
            worker(&rx, &idle);
            workers.fetch_sub(1, Ordering::AcqRel);
        });
    if let Err(e) = spawned {
        pool.workers.fetch_sub(1, Ordering::AcqRel);
        error!(error = %e, "cannot spawn gst teardown thread; pipeline will queue");
    }
}

fn worker(rx: &Mutex<Receiver<Job>>, idle: &AtomicUsize) {
    loop {
        idle.fetch_add(1, Ordering::AcqRel);
        let next = {
            // Poisoning only means a previous worker panicked mid-NULL; the
            // receiver itself is still usable and dropping the queue would
            // strand every pipeline behind it.
            let guard = rx.lock().unwrap_or_else(|p| p.into_inner());
            guard.recv_timeout(WORKER_IDLE_LINGER)
        };
        idle.fetch_sub(1, Ordering::AcqRel);
        match next {
            Ok(job) => run(job),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn run(job: Job) {
    let Job {
        pipeline,
        site,
        camera_id,
    } = job;
    let started = Instant::now();
    let res = pipeline.set_state(gst::State::Null);
    drop(pipeline);
    let elapsed = started.elapsed();
    COMPLETED.fetch_add(1, Ordering::Relaxed);
    if let Err(e) = res {
        debug!(site, ?camera_id, error = %e, "gst teardown: NULL transition returned an error");
    }
    if elapsed >= SLOW_TEARDOWN {
        SLOW.fetch_add(1, Ordering::Relaxed);
        warn!(
            site,
            ?camera_id,
            elapsed_ms = elapsed.as_millis() as u64,
            "gst teardown blocked; this would previously have parked a tokio worker"
        );
    } else {
        debug!(
            site,
            ?camera_id,
            elapsed_ms = elapsed.as_millis() as u64,
            "gst teardown complete"
        );
    }
}

/// Snapshot of the teardown pool, for the admin diagnostics surface and for
/// tests that need to wait for detached work to finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeardownStats {
    pub submitted: u64,
    pub completed: u64,
    /// Transitions that exceeded [`SLOW_TEARDOWN`] — the fault signal.
    pub slow: u64,
    pub workers: usize,
}

/// Current teardown-pool counters.
#[must_use]
pub fn stats() -> TeardownStats {
    TeardownStats {
        submitted: SUBMITTED.load(Ordering::Relaxed),
        completed: COMPLETED.load(Ordering::Relaxed),
        slow: SLOW.load(Ordering::Relaxed),
        workers: POOL.get().map_or(0, |p| p.workers.load(Ordering::Acquire)),
    }
}

/// Block until every submitted teardown has finished, or `timeout` elapses.
/// Returns `true` if the queue drained. Test/shutdown helper — never call this
/// from an async context.
#[must_use]
pub fn wait_until_drained(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let s = stats();
        if s.completed >= s.submitted {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_gst() -> bool {
        gst::init().is_ok()
    }

    /// A pipeline with no live source nulls promptly and the pool reports it.
    #[test]
    fn detached_teardown_completes_and_is_counted() {
        if !init_gst() {
            return;
        }
        let before = stats();
        let pipeline = gst::parse::launch("fakesrc num-buffers=1 ! fakesink")
            .expect("build test pipeline")
            .downcast::<gst::Pipeline>()
            .expect("pipeline");
        let _ = pipeline.set_state(gst::State::Playing);
        null_pipeline_detached(pipeline, "test::detached_teardown", Some(7));
        assert!(
            wait_until_drained(Duration::from_secs(10)),
            "teardown pool did not drain"
        );
        let after = stats();
        // Counters are process-global and the other tests in this module run
        // concurrently, so only the direction is assertable.
        assert!(after.submitted > before.submitted);
        assert!(after.completed > before.completed);
        assert!(after.workers >= 1, "a teardown worker should exist");
    }

    /// The whole point: submitting is non-blocking even when the caller is the
    /// only thread that matters. A submit that took longer than a few
    /// milliseconds would mean the NULL transition ran inline.
    #[test]
    fn submit_does_not_block_the_caller() {
        if !init_gst() {
            return;
        }
        let mut pipelines = Vec::new();
        for _ in 0..8 {
            let p = gst::parse::launch("fakesrc ! fakesink sync=false")
                .expect("build test pipeline")
                .downcast::<gst::Pipeline>()
                .expect("pipeline");
            let _ = p.set_state(gst::State::Playing);
            pipelines.push(p);
        }
        let started = Instant::now();
        for p in pipelines {
            null_pipeline_detached(p, "test::submit_nonblocking", None);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(250),
            "submitting 8 teardowns took {elapsed:?}; it ran inline"
        );
        assert!(wait_until_drained(Duration::from_secs(15)));
    }

    /// The pool must not grow a thread per teardown — a flapping camera would
    /// otherwise trade a pipeline leak for a thread leak.
    #[test]
    fn pool_reuses_threads() {
        if !init_gst() {
            return;
        }
        for _ in 0..32 {
            let p = gst::parse::launch("fakesrc num-buffers=1 ! fakesink")
                .expect("build test pipeline")
                .downcast::<gst::Pipeline>()
                .expect("pipeline");
            null_pipeline_detached(p, "test::pool_reuse", None);
            assert!(wait_until_drained(Duration::from_secs(10)));
        }
        assert!(
            stats().workers <= MAX_WORKERS,
            "teardown pool exceeded its worker ceiling"
        );
    }
}

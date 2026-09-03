//! `DetectorPool` — the W-DETECT D6/D7/D9c pattern.
//!
//! Holds N [`DetectorBackend`]s. Routes each detection request via an atomic
//! round-robin cursor to a `Ready` backend. If no backends are ready, falls
//! through to an in-process fallback (when configured) and latches a
//! one-shot `degraded` warning.
//!
//! On config change, [`fan_push_config`](Self::fan_push_config) sends the
//! same [`CameraConfigUpdate`] to every backend so per-camera state stays
//! consistent across slots.
//!
//! The pool itself is a [`Detector`] — the pipeline never sees the pool
//! abstraction; it just calls `detect`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use nexus_config::CameraConfigUpdate;
use nexus_types::{Detection, Frame};
use tracing::{info, warn};

use crate::backends::{BackendState, DetectorBackend};
use crate::detectors::{Detector, InferenceError};

pub struct DetectorPool {
    workers: Vec<Arc<dyn DetectorBackend>>,
    fallback: Option<Arc<dyn DetectorBackend>>,
    cursor: AtomicUsize,
    degraded: AtomicBool,
}

impl DetectorPool {
    pub fn new(
        workers: Vec<Arc<dyn DetectorBackend>>,
        fallback: Option<Arc<dyn DetectorBackend>>,
    ) -> Self {
        Self {
            workers,
            fallback,
            cursor: AtomicUsize::new(0),
            degraded: AtomicBool::new(false),
        }
    }

    /// Per-slot status snapshot — exposed by `/api/v1/backends` for OPS.
    pub fn snapshot(&self) -> Vec<BackendStatus> {
        let mut out: Vec<BackendStatus> = self
            .workers
            .iter()
            .map(|b| BackendStatus {
                slot: b.slot(),
                name: b.name().to_string(),
                state: b.state(),
                generation: b.generation(),
            })
            .collect();
        if let Some(f) = &self.fallback {
            out.push(BackendStatus {
                slot: f.slot(),
                name: format!("{} (fallback)", f.name()),
                state: f.state(),
                generation: f.generation(),
            });
        }
        out
    }

    /// Fan a config change out to every worker (and the fallback).
    ///
    /// SPEC-035: Pass B scene-fixture terms are stripped here, at the point of
    /// delivery, so they can never reach a per-frame detector slot no matter
    /// how the update was constructed. Fixtures are static scenery served by
    /// the slow-cadence fixture pass; re-detecting them every frame is waste.
    pub async fn fan_push_config(&self, update: &CameraConfigUpdate) {
        let update = &update.stripped_of_fixture_terms();
        for w in &self.workers {
            w.push_camera_config(update).await;
        }
        if let Some(f) = &self.fallback {
            f.push_camera_config(update).await;
        }
    }

    fn pick_ready(&self) -> Option<Arc<dyn DetectorBackend>> {
        let n = self.workers.len();
        if n == 0 {
            return None;
        }
        // Walk at most n positions starting from the next cursor slot.
        for _ in 0..n {
            let i = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
            let w = &self.workers[i];
            if w.state() == BackendState::Ready {
                return Some(w.clone());
            }
        }
        None
    }

    fn warn_degraded_once(&self) {
        if !self.degraded.swap(true, Ordering::AcqRel) {
            warn!(
                "DetectorPool degraded: no Ready workers, routing to fallback. \
                 Will not warn again until pool recovers."
            );
        }
    }

    fn clear_degraded_if_recovered(&self) {
        if self.degraded.load(Ordering::Acquire)
            && self
                .workers
                .iter()
                .any(|w| w.state() == BackendState::Ready)
            && self.degraded.swap(false, Ordering::AcqRel)
        {
            info!("DetectorPool recovered: at least one Ready worker.");
        }
    }
}

/// Per-slot ops view.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendStatus {
    pub slot: i32,
    pub name: String,
    pub state: BackendState,
    pub generation: u64,
}

#[async_trait]
impl Detector for DetectorPool {
    async fn detect(
        &self,
        frame: &Frame,
        prompts: &[String],
    ) -> Result<Vec<Detection>, InferenceError> {
        self.clear_degraded_if_recovered();

        if let Some(w) = self.pick_ready() {
            return w.detect(frame, prompts).await;
        }
        self.warn_degraded_once();
        if let Some(f) = &self.fallback {
            return f.detect(frame, prompts).await;
        }
        Err(InferenceError::Failed(
            "DetectorPool: no Ready workers and no fallback configured".into(),
        ))
    }

    async fn push_camera_config(&self, u: &CameraConfigUpdate) {
        self.fan_push_config(u).await;
    }

    fn name(&self) -> &'static str {
        "detector_pool"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::InProcessBackend;
    use crate::detectors::MockDetector;

    fn frame() -> Frame {
        Frame {
            camera_id: 1,
            frame_id: 0,
            captured_at: chrono::Utc::now(),
            width: 640,
            height: 480,
            format: nexus_types::PixelFormat::Rgb24,
            data: Arc::new(vec![0u8; 640 * 480 * 3]),
            trace_id: "t".into(),
        }
    }

    #[tokio::test]
    async fn pool_routes_round_robin() {
        let workers: Vec<Arc<dyn DetectorBackend>> = (0..3)
            .map(|i| Arc::new(InProcessBackend::new(i, Arc::new(MockDetector::new()))) as _)
            .collect();
        let pool = DetectorPool::new(workers, None);
        let f = frame();
        for _ in 0..6 {
            assert!(!pool.detect(&f, &[]).await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn pool_falls_through_when_no_ready_worker() {
        // No workers at all — must fall through to fallback.
        let fallback: Arc<dyn DetectorBackend> =
            Arc::new(InProcessBackend::new(-1, Arc::new(MockDetector::new())));
        let pool = DetectorPool::new(vec![], Some(fallback));
        assert!(!pool.detect(&frame(), &[]).await.unwrap().is_empty());
    }

    /// SPEC-035: a Pass B scene-fixture term must never reach a per-frame
    /// detector slot, however the update was built.
    ///
    /// Asserted on what the slot actually *receives* — a recording backend
    /// captures the fan-pushed update — rather than on the value the caller
    /// passed in, because the caller is exactly the thing that cannot be
    /// trusted to have filtered.
    #[tokio::test]
    async fn fixture_terms_never_reach_a_per_frame_detector_slot() {
        #[derive(Default)]
        struct Recorder {
            seen: std::sync::Mutex<Vec<String>>,
        }

        #[async_trait::async_trait]
        impl DetectorBackend for Recorder {
            async fn detect(
                &self,
                _f: &Frame,
                _p: &[String],
            ) -> Result<Vec<Detection>, InferenceError> {
                Ok(vec![])
            }
            async fn push_camera_config(&self, u: &CameraConfigUpdate) {
                *self.seen.lock().expect("poisoned") = u.prompts.clone();
            }
            fn slot(&self) -> i32 {
                0
            }
            fn name(&self) -> &'static str {
                "recorder"
            }
            fn state(&self) -> BackendState {
                BackendState::Ready
            }
            fn generation(&self) -> u64 {
                0
            }
        }

        let rec = Arc::new(Recorder::default());
        let pool = DetectorPool::new(vec![rec.clone() as Arc<dyn DetectorBackend>], None);

        let fixture = nexus_config::PASS_B_FIXTURE_TERMS[0];
        let update = CameraConfigUpdate {
            camera_id: 1,
            // Deliberately built by hand, bypassing `for_per_frame_detector`:
            // this is the untrusted path the delivery-point strip exists for.
            prompts: vec!["person".to_string(), fixture.to_string()],
            visual_prompts: vec![],
            model: nexus_config::ModelConfig::default(),
            generation: 1,
        };
        pool.fan_push_config(&update).await;

        let seen = rec.seen.lock().expect("poisoned").clone();
        assert!(
            seen.contains(&"person".to_string()),
            "ordinary per-frame terms must survive: {seen:?}"
        );
        assert!(
            !seen.contains(&fixture.to_string()),
            "fixture term `{fixture}` reached a per-frame detector slot: {seen:?}"
        );
    }
}
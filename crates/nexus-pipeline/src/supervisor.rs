//! Per-camera supervisor task. Wires source → gate → DetectorPool → tracker
//! → RuleEvaluator → store + bus + LatestFrameCache.
//!
//! Every per-frame work block is wrapped in a `tracing::info_span!("frame.lifecycle", …)`
//! that opens child spans for `decode/gate/infer/track/rules`. That's how
//! the `trace_id` field on [`nexus_types::Frame`] is actually backed.

use std::sync::Arc;

use nexus_bus::{topic, Bus, BusExt};
use nexus_config::CameraConfig;
use nexus_inference::Detector;
use nexus_rules::RuleEvaluator;
use nexus_store::EventStore;
use nexus_tracker::{annotate_motion_attributes, Tracker};
use nexus_types::{CameraId, Frame, FrameMetadata, PipelineState, PipelineStatus};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, info_span, warn, Instrument};

use crate::cache::LatestFrameCache;
use crate::gate::MotionGate;
use crate::source::{FrameSource, VirtualSource};

pub struct CameraHandle {
    pub camera_id: CameraId,
    pub task: JoinHandle<()>,
}

/// Build and launch one camera pipeline. Returns a join handle. If the source
/// fails, the supervisor logs and exits — the engine owns restart policy.
pub fn spawn_camera(
    cfg: CameraConfig,
    detector: Arc<dyn Detector>,
    tracker: Arc<dyn Tracker>,
    evaluator: Arc<RuleEvaluator>,
    store: Arc<dyn EventStore>,
    bus: Arc<dyn Bus>,
    cache: Arc<LatestFrameCache>,
) -> CameraHandle {
    let camera_id = cfg.id;
    let task = tokio::spawn(run_camera(cfg, detector, tracker, evaluator, store, bus, cache));
    CameraHandle { camera_id, task }
}

async fn run_camera(
    cfg: CameraConfig,
    detector: Arc<dyn Detector>,
    tracker: Arc<dyn Tracker>,
    evaluator: Arc<RuleEvaluator>,
    store: Arc<dyn EventStore>,
    bus: Arc<dyn Bus>,
    cache: Arc<LatestFrameCache>,
) {
    let span = info_span!(
        "camera.pipeline",
        camera_id = cfg.id,
        camera_name = %cfg.name,
        scheme = %cfg.url.scheme(),
    );
    async {
        let _ = bus
            .publish(
                topic::PIPELINE_STATUS,
                &PipelineStatus {
                    camera_id: cfg.id,
                    state: PipelineState::Initializing,
                    frames_decoded: 0,
                    frames_detected: 0,
                    last_frame_at: None,
                    last_error: None,
                },
            )
            .await;

        let (tx, mut rx) = mpsc::channel::<Frame>(8);
        let source = build_source(&cfg);
        let cam_id = cfg.id;
        let source_task = tokio::spawn(async move {
            if let Err(e) = source.run(tx).await {
                warn!(camera_id = cam_id, "frame source ended: {e}");
            }
        });

        let gate = MotionGate::new();
        let mut decoded: u64 = 0;
        let mut detected: u64 = 0;
        let prompts = cfg.prompts.clone();

        info!(camera_id = cfg.id, "pipeline running");

        while let Some(frame) = rx.recv().await {
            decoded += 1;
            let frame_id = frame.frame_id;
            let trace_id = frame.trace_id.clone();

            let frame_span = info_span!(
                "frame.lifecycle",
                camera_id = cfg.id,
                frame_id,
                trace_id = %trace_id,
            );
            let _enter = frame_span.enter();

            let pass = {
                let _g = info_span!("frame.gate").entered();
                gate.allow(&frame)
            };
            if !pass {
                debug!(camera_id = cfg.id, frame_id, "gate dropped frame");
                continue;
            }

            let detections = {
                let span = info_span!("frame.infer", model = %detector.name());
                match detector.detect(&frame, &prompts).instrument(span).await {
                    Ok(d) => d,
                    Err(e) => {
                        error!(camera_id = cfg.id, "detect failed: {e}");
                        continue;
                    }
                }
            };
            detected += 1;

            let mut tracked = {
                let _g = info_span!("frame.track", tracker = tracker.name()).entered();
                tracker.update(detections)
            };
            annotate_motion_attributes(&mut tracked);
            let tracked_arc = Arc::new(tracked.clone());

            // L7 cache update — see ARCHITECTURE.md.
            cache.put(cfg.id, Arc::new(frame.clone()), tracked_arc.clone());

            // Lightweight metadata onto the bus.
            let meta = FrameMetadata {
                camera_id: cfg.id,
                frame_id,
                captured_at: frame.captured_at,
                width: frame.width,
                height: frame.height,
                trace_id: trace_id.clone(),
                objects: tracked.clone(),
            };
            let _ = bus.publish(topic::FRAME_METADATA, &meta).await;

            let events = {
                let _g = info_span!("frame.rules").entered();
                evaluator.evaluate(cfg.id, frame_id, &trace_id, &tracked)
            };
            for ev in events {
                if let Err(e) = store.record_event(&ev).await {
                    warn!(event = %ev.event_id, "store.record_event failed: {e}");
                }
                let _ = bus.publish(topic::ALERT_EVENT, &ev).await;
            }
        }

        let _ = bus
            .publish(
                topic::PIPELINE_STATUS,
                &PipelineStatus {
                    camera_id: cfg.id,
                    state: PipelineState::Stopped,
                    frames_decoded: decoded,
                    frames_detected: detected,
                    last_frame_at: None,
                    last_error: None,
                },
            )
            .await;
        warn!(camera_id = cfg.id, decoded, detected, "pipeline stopped");
        let _ = source_task.await;
    }
    .instrument(span)
    .await
}

fn build_source(cfg: &CameraConfig) -> Box<dyn FrameSource + Send> {
    match cfg.url.scheme() {
        #[cfg(feature = "gstreamer")]
        "rtsp" | "rtsps" => Box::new(crate::source::RtspSource {
            camera_id: cfg.id,
            url: cfg.url.to_string(),
            max_fps: cfg.max_fps,
        }),
        _ => Box::new(VirtualSource {
            camera_id: cfg.id,
            width: 640,
            height: 480,
            fps: if cfg.max_fps == 0 { 5 } else { cfg.max_fps },
        }),
    }
}

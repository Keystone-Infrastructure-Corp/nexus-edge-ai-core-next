//! Per-camera supervisor task. Wires source → gate → DetectorPool → tracker
//! → RuleEvaluator → store + bus + LatestFrameCache.
//!
//! Every per-frame work block is wrapped in a `tracing::info_span!("frame.lifecycle", …)`
//! that opens child spans for `decode/gate/infer/track/rules`. That's how
//! the `trace_id` field on [`nexus_types::Frame`] is actually backed.

use std::path::PathBuf;
use std::sync::Arc;

use nexus_bus::{topic, Bus, BusExt};
use nexus_config::{AnnotatorConfig, CameraConfig, ClipsConfig, StaticObjectConfig};
use nexus_inference::{label_matches_any_prompt, Detector};
use nexus_rules::RuleEvaluator;
use nexus_store::{EventStore, MotionEventKind, NewMotionEvent, Store};
use nexus_tracker::{
    filter_excluded_zones, filter_zone_min_area, is_object_static, MotionDecision,
    MotionEventEmitter, MotionKind, StaticObjectFilter, TrackAnnotator, Tracker,
};
use nexus_types::{CameraId, Frame, FrameMetadata, PipelineState, PipelineStatus, TrackedObject};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, info_span, warn, Instrument};

use crate::cache::LatestFrameCache;
use crate::entity_sighting::{
    EntityLocalPersist, EntityLocalSeed, SightingHook, SightingScheduler,
};
use crate::gate::MotionGate;
use crate::post_roll::{PostRoll, PostRollAction};
use crate::recorder::{
    ClipFinal, ClipHandle, ClipRecorder, OpenClip, RecorderError, MAX_CLIP_DURATION_MS,
};
use crate::skip_policy::DetectorSkipPolicy;
use crate::source::{FrameSource, VirtualSource};
use crate::static_clear::StaticAnchorClearRegistry;
use crate::stats::FrameStatsRegistry;

/// Tunables for the per-camera [`SightingScheduler`]. Constructed
/// at the engine boot site so all per-camera supervisors share the
/// same cadence + minimum-stability thresholds. Passed by value into
/// [`spawn_camera`].
#[derive(Debug, Clone, Copy)]
pub struct SightingSchedulerConfig {
    pub min_track_age_frames: u32,
    pub emit_interval: std::time::Duration,
    /// M_PERF_CROWD B2 — above this concurrent-track count the
    /// scheduler swaps the periodic re-emit cadence to
    /// [`crowded_emit_interval`]. `0` disables crowded mode.
    pub crowded_track_threshold: u32,
    /// M_PERF_CROWD B2 — cadence used while the per-camera
    /// tracked-object count exceeds [`crowded_track_threshold`].
    pub crowded_emit_interval: std::time::Duration,
}

impl Default for SightingSchedulerConfig {
    fn default() -> Self {
        Self {
            min_track_age_frames: 5,
            emit_interval: std::time::Duration::from_secs(5),
            crowded_track_threshold: 15,
            crowded_emit_interval: std::time::Duration::from_secs(15),
        }
    }
}

pub struct CameraHandle {
    pub camera_id: CameraId,
    pub task: JoinHandle<()>,
}

/// Build and launch one camera pipeline. Returns a join handle. If the source
/// fails, the supervisor logs and exits — the engine owns restart policy.
///
/// `supervisor_w` / `supervisor_h` are the per-camera RGB analysis
/// frame size (derived from the camera's resolved detector input
/// size via [`crate::source::supervisor_frame_for`] at the engine
/// spawn site). The dims are baked into the freshly-built
/// `RtspSource` when the recorder does NOT provide a shared frame
/// source; with the shared source, the recorder owns the same
/// dims via [`crate::recorder::ClipRecorder::add_camera_ingester`].
#[allow(clippy::too_many_arguments)]
pub fn spawn_camera(
    cfg: CameraConfig,
    detector: Arc<dyn Detector>,
    tracker: Arc<dyn Tracker>,
    annotator_cfg: AnnotatorConfig,
    static_object_cfg: StaticObjectConfig,
    clips_cfg: ClipsConfig,
    state_dir: PathBuf,
    evaluator: Arc<RuleEvaluator>,
    store: Arc<Store>,
    recorder: Arc<dyn ClipRecorder>,
    bus: Arc<dyn Bus>,
    cache: Arc<LatestFrameCache>,
    stats: Arc<FrameStatsRegistry>,
    static_clear: Arc<StaticAnchorClearRegistry>,
    supervisor_w: u32,
    supervisor_h: u32,
    sighting_hook: Arc<dyn SightingHook>,
    sighting_cfg: SightingSchedulerConfig,
    sighting_seed: Vec<EntityLocalSeed>,
    sighting_persist: Arc<dyn EntityLocalPersist>,
) -> CameraHandle {
    let camera_id = cfg.id;
    let task = tokio::spawn(run_camera(
        cfg,
        detector,
        tracker,
        annotator_cfg,
        static_object_cfg,
        clips_cfg,
        state_dir,
        evaluator,
        store,
        recorder,
        bus,
        cache,
        stats,
        static_clear,
        supervisor_w,
        supervisor_h,
        sighting_hook,
        sighting_cfg,
        sighting_seed,
        sighting_persist,
    ));
    CameraHandle { camera_id, task }
}

#[allow(clippy::too_many_arguments)]
async fn run_camera(
    cfg: CameraConfig,
    detector: Arc<dyn Detector>,
    tracker: Arc<dyn Tracker>,
    annotator_cfg: AnnotatorConfig,
    static_object_cfg: StaticObjectConfig,
    clips_cfg: ClipsConfig,
    state_dir: PathBuf,
    evaluator: Arc<RuleEvaluator>,
    store: Arc<Store>,
    recorder: Arc<dyn ClipRecorder>,
    bus: Arc<dyn Bus>,
    cache: Arc<LatestFrameCache>,
    stats: Arc<FrameStatsRegistry>,
    static_clear: Arc<StaticAnchorClearRegistry>,
    supervisor_w: u32,
    supervisor_h: u32,
    sighting_hook: Arc<dyn SightingHook>,
    sighting_cfg: SightingSchedulerConfig,
    sighting_seed: Vec<EntityLocalSeed>,
    sighting_persist: Arc<dyn EntityLocalPersist>,
) {
    let span = info_span!(
        "camera.pipeline",
        camera_id = cfg.id,
        camera_name = %cfg.name,
        scheme = %cfg.ingest.url.scheme(),
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
        let source = build_source(&cfg, &recorder, supervisor_w, supervisor_h);
        let cam_id = cfg.id;
        let source_task = tokio::spawn(async move {
            if let Err(e) = source.run(tx).await {
                warn!(camera_id = cam_id, "frame source ended: {e}");
            }
        });

        let gate = MotionGate::new();
        // M_PERF_CROWD Phase E1 — adaptive detector cadence under crowd.
        // No-op (always-run) unless both
        // `behavior.detector_skip_crowded_threshold` and
        // `behavior.detector_skip_every_n_frames` are `Some`.
        let mut skip_policy = DetectorSkipPolicy::new(
            cfg.behavior.detector_skip_crowded_threshold,
            cfg.behavior.detector_skip_every_n_frames,
        );
        let mut decoded: u64 = 0;
        let mut detected: u64 = 0;
        let prompts = cfg.detector.prompts.clone();
        let zones = cfg.zones.clone();
        let mut annotator = TrackAnnotator::new(annotator_cfg);
        // Static-object filter is only built when the camera opted in.
        // We always pass the persistence path (under state_dir) so a
        // toggle from off → on picks up any registry that may already
        // exist on disk. Apply per-camera `anchor_ttl_secs` override on
        // top of the engine-wide `tracker.static_object` snapshot — the
        // override is the only field a camera can tune today, but the
        // pattern scales to additional knobs (dwell_frames, etc.) by
        // adding more `if let Some(...) = ...` clauses here.
        let mut effective_static_cfg = static_object_cfg;
        if let Some(ttl) = cfg.behavior.anchor_ttl_secs {
            effective_static_cfg.anchor_ttl_secs = ttl;
        }
        let mut static_filter = if cfg.behavior.parking_lot_mode {
            let path = state_dir
                .join("static_objects")
                .join(format!("cam-{}.json", cfg.id));
            Some(StaticObjectFilter::new(
                effective_static_cfg,
                cfg.id,
                Some(path),
            ))
        } else {
            None
        };
        // Snapshot the current operator-clear sequence so the first
        // frame after spawn doesn't trigger a spurious wipe just
        // because some other camera bumped its counter previously.
        let mut last_static_clear_seq = static_clear.current(cfg.id);

        // Motion-event emitter + per-camera clip handle. Single
        // open clip at a time per camera: opens on the first Born
        // event when no clip is open, closes on the frame where the
        // last live track disappears. clip_id is stamped on every
        // motion_events row before insert (schema invariant).
        let mut emitter = MotionEventEmitter::new(clips_cfg.motion_events_sample_hz);
        // Phase 5.6 · slice 4c-ii — per-camera entity-sighting
        // scheduler. Drives the engine's [`SightingHook`] (default
        // [`NoopSightingHook`]) once per stable track per
        // `emit_interval`. Cheap when the hook is the noop — just a
        // HashMap probe + counter bump per frame.
        let mut sighting_scheduler = SightingScheduler::new_with_persistence(
            cfg.id,
            sighting_cfg.min_track_age_frames,
            sighting_cfg.emit_interval,
            sighting_seed,
            sighting_persist,
        )
        .with_crowded_cadence(
            sighting_cfg.crowded_track_threshold,
            sighting_cfg.crowded_emit_interval,
        )
        .with_first_emit_jitter();
        let mut current_clip: Option<ClipHandle> = None;
        // Wall-clock anchor for the currently-open clip. Used to
        // enforce the M2.1 MAX_CLIP_DURATION_MS bound — once the
        // open clip exceeds 5min we force-close it and (if motion
        // is still active on this frame) the next Born will open a
        // fresh one. Reset to None on every close.
        let mut clip_opened_at: Option<chrono::DateTime<chrono::Utc>> = None;
        let mut post_roll = PostRoll::new(clips_cfg.post_roll_secs);

        info!(camera_id = cfg.id, "pipeline running");

        while let Some(frame) = rx.recv().await {
            decoded += 1;
            // M-Admin Phase 0 closeout: keep per-camera fps EMA +
            // last-frame timestamp + source dims up to date so the
            // UI can render a live health column without polling
            // the bus PIPELINE_STATUS topic on every frame.
            stats.observe_frame(cfg.id, frame.captured_at, frame.width, frame.height);

            // Honour any operator-initiated anchor wipe issued via
            // `DELETE /api/v1/cameras/{id}/static-anchors` since the
            // previous frame. Cheap: one atomic load per frame.
            // Skipped entirely for cameras where the filter is
            // disabled (`parking_lot_mode = false`).
            if let Some(filter) = static_filter.as_mut() {
                let current_seq = static_clear.current(cfg.id);
                if current_seq != last_static_clear_seq {
                    debug!(
                        camera_id = cfg.id,
                        seq = current_seq,
                        "static-anchor clear signalled — wiping in-memory + on-disk registry"
                    );
                    filter.clear();
                    last_static_clear_seq = current_seq;
                }
            }
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
                stats.observe_dropped(cfg.id);
                continue;
            }

            // M2.1: enforce MAX_CLIP_DURATION_MS. If the currently
            // open clip has been writing for >= 5 min, close it now
            // so a fresh one opens on the next Born (or right below
            // if motion is still live). Done BEFORE motion/event
            // handling so any alerts/motion on this frame attach
            // to the new clip rather than the about-to-be-closed
            // one.
            let mut force_reopen_after_rotation = false;
            if let (Some(handle), Some(opened_at)) = (current_clip, clip_opened_at) {
                let age_ms = (frame.captured_at - opened_at).num_milliseconds();
                if age_ms >= MAX_CLIP_DURATION_MS {
                    debug!(
                        camera_id = cfg.id,
                        clip_id = handle.clip_id,
                        age_ms,
                        max_ms = MAX_CLIP_DURATION_MS,
                        "rotating clip: max duration reached"
                    );
                    if let Err(e) = recorder
                        .close(
                            handle,
                            ClipFinal {
                                ended_at: frame.captured_at,
                            },
                        )
                        .await
                    {
                        warn!(
                            camera_id = cfg.id,
                            "recorder.close (max-duration rotation) failed: {e}"
                        );
                    }
                    current_clip = None;
                    clip_opened_at = None;
                    // Reset post-roll so the rotation isn't observed
                    // as a motion-end window.
                    post_roll.reset();
                    // If motion was still live (Born was already
                    // emitted prior to this frame), the upcoming
                    // motion lifecycle will see Live decisions but
                    // NOT another Born — so the existing
                    // open-on-Born trigger won't re-open. Flag it
                    // so the decisions loop opens on the first
                    // decision regardless of kind.
                    force_reopen_after_rotation = emitter.live_track_count(cfg.id) > 0;
                }
            }

            // M_PERF_CROWD Phase E1 — decide BEFORE running the
            // detector whether this frame is one that the skip policy
            // wants to drop. On a skip frame the detector is bypassed
            // entirely; the tracker still runs below with an empty
            // detection slice so ByteTrack's predict() advances and
            // existing tracks age normally.
            let skip_detector = skip_policy.should_skip();
            let detections = if skip_detector {
                debug!(
                    camera_id = cfg.id,
                    frame_id, "detector skipped (crowd skip policy)"
                );
                Vec::new()
            } else {
                let span = info_span!("frame.infer", model = %detector.name());
                match detector.detect(&frame, &prompts).instrument(span).await {
                    Ok(d) => d,
                    Err(e) => {
                        error!(camera_id = cfg.id, "detect failed: {e}");
                        continue;
                    }
                }
            };
            // Per-camera `prompts` whitelist applied uniformly across
            // every detector kind. Open-vocab models (yolo_world,
            // yoloe) also receive `prompts` as input to scope their
            // classes; this retain is idempotent for them. Closed-vocab
            // YOLO/COCO ignores the input `prompts` and emits every
            // mapped class, so this is the only enforcement point
            // that catches it. Empty prompts disables the filter
            // (see `label_matches_any_prompt`).
            let detections: Vec<_> = if prompts.is_empty() || skip_detector {
                detections
            } else {
                let before = detections.len();
                let kept: Vec<_> = detections
                    .into_iter()
                    .filter(|d| label_matches_any_prompt(&d.label, &prompts))
                    .collect();
                if before != kept.len() {
                    debug!(
                        camera_id = cfg.id,
                        frame_id,
                        before,
                        after = kept.len(),
                        "prompts whitelist dropped detections"
                    );
                }
                kept
            };
            if !skip_detector {
                detected += 1;
            }

            let mut tracked = {
                let _g = info_span!("frame.track", tracker = tracker.name()).entered();
                tracker.update(detections)
            };
            // M_PERF_CROWD Phase E1 — feed the post-tracker
            // tracked-object count back into the skip policy's EMA so
            // the next frame's skip decision reflects current crowd
            // density. No-op when the policy is disabled.
            skip_policy.observe(tracked.len());
            // M-Admin Phase 2 Step 1 — exclusion-zone enforcement.
            // Drop any tracked object whose bbox centre lies inside
            // a `ZoneKind::Exclusion` polygon for this camera, BEFORE
            // the annotator runs so excluded objects never enter
            // per-track state, the L7 cache, the FRAME_METADATA bus
            // event, or the rule evaluator. No-op when the camera
            // has no exclusion zones (the common case).
            {
                let _g = info_span!("frame.zone_filter").entered();
                let dropped = filter_excluded_zones(&frame, &zones, &mut tracked);
                if dropped > 0 {
                    debug!(
                        camera_id = cfg.id,
                        frame_id, dropped, "exclusion zone filter dropped objects"
                    );
                }
                // M_PERF_CROWD Phase B1 — per-zone min-bbox-area
                // override. Fast path no-op when no zone declares
                // `min_bbox_area_px_override`; otherwise drops tracked
                // objects whose centre lies in an override zone and
                // whose bbox area is below that zone's threshold.
                // Layered on top of the global
                // `ModelConfig::min_bbox_area_px` (which fires at the
                // inference wrapper before tracking).
                let dropped = filter_zone_min_area(&frame, &zones, &mut tracked);
                if dropped > 0 {
                    debug!(
                        camera_id = cfg.id,
                        frame_id, dropped, "per-zone min-area override dropped objects"
                    );
                }
            }
            {
                let _g = info_span!("frame.annotate", annotator = annotator.name()).entered();
                annotator.annotate(&frame, &zones, &mut tracked);
            }
            if let Some(sf) = static_filter.as_mut() {
                let _g = info_span!("frame.static_filter", filter = sf.name()).entered();
                // Mark suppressed tracks (writes
                // `tracker.is_static = true` into the object's
                // attributes map) but do NOT remove them. The live
                // viewer needs to see them to render the
                // "static" indicator; the partition below keeps
                // them out of rule eval + the motion lifecycle.
                sf.classify(&frame, &mut tracked);
            }
            let tracked_arc = Arc::new(tracked.clone());

            // L7 cache update — see ARCHITECTURE.md.
            let frame_arc = Arc::new(frame.clone());
            cache.put(cfg.id, frame_arc.clone(), tracked_arc.clone());

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

            // Partition: rules and the motion lifecycle only see
            // non-static tracks. A parked car shouldn't keep firing
            // rules or generating motion_events rows, but it MUST
            // still appear in the L7 cache + FRAME_METADATA above
            // so the live viewer can draw it (de-emphasised) and
            // so the operator can see the static-suppression in
            // action. When `static_filter` is `None`, no object can
            // be marked static so we just clone the full slice.
            let dynamic_tracked: Vec<TrackedObject> = if static_filter.is_some() {
                tracked
                    .iter()
                    .filter(|t| !is_object_static(t))
                    .cloned()
                    .collect()
            } else {
                tracked.clone()
            };

            // Phase 5.6 · slice 4c-ii — fire stable-track sightings
            // into the engine hook. Skips parked-car tracks the
            // static-object filter has masked off (same partition
            // as rule eval + motion lifecycle).
            sighting_scheduler.tick(
                &frame_arc,
                &dynamic_tracked,
                frame.captured_at,
                sighting_hook.as_ref(),
            );

            let events = {
                let _g = info_span!("frame.rules").entered();
                evaluator.evaluate(
                    cfg.id,
                    frame_id,
                    &trace_id,
                    frame.width,
                    frame.height,
                    &zones,
                    &dynamic_tracked,
                )
            };
            // Record + publish the events now so the row exists.
            // We defer the events.clip_id stamp until AFTER the
            // motion lifecycle has run for this frame, because a
            // new alert + first Born in the same frame must link
            // to the clip that gets opened on this frame, not the
            // previous one.
            let mut events_to_link: Vec<String> = Vec::new();
            for ev in events {
                let event_id = ev.event_id.to_string();
                if let Err(e) = store.record_event(&ev).await {
                    warn!(event = %ev.event_id, "store.record_event failed: {e}");
                } else {
                    events_to_link.push(event_id);
                }
                let _ = bus.publish(topic::ALERT_EVENT, &ev).await;
            }

            // Motion lifecycle. The emitter is pure — it just tells
            // us what changed. We turn its decisions into open/close
            // recorder calls + motion_events rows here.
            //
            // The synchronous emitter.tick() runs inside the span
            // via in_scope(); we don't hold an EnteredSpan guard
            // across recorder/store awaits because EnteredSpan is
            // !Send and would break tokio::spawn.
            let decisions = info_span!("frame.motion")
                .in_scope(|| emitter.tick(cfg.id, &dynamic_tracked, frame.captured_at));
            for d in &decisions {
                let should_open = current_clip.is_none()
                    && (matches!(d.kind, MotionKind::Born) || force_reopen_after_rotation);
                if should_open {
                    match recorder
                        .open(OpenClip {
                            camera_id: cfg.id,
                            started_at: d.captured_at,
                            frame_width: supervisor_w,
                            frame_height: supervisor_h,
                        })
                        .await
                    {
                        Ok(handle) => {
                            current_clip = Some(handle);
                            clip_opened_at = Some(d.captured_at);
                            // One-shot — only the first decision in
                            // this frame triggers the post-rotation
                            // reopen.
                            force_reopen_after_rotation = false;
                        }
                        Err(RecorderError::Refused) => {
                            // Watermark sampler has paused new
                            // clips. Drop ALL motion events for
                            // this frame: the schema requires
                            // clip_id NOT NULL and we have no
                            // open clip to attach to.
                            debug!(
                                camera_id = cfg.id,
                                "recorder refused open (panic mode); dropping motion frame"
                            );
                            break;
                        }
                        Err(e) => {
                            warn!(camera_id = cfg.id, "recorder.open failed: {e}");
                            break;
                        }
                    }
                }
                let Some(handle) = current_clip else {
                    // Open was refused earlier in this frame and
                    // we have no clip to stamp. Skip silently —
                    // the next Born will retry recorder.open.
                    continue;
                };
                if let Err(e) = insert_motion_decision(&store, handle, d).await {
                    warn!(camera_id = cfg.id, "insert_motion_event failed: {e}");
                }
            }

            // Stamp events.clip_id for any alerts that fired this
            // frame, now that the motion lifecycle has had a chance
            // to open a clip. Alerts on frames with no open clip
            // simply stay unlinked (clip_id NULL) — the timeline UI
            // shows them as "no surrounding video".
            if !events_to_link.is_empty() {
                if let Some(handle) = current_clip {
                    for event_id in &events_to_link {
                        if let Err(e) = store.link_event_to_clip(event_id, handle.clip_id).await {
                            warn!(
                                event = %event_id,
                                clip_id = handle.clip_id,
                                "link_event_to_clip failed: {e}"
                            );
                        }
                    }
                }
            }

            // Close the clip when the post-roll grace window
            // elapses without motion returning. Pre-B3 this fired
            // immediately on `live_track_count == 0`; B3 wraps that
            // condition in a deferred-close timer so two short
            // motion bursts inside `clips_cfg.post_roll_secs`
            // produce a single clip rather than two adjacent
            // micro-clips. Pre-roll is intentionally a separate PR.
            let has_live_motion = emitter.live_track_count(cfg.id) > 0;
            let action = post_roll.tick(frame.captured_at, has_live_motion);
            if matches!(action, PostRollAction::CloseNow) {
                if let Some(handle) = current_clip.take() {
                    if let Err(e) = recorder
                        .close(
                            handle,
                            ClipFinal {
                                ended_at: frame.captured_at,
                            },
                        )
                        .await
                    {
                        warn!(camera_id = cfg.id, "recorder.close failed: {e}");
                    }
                    clip_opened_at = None;
                }
            }
        }

        // Pipeline ended — close any clip still open so its row
        // doesn't sit forever with NULL ended_at.
        post_roll.reset();
        if let Some(handle) = current_clip.take() {
            let now = chrono::Utc::now();
            if let Err(e) = recorder.close(handle, ClipFinal { ended_at: now }).await {
                warn!(
                    camera_id = cfg.id,
                    "final recorder.close on shutdown failed: {e}"
                );
            }
            clip_opened_at = None;
        }
        // Suppress dead_assignment / unused_assignments warnings —
        // `clip_opened_at` is reset for invariant clarity even on the
        // shutdown path.
        let _ = clip_opened_at;
        emitter.forget_camera(cfg.id);

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

fn build_source(
    cfg: &CameraConfig,
    recorder: &Arc<dyn ClipRecorder>,
    #[cfg_attr(not(feature = "gstreamer"), allow(unused_variables))] supervisor_w: u32,
    #[cfg_attr(not(feature = "gstreamer"), allow(unused_variables))] supervisor_h: u32,
) -> Box<dyn FrameSource + Send> {
    // Prefer a frame source shared with the recorder's pre-roll
    // ingester whenever the recorder offers one. This collapses
    // what used to be two RTSP sessions per camera (one for the
    // detector's RGB feed, one for the recorder's H.264 tap) into
    // one — REQUIRED for cameras whose firmware caps concurrent
    // sessions at 1 per stream path (e.g. InSight 192.168.1.66).
    // The stub recorder (and any future non-pre-roll backend)
    // returns None here and we fall through to building a fresh
    // RtspSource as before.
    if let Some(shared) = recorder.shared_frame_source(cfg.id) {
        return shared;
    }
    match cfg.ingest.url.scheme() {
        #[cfg(feature = "gstreamer")]
        "rtsp" | "rtsps" => Box::new(crate::source::RtspSource {
            camera_id: cfg.id,
            url: cfg.ingest.url.to_string(),
            max_fps: cfg.ingest.max_fps,
            frame_width: supervisor_w,
            frame_height: supervisor_h,
            expected_codec: cfg.ingest.codec,
        }),
        // Without the `gstreamer` feature there is no real RTSP backend.
        // Refuse to silently fall back to a 640x480 black VirtualSource —
        // surface a loud error and return a FailingSource so the
        // supervisor's existing warn path makes the misconfiguration
        // visible in `/api/v1/cameras` (pipeline state stays Initializing →
        // error) instead of "running" with a fake feed.
        #[cfg(not(feature = "gstreamer"))]
        "rtsp" | "rtsps" => {
            let msg = format!(
                "camera {} url {} requires the `gstreamer` feature; rebuild \
                 nexus-engine with `cargo build --features gstreamer,...`",
                cfg.id, cfg.ingest.url
            );
            error!(camera_id = cfg.id, url = %cfg.ingest.url, "{}", msg);
            Box::new(crate::source::FailingSource { message: msg })
        }
        _ => Box::new(VirtualSource {
            camera_id: cfg.id,
            width: 640,
            height: 480,
            fps: if cfg.ingest.max_fps == 0 {
                5
            } else {
                cfg.ingest.max_fps
            },
        }),
    }
}

/// Translate one [`MotionDecision`] into a `motion_events` row write.
/// Lifted out of the loop body so the `match` on `kind` and the
/// attribute-serialization stay readable.
async fn insert_motion_decision(
    store: &Arc<Store>,
    handle: ClipHandle,
    d: &MotionDecision,
) -> Result<(), nexus_store::StoreError> {
    let kind = match d.kind {
        MotionKind::Born => MotionEventKind::Born,
        MotionKind::Updated => MotionEventKind::Updated,
        MotionKind::Died => MotionEventKind::Died,
    };
    // Fast-path the common empty-attributes case (avoids cloning the
    // map + allocating a serde_json::Value::Object wrapper for every
    // motion event in a busy scene).
    let attrs_json = if d.attributes.is_empty() {
        "{}".to_string()
    } else {
        serde_json::to_string(&d.attributes)
            .expect("serde_json::Map<String, Value> is infallible to serialize")
    };
    let new = NewMotionEvent {
        camera_id: d.camera_id,
        clip_id: handle.clip_id,
        track_id: d.track_id,
        kind,
        captured_at: d.captured_at,
        bbox: d.bbox,
        label: d.label.clone(),
        confidence: d.confidence,
        attributes_json: attrs_json,
    };
    store.insert_motion_event(&new).await.map(|_id| ())
}

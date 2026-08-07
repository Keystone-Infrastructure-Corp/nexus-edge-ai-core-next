//! Per-camera supervisor task. Wires source → gate → DetectorPool → tracker
//! → RuleEvaluator → store + bus + LatestFrameCache.
//!
//! Every per-frame work block is wrapped in a `tracing::info_span!("frame.lifecycle", …)`
//! that opens child spans for `decode/gate/infer/track/rules`. That's how
//! the `trace_id` field on [`nexus_types::Frame`] is actually backed.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nexus_bus::{topic, Bus, BusExt};
use nexus_config::{AnnotatorConfig, CameraConfig, ClipsConfig, StaticObjectConfig};
use nexus_inference::{label_matches_any_prompt, Detector};
use nexus_rules::RuleEvaluator;
use nexus_store::{MotionEventKind, NewMotionEvent, Store};
use nexus_tracker::{
    filter_excluded_zones, filter_zone_min_area, is_object_static, MotionDecision,
    MotionEventEmitter, MotionKind, StaticObjectFilter, TrackAnnotator, Tracker,
};
use nexus_types::{
    BBox, CameraId, Frame, FrameMetadata, FrameMetadataLite, PipelineState, PipelineStatus,
    PixelFormat, TrackLite, TrackedObject,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, info_span, warn, Instrument};

use crate::cache::LatestFrameCache;
use crate::crowd_hysteresis::CrowdHysteresis;
use crate::entity_sighting::{
    EntityLocalPersist, EntityLocalSeed, SightingHook, SightingScheduler,
};
use crate::gate::MotionGate;
use crate::post_roll::{PostRoll, PostRollAction};
use crate::recorder::{
    ClipFinal, ClipHandle, ClipRecorder, OpenClip, RecorderError, MAX_CLIP_DURATION_MS,
};
use crate::sink_router::{AlertClipScheduleGate, SinkRouter};
use crate::skip_policy::DetectorSkipPolicy;
use crate::source::{FrameSource, VirtualSource};
use crate::static_clear::StaticAnchorClearRegistry;
use crate::stats::FrameStatsRegistry;

/// How often (in analysis frames) the supervisor stats the in-flight
/// clip file to enforce the `max_clip_bytes` byte cap. Statting every
/// frame would add a syscall to the per-frame fast path for no
/// benefit — a runaway stream takes many frames to balloon — so we
/// sample. The cost of sampling is that the on-disk file can overrun
/// the cap by up to one interval's worth of growth before rotation
/// fires; the cold-upload quarantine ceiling
/// (`max_cold_upload_bytes = max_clip_bytes * 2`) absorbs that
/// overshoot.
const SIZE_STAT_INTERVAL_FRAMES: u32 = 60;

/// JPEG quality for alert snapshot thumbnails written at rule-fire.
/// Matches the live-view low-bitrate encoder — high enough for a
/// recognisable console thumbnail, small enough to keep the blob cheap.
const SNAPSHOT_JPEG_QUALITY: u8 = 72;

/// Encode an alert's supervisor frame to JPEG and persist it at
/// `<snapshots_dir>/<event_id>.jpg`.
///
/// The path is deterministic in `event_id`, so the cloud-console alert
/// sink can locate the file for SAS upload without the path travelling
/// through the durable outbox. Best-effort: any encode/write failure is
/// logged and yields `None` so a missing thumbnail never blocks the
/// alert. Encoding runs on the blocking pool because JPEG of a
/// 720p frame is a few milliseconds of CPU we keep off the async loop.
///
/// When `bbox` is `Some`, the object's bounding box is drawn onto the
/// frame before encoding so the snapshot the operator (and downstream
/// email/SureView sinks) sees is annotated, matching the "annotated
/// snapshot" contract on [`nexus_types::Artifacts::snapshot`].
async fn write_alert_snapshot(
    snapshots_dir: &Path,
    event_id: &str,
    frame: &Arc<Frame>,
    bbox: Option<BBox>,
    label: &str,
    confidence: Option<f32>,
) -> Option<String> {
    // The supervisor frame is guaranteed RGB24 (see source.rs); guard
    // anyway so a future format change fails closed rather than writing
    // a corrupt JPEG.
    if frame.format != PixelFormat::Rgb24 {
        return None;
    }
    let dir = snapshots_dir.to_path_buf();
    let frame = Arc::clone(frame);
    let id = event_id.to_string();
    let label = label.to_string();
    let join = tokio::task::spawn_blocking(move || {
        use image::ImageEncoder as _;
        let path = dir.join(format!("{id}.jpg"));
        // The frame buffer is shared (Arc<Frame>); copy it so the
        // bbox stroke doesn't mutate pixels other subscribers see.
        let mut pixels = frame.data.to_vec();
        if let Some(bbox) = bbox {
            let (stroke, radius) = crate::overlay::box_metrics(frame.width, frame.height);
            crate::overlay::draw_box_rgb24(
                &mut pixels,
                frame.width,
                frame.height,
                bbox.x1.round() as i64,
                bbox.y1.round() as i64,
                bbox.x2.round() as i64,
                bbox.y2.round() as i64,
                stroke,
                radius,
                crate::overlay::ALERT_RGB,
            );
            // Label chip ("person 0.96") anchored to the box top-left,
            // burned into the JPEG so the email / SureView copies show
            // it too — identical to the burned-in alert clip.
            let chip = crate::overlay::label_text(&label, confidence);
            crate::overlay::draw_label_chip_rgb24(
                &mut pixels,
                frame.width,
                frame.height,
                bbox.x1.round() as i64,
                bbox.y1.round() as i64,
                &chip,
                crate::alert_clip::label_px(frame.width),
            );
        }
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, SNAPSHOT_JPEG_QUALITY)
            .write_image(
                &pixels[..],
                frame.width,
                frame.height,
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|e| format!("jpeg encode: {e}"))?;
        std::fs::write(&path, &out).map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok::<PathBuf, String>(path)
    })
    .await;
    match join {
        Ok(Ok(path)) => Some(path.to_string_lossy().into_owned()),
        Ok(Err(e)) => {
            warn!(event = %event_id, "alert snapshot encode/write failed: {e}");
            None
        }
        Err(e) => {
            warn!(event = %event_id, "alert snapshot task join failed: {e}");
            None
        }
    }
}

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
    detector_low_res: Option<Arc<dyn Detector>>,
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
    // Effective `model.top_k` for this camera (per-camera `model_override`
    // wins over the global `inference.model.top_k`). Used by the G1 tile
    // cascade to re-truncate the merged stage-1 + stage-2 detection vector
    // so the operator's `top_k` is honoured GLOBALLY across stages rather
    // than per-stage. `None` disables the post-merge re-cap (matches the
    // pre-B2.1 behaviour for cameras without a configured cap).
    effective_top_k: Option<usize>,
    // M7 per-rule sink routing. Resolves, per firing rule, which
    // alert-delivery sinks each recorded alert is enqueued into the
    // `alert_sink_outbox` for. A `NoopSinkRouter` routes to nothing,
    // degrading the enqueue path to a plain event-record (pre-M7
    // behaviour) for harnesses that don't wire the dispatcher.
    sink_router: Arc<dyn SinkRouter>,
    // M-Event-Audit: gates alert-clip arming (and the `events.alerted`
    // stamp) on the live delivery schedule, so an off-schedule match is
    // logged + linked to its motion clip but builds no alert clip.
    // `NoopAlertClipScheduleGate` always arms (pre-M-Event-Audit
    // behaviour) for harnesses that don't wire the delivery cascade.
    alert_clip_gate: Arc<dyn AlertClipScheduleGate>,
) -> CameraHandle {
    let camera_id = cfg.id;
    let task = tokio::spawn(run_camera(
        cfg,
        detector,
        detector_low_res,
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
        effective_top_k,
        sink_router,
        alert_clip_gate,
    ));
    CameraHandle { camera_id, task }
}

#[allow(clippy::too_many_arguments)]
async fn run_camera(
    cfg: CameraConfig,
    detector: Arc<dyn Detector>,
    detector_low_res: Option<Arc<dyn Detector>>,
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
    effective_top_k: Option<usize>,
    sink_router: Arc<dyn SinkRouter>,
    alert_clip_gate: Arc<dyn AlertClipScheduleGate>,
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

        // M_PERF_CROWD Phase E2 — the supervisor may rebuild its
        // FrameSource under sustained crowd to consume a
        // recorder-side RGB tap that has been resized to
        // `supervisor_downscale_to_width`. The originals are the
        // engine-spawn-time dims (high-res); the `current_*`
        // mirror what the active source is producing right now.
        // Both are equal until the first E2 flip.
        let original_supervisor_w = supervisor_w;
        let original_supervisor_h = supervisor_h;
        let mut current_supervisor_w = supervisor_w;
        let mut current_supervisor_h = supervisor_h;

        let gate = MotionGate::new();
        // M_PERF_CROWD Phase E1 — adaptive detector cadence under crowd.
        // No-op (always-run) unless both
        // `behavior.detector_skip_crowded_threshold` and
        // `behavior.detector_skip_every_n_frames` are `Some`.
        let mut skip_policy = DetectorSkipPolicy::new(
            cfg.behavior.detector_skip_crowded_threshold,
            cfg.behavior.detector_skip_every_n_frames,
        );
        // M_PERF_CROWD Phase E3 — adaptive detector input downscale
        // under crowd. No-op (always returns `false`) unless both
        // `behavior.detector_downscale_crowded_threshold` and
        // `behavior.detector_downscale_sustained_secs` are `Some`
        // AND the router pre-built a low-res layer (i.e.
        // `detector_low_res` is `Some`). When all three are present,
        // the supervisor swaps to `detector_low_res` once the EMA has
        // held above the threshold for the sustained window.
        let mut crowd_hysteresis = CrowdHysteresis::new(
            cfg.behavior.detector_downscale_crowded_threshold,
            cfg.behavior.detector_downscale_sustained_secs,
            None,
        );
        let mut detector_downscaled = false;
        // M_PERF_CROWD Phase E2 — adaptive *supervisor frame*
        // downscale under crowd (sibling of E3's detector input
        // downscale). Tracked independently so operators can tune
        // each lever on its own. Asymmetric up/down windows: the
        // up-trigger fires quickly (typical 60s) but the
        // upscale-back trigger is intentionally slower (typical
        // 300s) because each flip rebuilds the recorder's
        // pre-roll ingester at new RGB dims and closes any open
        // clip. No-op (always returns `false`) unless the camera
        // opts in via the four `supervisor_downscale_*` knobs.
        let mut supervisor_hysteresis = CrowdHysteresis::new(
            cfg.behavior.supervisor_downscale_crowded_threshold,
            cfg.behavior.supervisor_downscale_sustained_secs,
            cfg.behavior.supervisor_upscale_clear_secs,
        );
        let mut supervisor_downscaled = false;
        let mut decoded: u64 = 0;
        let mut detected: u64 = 0;
        let prompts = cfg.detector.prompts.clone();
        let zones = cfg.zones.clone();
        // Phase 8.1: equipment classes the static filter should also
        // promote to anchors (captured before the annotator config moves).
        let anchor_classes = annotator_cfg.static_anchor_classes.clone();
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
            Some(StaticObjectFilter::with_anchor_classes(
                effective_static_cfg,
                cfg.id,
                Some(path),
                anchor_classes,
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
        // Alert snapshot output dir — created once per camera task so the
        // per-alert write path (below) never races a mkdir. Failure to
        // create it is non-fatal: snapshots are best-effort and the
        // per-alert write simply logs + skips.
        let snapshots_dir = state_dir.join("snapshots");
        if let Err(e) = tokio::fs::create_dir_all(&snapshots_dir).await {
            warn!(
                camera_id = cfg.id,
                dir = %snapshots_dir.display(),
                "failed to create alert snapshot dir (snapshots disabled for this camera): {e}"
            );
        }
        // Wall-clock anchor for the currently-open clip. Used to
        // enforce the M2.1 MAX_CLIP_DURATION_MS bound — once the
        // open clip exceeds 5min we force-close it and (if motion
        // is still active on this frame) the next Born will open a
        // fresh one. Reset to None on every close.
        let mut clip_opened_at: Option<chrono::DateTime<chrono::Utc>> = None;
        // Byte cap on the in-flight clip. A corrupt camera H.264
        // stream can balloon a single short clip to multiple GiB
        // long before the 5-min duration cap fires, and such a clip
        // wedges the cold replicator (it can't finish a multi-GiB
        // upload inside the SAS window). We stat the partial file
        // every `SIZE_STAT_INTERVAL_FRAMES` while a clip is open and
        // rotate on the same path as the duration cap once it crosses
        // `max_clip_bytes`. `0` disables the guard.
        let max_clip_bytes = clips_cfg.max_clip_bytes;
        let mut frames_since_size_stat: u32 = 0;
        let mut post_roll = PostRoll::new(clips_cfg.post_roll_secs);
        // M-Alert-Clip: gate every alert-clip hook on the config flag so
        // the hot path stays free when the feature is off (the default).
        let alert_clips_enabled = clips_cfg.alert_clips.enabled;
        // Guarantee every alert/event has an underlying full-resolution
        // motion clip even when the motion tracker never declared `Born`
        // for it (small / distant / brief motion that trips a rule on a
        // keyframe pass). When enabled, an alert firing on a frame with
        // no open clip force-opens a native-resolution motion clip and
        // each alert frame keeps it open through `post_roll_secs`.
        let record_motion_clip_on_alert = clips_cfg.record_motion_clip_on_alert;

        info!(camera_id = cfg.id, "pipeline running");

        'outer: loop {
        let (tx, mut rx) = mpsc::channel::<Frame>(8);
        let source = build_source(
            &cfg,
            &recorder,
            current_supervisor_w,
            current_supervisor_h,
        );
        let cam_id = cfg.id;
        let source_task = tokio::spawn(async move {
            if let Err(e) = source.run(tx).await {
                warn!(camera_id = cam_id, "frame source ended: {e}");
            }
        });
        // M_PERF_CROWD Phase E2 — set true when the per-frame body
        // requests an RGB-tap rebuild (the recorder's pre-roll
        // ingester has been replaced at new dims; the old source's
        // broadcast channel is closed). On exit from the inner
        // `while`, the outer loop spawns a fresh source against the
        // new RGB dims.
        let mut rebuild_source = false;

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

            // BUG-024 — this span MUST be attached with `.instrument()`,
            // never entered with `Span::enter()`. `Entered` is a
            // thread-local construct: `Registry::enter` pushes the span
            // id onto a `ThreadLocal<SpanStack>` and the guard's drop
            // pops from whatever worker happens to be running. With
            // `.await` points inside the guard's scope the tokio
            // work-stealing scheduler can migrate this task mid-frame,
            // the exit then pops the wrong thread's stack, and the span
            // is never closed. Orphaned entries also stay at the top of
            // the original worker's stack and become the contextual
            // parent of every span opened there afterwards — and a span
            // isn't closed until its children close — so leaked spans
            // chain and no link can ever be released. That leaked ~7% of
            // all frames on a 29-camera site, ~500 MB/h, until the unit
            // saturated its cgroup `MemoryHigh`. `Instrumented` enters
            // and exits inside each `poll`, so enter/exit always pair on
            // the thread that is actually polling.
            //
            // Consequence for the body below: it is an `async` block, so
            // `continue` / `break` for the frame loop are expressed as
            // `return ControlFlow::Continue(())` / `ControlFlow::Break(())`.
            let flow = async {
                let pass = {
                    let _g = info_span!("frame.gate").entered();
                    gate.allow(&frame)
                };
                if !pass {
                    debug!(camera_id = cfg.id, frame_id, "gate dropped frame");
                    stats.observe_dropped(cfg.id);
                    return ControlFlow::Continue(());
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
                    let duration_exceeded = age_ms >= MAX_CLIP_DURATION_MS;

                    // Byte-cap guard. Sampled every SIZE_STAT_INTERVAL_FRAMES
                    // to keep the per-frame fast path syscall-free. Rotates
                    // on the same close+reopen path as the duration cap so a
                    // corrupt byte-exploding stream can't produce a single
                    // multi-GiB clip that wedges the cold replicator.
                    let mut size_exceeded = false;
                    if max_clip_bytes > 0 {
                        frames_since_size_stat += 1;
                        if frames_since_size_stat >= SIZE_STAT_INTERVAL_FRAMES {
                            frames_since_size_stat = 0;
                            if let Some(size_bytes) = recorder.inflight_size_bytes(handle).await {
                                if size_bytes >= max_clip_bytes {
                                    size_exceeded = true;
                                    warn!(
                                        camera_id = cfg.id,
                                        clip_id = handle.clip_id,
                                        size_bytes,
                                        max_bytes = max_clip_bytes,
                                        "rotating clip: max size reached \
                                         (likely corrupt byte-exploding stream)"
                                    );
                                }
                            }
                        }
                    }

                    if duration_exceeded || size_exceeded {
                        if duration_exceeded {
                            debug!(
                                camera_id = cfg.id,
                                clip_id = handle.clip_id,
                                age_ms,
                                max_ms = MAX_CLIP_DURATION_MS,
                                "rotating clip: max duration reached"
                            );
                        }
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
                                "recorder.close (rotation) failed: {e}"
                            );
                        }
                        current_clip = None;
                        clip_opened_at = None;
                        frames_since_size_stat = 0;
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
                // M_PERF_CROWD Phase E3 — pick the detector for THIS
                // frame based on the hysteresis state observed on the
                // PREVIOUS frame. `detector_low_res` is only ever `Some`
                // when the camera opted in AND the router pre-built the
                // low-res layer; if either side is missing the supervisor
                // stays on the high-res detector regardless of crowd.
                let active_detector: &Arc<dyn Detector> = match (detector_downscaled, &detector_low_res)
                {
                    (true, Some(low)) => low,
                    _ => &detector,
                };
                let detections = if skip_detector {
                    debug!(
                        camera_id = cfg.id,
                        frame_id, "detector skipped (crowd skip policy)"
                    );
                    Vec::new()
                } else {
                    let span = info_span!("frame.infer", model = %active_detector.name());
                    match active_detector
                        .detect(&frame, &prompts)
                        .instrument(span)
                        .await
                    {
                        Ok(d) => d,
                        Err(e) => {
                            error!(camera_id = cfg.id, "detect failed: {e}");
                            return ControlFlow::Continue(());
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

                // M_TILE_REINFER (G1) — Phase B2 cascade: when this
                // camera opted in via behavior.tile_enabled = Some(true)
                // AND the stage-1 detection count crossed
                // behavior.tile_trigger AND we're not on a skipped
                // frame (E1 invariant: G1 disabled when E1 skip fires
                // same tick), re-run the SAME active_detector on a small
                // set of cropped sub-regions chosen by stage-1 density.
                // Stage-2 detections are mapped back to parent-frame
                // coordinates and concatenated with stage-1 before the
                // single tracker.update() call below.
                //
                // On crop or inference error we fall through to
                // stage-1-only (fail-soft) — the cascade is a compute-
                // optimality knob, not a correctness one. The merged
                // detection set is NOT re-deduped here; the tracker's
                // association layer handles overlapping boxes from
                // adjacent tiles as it would for any duplicate
                // detection. See docs/edge-core/M_TILE_REINFER.md.
                let detections = if !skip_detector
                    && cfg.behavior.tile_enabled == Some(true)
                    && cfg.behavior.tile_trigger.is_some_and(|t| {
                        let trigger = t as usize;
                        trigger > 0 && detections.len() >= trigger
                    }) {
                    let grid: crate::tile::TileGridConfig = cfg
                        .behavior
                        .tile_grid
                        .map(Into::into)
                        .unwrap_or(crate::tile::TileGridConfig::G2x2);
                    let max_tiles = cfg.behavior.tile_max_per_frame.unwrap_or(3);
                    let tiles =
                        crate::tile::pick_tiles(&detections, frame.width, frame.height, grid, max_tiles);
                    if tiles.is_empty() {
                        detections
                    } else {
                        let span =
                            info_span!("frame.tile_infer", model = %active_detector.name(), tiles = tiles.len());
                        let tile_started = std::time::Instant::now();
                        match crate::tile_executor::run_tile_inference(
                            active_detector.as_ref(),
                            &frame,
                            &tiles,
                            &prompts,
                        )
                        .instrument(span)
                        .await
                        {
                            Ok(stage2) => {
                                let tile_elapsed_ms =
                                    tile_started.elapsed().as_millis().min(u128::from(u64::MAX))
                                        as u64;
                                // Apply the same per-camera prompts
                                // whitelist to stage-2 outputs that the
                                // stage-1 block applied above. Idempotent
                                // for open-vocab detectors that already
                                // honour `prompts`; required for
                                // closed-vocab YOLO/COCO which emits all
                                // mapped classes regardless.
                                let stage2: Vec<_> = if prompts.is_empty() {
                                    stage2
                                } else {
                                    stage2
                                        .into_iter()
                                        .filter(|d| label_matches_any_prompt(&d.label, &prompts))
                                        .collect()
                                };
                                // M_TILE_REINFER (G1) — Phase B3 telemetry:
                                // record one tile invocation with the
                                // post-whitelist stage-2 count and elapsed
                                // wall-clock ms. `added` is what actually
                                // reaches the tracker so the UI can show
                                // "stage-2 detections / cascade".
                                stats.observe_tile_invocation(
                                    cfg.id,
                                    stage2.len() as u64,
                                    tile_elapsed_ms,
                                );
                                debug!(
                                    camera_id = cfg.id,
                                    frame_id,
                                    stage1 = detections.len(),
                                    tiles = tiles.len(),
                                    stage2 = stage2.len(),
                                    tile_ms = tile_elapsed_ms,
                                    "tile cascade merged stage-2 detections"
                                );
                                let mut merged = detections;
                                merged.extend(stage2);
                                // M_TILE_REINFER (G1) — Phase B2.1: enforce
                                // the operator's `top_k` GLOBALLY across the
                                // merged stage-1 + stage-2 vector. The stage-1
                                // wrapper and the per-tile wrapper each
                                // already capped to ≤k, so worst-case input
                                // here is `k × (1 + max_tiles)` — without
                                // this call the tracker would see up to that
                                // many boxes per cascade frame, silently
                                // exceeding the configured cap. Idempotent
                                // when `merged.len() ≤ k` (skips the sort).
                                if let Some(k) = effective_top_k {
                                    nexus_inference::caps::apply_top_k(&mut merged, k);
                                }
                                merged
                            }
                            Err(e) => {
                                // Fail-soft to stage-1 only. The
                                // alternative — dropping the frame — would
                                // make the cascade a correctness risk
                                // rather than a recall booster.
                                warn!(
                                    camera_id = cfg.id,
                                    frame_id,
                                    tiles = tiles.len(),
                                    "tile cascade failed, falling back to stage-1: {e}"
                                );
                                detections
                            }
                        }
                    }
                } else {
                    detections
                };

                let mut tracked = {
                    let _g = info_span!("frame.track", tracker = tracker.name()).entered();
                    tracker.update(detections)
                };
                // M_PERF_CROWD Phase E1 — feed the post-tracker
                // tracked-object count back into the skip policy's EMA so
                // the next frame's skip decision reflects current crowd
                // density. No-op when the policy is disabled.
                skip_policy.observe(tracked.len());
                // M_PERF_CROWD Phase E3 — same tracked-object count drives
                // the input-downscale hysteresis. The returned bool is the
                // desired downscale state for the NEXT frame; on the
                // current frame we already committed to a detector above.
                // No-op when the policy is disabled.
                detector_downscaled =
                    crowd_hysteresis.observe(tracked.len(), std::time::Instant::now());
                // M_PERF_CROWD Phase E2 — sustained-crowd supervisor
                // frame downscale. Independent hysteresis (asymmetric
                // up/down windows) over the same tracked-object EMA. On
                // a state flip and only when the camera opted in via
                // `behavior.supervisor_downscale_to_width`, ask the
                // recorder to rebuild its pre-roll ingester at the new
                // RGB dims; on success, close any open clip (so its
                // recorded `frame_width` matches the pixels going
                // forward), update `current_supervisor_*`, and break to
                // the outer loop which will spawn a fresh
                // `FrameSource` against the new shared RGB tap. No-op
                // when the policy is disabled or when the recorder has
                // no RGB-tap ingester for this camera (e.g. stub
                // recorder in tests).
                let want_supervisor_downscale =
                    supervisor_hysteresis.observe(tracked.len(), std::time::Instant::now());
                if want_supervisor_downscale != supervisor_downscaled {
                    if let Some(downscale_w) = cfg.behavior.supervisor_downscale_to_width {
                        let (target_w, target_h) = if want_supervisor_downscale {
                            crate::source::supervisor_frame_for(downscale_w)
                        } else {
                            (original_supervisor_w, original_supervisor_h)
                        };
                        match recorder.resize_camera_rgb_tap(cfg.id, target_w, target_h) {
                            Ok(true) => {
                                info!(
                                    camera_id = cfg.id,
                                    target_w,
                                    target_h,
                                    downscaled = want_supervisor_downscale,
                                    "supervisor frame size flipped (crowd hysteresis); rebuilding source"
                                );
                                supervisor_downscaled = want_supervisor_downscale;
                                current_supervisor_w = target_w;
                                current_supervisor_h = target_h;
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
                                        warn!(
                                            camera_id = cfg.id,
                                            "recorder.close (RGB tap resize) failed: {e}"
                                        );
                                    }
                                    clip_opened_at = None;
                                    post_roll.reset();
                                }
                                rebuild_source = true;
                                return ControlFlow::Break(());
                            }
                            Ok(false) => {
                                // Recorder reports no rebuild needed
                                // (stub recorder, ingester absent, or
                                // dims already match). Record the new
                                // desired state so we don't re-poll the
                                // recorder every frame, but don't
                                // restart the source.
                                supervisor_downscaled = want_supervisor_downscale;
                            }
                            Err(e) => {
                                warn!(
                                    camera_id = cfg.id,
                                    "resize_camera_rgb_tap failed: {e}; staying on current dims"
                                );
                                supervisor_downscaled = want_supervisor_downscale;
                            }
                        }
                    } else {
                        // Threshold + sustained-secs are set but the
                        // operator forgot the target width. Record the
                        // state so we don't log every frame; the
                        // operator's `nexus-doctor` config check should
                        // surface this misconfiguration.
                        supervisor_downscaled = want_supervisor_downscale;
                    }
                }
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
                    // Phase 8.1: hand the annotator the prior-frame static
                    // anchor set (empty when parking_lot_mode is off) so it
                    // can stamp `motion.near_static_vehicle_*`.
                    let anchors = static_filter
                        .as_ref()
                        .map(|f| f.anchors())
                        .unwrap_or(&[]);
                    annotator.annotate(&frame, &zones, anchors, &mut tracked);
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

                // Lightweight metadata onto the bus. `objects` is
                // `Arc<Vec<TrackedObject>>` (M_PERF_CROWD D1) so we
                // reuse the same allocation as `LatestFrameCache`
                // instead of cloning the vec a second time per frame.
                let meta = FrameMetadata {
                    camera_id: cfg.id,
                    frame_id,
                    captured_at: frame.captured_at,
                    width: frame.width,
                    height: frame.height,
                    trace_id: trace_id.clone(),
                    objects: Arc::clone(&tracked_arc),
                };
                let _ = bus.publish(topic::FRAME_METADATA, &meta).await;
                // M_PERF_CROWD F1 — same per-frame cadence on the
                // bandwidth-relief lite topic. Drops the per-object
                // `attributes` map (~400–600 B/object) so the SSE
                // overlay subscriber's broadcast buffer no longer
                // dominates `BusError::Lagged` under crowd load. The
                // attributes panel still subscribes to the full topic
                // via `?attributes=full`.
                let meta_lite = FrameMetadataLite {
                    camera_id: meta.camera_id,
                    frame_id: meta.frame_id,
                    captured_at: meta.captured_at,
                    width: meta.width,
                    height: meta.height,
                    trace_id: meta.trace_id.clone(),
                    objects: Arc::new(tracked.iter().map(TrackLite::from).collect()),
                };
                let _ = bus
                    .publish(topic::FRAME_METADATA_LITE, &meta_lite)
                    .await;

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

                // M-Alert-Clip: feed this frame's frame-aligned detection
                // boxes into the recorder's per-camera box timeline so an
                // alert clip armed later can burn them into the pre-roll +
                // post window. No-op unless enabled; cheap.
                //
                // Only tracks with a REAL detection on THIS frame
                // (`detection_bbox`) are burned. A predicted-only (coasting)
                // track has no detection this frame and would otherwise be
                // drawn at its stale EMA-smoothed position; as the tracker
                // coasts / re-spawns fragment tracks for one object those
                // stale boxes pile up into the "trailing" ghost boxes seen on
                // the clip. Dropping coasting tracks and NMS-deduping the
                // overlapping survivors yields one box per physical object.
                if alert_clips_enabled {
                    let mut boxes: Vec<crate::alert_clip::BurnBox> = dynamic_tracked
                        .iter()
                        .filter_map(|t| {
                            let b = t.detection_bbox?;
                            Some(crate::alert_clip::BurnBox {
                                x1: b.x1,
                                y1: b.y1,
                                x2: b.x2,
                                y2: b.y2,
                                label: t.label.clone(),
                                confidence: t.confidence,
                            })
                        })
                        .collect();
                    crate::alert_clip::dedupe_burn_boxes(&mut boxes);
                    recorder.push_alert_boxes(
                        cfg.id,
                        frame.captured_at,
                        boxes,
                        current_supervisor_w,
                        current_supervisor_h,
                    );
                }

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
                // M-Event-Audit: set once at least one match this frame is
                // within the delivery schedule (would be delivered). Gates
                // the alert-clip arm below; an off-schedule frame logs its
                // events + links the motion clip only.
                let mut any_deliverable = false;
                for mut ev in events {
                    let event_id = ev.event_id.to_string();
                    // M-Event-Audit: does this rule-fire fall within the
                    // active delivery schedule (global + per-rule cascade)?
                    // Drives both the alert-clip arm and the `events.alerted`
                    // audit flag stamped by `record_event_and_enqueue`.
                    let alerted = alert_clip_gate.should_build(&ev.rule_id, frame.captured_at);
                    // Alert snapshot — persist a JPEG of the frame that fired
                    // this rule at a deterministic
                    // `<state_dir>/snapshots/<event_id>.jpg` path BEFORE the
                    // outbox row is written, so the cloud-console sink (if
                    // enrolled) always finds the file when it processes the
                    // row. Best-effort: a missing thumbnail never blocks the
                    // alert. Also stamped onto `artifacts.snapshot` for bus
                    // subscribers / the local admin API.
                    let snap_conf = ev
                        .context
                        .get("confidence")
                        .and_then(serde_json::Value::as_f64)
                        .map(|f| f as f32);
                    if let Some(path) = write_alert_snapshot(
                        &snapshots_dir,
                        &event_id,
                        &frame_arc,
                        ev.bbox,
                        &ev.label,
                        snap_conf,
                    )
                    .await
                    {
                        ev.artifacts.snapshot = Some(path);
                    }
                    // M7 per-rule sink routing — resolve which configured
                    // sinks this rule delivers to, then record the event
                    // and enqueue an `alert_sink_outbox` row per sink in a
                    // single transaction. An empty resolution records the
                    // event with no outbox rows (identical to the pre-M7
                    // `record_event`), so a `NoopSinkRouter` or a config
                    // with no sinks keeps today's behaviour.
                    let sinks = sink_router.sinks_for(&ev.rule_id);
                    let sink_refs: Vec<&str> = sinks.iter().map(String::as_str).collect();
                    if let Err(e) = store
                        .record_event_and_enqueue_classified(&ev, &sink_refs, alerted)
                        .await
                    {
                        warn!(event = %ev.event_id, "store.record_event_and_enqueue failed: {e}");
                    } else {
                        events_to_link.push(event_id);
                        any_deliverable |= alerted;
                    }
                    let _ = bus.publish(topic::ALERT_EVENT, &ev).await;
                }

                // M-Alert-Clip: arm (or coalesce into) a short alert clip for
                // this burst and link every event fired this frame to it, so
                // clip-attaching sinks resolve the truncated clip within
                // ~post_secs instead of waiting on the up-to-5-min motion
                // clip. `arm_alert_clip` returns None (no-op) unless the
                // feature is enabled and the camera has a pre-roll ingester.
                //
                // M-Event-Audit: only arm when at least one recorded match
                // this frame is within the delivery schedule
                // (`any_deliverable`). Off-schedule matches keep their
                // `events.clip_id` motion-clip link and skip the expensive
                // decode -> burn-in -> re-encode entirely.
                if alert_clips_enabled && any_deliverable {
                    if let Some(alert_clip_id) =
                        recorder.arm_alert_clip(cfg.id, frame.captured_at).await
                    {
                        for eid in &events_to_link {
                            if let Err(e) = store.link_event_alert_clip(eid, alert_clip_id).await {
                                debug!(
                                    camera_id = cfg.id,
                                    event = %eid,
                                    "link_event_alert_clip failed: {e}"
                                );
                            }
                        }
                    }
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
                                frame_width: current_supervisor_w,
                                frame_height: current_supervisor_h,
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
                // to open a clip. When `record_motion_clip_on_alert` is
                // set, an alert on a frame with no open clip force-opens a
                // native-resolution motion clip so every event has
                // surrounding full-res video — matching the motion clip
                // (native resolution, pre-roll, post-roll), NOT the
                // reduced-resolution burned-in alert clip. When the flag is
                // off, alerts on frames with no open clip stay unlinked
                // (clip_id NULL) and the timeline UI shows "no surrounding
                // video".
                if !events_to_link.is_empty() {
                    if current_clip.is_none() && record_motion_clip_on_alert {
                        match recorder
                            .open(OpenClip {
                                camera_id: cfg.id,
                                started_at: frame.captured_at,
                                frame_width: current_supervisor_w,
                                frame_height: current_supervisor_h,
                            })
                            .await
                        {
                            Ok(handle) => {
                                debug!(
                                    camera_id = cfg.id,
                                    clip_id = handle.clip_id,
                                    "alert-triggered motion clip opened (no live motion track)"
                                );
                                current_clip = Some(handle);
                                clip_opened_at = Some(frame.captured_at);
                            }
                            Err(RecorderError::Refused) => {
                                // Watermark sampler has paused new clips
                                // (panic mode). The event stays unlinked;
                                // nothing else to do this frame.
                                debug!(
                                    camera_id = cfg.id,
                                    "recorder refused alert-triggered open (panic mode)"
                                );
                            }
                            Err(e) => {
                                warn!(
                                    camera_id = cfg.id,
                                    "alert-triggered recorder.open failed: {e}"
                                );
                            }
                        }
                    }
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
                //
                // An alert firing this frame counts as activity for the
                // deferred close (same as live motion) so an
                // alert-triggered clip — which may have opened with no
                // live tracker track at all — stays open and captures the
                // full post-roll window instead of closing on the very
                // next frame.
                let alert_kept_alive = record_motion_clip_on_alert && !events_to_link.is_empty();
                let has_live_motion = emitter.live_track_count(cfg.id) > 0 || alert_kept_alive;
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

                ControlFlow::Continue(())
            }
            .instrument(frame_span)
            .await;
            if flow.is_break() {
                break;
            }
        }

        // Inner `while let Some(frame) = rx.recv().await` ended.
        // Either the source died naturally (channel closed) or the
        // E2 hysteresis path requested a rebuild. In the rebuild
        // case the recorder has already swapped its pre-roll
        // ingester; we abort the now-stale source task, drain it,
        // and continue the outer loop to spawn a fresh source that
        // will subscribe to the new shared RGB tap.
        source_task.abort();
        let _ = source_task.await;
        if !rebuild_source {
            break 'outer;
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

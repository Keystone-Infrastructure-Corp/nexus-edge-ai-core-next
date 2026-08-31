//! Camera hot-reload reconciler — subscribes to
//! `topic::CONFIG_CHANGED` and diffs the live `cameras` table
//! against the set of supervisor tasks + pre-roll ingesters currently
//! running in this process. Any delta (new camera, deleted camera,
//! disabled→enabled toggle, URL change) is converged without
//! restarting the engine.
//!
//! Why this exists: every camera mutation in the admin API
//! (`PUT /api/v1/cameras/{id}`, `DELETE /api/v1/cameras/{id}`, including
//! the discovery → Add flow) writes the row + publishes a
//! `config.changed` bus event. Without a subscriber, the on-disk
//! state and the in-memory runtime drift apart until the next engine
//! restart. This module IS that subscriber.
//!
//! Reconciliation model — single async task that:
//!   1. Subscribes to `topic::CONFIG_CHANGED` once at startup.
//!   2. On each event (and once at startup so any cameras the engine
//!      already spawned are recorded in `handles`) calls
//!      [`reconcile`], which re-reads `store.list_cameras()` and
//!      compares it against the shared `handles` map.
//!   3. Adds, removes, or restarts supervisors + ingesters to make
//!      the runtime match the DB.
//!
//! Restart triggers today: ingest URL change, supervisor (analysis)
//! frame dimension change, and ingest codec change. Detector /
//! threshold / rule changes do not — those still require a process
//! restart (or a future, finer-grained hot-reload path). This
//! matches the UX where the admin UI surfaces camera ingest edits
//! as the primary live operation.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use nexus_bus::{topic, Bus, BusExt};
use nexus_config::TrackerConfig;
use nexus_config::{AnnotatorConfig, CameraConfig, ClipsConfig, StaticObjectConfig};
use nexus_inference::InferenceRouter;
use nexus_pipeline::{
    spawn_camera, ClipRecorder, DecodeHealthRegistry, FrameStatsRegistry, LatestFrameCache,
    StaticAnchorClearRegistry,
};
use nexus_rules::RuleEvaluator;
use nexus_store::Store;
use nexus_tracker::Tracker;
use nexus_types::{CameraId, CodecKind};
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Shared handle store. The reconciler owns the only mutator; other
/// modules may read for diagnostics. Wrapped in
/// [`parking_lot::Mutex`] (not tokio's) because every access is a
/// trivial map insert/remove + clone and we want the lock to be
/// usable from non-async helpers as well.
pub type HandleMap = Arc<Mutex<HashMap<CameraId, RunningCameraEntry>>>;

/// Per-camera runtime state. The `JoinHandle` is wrapped in `Arc`
/// so the shutdown path in `main.rs` can abort every supervisor by
/// iterating the map without taking exclusive ownership of each
/// entry.
#[derive(Clone)]
pub struct RunningCameraEntry {
    pub task: Arc<JoinHandle<()>>,
    /// Current ingest URL. Compared on each reconcile pass to
    /// decide whether a respawn is needed.
    pub url: String,
    /// Resolved supervisor (RGB analysis) frame `(width, height)` in
    /// effect for this camera. Compared on each reconcile pass so a
    /// UI-side change to `model_override.input_width` triggers a
    /// respawn (which rebuilds the GStreamer pipeline at the new
    /// caps) without needing a process restart. See
    /// [`nexus_pipeline::supervisor_frame_for`].
    pub supervisor_dims: (u32, u32),
    /// Configured ingest codec as stored in the DB (`None` = "auto",
    /// resolved per spawn via `discovery::rtsp_probe`). Compared on
    /// each reconcile pass so a UI-side codec edit forces a respawn
    /// — the new value is loaded into the GStreamer pipeline by the
    /// fresh ingester rather than continuing to decode with the
    /// previous depayloader/decoder chain.
    pub codec: Option<CodecKind>,
    /// Configured analysis substream URL (SPEC-069), or `None` when
    /// analysis reads the main stream. Compared on each reconcile pass
    /// — without it, applying a reprobe proposal writes the camera row,
    /// the reconciler decides nothing changed, and the second session
    /// is never started. The console would then show a camera set to
    /// analyse its substream that is doing no such thing.
    pub analysis_url: Option<String>,
}

/// Bundle of every dependency `spawn_camera()` needs. Constructed
/// once at engine boot and moved into the reconciler task; the task
/// keeps it for its entire lifetime.
pub struct ReconcilerArgs {
    pub router: Arc<InferenceRouter>,
    /// Tracker configuration snapshot — used to instantiate a
    /// fresh per-camera tracker on every `start_camera` call.
    /// Trackers are stateful (track ids, IoU history) and MUST
    /// NOT be shared across cameras, or detections from camera A
    /// will pollute camera B's track table and frame metadata.
    pub tracker_cfg: TrackerConfig,
    pub annotator: AnnotatorConfig,
    pub static_object: StaticObjectConfig,
    pub clips: ClipsConfig,
    pub state_dir: PathBuf,
    pub evaluator: Arc<RuleEvaluator>,
    pub store: Arc<Store>,
    pub recorder: Arc<dyn ClipRecorder>,
    pub bus: Arc<dyn Bus>,
    pub cache: Arc<LatestFrameCache>,
    pub frame_stats: Arc<FrameStatsRegistry>,
    pub decode_health: Arc<DecodeHealthRegistry>,
    /// SPEC-069 Phase 1 (P3) — cleared on camera removal alongside
    /// `decode_health` so a stale `analysis_stream` entry never survives
    /// a delete/rebuild.
    pub analysis_stream: Arc<nexus_pipeline::AnalysisStreamRegistry>,
    pub static_clear: Arc<StaticAnchorClearRegistry>,
    pub pre_roll_secs: u32,
    /// Fallback detector input width when a camera's
    /// `model_override` is absent. Sourced from
    /// `cfg.inference.model.input_width`. Drives the per-camera
    /// supervisor frame size via
    /// [`nexus_pipeline::supervisor_frame_for`].
    pub default_detector_width: u32,
    /// Fallback per-frame detection cap when a camera's
    /// `model_override.top_k` is absent. Sourced from
    /// `cfg.inference.model.top_k`. Threaded to `spawn_camera` so
    /// the G1 tile cascade can re-truncate the merged stage-1 +
    /// stage-2 vector and enforce the cap GLOBALLY across stages
    /// rather than per-stage (see
    /// `nexus_inference::caps::apply_top_k`).
    pub default_top_k: Option<usize>,
    /// Phase 5.6 · slice 4c-ii — engine-built hook that turns
    /// per-stable-track [`nexus_pipeline::SightingSnapshot`]s into
    /// `entity_sighting` wire envelopes. Cloned per `start_camera`
    /// call so the reconciler picks up a freshly-spawned camera
    /// with the same emit fan-out as the boot-time ones.
    pub sighting_hook: Arc<dyn nexus_pipeline::SightingHook>,
    /// Tunables for the per-camera [`nexus_pipeline::SightingScheduler`].
    pub sighting_cfg: nexus_pipeline::supervisor::SightingSchedulerConfig,
    /// Phase 5.6 · R4 — shared persistence sink for the
    /// per-camera scheduler's `entity_local_state` writes. Cloned
    /// into every `start_camera` call so a hot-added camera shares
    /// the same worker as the boot-time ones.
    pub sighting_persist: Arc<dyn nexus_pipeline::EntityLocalPersist>,
    /// Hydration window (seconds) used when `start_camera` loads a
    /// per-camera seed from the store. Matches the boot-time value.
    pub sighting_hydration_window_secs: u64,
    /// M7 per-rule sink routing — shared resolver of which configured
    /// sinks each recorded alert is enqueued to. Cloned into every
    /// `start_camera` call so a hot-added camera routes alerts with
    /// the same per-rule `sinks` semantics as the boot-time ones.
    pub sink_router: Arc<dyn nexus_pipeline::SinkRouter>,
    /// M-Event-Audit alert-clip schedule gate (delegates to the shared
    /// `CascadingPolicy`). Cloned per `start_camera` so a freshly
    /// spawned camera gates alert-clip arming on the same live delivery
    /// schedule as the boot-time ones.
    pub alert_clip_schedule_gate: Arc<dyn nexus_pipeline::AlertClipScheduleGate>,
    pub handles: HandleMap,
    /// Phase 10 Live View — so stopping a camera also reaps its LBR pump.
    /// Without this the pump is reaped only by an `lbr_unsubscribe` from the
    /// cloud or a tunnel drop, and a stopped camera leaves a task polling a
    /// cache entry that will never be refilled again.
    pub live_view: Arc<crate::live_view::LiveViewManager>,
}

/// Spawn the reconciler task. Returns its `JoinHandle` so the main
/// shutdown path can abort it alongside the other long-lived tasks.
pub fn spawn(args: ReconcilerArgs) -> JoinHandle<()> {
    tokio::spawn(async move { run(args).await })
}

async fn run(args: ReconcilerArgs) {
    let mut stream = match args
        .bus
        .subscribe::<serde_json::Value>(topic::CONFIG_CHANGED)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            error!(
                error = %e,
                "camera reconciler: failed to subscribe to config.changed; camera hot-add is disabled"
            );
            return;
        }
    };
    info!("camera reconciler: subscribed to config.changed");

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(v) => {
                // Schema:
                //   {"kind":"camera","action":"upsert"|"delete","camera_id":<id>}
                // Older publishers may omit `kind` — be conservative
                // and only ignore when `kind` is explicitly non-camera.
                if let Some(k) = v.get("kind").and_then(|k| k.as_str()) {
                    if k != "camera" {
                        debug!(kind = %k, "camera reconciler: ignoring non-camera event");
                        continue;
                    }
                }
                if let Err(e) = reconcile(&args).await {
                    error!(error = %e, "camera reconciler: pass failed");
                }
            }
            Err(e) => {
                // Lagged subscribers are not fatal — we re-read the
                // DB on the next event and converge eventually.
                warn!(error = %e, "camera reconciler: bus stream error");
            }
        }
    }
    warn!("camera reconciler: bus stream closed; exiting");
}

/// One reconciliation pass. Compares `store.list_cameras()` to the
/// in-memory `handles` map and:
///   * aborts the supervisor + removes the ingester for any camera
///     that is missing from the DB or has `ingest.enabled = false`;
///   * spawns a fresh supervisor + ingester for any enabled camera
///     not yet in the map;
///   * restarts the supervisor + ingester for any enabled camera
///     whose ingest URL has changed.
async fn reconcile(args: &ReconcilerArgs) -> anyhow::Result<()> {
    let live: Vec<CameraConfig> = args.store.list_cameras().await?;

    // Snapshot current state under a short lock so the rest of the
    // pass can run without holding it. The clone is cheap — at most
    // a few dozen entries on real installs.
    let current: HashMap<CameraId, RunningCameraEntry> = args.handles.lock().clone();

    let live_enabled: HashSet<CameraId> = live
        .iter()
        .filter(|c| c.ingest.enabled)
        .map(|c| c.id)
        .collect();

    // 1. Remove anything that is gone-or-disabled.
    for id in current.keys().copied().collect::<Vec<_>>() {
        if !live_enabled.contains(&id) {
            stop_camera(args, id);
            // Reap the LBR pump here and *only* here. A camera that is gone
            // or disabled has no frames to pump, so its encode task would
            // poll a cache entry that will never be refilled. The restart
            // path below deliberately leaves the pump running: the cloud
            // `LiveHub` only re-sends `lbr_subscribe` on viewer or tier
            // changes, so killing it on a URL edit would blank the wall cell
            // until an operator happened to re-focus the tile. Across a
            // restart the pump reports `stalled` for the gap and clears
            // itself when frames resume.
            args.live_view.stop(id);
        }
    }

    // 2. Add or restart anything that is enabled in the DB.
    for cam in live.into_iter().filter(|c| c.ingest.enabled) {
        let cam_id = cam.id;
        let url = cam.ingest.url.to_string();
        let analysis_url_str = cam.ingest.analysis_url.as_ref().map(ToString::to_string);
        let det_w = cam
            .detector
            .model_override
            .as_ref()
            .map(|m| m.input_width)
            .unwrap_or(args.default_detector_width);
        // M_NATIVE_ASPECT — supervisor width may be decoupled from the
        // detector input (clamped up so it never drops below it).
        let sup_input = cam.behavior.supervisor_width.unwrap_or(det_w).max(det_w);
        let want_dims = nexus_pipeline::supervisor_frame_for(sup_input);
        match current.get(&cam_id) {
            Some(entry)
                if entry.url == url
                    && entry.supervisor_dims == want_dims
                    && entry.codec == cam.ingest.codec
                    && entry.analysis_url == analysis_url_str =>
            {
                // No change — supervisor still alive, URL still the
                // same, supervisor dims still match, ingest codec
                // still matches what's in the DB. Skip (we
                // deliberately do not respawn on unrelated config
                // edits today).
                continue;
            }
            Some(entry) => {
                if entry.url != url {
                    info!(
                        camera_id = cam_id,
                        "camera reconciler: ingest URL changed; restarting supervisor"
                    );
                } else if entry.supervisor_dims != want_dims {
                    info!(
                        camera_id = cam_id,
                        prev_w = entry.supervisor_dims.0,
                        prev_h = entry.supervisor_dims.1,
                        new_w = want_dims.0,
                        new_h = want_dims.1,
                        "camera reconciler: detector input size changed; restarting supervisor"
                    );
                } else if entry.codec != cam.ingest.codec {
                    info!(
                        camera_id = cam_id,
                        prev_codec = ?entry.codec,
                        new_codec = ?cam.ingest.codec,
                        "camera reconciler: ingest codec changed; restarting supervisor"
                    );
                } else {
                    info!(
                        camera_id = cam_id,
                        "camera reconciler: analysis stream changed; restarting supervisor"
                    );
                }
                stop_camera(args, cam_id);
            }
            None => {}
        }
        start_camera(args, cam, &url, want_dims).await;
    }

    Ok(())
}

fn stop_camera(args: &ReconcilerArgs, cam_id: CameraId) {
    let removed = args.handles.lock().remove(&cam_id);
    if let Some(entry) = removed {
        entry.task.abort();
        info!(camera_id = cam_id, "camera reconciler: aborted supervisor");
    }
    args.recorder.remove_camera_ingester(cam_id);
    // The substream session lives in a separate map and would otherwise
    // keep its RTSP connection and decode chain alive with no
    // subscriber — a deleted camera has to give its capacity back.
    let _ = args
        .recorder
        .set_camera_analysis_ingester(cam_id, None, 0, 0, 0, CodecKind::H264);
    // Drop the last decoded frame. Without this the admin frame API and the
    // Phase 10 LBR pump keep serving a stopped camera's final image forever —
    // the cloud wall renders it under a "LIVE" badge, and a camera that went
    // green just before it stalled stays green on the wall indefinitely.
    args.cache.clear(cam_id);
    // Reset per-camera frame stats so the next spawn starts from a
    // clean slate (no stale fps_ema or counters from the previous
    // session).
    args.frame_stats.clear(cam_id);
    args.decode_health.clear(cam_id);
    args.analysis_stream.clear(cam_id);
}

/// Attach or detach a camera's SPEC-069 analysis substream session.
///
/// Boot and hot-add MUST both call this. A camera whose analysis session
/// is registered only on hot-add reverts to main-stream decode on the
/// next engine restart, and [`reconcile`]'s no-change guard — which
/// compares the *configured* `analysis_url` against the entry seeded at
/// boot — then keeps it there with no log line and no self-heal.
pub(crate) async fn apply_analysis_session(
    recorder: &dyn ClipRecorder,
    cam: &CameraConfig,
    main_codec: CodecKind,
    supervisor_dims: (u32, u32),
) -> bool {
    let cam_id = cam.id;
    let (sup_w, sup_h) = supervisor_dims;
    let Some(analysis_url) = cam.ingest.analysis_url.as_ref() else {
        if let Err(e) = recorder.set_camera_analysis_ingester(
            cam_id,
            None,
            cam.ingest.max_fps,
            sup_w,
            sup_h,
            main_codec,
        ) {
            error!(camera_id = cam_id, error = %e, "analysis session teardown failed");
        }
        return false;
    };
    // The substream carries its own codec — an H.265 main stream with an
    // H.264 substream is the common case, so the main stream's codec is
    // only the fallback when the probe cannot answer.
    let a_codec = match analysis_url.scheme() {
        "rtsp" | "rtsps" => crate::discovery::rtsp_probe::probe_codec_for_url(analysis_url)
            .await
            .unwrap_or(main_codec),
        _ => main_codec,
    };
    match recorder.set_camera_analysis_ingester(
        cam_id,
        Some(analysis_url.as_str()),
        cam.ingest.max_fps,
        sup_w,
        sup_h,
        a_codec,
    ) {
        Ok(()) => true,
        Err(e) => {
            error!(
                camera_id = cam_id,
                error = %e,
                "analysis substream session failed to start; analysis stays on the main stream"
            );
            false
        }
    }
}

async fn start_camera(
    args: &ReconcilerArgs,
    cam: CameraConfig,
    url: &str,
    supervisor_dims: (u32, u32),
) {
    let cam_id = cam.id;
    let (sup_w, sup_h) = supervisor_dims;
    let configured_codec = cam.ingest.codec;
    // Pre-roll ingester first so the recorder is ready by the time
    // the supervisor opens its first motion clip. Failure is logged
    // but non-fatal: detection still runs; clip opens for this
    // camera return Refused until the next reconcile pass.
    let codec = match cam.ingest.codec {
        Some(c) => c,
        None => {
            // Same boot-time autodetect as build_gst_recorder so a
            // hot-added "auto" camera (operator left codec=None)
            // gets probed instead of silently defaulting to h264.
            let scheme = cam.ingest.url.scheme();
            let probed = if scheme == "rtsp" || scheme == "rtsps" {
                crate::discovery::rtsp_probe::probe_codec_for_url(&cam.ingest.url).await
            } else {
                None
            };
            match probed {
                Some(c) => {
                    info!(
                        camera_id = cam_id,
                        %url,
                        codec = %c,
                        "codec autodetected at hot-add"
                    );
                    c
                }
                None => {
                    warn!(
                        camera_id = cam_id,
                        %url,
                        "camera codec unspecified and autodetect probe failed; defaulting to h264 — set `ingest.codec` in the camera config to silence"
                    );
                    CodecKind::H264
                }
            }
        }
    };
    if let Err(e) = args.recorder.add_camera_ingester(
        cam_id,
        url,
        args.pre_roll_secs,
        cam.ingest.max_fps,
        sup_w,
        sup_h,
        codec,
    ) {
        error!(
            camera_id = cam_id,
            %url,
            error = %e,
            "camera reconciler: ingester hot-add failed; clips will be refused for this camera"
        );
    }

    // SPEC-069 — the analysis session, when the camera has one. Shared
    // with the boot path so the two can never disagree about whether a
    // converted camera actually got its second session. The entry records
    // what was REGISTERED, not what was configured: recording `Some` after
    // a failed registration would match reconcile()'s no-change guard and
    // strand the camera on the main stream with no retry.
    let registered =
        apply_analysis_session(args.recorder.as_ref(), &cam, codec, (sup_w, sup_h)).await;
    let cam_analysis_url = registered
        .then(|| cam.ingest.analysis_url.as_ref().map(ToString::to_string))
        .flatten();

    let detector = args.router.detector_for_camera(&cam);
    let detector_low_res = args.router.detector_for_camera_low_res(&cam);
    // M_TILE_REINFER (G1) Phase B2.1 — effective per-camera `top_k`.
    // Per-camera `model_override.top_k` wins over the global
    // `inference.model.top_k`; both being `None` means no post-merge
    // re-cap (cascade-disabled cameras don't reach the helper anyway).
    let effective_top_k = cam
        .detector
        .model_override
        .as_ref()
        .and_then(|m| m.top_k)
        .or(args.default_top_k);
    // Fresh per-camera tracker — see `ReconcilerArgs::tracker_cfg`
    // for why this CANNOT be shared across cameras.
    let tracker: Arc<dyn Tracker> = Arc::from(nexus_tracker::build_tracker(&args.tracker_cfg));
    // Phase 5.6 · R4 — hydrate this camera's seed from
    // `entity_local_state` so the freshly-spawned scheduler reuses
    // any prior `entity_local_id` that's still inside the GC
    // window. Cheap: indexed by `(camera_id, last_seen_at)`.
    // Failure is non-fatal — we just start cold.
    let seed_for_cam: Vec<nexus_pipeline::EntityLocalSeed> = match args
        .store
        .load_recent_entity_locals_for_camera(
            cam_id,
            chrono::Utc::now()
                - chrono::Duration::seconds(args.sighting_hydration_window_secs as i64),
        )
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| nexus_pipeline::EntityLocalSeed {
                camera_id: r.camera_id,
                track_id: r.track_id,
                entity_local_id: r.entity_local_id,
                started_ts: r.started_ts,
                last_seen_at: r.last_seen_at,
            })
            .collect(),
        Err(e) => {
            warn!(
                camera_id = cam_id,
                error = %e,
                "reconciler: entity_local_state hydration failed; scheduler will start cold"
            );
            Vec::new()
        }
    };
    let handle = spawn_camera(
        cam,
        detector,
        detector_low_res,
        tracker,
        args.annotator.clone(),
        args.static_object.clone(),
        args.clips.clone(),
        args.state_dir.clone(),
        args.evaluator.clone(),
        args.store.clone(),
        args.recorder.clone(),
        args.bus.clone(),
        args.cache.clone(),
        args.frame_stats.clone(),
        args.static_clear.clone(),
        sup_w,
        sup_h,
        args.sighting_hook.clone(),
        args.sighting_cfg,
        seed_for_cam,
        args.sighting_persist.clone(),
        effective_top_k,
        args.sink_router.clone(),
        args.alert_clip_schedule_gate.clone(),
    );
    args.handles.lock().insert(
        cam_id,
        RunningCameraEntry {
            task: Arc::new(handle.task),
            url: url.to_string(),
            supervisor_dims,
            codec: configured_codec,
            analysis_url: cam_analysis_url,
        },
    );
    info!(
        camera_id = cam_id,
        %url,
        sup_w,
        sup_h,
        "camera reconciler: spawned supervisor + ingester"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::{CameraBehavior, CameraDetector, CameraIngest, CameraOnvif, CameraTalkDown};
    use nexus_pipeline::{ClipFinal, ClipHandle, ClipMeta, OpenClip, RecorderError};
    use url::Url;

    /// Records every `set_camera_analysis_ingester` call so a test can assert
    /// what the caller actually asked the recorder to do.
    #[derive(Default)]
    struct RecordingRecorder {
        calls: Mutex<Vec<(CameraId, Option<String>, CodecKind)>>,
    }

    #[async_trait::async_trait]
    impl ClipRecorder for RecordingRecorder {
        async fn open(&self, _args: OpenClip) -> Result<ClipHandle, RecorderError> {
            Err(RecorderError::Refused)
        }
        async fn close(
            &self,
            _handle: ClipHandle,
            _args: ClipFinal,
        ) -> Result<ClipMeta, RecorderError> {
            Err(RecorderError::Refused)
        }
        fn set_panic(&self, _panic: bool) {}
        fn is_panic(&self) -> bool {
            false
        }
        fn kind(&self) -> &'static str {
            "recording"
        }
        fn set_camera_analysis_ingester(
            &self,
            camera_id: CameraId,
            analysis_url: Option<&str>,
            _max_fps: u32,
            _rgb_w: u32,
            _rgb_h: u32,
            codec: CodecKind,
        ) -> Result<(), RecorderError> {
            self.calls
                .lock()
                .push((camera_id, analysis_url.map(str::to_string), codec));
            Ok(())
        }
    }

    fn cam(analysis: Option<&str>) -> CameraConfig {
        CameraConfig {
            id: 7,
            name: "lot-1".into(),
            ingest: CameraIngest {
                url: Url::parse("rtsp://admin:secret@10.0.0.5:554/Streaming/Channels/101").unwrap(),
                analysis_url: analysis.map(|a| Url::parse(a).unwrap()),
                enabled: true,
                max_fps: 15,
                codec: Some(CodecKind::H265),
            },
            detector: CameraDetector {
                prompts: vec![],
                visual_prompts: vec![],
                model_override: None,
            },
            behavior: CameraBehavior::default(),
            onvif: CameraOnvif::default(),
            talk_down: CameraTalkDown::default(),
            zones: vec![],
        }
    }

    /// Boot builds only main ingesters, so before this fix the analysis
    /// session existed only after a hot-add — and the no-change guard in
    /// [`reconcile`] then compared the *configured* `analysis_url` against
    /// the boot-seeded entry, matched, and skipped forever. Both call sites
    /// now go through one helper; this pins its contract.
    #[tokio::test]
    async fn a_camera_with_an_analysis_url_registers_a_substream_session() {
        let rec = RecordingRecorder::default();
        // http:// keeps the codec probe off the network: the substream's own
        // codec is only probed for rtsp/rtsps.
        apply_analysis_session(
            &rec,
            &cam(Some("http://10.0.0.5/sub")),
            CodecKind::H265,
            (512, 288),
        )
        .await;

        let calls = rec.calls.lock().clone();
        assert_eq!(calls.len(), 1, "exactly one registration call");
        assert_eq!(
            calls[0],
            (7, Some("http://10.0.0.5/sub".to_string()), CodecKind::H265),
            "the analysis session must be registered for the camera's substream URL"
        );
    }

    /// The other half of invariant I5 — there is no third arrangement. A
    /// camera with no substream must actively tear any previous analysis
    /// session down, not merely be skipped, or a camera that had its
    /// `analysis_url` cleared keeps decoding the substream forever.
    #[tokio::test]
    async fn a_camera_without_an_analysis_url_tears_the_session_down() {
        let rec = RecordingRecorder::default();
        apply_analysis_session(&rec, &cam(None), CodecKind::H264, (512, 288)).await;

        let calls = rec.calls.lock().clone();
        assert_eq!(calls.len(), 1, "teardown is a call, not a skip");
        assert_eq!(
            calls[0].1, None,
            "a camera with no substream must clear any registered analysis session"
        );
    }

    /// The shared helper is only half the fix — boot has to call it.
    /// `build_gst_recorder` is `#[cfg(feature = "gstreamer")]` and builds
    /// real RTSP ingesters, so it cannot be unit-constructed; this pins the
    /// wiring at the source level instead, the same audit-test shape as
    /// `gst_clip_recorder::pipeline_string_is_codec_passthrough`.
    #[test]
    fn the_boot_path_registers_analysis_sessions() {
        assert!(
            include_str!("main.rs").contains("apply_analysis_session"),
            "build_gst_recorder must register SPEC-069 analysis sessions at boot. \
             Without it a converted camera reverts to main-stream decode on the \
             first restart, and reconcile()'s no-change guard — which compares the \
             configured analysis_url against the boot-seeded entry — keeps it there \
             forever with no log line and no self-heal."
        );
    }
}

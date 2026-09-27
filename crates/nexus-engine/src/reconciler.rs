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
//!   2. On each event, and every [`SUPERVISE_INTERVAL`] with no event,
//!      calls [`reconcile`], which re-reads `store.list_cameras()` and
//!      compares it against the shared `handles` map (seeded by `main`
//!      with the cameras it spawned at boot).
//!   3. Adds, removes, or restarts supervisors + ingesters to make
//!      the runtime match the DB.
//!
//! Restart triggers today: a supervisor that exited without being
//! stopped (nothing announces that, hence the periodic pass), ingest
//! URL change, supervisor (analysis) frame dimension change, ingest
//! codec change, and an `analysis_url` (SPEC-069) that differs from the
//! one the entry recorded: a changed or removed `analysis_url`, or a
//! registration that failed and is retried this way. Detector /
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
/// entry. Made only by [`EntryKey::spawned`].
#[derive(Clone)]
pub struct RunningCameraEntry {
    pub task: Arc<JoinHandle<()>>,
    /// What the supervisor was started for, which each reconcile pass
    /// compares with what the camera's row asks for now.
    key: EntryKey,
}

/// What [`reconcile`]'s no-change guard compares for a camera. Its fields
/// are private to this module, so every side derives each one here, one
/// way: the guard's side by `EntryKey::wanted`, boot's entries by
/// [`EntryKey::at_boot`], and `start_camera`'s from the guard's.
#[derive(Clone, PartialEq)]
pub(crate) struct EntryKey {
    /// Current ingest URL. Compared on each reconcile pass to
    /// decide whether a respawn is needed.
    url: String,
    /// Resolved supervisor (RGB analysis) frame `(width, height)` in
    /// effect for this camera. Compared on each reconcile pass so a
    /// UI-side change to `model_override.input_width` triggers a
    /// respawn (which rebuilds the GStreamer pipeline at the new
    /// caps) without needing a process restart. See
    /// [`supervisor_dims_for`].
    supervisor_dims: (u32, u32),
    /// Configured ingest codec as stored in the DB (`None` = "auto",
    /// resolved per spawn via `discovery::rtsp_probe`). Compared on
    /// each reconcile pass so a UI-side codec edit forces a respawn
    /// — the new value is loaded into the GStreamer pipeline by the
    /// fresh ingester rather than continuing to decode with the
    /// previous depayloader/decoder chain.
    codec: Option<CodecKind>,
    /// On an entry, the analysis substream URL (SPEC-069) the recorder
    /// accepted — what `apply_analysis_session` returned, at boot or in
    /// `start_camera` — or `None` when the camera has none or its
    /// registration failed or was never attempted, which the next pass
    /// retries by restarting the camera. Boot on the gstreamer recorder
    /// registers only the cameras whose main ingester built. On the
    /// guard's side, the configured URL. Compared on each reconcile pass
    /// — without it, applying a reprobe proposal writes the camera row,
    /// the reconciler decides nothing changed, and the second session
    /// is never started. The console would then show a camera set to
    /// analyse its substream that is doing no such thing.
    analysis_url: Option<String>,
}

impl EntryKey {
    /// The guard's side: what an entry for `cam` holds when nothing has
    /// changed. An entry matches its configured `analysis_url` only once
    /// the recorder accepted it.
    fn wanted(args: &ReconcilerArgs, cam: &CameraConfig) -> Self {
        Self {
            url: cam.ingest.url.to_string(),
            supervisor_dims: supervisor_dims_for(cam, args.default_detector_width),
            codec: cam.ingest.codec,
            analysis_url: cam.ingest.analysis_url.as_ref().map(ToString::to_string),
        }
    }

    /// A boot entry's key: the guard's, with the substream URL boot
    /// registered for `cam` in place of the configured one. The call drains
    /// that camera's entry from `boot_analysis`, so the key it returns is the
    /// only record of it. An extra call as a bare statement fails
    /// `clippy -D warnings` on this `#[must_use]`; `let _ =` still passes.
    #[must_use = "this drains the camera's boot entry, which only the returned key records"]
    pub(crate) fn at_boot(
        args: &ReconcilerArgs,
        cam: &CameraConfig,
        boot_analysis: &mut BootAnalysis,
    ) -> Self {
        Self {
            analysis_url: boot_analysis.0.remove(&cam.id),
            ..Self::wanted(args, cam)
        }
    }

    /// The frame the camera's supervisor is spawned at.
    pub(crate) fn supervisor_dims(&self) -> (u32, u32) {
        self.supervisor_dims
    }

    /// The entry for the supervisor spawned for this key.
    #[must_use = "an entry the handle map never holds is not supervised"]
    pub(crate) fn spawned(self, task: JoinHandle<()>) -> RunningCameraEntry {
        RunningCameraEntry {
            task: Arc::new(task),
            key: self,
        }
    }
}

/// The substream URL each boot entry records, by camera: what boot's one
/// registration pass returned. Only [`register_analysis_sessions`] makes
/// one and only [`EntryKey::at_boot`] reads it, so `main` cannot build the
/// map, and can change it only by draining entries through `at_boot`.
/// Deliberate forges are still possible: an empty registration pass; a pass
/// over a throwaway `StubClipRecorder`, which accepts every substream; and a
/// second `at_boot` call for a camera, or one made with a doctored copy of
/// it, which drains its entry so the real key records `None`.
#[must_use = "each boot entry's analysis_url comes from this map, through EntryKey::at_boot"]
pub(crate) struct BootAnalysis(HashMap<CameraId, String>);

/// The supervisor (RGB analysis) frame `(width, height)` a camera runs at:
/// its detector input width — the `model_override`'s, else
/// `default_detector_width` — raised to `behavior.supervisor_width` when
/// that is larger. The only copy of the rule: the no-change guard, boot's
/// entries and the boot RGB tap in `build_gst_recorder` all size from it,
/// and `camera_reprobe` ranks substreams against it.
pub(crate) fn supervisor_dims_for(cam: &CameraConfig, default_detector_width: u32) -> (u32, u32) {
    let det_w = cam
        .detector
        .model_override
        .as_ref()
        .map(|m| m.input_width)
        .unwrap_or(default_detector_width);
    // M_NATIVE_ASPECT — supervisor width may be decoupled from the
    // detector input (clamped up so it never drops below it).
    let sup_input = cam.behavior.supervisor_width.unwrap_or(det_w).max(det_w);
    nexus_pipeline::supervisor_frame_for(sup_input)
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
    tokio::spawn(async move { run(args, SUPERVISE_INTERVAL).await })
}

/// How often a pass runs with no `config.changed` event, so a supervisor
/// that exited is restarted: inside the 90 s camera-offline window, and the
/// same cap as `RtspSource`'s reconnect backoff.
const SUPERVISE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

async fn run(args: ReconcilerArgs, supervise_every: std::time::Duration) {
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

    // First periodic pass one period after start, not at once: `main` has
    // only just spawned every camera and seeded `handles`.
    let mut supervise = tokio::time::interval_at(
        tokio::time::Instant::now() + supervise_every,
        supervise_every,
    );
    // A slow pass pushes the next one back rather than queuing a burst.
    supervise.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            msg = stream.next() => {
                let Some(msg) = msg else { break };
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
                        // DB on the next event or periodic pass and converge.
                        warn!(error = %e, "camera reconciler: bus stream error");
                    }
                }
            }
            _ = supervise.tick() => {
                if let Err(e) = reconcile(&args).await {
                    error!(error = %e, "camera reconciler: periodic pass failed");
                }
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
///     whose ingest URL has changed, or whose supervisor has exited.
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
        let want = EntryKey::wanted(args, &cam);
        match current.get(&cam_id) {
            // `==` compares url, dims, codec, then analysis_url.
            Some(entry) if entry.key == want && !entry.task.is_finished() => {
                // No change — supervisor still alive, URL still the
                // same, supervisor dims still match, ingest codec
                // still matches what's in the DB. Skip (we
                // deliberately do not respawn on unrelated config
                // edits today).
                continue;
            }
            Some(entry) => {
                // A finished task here was not stopped by us: `stop_camera`
                // removes the entry before it aborts the task.
                if entry.task.is_finished() {
                    error!(
                        camera_id = cam_id,
                        "camera reconciler: supervisor exited without being stopped; restarting"
                    );
                } else if entry.key.url != want.url {
                    info!(
                        camera_id = cam_id,
                        "camera reconciler: ingest URL changed; restarting supervisor"
                    );
                } else if entry.key.supervisor_dims != want.supervisor_dims {
                    info!(
                        camera_id = cam_id,
                        prev_w = entry.key.supervisor_dims.0,
                        prev_h = entry.key.supervisor_dims.1,
                        new_w = want.supervisor_dims.0,
                        new_h = want.supervisor_dims.1,
                        "camera reconciler: detector input size changed; restarting supervisor"
                    );
                } else if entry.key.codec != want.codec {
                    info!(
                        camera_id = cam_id,
                        prev_codec = ?entry.key.codec,
                        new_codec = ?want.codec,
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
        start_camera(args, cam, want).await;
    }

    Ok(())
}

fn stop_camera(args: &ReconcilerArgs, cam_id: CameraId) {
    // Remove before abort: a finished task still in the map reads as a crash.
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

/// Attach or detach a camera's SPEC-069 analysis substream session, and
/// return what the camera's entry records: the configured URL once the
/// recorder accepted it, or `None` for a camera with no substream or a
/// registration that failed, which [`reconcile`]'s no-change guard then
/// retries.
///
/// Both writers of an entry's [`EntryKey`] `analysis_url` record this and
/// nothing else: `start_camera`, and boot through
/// [`register_analysis_sessions`]. Recording anything else makes the next
/// pass restart a healthy camera, or strands a failed registration with no
/// retry. Boot on the gstreamer recorder calls this only for the cameras
/// whose main ingester built; any other camera with a substream records
/// `None` with no attempt, which the guard retries like a failed
/// registration.
#[must_use = "this is what the entry's analysis_url records"]
pub(crate) async fn apply_analysis_session(
    recorder: &dyn ClipRecorder,
    cam: &CameraConfig,
    main_codec: CodecKind,
    supervisor_dims: (u32, u32),
) -> Option<String> {
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
        return None;
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
        Ok(()) => Some(analysis_url.to_string()),
        Err(e) => {
            error!(
                camera_id = cam_id,
                error = %e,
                "analysis substream session failed to start; analysis stays on the main stream"
            );
            None
        }
    }
}

/// Boot's registration pass, run once by every `build_recorder` branch. The
/// map holds the URL each boot entry records: the answer `start_camera`
/// records after a restart. A second call would re-probe each RTSP
/// substream and can rebuild a live session. The gstreamer branch passes
/// only the cameras whose main ingester built, so any other camera records
/// `None` with no attempt.
#[must_use = "this is what each boot entry's analysis_url records"]
pub(crate) async fn register_analysis_sessions(
    recorder: &dyn ClipRecorder,
    pending: Vec<(&CameraConfig, CodecKind, (u32, u32))>,
) -> BootAnalysis {
    let mut registered = HashMap::new();
    for (cam, codec, dims) in pending {
        if let Some(url) = apply_analysis_session(recorder, cam, codec, dims).await {
            registered.insert(cam.id, url);
        }
    }
    BootAnalysis(registered)
}

async fn start_camera(args: &ReconcilerArgs, cam: CameraConfig, want: EntryKey) {
    let cam_id = cam.id;
    let (sup_w, sup_h) = want.supervisor_dims;
    let url = want.url.clone();
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
        &url,
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
    let cam_analysis_url =
        apply_analysis_session(args.recorder.as_ref(), &cam, codec, (sup_w, sup_h)).await;

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
    // `..want` keeps the configured codec, never `codec` above: the guard
    // compares the DB's value, so recording the resolved one would restart
    // every auto-codec camera on every pass.
    let entry = EntryKey {
        analysis_url: cam_analysis_url,
        ..want
    }
    .spawned(handle.task);
    args.handles.lock().insert(cam_id, entry);
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
    use nexus_config::{
        CameraBehavior, CameraDetector, CameraIngest, CameraOnvif, CameraTalkDown, RecorderKind,
    };
    use nexus_pipeline::{ClipFinal, ClipHandle, ClipMeta, OpenClip, RecorderError};
    use std::sync::atomic::{AtomicUsize, Ordering};
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
        let recorded = apply_analysis_session(
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
        assert_eq!(
            recorded.as_deref(),
            Some(SUBSTREAM),
            "an accepted registration is recorded as the configured substream URL"
        );
    }

    /// The other half of invariant I5 — there is no third arrangement. A
    /// camera with no substream must actively tear any previous analysis
    /// session down, not merely be skipped, or a camera that had its
    /// `analysis_url` cleared keeps decoding the substream forever.
    #[tokio::test]
    async fn a_camera_without_an_analysis_url_tears_the_session_down() {
        let rec = RecordingRecorder::default();
        let recorded = apply_analysis_session(&rec, &cam(None), CodecKind::H264, (512, 288)).await;

        let calls = rec.calls.lock().clone();
        assert_eq!(calls.len(), 1, "teardown is a call, not a skip");
        assert_eq!(
            calls[0].1, None,
            "a camera with no substream must clear any registered analysis session"
        );
        assert_eq!(
            recorded, None,
            "a camera with no substream records no analysis URL"
        );
    }

    /// The one supervisor-frame rule: the detector input width (the
    /// camera's `model_override`, else the default), raised to
    /// `supervisor_width` when that is larger and never lowered by it
    /// (M_NATIVE_ASPECT).
    #[test]
    fn supervisor_dims_follow_the_detector_width_and_never_drop_below_it() {
        let shaped = |shape: &dyn Fn(&mut CameraConfig)| {
            let mut c = cam(None);
            shape(&mut c);
            c
        };
        let model_width = |c: &mut CameraConfig, input_width: u32| {
            c.detector.model_override = Some(nexus_config::ModelConfig {
                kind: "mock".into(),
                input_width,
                ..Default::default()
            })
        };
        for (what, cam, default_width, want) in [
            ("the default width", cam(None), 512, (512, 288)),
            ("another default width", cam(None), 640, (640, 360)),
            (
                "a model override, over the default",
                shaped(&|c| model_width(c, 640)),
                512,
                (640, 360),
            ),
            (
                "a supervisor_width below the detector input",
                shaped(&|c| c.behavior.supervisor_width = Some(256)),
                512,
                (512, 288),
            ),
            (
                "a supervisor_width above the detector input",
                shaped(&|c| c.behavior.supervisor_width = Some(1024)),
                512,
                (1024, 576),
            ),
        ] {
            assert_eq!(supervisor_dims_for(&cam, default_width), want, "{what}");
        }
    }

    /// The shared helper is only half the fix — boot has to call it and
    /// record what it returned. Recording is the compiler's job: `main` can
    /// build a boot entry only through [`EntryKey::at_boot`], which reads a
    /// [`BootAnalysis`] (a map only `register_analysis_sessions` makes), and
    /// it cannot touch an entry's fields. Registering is not. The
    /// real `build_gst_recorder` compiles only with the `gstreamer` feature.
    /// The label-gated `system-libs` CI job clippies that arm with
    /// `-D warnings`, but CI runs none of this crate's tests with the
    /// feature, so a test of the arm would never run there. This pins the
    /// arm's wiring at the source level instead, the same audit-test shape as
    /// `gst_clip_recorder::pipeline_string_is_codec_passthrough`. Each needle
    /// must occur exactly once, so it names one line: the gstreamer arm's
    /// registration, and that arm's return of what it registered. The stub
    /// arms are driven for real by
    /// `the_stub_boot_path_registers_every_enabled_substream`.
    #[test]
    fn the_boot_path_registers_analysis_sessions() {
        let main_rs = include_str!("main.rs");
        for (needle, consequence) in [
            (
                "register_analysis_sessions(&rec, analysis_pending)",
                "build_gst_recorder must register the SPEC-069 analysis sessions at boot: \
                 without it every converted camera boots with no substream, and the first \
                 reconcile pass restarts it, main ingester included.",
            ),
            (
                "Ok((Arc::new(rec), webrtc, analysis))",
                "build_gst_recorder must return what it registered: an empty map boots every \
                 converted camera with no substream recorded, and the first reconcile pass \
                 restarts it, main ingester included.",
            ),
        ] {
            assert_eq!(
                main_rs.matches(needle).count(),
                1,
                "`{needle}` must occur exactly once in main.rs. {consequence}"
            );
        }
    }

    /// What a camera's frame source does each time a supervisor builds one.
    #[derive(Clone, Copy)]
    enum SourceScript {
        /// The first source delivers frames for a moment and then returns;
        /// every later one keeps running.
        EndsOnce,
        /// Every source returns at once, the way a failure that retrying
        /// cannot fix would.
        AlwaysEnds,
        /// Every source keeps running.
        NeverEnds,
    }

    /// Delivers frames for `after` and then returns, the way a `FrameSource`
    /// whose `run` gives up would. A returning source is the only way
    /// `run_camera` finishes without being aborted: its analysis loop ends
    /// when the source's sender drops.
    struct SourceThatEnds {
        camera_id: CameraId,
        after: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl nexus_pipeline::FrameSource for SourceThatEnds {
        async fn run(
            self: Box<Self>,
            tx: tokio::sync::mpsc::Sender<nexus_types::Frame>,
        ) -> Result<(), nexus_pipeline::FrameSourceError> {
            let frames = Box::new(nexus_pipeline::VirtualSource {
                camera_id: self.camera_id,
                width: 512,
                height: 288,
                fps: 10,
            });
            let _ = tokio::time::timeout(self.after, frames.run(tx)).await;
            Err(nexus_pipeline::FrameSourceError::Backend(
                "upstream session ended".into(),
            ))
        }
    }

    /// Hands each camera's supervisors the sources its [`SourceScript`]
    /// names, and counts what the reconciler asked of it per camera.
    struct ScriptedRecorder {
        scripts: HashMap<CameraId, SourceScript>,
        sources_built: Mutex<HashMap<CameraId, usize>>,
        ingesters_removed: Mutex<HashMap<CameraId, usize>>,
    }

    impl ScriptedRecorder {
        fn new(scripts: &[(CameraId, SourceScript)]) -> Arc<Self> {
            Arc::new(Self {
                scripts: scripts.iter().copied().collect(),
                sources_built: Mutex::new(HashMap::new()),
                ingesters_removed: Mutex::new(HashMap::new()),
            })
        }

        fn sources_built(&self, camera_id: CameraId) -> usize {
            self.sources_built
                .lock()
                .get(&camera_id)
                .copied()
                .unwrap_or(0)
        }

        fn ingesters_removed(&self, camera_id: CameraId) -> usize {
            self.ingesters_removed
                .lock()
                .get(&camera_id)
                .copied()
                .unwrap_or(0)
        }
    }

    #[async_trait::async_trait]
    impl ClipRecorder for ScriptedRecorder {
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
            "scripted"
        }
        fn remove_camera_ingester(&self, camera_id: CameraId) {
            *self.ingesters_removed.lock().entry(camera_id).or_insert(0) += 1;
        }
        fn shared_frame_source(
            &self,
            camera_id: CameraId,
        ) -> Option<Box<dyn nexus_pipeline::FrameSource + Send>> {
            let built = {
                let mut counts = self.sources_built.lock();
                let n = counts.entry(camera_id).or_insert(0);
                *n += 1;
                *n
            };
            let ends_after = match self.scripts[&camera_id] {
                SourceScript::EndsOnce if built == 1 => Some(std::time::Duration::from_millis(300)),
                SourceScript::AlwaysEnds => Some(std::time::Duration::ZERO),
                SourceScript::EndsOnce | SourceScript::NeverEnds => None,
            };
            Some(match ends_after {
                Some(after) => Box::new(SourceThatEnds { camera_id, after }),
                None => Box::new(nexus_pipeline::VirtualSource {
                    camera_id,
                    width: 512,
                    height: 288,
                    fps: 10,
                }),
            })
        }
    }

    /// `cam(None)` under another id, for tests that need two cameras.
    fn cam_with_id(id: CameraId) -> CameraConfig {
        CameraConfig {
            id,
            name: format!("lot-{id}"),
            ..cam(None)
        }
    }

    async fn reconciler_args(
        recorder: Arc<dyn ClipRecorder>,
        dir: &std::path::Path,
        cameras: &[CameraConfig],
    ) -> ReconcilerArgs {
        let store = Arc::new(
            Store::open(&nexus_config::StoreConfig {
                url: format!("sqlite://{}?mode=rwc", dir.join("nexus.db").display()),
                ..nexus_config::StoreConfig::default()
            })
            .await
            .expect("open store"),
        );
        for camera in cameras {
            store.upsert_camera(camera).await.expect("seed camera row");
        }
        let inference = nexus_config::InferenceConfig {
            backend: nexus_config::InferenceBackendKind::InProcess,
            model: nexus_config::ModelConfig {
                kind: "mock".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let router = Arc::new(InferenceRouter::build(&inference, cameras).expect("router"));
        let tracker_cfg = TrackerConfig::default();
        let cache = Arc::new(LatestFrameCache::new());
        ReconcilerArgs {
            router,
            annotator: tracker_cfg.annotator.clone(),
            static_object: tracker_cfg.static_object.clone(),
            tracker_cfg,
            clips: ClipsConfig::default(),
            state_dir: dir.to_path_buf(),
            evaluator: Arc::new(
                RuleEvaluator::new(&nexus_config::RulesConfig::default(), &[]).expect("rules"),
            ),
            store,
            recorder,
            bus: Arc::new(nexus_bus::BroadcastBus::new(64)),
            cache: cache.clone(),
            frame_stats: Arc::new(FrameStatsRegistry::new()),
            decode_health: Arc::new(DecodeHealthRegistry::new()),
            analysis_stream: Arc::new(nexus_pipeline::AnalysisStreamRegistry::new()),
            static_clear: StaticAnchorClearRegistry::new(),
            pre_roll_secs: 0,
            default_detector_width: 512,
            default_top_k: None,
            sighting_hook: Arc::new(nexus_pipeline::NoopSightingHook),
            sighting_cfg: nexus_pipeline::supervisor::SightingSchedulerConfig::default(),
            sighting_persist: Arc::new(nexus_pipeline::NoopEntityLocalPersist),
            sighting_hydration_window_secs: 0,
            sink_router: Arc::new(nexus_pipeline::NoopSinkRouter),
            alert_clip_schedule_gate: Arc::new(nexus_pipeline::NoopAlertClipScheduleGate),
            handles: Arc::new(Mutex::new(HashMap::new())),
            live_view: crate::live_view::LiveViewManager::new(
                cache,
                Arc::new(nexus_cloud_client::TunnelOutbox::new()),
            ),
        }
    }

    /// Poll `done` until it holds, failing with `what` after 10 s. The
    /// deadline only bounds a broken run; a passing one returns as soon as
    /// the condition is met.
    async fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    fn supervisor_ended(handles: &HandleMap, camera_id: CameraId) -> bool {
        handles
            .lock()
            .get(&camera_id)
            .is_some_and(|e| e.task.is_finished())
    }

    fn abort_all(handles: &HandleMap) {
        for (_, entry) in handles.lock().drain() {
            entry.task.abort();
        }
    }

    /// `spawn_camera` documents that the engine owns restart policy for a
    /// supervisor that exits. The reconciler is the engine's only owner of
    /// camera lifecycle, and its no-change guard used to treat "an entry
    /// with the same config" as "a live supervisor" without ever looking at
    /// the stored `JoinHandle` — so a camera whose supervisor ended on its
    /// own stayed in the map, dead, through every later pass.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_supervisor_that_ends_on_its_own_is_brought_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::EndsOnce)]);
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;

        reconcile(&args).await.expect("first pass");
        wait_until(
            "precondition: the supervisor should end once its frame source returns",
            || supervisor_ended(&args.handles, 7),
        )
        .await;

        // The pass every trigger runs — a config.changed for any camera.
        reconcile(&args).await.expect("second pass");

        if supervisor_ended(&args.handles, 7) {
            abort_all(&args.handles);
            panic!(
                "camera 7's supervisor ended without being asked to, and the next reconcile \
                 pass left the dead handle in place: the camera analyses and records nothing \
                 until its config changes or the engine restarts"
            );
        }
        // Frames from the brought-back pipeline, not the dead one: a restart
        // clears the camera's frame stats, so any frame counted now came from
        // a fresh source.
        wait_until(
            "the brought-back supervisor never delivered a frame",
            || {
                args.frame_stats
                    .snapshot(7)
                    .is_some_and(|s| s.frames_emitted > 0)
            },
        )
        .await;
        let still_alive = !supervisor_ended(&args.handles, 7);
        abort_all(&args.handles);
        assert!(still_alive, "the brought-back supervisor must keep running");
        assert_eq!(
            recorder.sources_built(7),
            2,
            "a brought-back supervisor builds exactly one fresh frame source"
        );
    }

    /// Nothing publishes `config.changed` when a supervisor exits, so a
    /// reconciler that only wakes on that event never runs the pass that
    /// would restart it. The periodic pass is what brings the camera back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_exited_supervisor_is_restarted_without_any_config_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::EndsOnce)]);
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;
        let handles = args.handles.clone();

        // Stands in for the boot-time spawn in `main`, which seeds `handles`
        // before the reconciler task starts.
        reconcile(&args).await.expect("initial pass");
        wait_until(
            "precondition: the supervisor should end once its frame source returns",
            || supervisor_ended(&handles, 7),
        )
        .await;

        // No config.changed is ever published from here on.
        let reconciler = tokio::spawn(run(args, std::time::Duration::from_millis(50)));
        wait_until(
            "camera 7's supervisor exited and the reconciler never restarted it \
             without a config.changed event",
            || recorder.sources_built(7) == 2 && !supervisor_ended(&handles, 7),
        )
        .await;
        reconciler.abort();
        abort_all(&handles);
    }

    /// The periodic pass must be a no-op for a camera that is fine: same
    /// task, no fresh source, no ingester teardown — or every healthy camera
    /// would drop its RTSP session every 30 s. The neighbour whose source
    /// fails every time is restarted on each pass, which is what proves the
    /// passes ran.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_healthy_camera_is_left_alone_across_periodic_passes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder =
            ScriptedRecorder::new(&[(7, SourceScript::NeverEnds), (8, SourceScript::AlwaysEnds)]);
        let args =
            reconciler_args(recorder.clone(), dir.path(), &[cam(None), cam_with_id(8)]).await;
        let handles = args.handles.clone();

        reconcile(&args).await.expect("initial pass");
        let original = handles.lock().get(&7).cloned().expect("camera 7 spawned");

        let reconciler = tokio::spawn(run(args, std::time::Duration::from_millis(50)));
        wait_until("three periodic passes restarting camera 8", || {
            recorder.sources_built(8) >= 4
        })
        .await;
        reconciler.abort();

        let now = handles
            .lock()
            .get(&7)
            .cloned()
            .expect("camera 7 still running");
        let same_task = Arc::ptr_eq(&now.task, &original.task);
        let alive = !now.task.is_finished();
        abort_all(&handles);
        assert!(
            same_task,
            "a periodic pass replaced a healthy camera's supervisor"
        );
        assert!(
            alive,
            "the healthy camera's supervisor must still be running"
        );
        assert_eq!(
            recorder.sources_built(7),
            1,
            "a healthy camera's frame source was rebuilt"
        );
        assert_eq!(
            recorder.ingesters_removed(7),
            0,
            "a periodic pass tore down a healthy camera's ingester"
        );
    }

    /// An `http://` substream keeps the codec probe off the network:
    /// `apply_analysis_session` only probes `rtsp` / `rtsps`.
    const SUBSTREAM: &str = "http://10.0.0.5/sub";

    /// What `build_gst_recorder` hands [`register_analysis_sessions`]: each
    /// enabled camera with a substream, at the codec and dims its main
    /// ingester was built with. The test cameras pin their codec, so boot
    /// would not probe.
    fn pending_like_build_gst_recorder<'a>(
        args: &ReconcilerArgs,
        cams: &'a [CameraConfig],
    ) -> Vec<(&'a CameraConfig, CodecKind, (u32, u32))> {
        cams.iter()
            .filter(|c| c.ingest.enabled && c.ingest.analysis_url.is_some())
            .map(|c| {
                let codec = c.ingest.codec.expect("test cameras pin their codec");
                (
                    c,
                    codec,
                    supervisor_dims_for(c, args.default_detector_width),
                )
            })
            .collect()
    }

    /// Seeds `handles` the way `main`'s boot loop does (`for cam in cameras`
    /// … `running.lock().insert`): each enabled camera's entry is the real
    /// [`EntryKey::at_boot`], and its supervisor is spawned at that key's
    /// dims. Only the `spawn_camera` call is a copy of `main`'s. It registers
    /// nothing: `build_recorder` did that before the loop.
    async fn seed_like_boot(args: &ReconcilerArgs, mut boot_analysis: BootAnalysis) {
        for cam in args.store.list_cameras().await.expect("list cameras") {
            if !cam.ingest.enabled {
                continue;
            }
            let cam_id = cam.id;
            let key = EntryKey::at_boot(args, &cam, &mut boot_analysis);
            let (sup_w, sup_h) = key.supervisor_dims();
            let detector = args.router.detector_for_camera(&cam);
            let detector_low_res = args.router.detector_for_camera_low_res(&cam);
            let tracker: Arc<dyn Tracker> =
                Arc::from(nexus_tracker::build_tracker(&args.tracker_cfg));
            let effective_top_k = cam
                .detector
                .model_override
                .as_ref()
                .and_then(|m| m.top_k)
                .or(args.default_top_k);
            let h = spawn_camera(
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
                Vec::new(),
                args.sighting_persist.clone(),
                effective_top_k,
                args.sink_router.clone(),
                args.alert_clip_schedule_gate.clone(),
            );
            args.handles.lock().insert(cam_id, key.spawned(h.task));
        }
    }

    /// Runs `passes` reconcile passes — the pass each 30 s tick runs — and
    /// reports, per pass, whether camera 7's supervisor was replaced. Every
    /// supervisor must still be running when a pass starts, so a replacement
    /// is never the restart of one that exited on its own, which would read
    /// as, or hide, the restart under test. After a replacement it waits for
    /// the new supervisor to build its frame source, so the next pass sees a
    /// running camera and the counts have settled.
    async fn restarts_per_pass(
        args: &ReconcilerArgs,
        passes: usize,
        sources_built: impl Fn() -> usize,
    ) -> Vec<bool> {
        let task_of_7 = || {
            args.handles
                .lock()
                .get(&7)
                .map(|e| e.task.clone())
                .expect("camera 7 running")
        };
        let mut restarted = Vec::with_capacity(passes);
        for pass in 1..=passes {
            for (id, entry) in args.handles.lock().iter() {
                assert!(
                    !entry.task.is_finished(),
                    "camera {id}'s supervisor exited before pass {pass}: that pass would \
                     restart it for exiting, not for the change under test"
                );
            }
            let before = task_of_7();
            let built_before = sources_built();
            reconcile(args).await.expect("reconcile pass");
            let replaced = !Arc::ptr_eq(&before, &task_of_7());
            if replaced {
                wait_until(
                    "the restarted supervisor never built its frame source",
                    || sources_built() > built_before,
                )
                .await;
            }
            restarted.push(replaced);
        }
        restarted
    }

    /// `GstClipRecorder`'s substream registration without GStreamer:
    /// `set_camera_analysis_ingester(Some)` counts a try and returns `Err`
    /// for the first `refuse_first` tries, then `Ok`; `None` returns `Ok`.
    /// Counts what each restart asks of the main ingester and the frame
    /// source. The tests using it have one camera, so counts are not keyed.
    struct SubstreamRecorder {
        refuse_first: AtomicUsize,
        registrations_tried: AtomicUsize,
        ingesters_removed: AtomicUsize,
        sources_built: AtomicUsize,
    }

    impl SubstreamRecorder {
        fn new(refuse_first: usize) -> Arc<Self> {
            Arc::new(Self {
                refuse_first: refuse_first.into(),
                registrations_tried: Default::default(),
                ingesters_removed: Default::default(),
                sources_built: Default::default(),
            })
        }

        fn registrations_tried(&self) -> usize {
            self.registrations_tried.load(Ordering::SeqCst)
        }
        fn ingesters_removed(&self) -> usize {
            self.ingesters_removed.load(Ordering::SeqCst)
        }
        fn sources_built(&self) -> usize {
            self.sources_built.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ClipRecorder for SubstreamRecorder {
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
            "substream"
        }
        fn remove_camera_ingester(&self, _camera_id: CameraId) {
            self.ingesters_removed.fetch_add(1, Ordering::SeqCst);
        }
        fn set_camera_analysis_ingester(
            &self,
            _camera_id: CameraId,
            analysis_url: Option<&str>,
            _max_fps: u32,
            _rgb_w: u32,
            _rgb_h: u32,
            _codec: CodecKind,
        ) -> Result<(), RecorderError> {
            if analysis_url.is_none() {
                return Ok(());
            }
            self.registrations_tried.fetch_add(1, Ordering::SeqCst);
            let refused = self
                .refuse_first
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            if refused {
                return Err(RecorderError::Io(std::io::Error::other(
                    "analysis ingester: scripted refusal",
                )));
            }
            Ok(())
        }
        fn shared_frame_source(
            &self,
            camera_id: CameraId,
        ) -> Option<Box<dyn nexus_pipeline::FrameSource + Send>> {
            self.sources_built.fetch_add(1, Ordering::SeqCst);
            Some(Box::new(nexus_pipeline::VirtualSource {
                camera_id,
                width: 512,
                height: 288,
                fps: 10,
            }))
        }
    }

    /// Boot and `start_camera` must record one answer for a camera's SPEC-069
    /// substream: what registering it returned. Boot used to ask
    /// `has_analysis_ingester` instead, whose default (`false`) contradicted
    /// the setter's default (`Ok`). With a recorder that kept both defaults
    /// — the stub — boot recorded `None` for a camera configured with a
    /// substream, and the first periodic pass read the difference as a change
    /// and restarted a healthy camera's supervisor and frame source (the stub
    /// has no main ingester to lose). Camera 8 has no substream and must be
    /// left alone too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_boot_seeded_camera_with_a_substream_is_left_alone_by_periodic_passes() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Overrides no analysis method, so it answers as the stub does.
        let recorder =
            ScriptedRecorder::new(&[(7, SourceScript::NeverEnds), (8, SourceScript::NeverEnds)]);
        let cams = [cam(Some(SUBSTREAM)), cam_with_id(8)];
        let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;
        // The stub's boot registration, the one `build_recorder` runs.
        let boot_analysis =
            crate::register_stub_analysis_sessions(args.recorder.as_ref(), &cams).await;
        seed_like_boot(&args, boot_analysis).await;
        wait_until(
            "precondition: both boot supervisors should build their frame source",
            || recorder.sources_built(7) == 1 && recorder.sources_built(8) == 1,
        )
        .await;
        let booted_8 = args
            .handles
            .lock()
            .get(&8)
            .map(|e| e.task.clone())
            .expect("camera 8 seeded");

        let restarts = restarts_per_pass(&args, 3, || recorder.sources_built(7)).await;

        let entry_7 = args
            .handles
            .lock()
            .get(&7)
            .cloned()
            .expect("camera 7 running");
        let same_8 = args
            .handles
            .lock()
            .get(&8)
            .is_some_and(|e| Arc::ptr_eq(&e.task, &booted_8));
        abort_all(&args.handles);
        assert_eq!(
            restarts,
            vec![false, false, false],
            "a periodic pass restarted camera 7, whose config never changed: boot recorded \
             a different analysis_url from the one start_camera records"
        );
        assert!(same_8, "a periodic pass replaced camera 8's supervisor");
        for id in [7, 8] {
            assert_eq!(
                recorder.ingesters_removed(id),
                0,
                "a periodic pass tore down camera {id}'s main ingester"
            );
            assert_eq!(
                recorder.sources_built(id),
                1,
                "camera {id}'s frame source was rebuilt"
            );
        }
        assert_eq!(
            entry_7.key.analysis_url.as_deref(),
            Some(SUBSTREAM),
            "boot must record the substream the recorder accepted"
        );
    }

    /// `main`'s `build_recorder`, with inert values for everything but the
    /// recorder kind and the cameras. Returns the recorder and the analysis
    /// URL each boot entry records.
    async fn build_recorder_like_main(
        kind: &RecorderKind,
        dir: &std::path::Path,
        cameras: &[CameraConfig],
    ) -> (Arc<dyn ClipRecorder>, BootAnalysis) {
        let store = Arc::new(
            Store::open(&nexus_config::StoreConfig {
                url: format!("sqlite://{}?mode=rwc", dir.join("recorder.db").display()),
                ..nexus_config::StoreConfig::default()
            })
            .await
            .expect("recorder store"),
        );
        let (recorder, _webrtc, boot_analysis) = crate::build_recorder(
            kind,
            store,
            &dir.join("clips"),
            cameras,
            512,
            0,
            nexus_config::DecodeMode::default(),
            Arc::new(nexus_bus::BroadcastBus::new(64)),
            Arc::new(crate::usb_watch::UsbRegistry::new()),
            nexus_pipeline::recorder::PreferredUsbLabel::default(),
            nexus_config::AlertClipsConfig::default(),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            Arc::new(DecodeHealthRegistry::new()),
            Arc::new(nexus_pipeline::AnalysisStreamRegistry::new()),
        )
        .await
        .expect("build_recorder");
        (recorder, boot_analysis)
    }

    /// The stub arms of `build_recorder` run the same registration pass as
    /// `build_gst_recorder`: each enabled camera with a substream, once, and
    /// they return the URL each boot entry records. Camera 9 is disabled and
    /// has a substream, so the map must leave it out. Boot seeded from that
    /// recorder and that map must then be a no-op on every pass.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_stub_boot_path_registers_every_enabled_substream() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The stub offers no shared frame source, so each supervisor builds
        // its own from the main URL: `virtual://` gives one that never ends on
        // any build, where `rtsp://` without gstreamer ends at once.
        let with = |id: CameraId, analysis: Option<&str>, enabled: bool| {
            let mut c = cam_with_id(id);
            c.ingest.url = Url::parse(&format!("virtual://lot-{id}")).unwrap();
            c.ingest.analysis_url = analysis.map(|a| Url::parse(a).unwrap());
            c.ingest.enabled = enabled;
            c
        };
        let cams = [
            with(7, Some(SUBSTREAM), true),
            with(8, None, true),
            with(9, Some(SUBSTREAM), false),
        ];
        let want = HashMap::from([(7, SUBSTREAM.to_string())]);

        let (recorder, registered) =
            build_recorder_like_main(&RecorderKind::Stub, dir.path(), &cams).await;
        assert_eq!(
            registered.0, want,
            "the stub arm must register the substream of every enabled camera, and only those"
        );

        // Without the feature, `RecorderKind::Gstreamer` falls back to the stub.
        #[cfg(not(feature = "gstreamer"))]
        {
            let fallback_dir = tempfile::tempdir().expect("tempdir");
            let (_, registered) =
                build_recorder_like_main(&RecorderKind::Gstreamer, fallback_dir.path(), &cams)
                    .await;
            assert_eq!(
                registered.0, want,
                "the non-gstreamer fallback must register the way the stub arm does"
            );
        }

        let args = reconciler_args(recorder, dir.path(), &cams).await;
        seed_like_boot(&args, registered).await;
        let task_of = |id: CameraId| {
            args.handles
                .lock()
                .get(&id)
                .map(|e| e.task.clone())
                .expect("camera running")
        };
        let booted = [(7, task_of(7)), (8, task_of(8))];
        for pass in 1..=3 {
            for (id, task) in &booted {
                assert!(
                    !task.is_finished(),
                    "camera {id}'s supervisor exited before pass {pass}"
                );
            }
            reconcile(&args).await.expect("reconcile pass");
            for (id, task) in &booted {
                assert!(
                    Arc::ptr_eq(&task_of(*id), task),
                    "pass {pass} restarted camera {id}, whose config never changed"
                );
            }
        }
        abort_all(&args.handles);
    }

    /// `GstClipRecorder`'s side of the rule, without GStreamer: boot's one
    /// registration pass is the only registration, and what it returned is
    /// what boot records, so no pass registers the substream again or
    /// restarts the camera.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_substream_registered_at_boot_is_not_registered_again_or_restarted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = SubstreamRecorder::new(0);
        let cams = [cam(Some(SUBSTREAM))];
        let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;
        let boot_analysis = register_analysis_sessions(
            args.recorder.as_ref(),
            pending_like_build_gst_recorder(&args, &cams),
        )
        .await;
        seed_like_boot(&args, boot_analysis).await;
        wait_until(
            "precondition: the boot supervisor should build its frame source",
            || recorder.sources_built() == 1,
        )
        .await;
        let booted = args
            .handles
            .lock()
            .get(&7)
            .map(|e| e.task.clone())
            .expect("camera 7 seeded");

        let restarts = restarts_per_pass(&args, 3, || recorder.sources_built()).await;

        let same_task = args
            .handles
            .lock()
            .get(&7)
            .is_some_and(|e| Arc::ptr_eq(&e.task, &booted));
        abort_all(&args.handles);
        assert_eq!(
            restarts,
            vec![false, false, false],
            "a periodic pass restarted a camera whose substream registered at boot"
        );
        assert!(same_task, "camera 7's boot supervisor was replaced");
        assert_eq!(
            recorder.ingesters_removed(),
            0,
            "a periodic pass tore down the camera's main ingester"
        );
        assert_eq!(
            recorder.sources_built(),
            1,
            "the camera's frame source was rebuilt"
        );
        assert_eq!(
            recorder.registrations_tried(),
            1,
            "boot registers the substream once, and no pass registers it again"
        );
    }

    /// The rule's other half, at both writers: a registration that failed is
    /// recorded as `None`, never as the configured URL, so the next pass sees
    /// the difference and retries it — by restarting the whole camera, main
    /// ingester included, the only retry the reconciler has. The recorder
    /// refuses twice: boot records `None` for its refused try, and
    /// `start_camera` records `None` for the first pass's refused retry, so
    /// the second pass retries again and its try is accepted. Recording the
    /// configured URL at either writer would stop the retries and strand the
    /// camera on its main stream.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_substream_registration_that_failed_at_boot_is_retried_by_the_next_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = SubstreamRecorder::new(2);
        let cams = [cam(Some(SUBSTREAM))];
        let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;
        let boot_analysis = register_analysis_sessions(
            args.recorder.as_ref(),
            pending_like_build_gst_recorder(&args, &cams),
        )
        .await;
        seed_like_boot(&args, boot_analysis).await;
        wait_until(
            "precondition: the boot supervisor should build its frame source",
            || recorder.sources_built() == 1,
        )
        .await;

        let restarts = restarts_per_pass(&args, 3, || recorder.sources_built()).await;

        let entry_url = args
            .handles
            .lock()
            .get(&7)
            .and_then(|e| e.key.analysis_url.clone());
        abort_all(&args.handles);
        assert_eq!(
            restarts,
            vec![true, true, false],
            "passes 1 and 2 must each retry a refused registration and pass 3 must leave the \
             accepted one alone: a writer that records the configured URL for a refused try \
             stops the retries and strands the camera on its main stream"
        );
        assert_eq!(
            recorder.registrations_tried(),
            3,
            "boot's refused try, the first pass's refused retry, then the second pass's \
             accepted one"
        );
        assert_eq!(
            recorder.ingesters_removed(),
            2,
            "each retry is a whole-camera restart: one main-ingester teardown per retry"
        );
        assert_eq!(
            recorder.sources_built(),
            3,
            "boot's frame source, then one per restart"
        );
        assert_eq!(
            entry_url.as_deref(),
            Some(SUBSTREAM),
            "the accepted retry's registration must be recorded"
        );
    }

    /// Every field the no-change guard compares, for camera shapes no other
    /// test builds: an auto codec, a `supervisor_width` below and above the
    /// detector input, and a `model_override`. Each camera is seeded by
    /// boot's [`EntryKey::at_boot`], then started by a pass through
    /// `start_camera`, and after each the next two passes must leave it
    /// alone. The entry must record the configured codec (`None`), never the
    /// one the start resolved, and the supervisor frame clamped up to the
    /// detector input.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_camera_shape_is_left_alone_after_boot_and_after_a_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shaped = |id: CameraId, shape: &dyn Fn(&mut CameraConfig)| {
            let mut c = cam_with_id(id);
            // Not `rtsp://`: an auto codec would probe the network.
            c.ingest.url = Url::parse(&format!("virtual://lot-{id}")).unwrap();
            shape(&mut c);
            c
        };
        let cams = [
            shaped(7, &|c| c.ingest.codec = None),
            shaped(8, &|c| c.behavior.supervisor_width = Some(256)),
            shaped(9, &|c| c.behavior.supervisor_width = Some(1024)),
            shaped(10, &|c| {
                c.detector.model_override = Some(nexus_config::ModelConfig {
                    kind: "mock".into(),
                    input_width: 640,
                    ..Default::default()
                })
            }),
        ];
        // (id, configured codec, supervisor frame) each entry must record.
        let want = [
            (7, None, (512, 288)),
            (8, Some(CodecKind::H265), (512, 288)),
            (9, Some(CodecKind::H265), (1024, 576)),
            (10, Some(CodecKind::H265), (640, 360)),
        ];
        let recorder = ScriptedRecorder::new(&[
            (7, SourceScript::NeverEnds),
            (8, SourceScript::NeverEnds),
            (9, SourceScript::NeverEnds),
            (10, SourceScript::NeverEnds),
        ]);
        let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;

        let left_alone_by_two_passes = |phase: &'static str| {
            let args = &args;
            let want = &want;
            async move {
                let snapshot: HashMap<CameraId, RunningCameraEntry> = args.handles.lock().clone();
                for (id, codec, dims) in want {
                    let entry = &snapshot[id];
                    assert_eq!(&entry.key.codec, codec, "{phase}: camera {id} codec");
                    assert_eq!(
                        &entry.key.supervisor_dims, dims,
                        "{phase}: camera {id} dims"
                    );
                }
                for pass in 1..=2 {
                    for (id, entry) in &snapshot {
                        assert!(
                            !entry.task.is_finished(),
                            "{phase}: camera {id}'s supervisor exited before pass {pass}"
                        );
                    }
                    reconcile(args).await.expect("reconcile pass");
                    for (id, entry) in &snapshot {
                        let same = args
                            .handles
                            .lock()
                            .get(id)
                            .is_some_and(|e| Arc::ptr_eq(&e.task, &entry.task));
                        assert!(
                            same,
                            "{phase}: pass {pass} restarted camera {id}, whose config never changed"
                        );
                    }
                }
            }
        };

        // No camera has a substream, so boot's registration pass is empty.
        let boot_analysis = register_analysis_sessions(
            args.recorder.as_ref(),
            pending_like_build_gst_recorder(&args, &cams),
        )
        .await;
        seed_like_boot(&args, boot_analysis).await;
        left_alone_by_two_passes("after boot").await;

        // Drop every entry so the next pass starts each camera through
        // `start_camera`, the other writer.
        abort_all(&args.handles);
        reconcile(&args).await.expect("start pass");
        left_alone_by_two_passes("after a start").await;
        abort_all(&args.handles);
    }
}

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
//!      calls [`reconcile`], which re-reads `store.list_readable_cameras()` and
//!      compares it against the shared `handles` map (seeded by `main`
//!      with the cameras it spawned at boot).
//!   3. Adds, removes, or restarts supervisors + ingesters to make
//!      the runtime match the DB.
//!
//! Restart triggers today: a supervisor that exited without being
//! stopped (nothing announces that, hence the periodic pass), ingest
//! URL change, supervisor (analysis) frame dimension change, ingest
//! codec change, an `analysis_url` (SPEC-069) that was added, changed
//! or removed, and a change to most — not all — of the camera's
//! `behavior` block ([`changed_behavior_field`] names the three it
//! deliberately ignores). Rules are not a trigger either, because they
//! hot-reload through the rule evaluator. Zones are a trigger: `run_camera`
//! reads them once, so a saved edit restarts that camera alone (SPEC-088).
//! Detector prompts are not a trigger and do not hot-reload: they are frozen
//! in the supervisor's `cfg` snapshot the same way behavior was, and nothing
//! in this process pushes them to a live task today, so an edit to them still
//! needs a process restart.
//!
//! A registration the recorder refused, and a substream session the
//! SPEC-069 fallback shut down, are not restart triggers: each pass
//! registers them again on their own, and the camera keeps running.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use nexus_bus::{topic, Bus, BusExt};
use nexus_config::TrackerConfig;
use nexus_config::{
    AnnotatorConfig, CameraBehavior, CameraConfig, ClipsConfig, StaticObjectConfig, ZoneConfig,
};
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
/// entry. Made only by [`spawn_supervisor`].
#[derive(Clone)]
pub struct RunningCameraEntry {
    pub task: Arc<JoinHandle<()>>,
    /// The supervisor's graceful stop ([`nexus_pipeline::CameraHandle::stop`]),
    /// which [`stop_camera`] uses so an open clip is closed, not abandoned.
    stop: Arc<tokio::sync::watch::Sender<bool>>,
    /// What the supervisor was started for, which each reconcile pass
    /// compares with what the camera's row asks for now.
    key: EntryKey,
    /// What the camera's start registered with the recorder beside the
    /// supervisor. Each pass registers it again without restarting the
    /// camera ([`retry_sessions`]); the no-change guard never reads it.
    sessions: Sessions,
    /// SPEC-088 — the supervisor's zone generation, which [`stop_camera`]
    /// names when it stops it.
    zone_generation: u64,
}

impl RunningCameraEntry {
    /// Whether the supervisor finished without having been asked to stop: its
    /// frame source returned, or it panicked. [`stop_camera`] keeps an entry
    /// in the map while it stops the task, so a finished task alone no longer
    /// separates that from a crash. A crash that coincides with the stop
    /// request reads as a stop, which is harmless: `stop_camera` removes the
    /// entry and the camera restarts at once.
    pub(crate) fn exited_on_its_own(&self) -> bool {
        self.task.is_finished() && !*self.stop.borrow()
    }
}

/// What [`reconcile`]'s no-change guard compares for a camera: the config
/// its supervisor was started for, and nothing the recorder answered. Its
/// fields are private to this module, and every side derives them in
/// [`EntryKey::wanted`], so a boot entry, a started entry and the guard's
/// expectation cannot disagree.
#[derive(Clone)]
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
    /// The configured analysis substream URL (SPEC-069). Compared on each
    /// reconcile pass — without it, applying a reprobe proposal writes the
    /// camera row, the reconciler decides nothing changed, and the second
    /// session is never started. The console would then show a camera set
    /// to analyse its substream that is doing no such thing. Whether the
    /// recorder accepted it is [`Sessions`], never this: a refused
    /// registration recorded here would restart the camera, main recording
    /// ingester included, on every pass (BUG-224).
    analysis_url: Option<String>,
    /// The camera's own `model_override.score_threshold`, as bits. The
    /// router wraps the shared detector with it at spawn, so only a
    /// respawn applies an edit.
    score_threshold: Option<u32>,
    /// The camera's `behavior` block. Most of it is consumed once inside
    /// `run_camera` — the static-object filter, the detector-skip policy,
    /// the crowd-hysteresis pair, the tile grid — and then frozen in that
    /// task's `cfg` snapshot, so a restart is the only way to apply an edit
    /// (BUG-285). Which fields that covers is decided in
    /// [`changed_behavior_field`], not here.
    behavior: CameraBehavior,
    /// The camera's zones, in saved order and as saved. `run_camera` reads
    /// them once into its exclusion and min-area filters, the annotator and
    /// the rules, so a restart is the only way to apply an edit (SPEC-088,
    /// #399). Nothing is sorted or rewritten: only a real edit differs.
    zones: Vec<ZoneConfig>,
}

/// Field by field rather than derived, because the behavior half compares
/// only what [`changed_behavior_field`] says a restart can apply. `self` is
/// destructured so a new field breaks the build here instead of being
/// silently left out of the guard, which the derive used to prevent.
impl PartialEq for EntryKey {
    fn eq(&self, other: &Self) -> bool {
        let EntryKey {
            url,
            supervisor_dims,
            codec,
            analysis_url,
            score_threshold,
            behavior,
            zones,
        } = self;
        *url == other.url
            && *supervisor_dims == other.supervisor_dims
            && *codec == other.codec
            && *analysis_url == other.analysis_url
            && *score_threshold == other.score_threshold
            && changed_behavior_field(behavior, &other.behavior).is_none()
            && *zones == other.zones
    }
}

impl EntryKey {
    /// What an entry for `cam` holds when nothing has changed.
    fn wanted(args: &ReconcilerArgs, cam: &CameraConfig) -> Self {
        Self {
            url: cam.ingest.url.to_string(),
            supervisor_dims: supervisor_dims_for(cam, args.default_detector_width),
            codec: cam.ingest.codec,
            analysis_url: cam.ingest.analysis_url.as_ref().map(ToString::to_string),
            score_threshold: cam
                .detector
                .model_override
                .as_ref()
                .map(|m| m.score_threshold.to_bits()),
            behavior: cam.behavior.clone(),
            zones: cam.zones.clone(),
        }
    }

    /// A boot camera's start: the guard's key, with the frame its boot RGB
    /// tap was built at in place of the guard's, and what boot registered
    /// for it. A camera with no boot tap builds its own frame source at the
    /// guard's frame. The call drains that camera's entries from `boot`, so
    /// the start it returns is the only record of them. An extra call as a
    /// bare statement fails `clippy -D warnings` on this `#[must_use]`;
    /// `let _ =` still passes.
    #[must_use = "this drains the camera's boot entry, which only the returned start records"]
    pub(crate) fn at_boot(
        args: &ReconcilerArgs,
        cam: &CameraConfig,
        boot: &mut BootSessions,
    ) -> Start {
        let want = Self::wanted(args, cam);
        Start {
            key: Self {
                supervisor_dims: boot.1.remove(&cam.id).unwrap_or(want.supervisor_dims),
                ..want
            },
            sessions: boot.0.remove(&cam.id).unwrap_or(Sessions {
                main_pending: None,
                analysis: None,
            }),
        }
    }
}

/// The name of the first `behavior` field that differs from the block a
/// running supervisor was started with in a way only a restart can apply,
/// or `None` when the running supervisor is still correct. Returning the
/// name rather than a bool is what lets the restart log say *which* field
/// moved.
///
/// `want` is destructured exhaustively on purpose. Adding a field to
/// [`CameraBehavior`] breaks this build and forces a decision about it:
/// tracking these one at a time by hand is what let a `parking_lot_mode`
/// toggle sit dead until the next process restart (BUG-285). A `..`
/// rest-pattern here would reintroduce the bug on the next field added.
///
/// Three fields are deliberately excluded, because restarting does not
/// apply them and the video gap would buy nothing:
///
/// * `supervisor_width` — feeds `supervisor_dims`, which [`EntryKey`]
///   already compares. It resolves through [`supervisor_dims_for`], so an
///   edit that does not move the resolved frame is genuinely a no-op.
///   **This exclusion is only sound while `supervisor_dims_for` reads
///   `supervisor_width`.** If it ever stops, move the field into the
///   compared chain in the same commit.
/// * `detector_downscale_to_width` / `detector_downscale_to_height` — these
///   select a pre-built inference layer keyed on `(kind, width, height)`,
///   and `InferenceRouter` builds its layer set once at boot from the
///   boot-time camera list. Restarting on an edit would usually look up a
///   layer nobody built (unless another camera booted with that target),
///   get `None`, and leave the camera on its high-res detector — losing
///   adaptive downscale that had been working, on top of the gap. Applying
///   these needs a router rebuild, which is separate work.
///
///   **This only avoids the gap when they are edited alone.** Edited
///   together with any compared field, the compared field triggers the
///   restart and the camera comes back on its high-res detector as above.
///   `spawn_supervisor` emits a `warn!` for that state so it is diagnosable
///   rather than silent.
fn changed_behavior_field(started: &CameraBehavior, want: &CameraBehavior) -> Option<&'static str> {
    let CameraBehavior {
        parking_lot_mode,
        anchor_ttl_secs,
        detector_skip_crowded_threshold,
        detector_skip_every_n_frames,
        detector_downscale_crowded_threshold,
        detector_downscale_sustained_secs,
        supervisor_downscale_crowded_threshold,
        supervisor_downscale_sustained_secs,
        supervisor_upscale_clear_secs,
        supervisor_downscale_to_width,
        tile_enabled,
        tile_trigger,
        tile_max_per_frame,
        tile_grid,
        tile_discovery_cells,
        tile_discovery_revisit_secs,
        // Excluded — see the doc comment above. Named, not `..`, so a new
        // field cannot join them by accident.
        supervisor_width: _,
        detector_downscale_to_width: _,
        detector_downscale_to_height: _,
    } = want;

    if *parking_lot_mode != started.parking_lot_mode {
        Some("parking_lot_mode")
    } else if *anchor_ttl_secs != started.anchor_ttl_secs {
        Some("anchor_ttl_secs")
    } else if *detector_skip_crowded_threshold != started.detector_skip_crowded_threshold {
        Some("detector_skip_crowded_threshold")
    } else if *detector_skip_every_n_frames != started.detector_skip_every_n_frames {
        Some("detector_skip_every_n_frames")
    } else if *detector_downscale_crowded_threshold != started.detector_downscale_crowded_threshold
    {
        Some("detector_downscale_crowded_threshold")
    } else if *detector_downscale_sustained_secs != started.detector_downscale_sustained_secs {
        Some("detector_downscale_sustained_secs")
    } else if *supervisor_downscale_crowded_threshold
        != started.supervisor_downscale_crowded_threshold
    {
        Some("supervisor_downscale_crowded_threshold")
    } else if *supervisor_downscale_sustained_secs != started.supervisor_downscale_sustained_secs {
        Some("supervisor_downscale_sustained_secs")
    } else if *supervisor_upscale_clear_secs != started.supervisor_upscale_clear_secs {
        Some("supervisor_upscale_clear_secs")
    } else if *supervisor_downscale_to_width != started.supervisor_downscale_to_width {
        Some("supervisor_downscale_to_width")
    } else if *tile_enabled != started.tile_enabled {
        Some("tile_enabled")
    } else if *tile_trigger != started.tile_trigger {
        Some("tile_trigger")
    } else if *tile_max_per_frame != started.tile_max_per_frame {
        Some("tile_max_per_frame")
    } else if *tile_grid != started.tile_grid {
        Some("tile_grid")
    } else if *tile_discovery_cells != started.tile_discovery_cells {
        Some("tile_discovery_cells")
    } else if *tile_discovery_revisit_secs != started.tile_discovery_revisit_secs {
        Some("tile_discovery_revisit_secs")
    } else {
        None
    }
}

/// What a camera's start registered with the recorder beside its
/// supervisor, recorded by both writers, boot and `start_camera`, before the
/// spawn, and kept current by [`retry_sessions`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sessions {
    /// The main ingester's codec, as the start resolved it, while its
    /// registration is outstanding; `None` once the recorder accepted it.
    main_pending: Option<CodecKind>,
    /// The camera's substream session. `None` when it has none, or when its
    /// main ingester was not registered before the spawn: the supervisor
    /// then builds its own frame source, which reads no substream, so none
    /// is registered until the camera next starts.
    analysis: Option<AnalysisSession>,
}

/// A camera's substream session as its start registered it (SPEC-069).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AnalysisSession {
    /// The codec the start resolved, probing for it: each pass registers
    /// the session again with this one, so no pass probes the camera.
    codec: CodecKind,
    /// Whether the recorder accepted the latest registration.
    registered: bool,
}

/// A camera about to be spawned: the key its entry records, and what its
/// start registered. Made by [`EntryKey::at_boot`] and `start_camera`,
/// after the registrations, and spawned only by [`spawn_supervisor`], so
/// no supervisor is spawned before its sessions are registered.
#[must_use = "a start that is never spawned leaves the camera without a supervisor"]
pub(crate) struct Start {
    key: EntryKey,
    sessions: Sessions,
}

/// What each boot entry records of boot's registrations, by camera. Only
/// [`register_boot_sessions`] makes one and only [`EntryKey::at_boot`] reads
/// it, so `main` cannot build the map, and can change it only by draining
/// entries through `at_boot`. A camera with no entry registered its main
/// ingester and has no substream. Deliberate forges are still possible: an
/// empty registration pass; a pass over a throwaway `StubClipRecorder`, which
/// accepts every substream; and a second `at_boot` call for a camera, or one
/// made with a doctored copy of it, which drains its entry.
///
/// It also holds the frame each camera's boot RGB tap was built at, from
/// [`BootSessions::with_taps`], which each boot entry records as its
/// supervisor dims: the tap and the entry then cannot disagree, and a tap
/// sized apart from the guard is a changed frame the first reconcile pass
/// restarts, not a supervisor misreading its frames until the next restart.
#[must_use = "each boot entry's sessions come from this map, through EntryKey::at_boot"]
pub(crate) struct BootSessions(HashMap<CameraId, Sessions>, HashMap<CameraId, (u32, u32)>);

impl BootSessions {
    /// The frame `(width, height)` each camera's boot RGB tap was built at,
    /// read back from the built ingester. Only `build_gst_recorder` builds
    /// taps; the stub arms have none.
    #[cfg_attr(not(feature = "gstreamer"), allow(dead_code))]
    pub(crate) fn with_taps(self, taps: HashMap<CameraId, (u32, u32)>) -> Self {
        Self(self.0, taps)
    }
}

/// The supervisor (RGB analysis) frame `(width, height)` a camera runs at:
/// its detector input width — the `model_override`'s, else
/// `default_detector_width` — raised to `behavior.supervisor_width` when
/// that is larger. The only copy of the rule: the no-change guard and the
/// boot RGB tap in `build_gst_recorder` size from it, boot's entries record
/// the tap's frame, and `camera_reprobe` and discovery rank substreams
/// against it.
pub(crate) fn supervisor_dims_for(cam: &CameraConfig, default_detector_width: u32) -> (u32, u32) {
    supervisor_dims_from(
        cam.detector.model_override.as_ref().map(|m| m.input_width),
        cam.behavior.supervisor_width,
        default_detector_width,
    )
}

/// [`supervisor_dims_for`] on the two camera fields it reads, for a caller
/// with no `CameraConfig` yet: discovery ranks for a camera it has not
/// created, which has neither field.
pub(crate) fn supervisor_dims_from(
    model_override_width: Option<u32>,
    supervisor_width: Option<u32>,
    default_detector_width: u32,
) -> (u32, u32) {
    let det_w = model_override_width.unwrap_or(default_detector_width);
    // M_NATIVE_ASPECT — supervisor width may be decoupled from the
    // detector input (clamped up so it never drops below it).
    let sup_input = supervisor_width.unwrap_or(det_w).max(det_w);
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
    /// The camera rows the last read of the camera list could not decode,
    /// which therefore do not run: set by `main` from its boot read and by
    /// every [`reconcile`] pass whose read succeeds (a failed one leaves the
    /// last set), and reported by the health roll-up.
    pub unreadable_cameras: Arc<Mutex<Vec<CameraId>>>,
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
    // Without the subscription the loop keeps ticking on a stream that never
    // yields: losing hot-add must not also stop the periodic pass that
    // restarts an exited supervisor.
    let mut stream = match args
        .bus
        .subscribe::<serde_json::Value>(topic::CONFIG_CHANGED)
        .await
    {
        Ok(s) => {
            info!("camera reconciler: subscribed to config.changed");
            s
        }
        Err(e) => {
            error!(
                error = %e,
                "camera reconciler: failed to subscribe to config.changed; camera edits now \
                 apply only on the periodic pass"
            );
            Box::pin(futures::stream::pending())
        }
    };

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
                let Some(msg) = msg else {
                    error!(
                        "camera reconciler: config.changed stream ended; camera edits now \
                         apply only on the periodic pass"
                    );
                    stream = Box::pin(futures::stream::pending());
                    continue;
                };
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
}

/// One reconciliation pass. Compares `store.list_readable_cameras()` to the
/// in-memory `handles` map and:
///   * stops the supervisor + removes the ingester for any camera
///     that is missing from the DB, has `ingest.enabled = false`, or
///     whose row this build cannot read;
///   * spawns a fresh supervisor + ingester for any enabled camera
///     not yet in the map;
///   * restarts the supervisor + ingester for any enabled camera
///     whose ingest URL has changed, or whose supervisor has exited.
async fn reconcile(args: &ReconcilerArgs) -> anyhow::Result<()> {
    let zone_application = args.frame_stats.zone_application();
    // SPEC-088 — no save commits between this read and its requests below.
    let reading = zone_application.saves().read().await;
    let (live, unreadable) = args.store.list_readable_cameras().await?;
    *args.unreadable_cameras.lock() = unreadable;

    // Snapshot current state under a short lock so the rest of the
    // pass can run without holding it. The clone is cheap — at most
    // a few dozen entries on real installs.
    let current: HashMap<CameraId, RunningCameraEntry> = args.handles.lock().clone();

    let live_enabled: HashSet<CameraId> = live
        .iter()
        .filter(|c| c.ingest.enabled)
        .map(|c| c.id)
        .collect();
    // SPEC-088 — a pass reads the saved zones and enabled flags, so it is a
    // request too: a save that bypassed the API's (which requests on its own)
    // still lets the generation started for it acknowledge.
    for cam in &live {
        zone_application.request(
            cam.id,
            &nexus_pipeline::zone_application::zone_revision(&cam.zones),
            cam.ingest.enabled,
        );
    }
    drop(reading);

    // 1. Remove anything that is gone-or-disabled, or whose row this build
    //    cannot read: boot would not have started it either.
    for id in current.keys().copied().collect::<Vec<_>>() {
        if !live_enabled.contains(&id) {
            if !stop_camera(args, id).await {
                continue;
            }
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
            // A disabled camera reads inactive, its last acknowledgement kept
            // for comparison; one that is gone has nothing left to report.
            if !live.iter().any(|c| c.id == id) {
                zone_application.remove(id);
            }
        }
    }

    // 2. Add or restart anything that is enabled in the DB.
    for cam in live.into_iter().filter(|c| c.ingest.enabled) {
        let cam_id = cam.id;
        let want = EntryKey::wanted(args, &cam);
        match current.get(&cam_id) {
            // `==` compares url, dims, codec, analysis_url, score threshold, behavior, then zones.
            Some(entry)
                if entry.key == want
                    && !entry.task.is_finished()
                    && zone_application.runs(cam_id, entry.zone_generation) =>
            {
                // No change — supervisor still alive, URL still the
                // same, supervisor dims still match, ingest codec
                // still matches what's in the DB. Skip (we
                // deliberately do not respawn on unrelated config
                // edits today), and register again what the camera's
                // start registered, which never restarts it.
                retry_sessions(args, &cam, entry);
                continue;
            }
            Some(entry) => {
                // `stop_camera` runs in this task, so the only entry a pass
                // sees mid-stop is one an earlier pass could not finish
                // stopping; any other finished task was not stopped by us.
                if entry.exited_on_its_own() {
                    error!(
                        camera_id = cam_id,
                        "camera reconciler: supervisor exited without being stopped; restarting"
                    );
                } else if *entry.stop.borrow() {
                    info!(
                        camera_id = cam_id,
                        "camera reconciler: checking again on a camera still stopping (its \
                         supervisor outlived its abort, or its recorder work was not drained)"
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
                } else if entry.key.analysis_url != want.analysis_url {
                    info!(
                        camera_id = cam_id,
                        "camera reconciler: analysis stream changed; restarting supervisor"
                    );
                } else if entry.key.score_threshold != want.score_threshold {
                    info!(
                        camera_id = cam_id,
                        "camera reconciler: score threshold changed; restarting supervisor"
                    );
                } else if entry.key.zones != want.zones {
                    info!(
                        camera_id = cam_id,
                        "camera reconciler: camera zones changed; restarting supervisor"
                    );
                } else if entry.key == want {
                    info!(
                        camera_id = cam_id,
                        "camera reconciler: supervisor's zone generation is no longer running \
                         (its source ended, or the camera was deleted and re-created); \
                         restarting supervisor"
                    );
                } else {
                    info!(
                        camera_id = cam_id,
                        field = changed_behavior_field(&entry.key.behavior, &want.behavior)
                            .unwrap_or("unknown"),
                        "camera reconciler: camera behavior changed; restarting supervisor"
                    );
                }
                if !stop_camera(args, cam_id).await {
                    continue;
                }
            }
            None => {}
        }
        start_camera(args, cam, want).await;
    }

    Ok(())
}

/// How long [`stop_camera`] waits for a supervisor to stop on its own before
/// aborting it. Stopping finishes the frame in flight and closes the open
/// clip, well under a second normally; the bound is for a supervisor wedged
/// in an await (a hung detector or recorder call), which would otherwise
/// hold up every camera behind it in the pass. Stops run one after another
/// in a pass, so normal closes are serialized too.
///
/// It sits well above the recorder's own worst-case close — up to 2 s for
/// the live pump plus `EOS_DRAIN_TIMEOUT` (5 s) for the drain — so a normal
/// close finishes inside it. An abort that lands inside an open or close no
/// longer abandons the clip: `GstClipRecorder` runs both in tasks it owns and
/// its shutdown drain waits for them (SPEC-088 Z-4). A clip an aborted
/// supervisor merely held is finalized by [`stop_camera`]'s drain of the
/// camera, also bounded by this; a camera not drained within it stays
/// stopping.
const SUPERVISOR_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Whether the camera's supervisor has ended, the recorder work it left is
/// done and its camera was torn down. `false` means the camera is still
/// stopping: its supervisor outlived its abort, or the recorder has not
/// drained the camera. It stays in the map, nothing else is torn down, and
/// the caller must not start a replacement; a later pass checks on it again
/// without waiting.
async fn stop_camera(args: &ReconcilerArgs, cam_id: CameraId) -> bool {
    // The entry stays in the map until the supervisor has ended and the
    // recorder has drained the camera. Shutdown (`main.rs`) aborts the
    // reconciler without joining it, then aborts every supervisor it finds in
    // the map before the recorder's shutdown snapshot.
    // An entry removed up front would leave a supervisor that is still mid-stop
    // (a frame paused in `recorder.open`, say) out of that sweep, free to open
    // a clip after the snapshot that nothing would finalize. The price is that
    // a finished task in the map no longer always means a crash:
    // [`RunningCameraEntry::exited_on_its_own`] tells the two apart.
    let stopping = args.handles.lock().get(&cam_id).map(|entry| {
        (
            entry.stop.clone(),
            entry.task.clone(),
            entry.zone_generation,
        )
    });
    // Only an earlier pass that could not finish the stop left a stop request
    // behind in the map: look again, without waiting.
    let mut recheck = false;
    if let Some((stop, task, zone_generation)) = stopping {
        recheck = *stop.borrow();
        let ended = if recheck {
            task.is_finished()
        } else {
            // From here on the generation's zones no longer count as applied.
            args.frame_stats
                .zone_application()
                .stop(cam_id, zone_generation);
            // Stop, not abort: an aborted supervisor never reaches the close of
            // its open motion clip, which is abandoned (BUG-285). The receiver
            // drops when `run_camera` returns, which is what `closed` awaits.
            // Stored even with no receiver left, so the entry reads as stopping.
            stop.send_replace(true);
            if tokio::time::timeout(SUPERVISOR_STOP_TIMEOUT, stop.closed())
                .await
                .is_ok()
            {
                info!(camera_id = cam_id, "camera reconciler: stopped supervisor");
                true
            } else {
                task.abort();
                // The abort lands at the supervisor's next await; until it has,
                // the entry stays in shutdown's sweep. Bounded, because a
                // supervisor wedged in synchronous code never reaches one.
                let ended = tokio::time::timeout(SUPERVISOR_STOP_TIMEOUT, stop.closed())
                    .await
                    .is_ok();
                warn!(
                    camera_id = cam_id,
                    timeout_secs = SUPERVISOR_STOP_TIMEOUT.as_secs(),
                    ended,
                    "camera reconciler: supervisor did not stop in time; aborted it, and any \
                     clip it had open is finalized before the camera is torn down"
                );
                ended
            }
        };
        if !ended {
            // Removing it would hide a live supervisor from shutdown's sweep
            // and start a second one for the camera beside it.
            warn!(
                camera_id = cam_id,
                "camera reconciler: aborted supervisor is still running; keeping it tracked \
                 and not restarting the camera until it ends"
            );
            return false;
        }
    }
    // Only now that the supervisor is done: its clip closes on this ingester.
    // An aborted one skipped the close of a clip it held, or left an open or
    // close in flight, which would keep recording beside the replacement's
    // (SPEC-088 Z-4). Until the drain succeeds the camera stays stopping, as
    // a supervisor that outlived its abort does; the recorder's owned tasks
    // run on meanwhile, and shutdown waits for them.
    let bound = if recheck {
        std::time::Duration::ZERO
    } else {
        SUPERVISOR_STOP_TIMEOUT
    };
    let drained = match tokio::time::timeout(bound, args.recorder.drain_camera(cam_id)).await {
        Ok(drained) => drained.map_err(|e| e.to_string()),
        Err(_) => Err("still in flight".to_owned()),
    };
    if let Err(error) = drained {
        warn!(
            camera_id = cam_id,
            error,
            "camera reconciler: the stopped supervisor's recorder work is not finished; keeping \
             the camera stopping and not restarting it until a pass has drained it"
        );
        return false;
    }
    args.handles.lock().remove(&cam_id);
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
    true
}

/// Attach or detach a camera's SPEC-069 analysis substream session, and
/// return the session its entry records: the codec it was registered with
/// and whether the recorder accepted it, or `None` for a camera with no
/// substream. This is the one place a substream's codec is probed, once per
/// start; [`retry_sessions`] registers again with the codec returned here.
#[must_use = "this is what the entry's sessions record"]
pub(crate) async fn apply_analysis_session(
    recorder: &dyn ClipRecorder,
    cam: &CameraConfig,
    main_codec: CodecKind,
    supervisor_dims: (u32, u32),
) -> Option<AnalysisSession> {
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
    let codec = match analysis_url.scheme() {
        "rtsp" | "rtsps" => crate::discovery::rtsp_probe::probe_codec_for_url(analysis_url)
            .await
            .unwrap_or(main_codec),
        _ => main_codec,
    };
    let registered = match recorder.set_camera_analysis_ingester(
        cam_id,
        Some(analysis_url.as_str()),
        cam.ingest.max_fps,
        sup_w,
        sup_h,
        codec,
    ) {
        Ok(()) => true,
        Err(e) => {
            error!(
                camera_id = cam_id,
                error = %e,
                "analysis substream session failed to start; analysis stays on the main stream \
                 until a pass registers it"
            );
            false
        }
    };
    Some(AnalysisSession { codec, registered })
}

/// What a camera's start registers beside its supervisor, before the spawn,
/// the same for both writers: `start_camera`, and boot through
/// [`register_boot_sessions`]. `main_registered` answers the camera's main
/// ingester registration, which the caller made. The substream is
/// registered only once the main ingester is: without one the supervisor
/// builds its own frame source, which reads no substream.
async fn start_sessions(
    recorder: &dyn ClipRecorder,
    cam: &CameraConfig,
    main_codec: CodecKind,
    supervisor_dims: (u32, u32),
    main_registered: bool,
) -> Sessions {
    if !main_registered {
        return Sessions {
            main_pending: Some(main_codec),
            analysis: None,
        };
    }
    recorder.set_camera_codec_pin(cam.id, cam.ingest.codec);
    Sessions {
        main_pending: None,
        analysis: apply_analysis_session(recorder, cam, main_codec, supervisor_dims).await,
    }
}

/// Boot's registration pass, run once by every `build_recorder` branch over
/// each camera it has something to record for: the codec and supervisor
/// frame of its main ingester, and whether that ingester registered. Each
/// boot entry records what `start_camera` records after a restart. A second
/// call would re-probe each RTSP substream.
#[must_use = "this is what each boot entry's sessions record"]
pub(crate) async fn register_boot_sessions(
    recorder: &dyn ClipRecorder,
    cameras: Vec<(&CameraConfig, CodecKind, (u32, u32), bool)>,
) -> BootSessions {
    let mut sessions = HashMap::new();
    for (cam, codec, dims, main_registered) in cameras {
        let registered = start_sessions(recorder, cam, codec, dims, main_registered).await;
        sessions.insert(cam.id, registered);
    }
    BootSessions(sessions, HashMap::new())
}

/// Register again, without restarting the camera, what `entry`'s start
/// registered beside its supervisor (the same URL, frame and codec, at the
/// camera's current `max_fps`), and record the answers in the entry:
/// SPEC-069's retry "on the long backoff already used for session
/// rebuilds", once per pass (each 30 s tick, and each config change). A
/// main ingester is retried while it is outstanding. The substream session
/// is registered on every pass, with the codec its start resolved, because a
/// session the SPEC-069 fallback shut down leaves nothing in the entry to
/// say so. For a running session the recorder's registration is a no-op; for
/// one that was shut down it starts a new one, which the camera's running
/// frame source takes up once it delivers.
fn retry_sessions(args: &ReconcilerArgs, cam: &CameraConfig, entry: &RunningCameraEntry) {
    let cam_id = cam.id;
    let (sup_w, sup_h) = entry.key.supervisor_dims;
    let mut sessions = entry.sessions;
    if let Some(codec) = sessions.main_pending {
        match args.recorder.add_camera_ingester(
            cam_id,
            &entry.key.url,
            args.pre_roll_secs,
            cam.ingest.max_fps,
            sup_w,
            sup_h,
            codec,
        ) {
            Ok(()) => {
                info!(
                    camera_id = cam_id,
                    "camera reconciler: ingester registered on retry; clips resume for this camera"
                );
                args.recorder.set_camera_codec_pin(cam_id, cam.ingest.codec);
                sessions.main_pending = None;
            }
            Err(e) => {
                debug!(camera_id = cam_id, error = %e, "camera reconciler: ingester retry failed")
            }
        }
    }
    if let (Some(session), Some(url)) = (
        sessions.analysis.as_mut(),
        entry.key.analysis_url.as_deref(),
    ) {
        let registered = match args.recorder.set_camera_analysis_ingester(
            cam_id,
            Some(url),
            cam.ingest.max_fps,
            sup_w,
            sup_h,
            session.codec,
        ) {
            Ok(()) => true,
            Err(e) => {
                debug!(camera_id = cam_id, error = %e, "camera reconciler: analysis substream retry failed");
                false
            }
        };
        if registered && !session.registered {
            info!(
                camera_id = cam_id,
                "camera reconciler: analysis substream session registered on retry"
            );
        }
        session.registered = registered;
    }
    if sessions != entry.sessions {
        if let Some(live) = args.handles.lock().get_mut(&cam_id) {
            live.sessions = sessions;
        }
    }
}

async fn start_camera(args: &ReconcilerArgs, cam: CameraConfig, want: EntryKey) {
    let cam_id = cam.id;
    let (sup_w, sup_h) = want.supervisor_dims;
    let url = want.url.clone();
    // Pre-roll ingester first so the recorder is ready by the time
    // the supervisor opens its first motion clip. Failure is logged
    // but non-fatal: detection still runs; clip opens for this
    // camera return Refused until a later pass registers it.
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
                        codec = %c,
                        "codec autodetected at hot-add"
                    );
                    c
                }
                None => {
                    warn!(
                        camera_id = cam_id,
                        "camera codec unspecified and autodetect probe failed; defaulting to h264 — set `ingest.codec` in the camera config to silence"
                    );
                    CodecKind::H264
                }
            }
        }
    };
    let main_registered = match args.recorder.add_camera_ingester(
        cam_id,
        &url,
        args.pre_roll_secs,
        cam.ingest.max_fps,
        sup_w,
        sup_h,
        codec,
    ) {
        Ok(()) => true,
        Err(e) => {
            error!(
                camera_id = cam_id,
                error = %e,
                "camera reconciler: ingester hot-add failed; clips will be refused for this camera \
                 until a pass registers it"
            );
            false
        }
    };

    // SPEC-069 — the analysis session, when the camera has one. Shared
    // with the boot path so the two can never disagree about whether a
    // converted camera actually got its second session.
    let sessions = start_sessions(
        args.recorder.as_ref(),
        &cam,
        codec,
        want.supervisor_dims,
        main_registered,
    )
    .await;

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
    // The entry records `want` as it is, the configured codec included, never
    // `codec` above: the guard compares the DB's value, so recording the
    // resolved one would restart every auto-codec camera on every pass.
    let entry = spawn_supervisor(
        args,
        cam,
        Start {
            key: want,
            sessions,
        },
        seed_for_cam,
    );
    args.handles.lock().insert(cam_id, entry);
    info!(
        camera_id = cam_id,
        sup_w, sup_h, "camera reconciler: spawned supervisor + ingester"
    );
}

/// Spawn `cam`'s supervisor at `start`'s supervisor frame, and return the
/// entry that records it. The only caller of `spawn_camera`, for boot and
/// for `start_camera`, so a supervisor runs at the frame its entry records;
/// and a `start` exists only once its registrations have run, so no
/// supervisor builds its frame source before its sessions are registered.
#[must_use = "an entry the handle map never holds is not supervised"]
pub(crate) fn spawn_supervisor(
    args: &ReconcilerArgs,
    cam: CameraConfig,
    start: Start,
    sighting_seed: Vec<nexus_pipeline::EntityLocalSeed>,
) -> RunningCameraEntry {
    let (sup_w, sup_h) = start.key.supervisor_dims;
    let detector = args.router.detector_for_camera(&cam);
    let detector_low_res = args.router.detector_for_camera_low_res(&cam);
    if let Some((requested, effective)) = args.router.score_thresholds_for_camera(&cam) {
        if effective > requested {
            warn!(
                camera_id = cam.id,
                requested,
                effective,
                "camera reconciler: score_threshold is below what this camera's shared \
                 inference layer was built with at boot, so `effective` applies until \
                 the engine restarts"
            );
        } else {
            info!(
                camera_id = cam.id,
                requested, effective, "camera reconciler: detector score threshold"
            );
        }
    }
    // The router builds its layer set once at boot, so a downscale target
    // edited since then has no layer and adaptive downscale is silently off
    // for this camera until the process restarts. Say so: this is reachable
    // whenever a downscale-target edit rides along with a field that does
    // trigger a restart (see [`changed_behavior_field`]). Both targets are
    // required: with either unset there is no layer to look up at all.
    if detector_low_res.is_none()
        && cam.behavior.detector_downscale_crowded_threshold.is_some()
        && cam.behavior.detector_downscale_sustained_secs.is_some()
        && cam.behavior.detector_downscale_to_width.is_some()
        && cam.behavior.detector_downscale_to_height.is_some()
    {
        warn!(
            camera_id = cam.id,
            to_width = ?cam.behavior.detector_downscale_to_width,
            to_height = ?cam.behavior.detector_downscale_to_height,
            "camera reconciler: adaptive detector downscale is configured but no \
             low-res layer was built at boot for this target; camera stays on its \
             high-res detector"
        );
    }
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
        sighting_seed,
        args.sighting_persist.clone(),
        effective_top_k,
        args.sink_router.clone(),
        args.alert_clip_schedule_gate.clone(),
    );
    RunningCameraEntry {
        task: Arc::new(handle.task),
        stop: Arc::new(handle.stop),
        key: start.key,
        sessions: start.sessions,
        zone_generation: handle.zone_generation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::{
        CameraBehavior, CameraDetector, CameraIngest, CameraOnvif, CameraTalkDown, RecorderKind,
        TileGridConfig, ZoneKind,
    };
    use nexus_pipeline::zone_application::{
        zone_revision, ZoneApplicationError, ZoneApplicationState, ZonesRuntime,
        APPLICATION_TIMEOUT,
    };
    use nexus_pipeline::{ClipFinal, ClipHandle, ClipMeta, OpenClip, RecorderError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use url::Url;

    /// Records every `set_camera_analysis_ingester` call so a test can assert
    /// what the caller actually asked the recorder to do.
    #[derive(Default)]
    struct RecordingRecorder {
        calls: Mutex<Vec<(CameraId, Option<String>, CodecKind)>>,
        codec_pins: Mutex<Vec<(CameraId, Option<CodecKind>)>>,
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
        fn set_camera_codec_pin(&self, camera_id: CameraId, codec: Option<CodecKind>) {
            self.codec_pins.lock().push((camera_id, codec));
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

    #[tokio::test]
    async fn session_registration_preserves_only_explicit_codec_pins() {
        let rec = RecordingRecorder::default();
        let mut camera = cam(None);
        for pin in [None, Some(CodecKind::H264), Some(CodecKind::H265), None] {
            camera.ingest.codec = pin;
            start_sessions(&rec, &camera, CodecKind::H264, (512, 288), true).await;
        }
        assert_eq!(
            *rec.codec_pins.lock(),
            vec![
                (7, None),
                (7, Some(CodecKind::H264)),
                (7, Some(CodecKind::H265)),
                (7, None)
            ]
        );
        rec.codec_pins.lock().clear();
        start_sessions(&rec, &camera, CodecKind::H264, (512, 288), false).await;
        assert!(
            rec.codec_pins.lock().is_empty(),
            "failed registration must not pin another ingester"
        );
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
            recorded,
            Some(AnalysisSession {
                codec: CodecKind::H265,
                registered: true
            }),
            "an accepted registration is recorded as registered, with the codec it used"
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
            ("a width off the ladder", cam(None), 600, (600, 338)),
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
    /// [`BootSessions`] (a map only `register_boot_sessions` makes), and
    /// it cannot touch an entry's fields. Registering is not. The
    /// real `build_gst_recorder` compiles only with the `gstreamer` feature.
    /// The label-gated `system-libs` CI job clippies that arm with
    /// `-D warnings`, but CI runs none of this crate's tests with the
    /// feature, so a test of the arm would never run there. This pins the
    /// arm's wiring at the source level instead, the same audit-test shape as
    /// `gst_clip_recorder::pipeline_string_is_codec_passthrough`. Each needle
    /// must occur exactly once, so it names one line: the gstreamer arm's
    /// registration, its record of a main ingester that did not build, the
    /// boot taps it hands on, and that arm's return of what it registered.
    /// The stub arms are driven for real by
    /// `the_stub_boot_path_registers_every_enabled_substream`.
    #[test]
    fn the_boot_path_registers_analysis_sessions() {
        let main_rs = include_str!("main.rs");
        for (needle, consequence) in [
            (
                "register_boot_sessions(&rec, boot_pending)",
                "build_gst_recorder must register the SPEC-069 analysis sessions at boot: \
                 without it every converted camera boots recording no substream session, and \
                 no pass ever registers one.",
            ),
            (
                "boot_pending.push((cam, codec, (rgb_w, rgb_h), false));",
                "build_gst_recorder must record a camera whose main ingester did not build: \
                 without it no pass retries the ingester, and the camera records nothing.",
            ),
            (
                "Ok((Arc::new(rec), webrtc, analysis))",
                "build_gst_recorder must return what it registered: an empty map boots every \
                 converted camera recording no substream session, and no pass ever registers \
                 one.",
            ),
            (
                ".with_taps(taps)",
                "build_gst_recorder must hand boot the frame each RGB tap was built at: \
                 without it a boot entry records the guard's frame, and a tap sized apart \
                 from it feeds its supervisor frames of another size until the camera \
                 next restarts. a_boot_entry_records_the_frame_its_rgb_tap_was_built_at \
                 checks the real arm, but CI runs no gstreamer tests of this crate.",
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
        /// Every source fails before delivering a frame.
        NoFrames,
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
                SourceScript::NoFrames => {
                    return Some(Box::new(nexus_pipeline::FailingSource {
                        message: "no frames".into(),
                    }))
                }
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
            unreadable_cameras: Arc::default(),
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

    fn sessions_of(handles: &HandleMap, camera_id: CameraId) -> Sessions {
        handles
            .lock()
            .get(&camera_id)
            .map(|e| e.sessions)
            .expect("camera running")
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

    /// BUG-285 — `behavior.parking_lot_mode` is read once in `run_camera` to
    /// build the static-object filter and then frozen in that task's `cfg`
    /// snapshot. A guard that does not notice the flip leaves the old
    /// supervisor running, nothing re-enters `run_camera`, and the operator's
    /// toggle does nothing until the engine restarts — with no log line.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_parking_lot_mode_toggle_restarts_the_camera() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;

        reconcile(&args).await.expect("initial pass");
        // Pin the first supervisor's source before the edit, so the count
        // below is exactly one source per supervisor.
        wait_until(
            "precondition: the first supervisor built its source",
            || recorder.sources_built(7) == 1,
        )
        .await;
        let original = args
            .handles
            .lock()
            .get(&7)
            .cloned()
            .expect("camera 7 spawned");

        let mut toggled = cam(None);
        toggled.behavior.parking_lot_mode = true;
        args.store
            .upsert_camera(&toggled)
            .await
            .expect("toggle parking-lot mode");
        reconcile(&args).await.expect("pass after the edit");

        let now = args
            .handles
            .lock()
            .get(&7)
            .cloned()
            .expect("camera 7 running");
        if Arc::ptr_eq(&now.task, &original.task) {
            abort_all(&args.handles);
            panic!(
                "flipping parking_lot_mode left the supervisor that was spawned with it off: \
                 the static-object filter is built once at spawn, so parking-lot mode stays \
                 off until the engine restarts"
            );
        }
        // The supervisor builds its frame source inside its own task.
        wait_until(
            "the restarted supervisor never built a fresh frame source",
            || recorder.sources_built(7) == 2,
        )
        .await;
        abort_all(&args.handles);
    }

    /// The router wraps the shared detector with the camera's own
    /// `score_threshold` when the supervisor is spawned, so an edit that
    /// leaves the old supervisor running changes nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_score_threshold_edit_restarts_the_camera() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;

        reconcile(&args).await.expect("initial pass");
        let original = args
            .handles
            .lock()
            .get(&7)
            .cloned()
            .expect("camera 7 spawned");

        let mut edited = cam(None);
        edited.detector.model_override = Some(nexus_config::ModelConfig {
            kind: "mock".into(),
            score_threshold: 0.2,
            ..Default::default()
        });
        args.store
            .upsert_camera(&edited)
            .await
            .expect("edit the score threshold");
        reconcile(&args).await.expect("pass after the edit");

        let now = args
            .handles
            .lock()
            .get(&7)
            .cloned()
            .expect("camera 7 running");
        let restarted = !Arc::ptr_eq(&now.task, &original.task);
        abort_all(&args.handles);
        assert!(
            restarted,
            "the supervisor kept the detector wrapped at the old threshold"
        );
    }

    /// What [`ClipLogRecorder`] was asked to do, in the order it was asked.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ClipCall {
        Open(i64),
        Close(i64),
        IngesterRemoved,
    }

    /// Accepts every clip and logs each open, close and ingester removal in
    /// call order, by camera, so a test can see whether a restart finalized
    /// the clip its old supervisor had open, and while the ingester recording
    /// it still existed. Clip ids count from 1 per camera. Frames come from a
    /// `VirtualSource` that never ends; it counts the sources each camera's
    /// supervisors built, one per start.
    #[derive(Default)]
    struct ClipLogRecorder {
        calls: Mutex<Vec<(CameraId, ClipCall)>>,
        sources_built: Mutex<HashMap<CameraId, usize>>,
        /// Set by [`Self::wedge_store_on_open`]: the store whose every pooled
        /// connection the next open checks out.
        wedge: Mutex<Option<Arc<Store>>>,
        /// The connections a wedged open holds.
        wedged: Mutex<Vec<sqlx::pool::PoolConnection<sqlx::Sqlite>>>,
        /// What the next drains do, in order; once used up, each succeeds.
        drains: Mutex<std::collections::VecDeque<Drain>>,
        /// When set, a close is logged as it begins and then waits for a
        /// permit of this gate.
        close_gate: Option<tokio::sync::Semaphore>,
        /// When set, a camera's first frame source ends once this is
        /// notified, as [`SourceUntil`].
        end_first_source: Option<Arc<tokio::sync::Notify>>,
    }

    /// Delivers frames until `end` is notified, then returns as
    /// [`SourceThatEnds`] does.
    struct SourceUntil {
        camera_id: CameraId,
        end: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl nexus_pipeline::FrameSource for SourceUntil {
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
            tokio::select! {
                _ = frames.run(tx) => {}
                () = self.end.notified() => {}
            }
            Err(nexus_pipeline::FrameSourceError::Backend(
                "upstream session ended".into(),
            ))
        }
    }

    /// A [`ClipLogRecorder`] drain that does not succeed at once.
    enum Drain {
        /// Never finishes, as a close or open stalled in the recorder.
        Stalls,
        /// Reports a clip it could not finalize.
        Fails,
        /// Succeeds once this reads true: the recorder's own tasks run its
        /// work on whether or not a drain waits for them.
        Until(tokio::sync::watch::Receiver<bool>),
    }

    impl ClipLogRecorder {
        /// The next open hands its clip over with every connection of
        /// `store`'s pool checked out, so the supervisor's next store write
        /// waits for one, as on a store too busy to answer.
        fn wedge_store_on_open(&self, store: Arc<Store>) {
            *self.wedge.lock() = Some(store);
        }

        /// Give back the connections a wedged open took.
        fn unwedge_store(&self) {
            self.wedged.lock().clear();
        }

        /// The clips of `camera_id` opened and not closed.
        fn open_clips(&self, camera_id: CameraId) -> Vec<i64> {
            let calls = self.calls_of(camera_id);
            calls
                .iter()
                .filter_map(|c| match c {
                    ClipCall::Open(id) if !calls.contains(&ClipCall::Close(*id)) => Some(*id),
                    _ => None,
                })
                .collect()
        }

        fn calls(&self) -> Vec<ClipCall> {
            self.calls.lock().iter().map(|(_, call)| *call).collect()
        }

        fn calls_of(&self, camera_id: CameraId) -> Vec<ClipCall> {
            self.calls
                .lock()
                .iter()
                .filter(|(id, _)| *id == camera_id)
                .map(|(_, call)| *call)
                .collect()
        }

        fn opens(&self) -> usize {
            self.calls()
                .iter()
                .filter(|c| matches!(c, ClipCall::Open(_)))
                .count()
        }

        fn opens_of(&self, camera_id: CameraId) -> usize {
            self.calls_of(camera_id)
                .iter()
                .filter(|c| matches!(c, ClipCall::Open(_)))
                .count()
        }

        fn sources_built(&self, camera_id: CameraId) -> usize {
            self.sources_built
                .lock()
                .get(&camera_id)
                .copied()
                .unwrap_or(0)
        }
    }

    #[async_trait::async_trait]
    impl ClipRecorder for ClipLogRecorder {
        async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
            let clip_id = self.opens_of(args.camera_id) as i64 + 1;
            let wedge = self.wedge.lock().take();
            if let Some(store) = wedge {
                for _ in 0..store.pool().options().get_max_connections() {
                    let conn = store
                        .pool()
                        .acquire()
                        .await
                        .expect("check out a connection");
                    self.wedged.lock().push(conn);
                }
            }
            self.calls
                .lock()
                .push((args.camera_id, ClipCall::Open(clip_id)));
            Ok(ClipHandle {
                clip_id,
                camera_id: args.camera_id,
            })
        }
        async fn close(
            &self,
            handle: ClipHandle,
            _args: ClipFinal,
        ) -> Result<ClipMeta, RecorderError> {
            self.calls
                .lock()
                .push((handle.camera_id, ClipCall::Close(handle.clip_id)));
            if let Some(gate) = &self.close_gate {
                gate.acquire().await.expect("gate is never closed").forget();
            }
            Ok(ClipMeta {
                clip_id: handle.clip_id,
                camera_id: handle.camera_id,
                path: PathBuf::new(),
                duration_ms: 0,
                size_bytes: 0,
                codec: "h264".into(),
                container: "mp4".into(),
                discarded: false,
            })
        }
        fn set_panic(&self, _panic: bool) {}
        fn is_panic(&self) -> bool {
            false
        }
        fn kind(&self) -> &'static str {
            "clip-log"
        }
        async fn drain_camera(&self, camera_id: CameraId) -> Result<(), RecorderError> {
            let next = self.drains.lock().pop_front();
            match next {
                Some(Drain::Stalls) => std::future::pending::<()>().await,
                Some(Drain::Fails) => {
                    return Err(RecorderError::Io(std::io::Error::other(
                        "a clip could not be finalized",
                    )))
                }
                Some(Drain::Until(mut done)) => {
                    let _ = done.wait_for(|done| *done).await;
                }
                None => {}
            }
            for clip_id in self.open_clips(camera_id) {
                self.calls
                    .lock()
                    .push((camera_id, ClipCall::Close(clip_id)));
            }
            Ok(())
        }
        fn remove_camera_ingester(&self, camera_id: CameraId) {
            self.calls
                .lock()
                .push((camera_id, ClipCall::IngesterRemoved));
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
            if let Some(end) = self.end_first_source.as_ref().filter(|_| built == 1) {
                return Some(Box::new(SourceUntil {
                    camera_id,
                    end: end.clone(),
                }));
            }
            Some(Box::new(nexus_pipeline::VirtualSource {
                camera_id,
                width: 512,
                height: 288,
                fps: 10,
            }))
        }
    }

    /// SPEC-088 review 8 — a supervisor that applied its zones and whose
    /// source then ends with a clip open runs them no more from that moment,
    /// not from when its final close returns, which can take as long as the
    /// recorder's drain. Until then the read said applied, and the
    /// reconciler's no-change guard, which asks the registry whether the
    /// generation still runs, kept the camera. Once the close completes, a
    /// pass restarts the camera as for any supervisor that ended on its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_supervisor_ending_with_its_final_close_pending_stops_reading_applied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let end_source = Arc::new(tokio::sync::Notify::new());
        let recorder = Arc::new(ClipLogRecorder {
            close_gate: Some(tokio::sync::Semaphore::new(0)),
            end_first_source: Some(end_source.clone()),
            ..ClipLogRecorder::default()
        });
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;
        let a = zone_revision(&[]);

        reconcile(&args).await.expect("first pass");
        wait_until(
            "precondition: the supervisor applied A and holds an open clip",
            || recorder.opens() == 1 && status(&args, &a).state == ZoneApplicationState::Applied,
        )
        .await;
        let original = entry_of(&args.handles, 7);
        end_source.notify_one();
        wait_until("the supervisor's final close began", || {
            recorder.calls_of(7) == [ClipCall::Open(1), ClipCall::Close(1)]
        })
        .await;
        let pending = status(&args, &a);
        let runs = args
            .frame_stats
            .zone_application()
            .runs(7, original.zone_generation);
        let ended_mid_close = original.task.is_finished();
        recorder
            .close_gate
            .as_ref()
            .expect("closes are gated")
            .add_permits(1 << 16);
        wait_until("the supervisor ended once its close completed", || {
            original.task.is_finished()
        })
        .await;
        reconcile(&args).await.expect("pass that restarts it");
        wait_until("the replacement applied A", || {
            let s = status(&args, &a);
            s.state == ZoneApplicationState::Applied && s.generation > original.zone_generation
        })
        .await;
        let replaced = !Arc::ptr_eq(&entry_of(&args.handles, 7).task, &original.task);
        abort_all(&args.handles);

        assert!(
            !ended_mid_close,
            "precondition: the close was still pending"
        );
        assert_eq!(
            (pending.applied_revision, pending.state, pending.error_code),
            (Some(a), ZoneApplicationState::Applying, None),
            "an ended supervisor read applied while its final close was pending"
        );
        assert!(
            !runs,
            "the registry still counted the ended generation as running, so a pass would keep it"
        );
        assert!(
            replaced,
            "the pass after the close did not restart the camera"
        );
    }

    /// SPEC-088 review 9 — the pass that finds an ended supervisor's
    /// generation no longer running can come while that supervisor's final
    /// close is still pending. It stops the camera the long way: it waits
    /// out [`SUPERVISOR_STOP_TIMEOUT`], aborts the supervisor inside its
    /// close and drains the camera. Until the drain succeeds no replacement
    /// starts and A never reads applied.
    #[tokio::test]
    async fn a_pass_during_a_pending_final_close_aborts_and_drains_before_replacing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let end_source = Arc::new(tokio::sync::Notify::new());
        let recorder = Arc::new(ClipLogRecorder {
            close_gate: Some(tokio::sync::Semaphore::new(0)),
            end_first_source: Some(end_source.clone()),
            ..ClipLogRecorder::default()
        });
        let args = Arc::new(reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await);
        let a = zone_revision(&[]);

        reconcile(&args).await.expect("first pass");
        wait_until(
            "precondition: the supervisor applied A and holds an open clip",
            || recorder.opens() == 1 && status(&args, &a).state == ZoneApplicationState::Applied,
        )
        .await;
        let original = entry_of(&args.handles, 7);
        let is_original = |h: &HandleMap| {
            h.lock()
                .get(&7)
                .is_some_and(|e| Arc::ptr_eq(&e.task, &original.task))
        };
        end_source.notify_one();
        wait_until("the supervisor's final close began", || {
            recorder.calls_of(7) == [ClipCall::Open(1), ClipCall::Close(1)]
        })
        .await;
        // The first drain does not finish within its bound.
        recorder.drains.lock().push_back(Drain::Stalls);

        // Both bounds elapse on the paused clock; every virtual second the
        // test records what the camera reads as.
        tokio::time::pause();
        let began = tokio::time::Instant::now();
        let pass = tokio::spawn({
            let args = args.clone();
            async move { reconcile(&args).await }
        });
        let mut seen = Vec::new();
        while !pass.is_finished() {
            seen.push((status(&args, &a).state, is_original(&args.handles)));
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let waited = began.elapsed();
        pass.await
            .expect("pass task")
            .expect("pass during the pending close");
        tokio::time::resume();
        let aborted_and_kept = original.task.is_finished() && is_original(&args.handles);
        let before_drain = recorder.calls_of(7);
        recorder
            .close_gate
            .as_ref()
            .expect("closes are gated")
            .add_permits(1 << 16);
        reconcile(&args).await.expect("pass whose drain succeeds");
        wait_until("the replacement applied A", || {
            let s = status(&args, &a);
            s.state == ZoneApplicationState::Applied && s.generation > original.zone_generation
        })
        .await;
        let calls = recorder.calls_of(7);
        abort_all(&args.handles);

        assert!(
            waited >= SUPERVISOR_STOP_TIMEOUT * 2,
            "the pass did not wait out the stop and then the drain: {waited:?}"
        );
        assert!(
            aborted_and_kept,
            "the supervisor was not aborted in its close, or the camera left the map"
        );
        assert!(
            seen.len() > 2
                && seen
                    .iter()
                    .all(|&(state, original)| state != ZoneApplicationState::Applied && original),
            "a replacement started or A read applied before the drain succeeded: {seen:?}"
        );
        assert_eq!(
            before_drain,
            [ClipCall::Open(1), ClipCall::Close(1)],
            "the camera was torn down before its drain succeeded"
        );
        assert_eq!(
            calls[..3],
            [
                ClipCall::Open(1),
                ClipCall::Close(1),
                ClipCall::IngesterRemoved
            ],
            "once the drain succeeded, one pass tears the camera down and replaces it"
        );
    }

    /// BUG-285 review — the restart a behavior edit triggers used to abort
    /// the old supervisor mid-await. `run_camera` finalizes its open motion
    /// clip only after its frame loop ends, which an abort skips, so every
    /// restart with motion on screen abandoned a clip: opened, never closed,
    /// its row left with no `ended_at` and its file never finalized.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_behavior_edit_closes_the_open_clip_before_the_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = Arc::new(ClipLogRecorder::default());
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;

        reconcile(&args).await.expect("initial pass");
        // The mock detector reports a person on every frame, so the motion
        // tracker declares it `Born` and the supervisor opens a clip.
        wait_until("precondition: the first supervisor opened a clip", || {
            recorder.opens() == 1
        })
        .await;
        assert_eq!(
            recorder.calls(),
            [ClipCall::Open(1)],
            "precondition: the first clip is still open when the edit lands"
        );
        let original = args
            .handles
            .lock()
            .get(&7)
            .cloned()
            .expect("camera 7 spawned");

        let mut toggled = cam(None);
        toggled.behavior.parking_lot_mode = true;
        args.store
            .upsert_camera(&toggled)
            .await
            .expect("toggle parking-lot mode");
        reconcile(&args).await.expect("pass after the edit");

        let replaced = args
            .handles
            .lock()
            .get(&7)
            .is_some_and(|now| !Arc::ptr_eq(&now.task, &original.task));
        wait_until("the replacement supervisor opened a clip", || {
            recorder.opens() == 2
        })
        .await;
        abort_all(&args.handles);
        assert!(replaced, "the behavior edit must replace the supervisor");
        let calls = recorder.calls();
        assert_eq!(
            calls[..calls.len().min(4)],
            [
                ClipCall::Open(1),
                ClipCall::Close(1),
                ClipCall::IngesterRemoved,
                ClipCall::Open(2),
            ],
            "the restart must close the old supervisor's open clip, while its ingester still \
             exists, before the replacement opens one: an aborted supervisor abandons the clip"
        );
    }

    /// SPEC-088 Z-4 — the forced stop after the handoff. A supervisor holding
    /// an open clip waits on the store (every pooled connection taken, as
    /// when the store cannot answer) past [`SUPERVISOR_STOP_TIMEOUT`], and
    /// `stop_camera` aborts it there, which skips its close. The clip must be
    /// finalized, once, before the camera's ingester goes and before the
    /// replacement opens one: left open, it keeps recording beside the
    /// replacement's, with no end, until the process exits.
    #[tokio::test]
    async fn a_forced_stop_finalizes_the_clip_the_aborted_supervisor_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = Arc::new(ClipLogRecorder::default());
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;
        recorder.wedge_store_on_open(args.store.clone());

        reconcile(&args).await.expect("initial pass");
        wait_until("precondition: the supervisor holds an open clip", || {
            recorder.opens() == 1
        })
        .await;
        let original = entry_of(&args.handles, 7);

        // Nothing below waits on I/O, so the stop's timeout elapses on the
        // runtime's paused clock while the supervisor waits on the store.
        tokio::time::pause();
        let stopped = stop_camera(&args, 7).await;
        tokio::time::resume();
        let ended = original.task.is_finished();
        recorder.unwedge_store();
        reconcile(&args)
            .await
            .expect("pass that starts the replacement");
        wait_until("the replacement opened a clip", || recorder.opens() == 2).await;
        abort_all(&args.handles);

        assert!(stopped && ended, "the wedged supervisor was not stopped");
        assert_eq!(
            recorder.calls_of(7),
            [
                ClipCall::Open(1),
                ClipCall::Close(1),
                ClipCall::IngesterRemoved,
                ClipCall::Open(2),
            ],
            "the clip the aborted supervisor held must be finalized once, while its ingester \
             exists, before the replacement opens one"
        );
    }

    /// SPEC-088 Z-4 — the supervisor has ended, but the recorder work it left
    /// (an open yet to register its clip, a close that took one) is not done
    /// within the stop's bound, or a clip could not be finalized. The camera
    /// must stay stopping: entry kept, ingester kept, no replacement. Later
    /// passes check again without waiting, and only a drain that succeeds
    /// lets one pass tear the camera down and start one replacement.
    #[tokio::test]
    async fn a_camera_stays_stopping_until_its_recorder_drain_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = Arc::new(ClipLogRecorder::default());
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;

        reconcile(&args).await.expect("initial pass");
        wait_until("precondition: the supervisor opened a clip", || {
            recorder.opens() == 1
        })
        .await;
        let original = entry_of(&args.handles, 7);
        let is_original = |h: &HandleMap| {
            h.lock()
                .get(&7)
                .is_some_and(|e| Arc::ptr_eq(&e.task, &original.task))
        };
        recorder
            .drains
            .lock()
            .extend([Drain::Stalls, Drain::Stalls, Drain::Fails]);

        // The drain's bound elapses on the paused clock.
        tokio::time::pause();
        let stopped = stop_camera(&args, 7).await;
        tokio::time::resume();
        let kept = !stopped && is_original(&args.handles) && original.task.is_finished();
        // Real time from here: a pass that waited for the drain again would
        // take 20 s.
        let stalled =
            tokio::time::timeout(std::time::Duration::from_secs(5), reconcile(&args)).await;
        let unreplaced_while_stalled = is_original(&args.handles);
        reconcile(&args).await.expect("pass whose drain fails");
        let unreplaced_after_failure = is_original(&args.handles);
        let before_success = recorder.calls_of(7);
        reconcile(&args).await.expect("pass whose drain succeeds");
        wait_until("the replacement opened a clip", || recorder.opens() == 2).await;
        let replacement = entry_of(&args.handles, 7);
        abort_all(&args.handles);

        assert!(kept, "a camera whose drain timed out left the map");
        assert!(
            matches!(stalled, Ok(Ok(()))),
            "a later pass waited for the drain again"
        );
        assert!(
            unreplaced_while_stalled && unreplaced_after_failure,
            "a replacement started before the camera's drain succeeded"
        );
        assert_eq!(
            before_success,
            [ClipCall::Open(1), ClipCall::Close(1)],
            "the camera was torn down before its drain succeeded"
        );
        assert!(!Arc::ptr_eq(&replacement.task, &original.task));
        assert_eq!(
            recorder.calls_of(7),
            [
                ClipCall::Open(1),
                ClipCall::Close(1),
                ClipCall::IngesterRemoved,
                ClipCall::Open(2),
            ],
            "once the drain succeeded, one pass tears the camera down and starts one replacement"
        );
    }

    /// PR #405 review: once a stop pass timed out on the camera's drain, a
    /// later pass drains it with no wait. The recorder work runs on in the
    /// recorder's own tasks meanwhile, so work that finishes shortly after
    /// the timed-out pass lets the next pass tear the camera down and start
    /// one replacement: the camera does not stay stopping.
    #[tokio::test]
    async fn a_drain_finishing_after_its_pass_timed_out_lets_the_next_pass_restart_the_camera() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = Arc::new(ClipLogRecorder::default());
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;
        reconcile(&args).await.expect("initial pass");
        wait_until("precondition: the supervisor opened a clip", || {
            recorder.opens() == 1
        })
        .await;
        let original = entry_of(&args.handles, 7);
        let (finish, work) = tokio::sync::watch::channel(false);
        recorder
            .drains
            .lock()
            .extend([Drain::Until(work.clone()), Drain::Until(work)]);

        // The drain's bound elapses on the paused clock.
        tokio::time::pause();
        let stopped = stop_camera(&args, 7).await;
        tokio::time::resume();
        finish.send_replace(true);
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), reconcile(&args)).await;
        wait_until("the replacement opened a clip", || recorder.opens() == 2).await;
        let replacement = entry_of(&args.handles, 7);
        abort_all(&args.handles);

        assert!(
            !stopped,
            "precondition: the first pass timed out on the drain"
        );
        assert!(
            matches!(next, Ok(Ok(()))),
            "the next pass waited on the drain"
        );
        assert!(!Arc::ptr_eq(&replacement.task, &original.task));
        assert_eq!(
            recorder.calls_of(7),
            [
                ClipCall::Open(1),
                ClipCall::Close(1),
                ClipCall::IngesterRemoved,
                ClipCall::Open(2),
            ],
            "the next pass did not tear the camera down and start one replacement"
        );
    }

    /// PR #405 review: work still in flight keeps the camera stopping across
    /// every pass that checks it, none of them waiting on it. Once it ends
    /// with a clip not finalized, the clip holds the camera stopping for its
    /// remaining attempts (SPEC-088: three in all), and the drain that
    /// releases it lets that pass restart the camera, once.
    #[tokio::test]
    async fn a_camera_stays_stopping_while_its_work_is_in_flight_and_its_clip_holds_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = Arc::new(ClipLogRecorder::default());
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;
        reconcile(&args).await.expect("initial pass");
        wait_until("precondition: the supervisor opened a clip", || {
            recorder.opens() == 1
        })
        .await;
        let original = entry_of(&args.handles, 7);
        let is_original = |h: &HandleMap| {
            h.lock()
                .get(&7)
                .is_some_and(|e| Arc::ptr_eq(&e.task, &original.task))
        };
        let (finish, work) = tokio::sync::watch::channel(false);
        recorder.drains.lock().extend([
            Drain::Until(work.clone()),
            Drain::Until(work.clone()),
            Drain::Until(work),
            Drain::Fails,
            Drain::Fails,
        ]);

        tokio::time::pause();
        let stopped = stop_camera(&args, 7).await;
        tokio::time::resume();
        let mut held = Vec::new();
        for _ in 0..2 {
            let pass =
                tokio::time::timeout(std::time::Duration::from_secs(5), reconcile(&args)).await;
            held.push(matches!(pass, Ok(Ok(()))) && is_original(&args.handles));
        }
        finish.send_replace(true);
        for _ in 0..2 {
            reconcile(&args).await.expect("pass whose drain fails");
            held.push(is_original(&args.handles));
        }
        let before_release = recorder.calls_of(7);
        reconcile(&args)
            .await
            .expect("pass whose drain releases the clip");
        wait_until("the replacement opened a clip", || recorder.opens() == 2).await;
        let replacement = entry_of(&args.handles, 7);
        abort_all(&args.handles);

        assert!(
            !stopped,
            "precondition: the first pass timed out on the drain"
        );
        assert_eq!(
            held, [true; 4],
            "a pass waited on the drain or replaced the camera before its drain succeeded"
        );
        assert_eq!(
            before_release,
            [ClipCall::Open(1), ClipCall::Close(1)],
            "the camera was torn down before its drain succeeded"
        );
        assert!(!Arc::ptr_eq(&replacement.task, &original.task));
        assert_eq!(
            recorder.calls_of(7),
            [
                ClipCall::Open(1),
                ClipCall::Close(1),
                ClipCall::IngesterRemoved,
                ClipCall::Open(2),
            ],
            "the releasing pass did not tear the camera down and start one replacement"
        );
    }

    fn entry_of(handles: &HandleMap, camera_id: CameraId) -> RunningCameraEntry {
        handles
            .lock()
            .get(&camera_id)
            .cloned()
            .expect("camera running")
    }

    /// One zone-only edit to a camera, and whether its next supervisor must
    /// still see the mock detector's person, whose box centre sits near
    /// (0.5, 0.65) of the frame: seen, it opens a clip; filtered out by the
    /// zones, it never reaches the motion lifecycle.
    type ZoneEdit = (&'static str, fn(&mut Vec<ZoneConfig>), bool);

    /// SPEC-088 (#399) — `run_camera` reads `cfg.zones` once, and the key left
    /// them out, so a saved zone edit published `config.changed`, the guard
    /// matched, and the camera kept filtering with the zones it started with
    /// until the engine restarted. Each edit changes one thing about camera
    /// 7's zones; the person shows which zones the replacement runs. Camera 8
    /// is the neighbour that must not notice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zone_only_edit_restarts_only_affected_camera_and_finalizes_clip() {
        const OVER_PERSON: [(f32, f32); 4] = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        const CORNER: [(f32, f32); 3] = [(0.0, 0.0), (0.2, 0.0), (0.0, 0.2)];
        let edits: [ZoneEdit; 6] = [
            (
                "add an exclusion zone over the person",
                |z| {
                    z.push(ZoneConfig {
                        id: "z1".into(),
                        name: "Driveway".into(),
                        polygon: OVER_PERSON.to_vec(),
                        kind: ZoneKind::Exclusion,
                        min_bbox_area_px_override: None,
                    })
                },
                false,
            ),
            (
                "change its kind to inclusion",
                |z| z[0].kind = ZoneKind::Inclusion,
                true,
            ),
            (
                "give it a minimum area above the person's box",
                |z| z[0].min_bbox_area_px_override = Some(u32::MAX),
                false,
            ),
            (
                "move its polygon off the person",
                |z| z[0].polygon = CORNER.to_vec(),
                true,
            ),
            ("rename it", |z| z[0].name = "Gate".into(), true),
            ("remove it", Vec::clear, true),
        ];

        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = Arc::new(ClipLogRecorder::default());
        let args =
            reconciler_args(recorder.clone(), dir.path(), &[cam(None), cam_with_id(8)]).await;
        // The frame of the current session the analysis loop last finished,
        // zone filter included. A restart clears it.
        let analysed = |camera_id| {
            args.cache
                .get(camera_id)
                .and_then(|e| e.objects_frame_id)
                .unwrap_or(0)
        };

        reconcile(&args).await.expect("initial pass");
        wait_until("precondition: both cameras opened a clip", || {
            recorder.opens_of(7) == 1 && recorder.opens_of(8) == 1
        })
        .await;
        // A supervisor that sees the person opens its clip by this frame, so
        // one that analyses well past it without opening one has filtered
        // the person out.
        let filtered_by = 3 * analysed(7) + 5;
        let neighbour = entry_of(&args.handles, 8);
        let mut camera = cam(None);

        for (what, edit, sees_person) in edits {
            let before = entry_of(&args.handles, 7);
            let logged = recorder.calls_of(7).len();
            let opens = recorder.opens_of(7) as i64;
            let clip_open = recorder.calls_of(7).last() == Some(&ClipCall::Open(opens));
            let sources = recorder.sources_built(7);
            edit(&mut camera.zones);
            args.store.upsert_camera(&camera).await.expect(what);
            reconcile(&args).await.expect(what);

            let now = entry_of(&args.handles, 7);
            if Arc::ptr_eq(&now.task, &before.task) {
                abort_all(&args.handles);
                panic!(
                    "{what}: the saved zones did not restart camera 7, so it keeps filtering \
                     with the zones its supervisor started with until the engine restarts"
                );
            }
            assert_eq!(
                now.key.zones, camera.zones,
                "{what}: the new supervisor is recorded as running exactly the saved zones"
            );
            if sees_person {
                wait_until(&format!("{what}: the person must open a clip"), || {
                    recorder.opens_of(7) as i64 == opens + 1
                })
                .await;
            } else {
                wait_until(&format!("{what}: the replacement must analyse"), || {
                    analysed(7) > filtered_by
                })
                .await;
                assert!(
                    args.cache.get(7).is_some_and(|e| e.objects.is_empty()),
                    "{what}: the zones must keep the person out of the frame's objects"
                );
            }
            let want: Vec<ClipCall> = [
                clip_open.then_some(ClipCall::Close(opens)),
                Some(ClipCall::IngesterRemoved),
                sees_person.then_some(ClipCall::Open(opens + 1)),
            ]
            .into_iter()
            .flatten()
            .collect();
            assert_eq!(
                recorder.calls_of(7)[logged..],
                want,
                "{what}: one stop that finalizes the open clip once, while its ingester exists, \
                 then one start that runs the saved zones"
            );
            assert_eq!(recorder.sources_built(7), sources + 1, "{what}: one start");
        }

        // Saving the same zones again is not an edit.
        let before = entry_of(&args.handles, 7);
        let logged = recorder.calls_of(7).len();
        args.store
            .upsert_camera(&camera)
            .await
            .expect("identical save");
        reconcile(&args)
            .await
            .expect("pass after an identical save");
        let unchanged = Arc::ptr_eq(&entry_of(&args.handles, 7).task, &before.task);
        let now_neighbour = entry_of(&args.handles, 8);
        abort_all(&args.handles);
        assert!(unchanged, "an identical save restarted camera 7");
        assert_eq!(
            recorder.calls_of(7).len(),
            logged,
            "an identical save stopped or reopened camera 7's recording"
        );
        assert!(
            Arc::ptr_eq(&now_neighbour.task, &neighbour.task),
            "camera 7's zone edits restarted camera 8"
        );
        assert!(
            !recorder.calls_of(8).contains(&ClipCall::IngesterRemoved)
                && recorder.sources_built(8) == 1,
            "camera 7's zone edits stopped or restarted camera 8's recording"
        );
    }

    /// Camera `id`'s zone status as the stats read builds it: the revision
    /// and enabled flag saved now against what its generations acknowledged.
    async fn zones_status(
        args: &ReconcilerArgs,
        id: CameraId,
        now: std::time::Instant,
    ) -> Option<ZonesRuntime> {
        let (cameras, _) = args
            .store
            .list_readable_cameras()
            .await
            .expect("read cameras");
        let saved = cameras.into_iter().find(|c| c.id == id)?;
        Some(args.frame_stats.zone_application().snapshot(
            id,
            &zone_revision(&saved.zones),
            saved.ingest.enabled,
            now,
        ))
    }

    /// Camera 7's status, enabled with `desired` saved, read now.
    fn status(args: &ReconcilerArgs, desired: &str) -> ZonesRuntime {
        args.frame_stats
            .zone_application()
            .snapshot(7, desired, true, std::time::Instant::now())
    }

    /// What the API's camera save does: persist, then tell the registry,
    /// under the registry's save lock.
    async fn save(args: &ReconcilerArgs, camera: &CameraConfig) {
        let _saving = args.frame_stats.zone_application().saves().write().await;
        args.store.upsert_camera(camera).await.expect("save camera");
        args.frame_stats.zone_application().request(
            camera.id,
            &zone_revision(&camera.zones),
            camera.ingest.enabled,
        );
    }

    fn corner() -> ZoneConfig {
        ZoneConfig {
            id: "z1".into(),
            name: "Corner".into(),
            polygon: vec![(0.0, 0.0), (0.2, 0.0), (0.0, 0.2)],
            kind: ZoneKind::Exclusion,
            min_bbox_area_px_override: None,
        }
    }

    /// The time a stale acknowledgement claims, which no read may show.
    const STALE: chrono::DateTime<chrono::Utc> = chrono::DateTime::<chrono::Utc>::UNIX_EPOCH;

    /// SPEC-088 — a save is applied only once the generation started with it
    /// has processed a frame: between the save and that frame the read says
    /// applying with the saved revision, never the old generation's applied,
    /// and the stopped generation can no longer acknowledge anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_zone_save_reads_applying_until_its_generation_processes_a_frame() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let registry = args.frame_stats.zone_application();
        let applied = |desired: &str| status(&args, desired).state == ZoneApplicationState::Applied;
        let unzoned = zone_revision(&[]);

        reconcile(&args).await.expect("first pass");
        let first = zones_status(&args, 7, std::time::Instant::now())
            .await
            .expect("a started camera reports its zones");
        assert_eq!(first.desired_revision, unzoned);
        wait_until("the first generation applied its zones", || {
            applied(&unzoned)
        })
        .await;
        let first_request = registry
            .current_request(7, first.generation)
            .expect("the first generation runs the saved zones");

        let mut camera = cam(None);
        camera.zones.push(ZoneConfig {
            id: "z1".into(),
            name: "Corner".into(),
            polygon: vec![(0.0, 0.0), (0.2, 0.0), (0.0, 0.2)],
            kind: ZoneKind::Exclusion,
            min_bbox_area_px_override: None,
        });
        let zoned = zone_revision(&camera.zones);
        args.store.upsert_camera(&camera).await.expect("save zones");
        let saved = zones_status(&args, 7, std::time::Instant::now())
            .await
            .expect("still reported");
        assert_eq!(
            (
                saved.desired_revision,
                saved.applied_revision,
                saved.generation,
                saved.state
            ),
            (
                zoned.clone(),
                Some(unzoned.clone()),
                first.generation,
                ZoneApplicationState::Applying
            ),
            "a read after the save, before the restart, must not keep the old verdict"
        );

        reconcile(&args).await.expect("pass after the save");
        let restarted = zones_status(&args, 7, std::time::Instant::now())
            .await
            .expect("still reported");
        assert!(
            restarted.generation > first.generation,
            "the restart is a new generation"
        );
        assert!(
            restarted.state != ZoneApplicationState::Applied
                || restarted.applied_revision.as_deref() == Some(zoned.as_str()),
            "only the saved revision can read applied: {restarted:?}"
        );
        wait_until("the new generation applied the saved zones", || {
            applied(&zoned)
        })
        .await;
        let now = status(&args, &zoned);
        assert_eq!(
            (
                now.applied_revision.as_deref(),
                now.generation,
                now.applied_generation
            ),
            (
                Some(zoned.as_str()),
                restarted.generation,
                Some(restarted.generation)
            )
        );
        assert!(now.applied_at.is_some());

        registry.acknowledge(7, first.generation, first_request, &unzoned, STALE);
        let after_stale = status(&args, &zoned);
        abort_all(&args.handles);
        assert_eq!(
            after_stale, now,
            "the stopped generation's late acknowledgement must be ignored"
        );
    }

    /// SPEC-088 review — the sequence the review found: A applied, B saved,
    /// the generation running A stopped, and A saved again before the
    /// replacement starts (it may be awaiting its codec probe or hydration).
    /// No supervisor runs A, so the read must not say applied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stopped_generation_does_not_read_applied_before_its_replacement_starts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let a = zone_revision(&[]);
        reconcile(&args).await.expect("first pass");
        wait_until("the first generation applied A", || {
            status(&args, &a).state == ZoneApplicationState::Applied
        })
        .await;

        let mut b = cam(None);
        b.zones.push(ZoneConfig {
            id: "z1".into(),
            name: "Corner".into(),
            polygon: vec![(0.0, 0.0), (0.2, 0.0), (0.0, 0.2)],
            kind: ZoneKind::Exclusion,
            min_bbox_area_px_override: None,
        });
        args.store.upsert_camera(&b).await.expect("save B");
        assert!(stop_camera(&args, 7).await, "the supervisor stops");
        args.store.upsert_camera(&cam(None)).await.expect("save A");
        let read = zones_status(&args, 7, std::time::Instant::now())
            .await
            .expect("still reported");
        abort_all(&args.handles);

        assert_eq!(
            (read.desired_revision, read.applied_revision, read.state),
            (a.clone(), Some(a), ZoneApplicationState::Applying),
            "a stopped generation's zones read applied"
        );
    }

    /// Errs on every frame, counting them.
    #[derive(Default)]
    struct FailingDetector {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl nexus_inference::Detector for FailingDetector {
        async fn detect(
            &self,
            _frame: &nexus_types::Frame,
            _prompts: &[String],
        ) -> Result<Vec<nexus_types::Detection>, nexus_inference::InferenceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(nexus_inference::InferenceError::Failed(
                "every frame fails".into(),
            ))
        }

        fn name(&self) -> &'static str {
            "failing"
        }
    }

    /// `spawn_supervisor`'s spawn of camera 7 with `detector` in place of the
    /// router's.
    fn spawn_with_detector(
        args: &ReconcilerArgs,
        detector: Arc<dyn nexus_inference::Detector>,
    ) -> nexus_pipeline::CameraHandle {
        spawn_camera(
            cam(None),
            detector,
            None,
            Arc::from(nexus_tracker::build_tracker(&args.tracker_cfg)),
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
            512,
            288,
            args.sighting_hook.clone(),
            args.sighting_cfg,
            Vec::new(),
            args.sighting_persist.clone(),
            None,
            args.sink_router.clone(),
            args.alert_clip_schedule_gate.clone(),
        )
    }

    /// Parks every `detect` call until the test lets it through, counting
    /// the calls begun, so a test can act while a frame is being processed.
    struct GatedDetector {
        calls: AtomicUsize,
        gate: tokio::sync::Semaphore,
    }

    #[async_trait::async_trait]
    impl nexus_inference::Detector for GatedDetector {
        async fn detect(
            &self,
            _frame: &nexus_types::Frame,
            _prompts: &[String],
        ) -> Result<Vec<nexus_types::Detection>, nexus_inference::InferenceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.gate
                .acquire()
                .await
                .expect("gate is never closed")
                .forget();
            Ok(Vec::new())
        }

        fn name(&self) -> &'static str {
            "gated"
        }
    }

    /// SPEC-088 review 8 — a frame acknowledges the request current when it
    /// began, never one saved while it was being processed. A→B→A saved
    /// while a frame of the generation that applied A is in its detector is
    /// a new request for A: that frame must not certify it; the next does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_save_landing_mid_frame_is_acknowledged_only_by_a_later_frame() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let a = zone_revision(&[]);
        let mut b_cam = cam(None);
        b_cam.zones.push(corner());
        let detector = Arc::new(GatedDetector {
            calls: AtomicUsize::new(0),
            gate: tokio::sync::Semaphore::new(0),
        });
        let handle = spawn_with_detector(&args, detector.clone());
        let in_detector = |n: usize| {
            let detector = detector.clone();
            move || detector.calls.load(Ordering::SeqCst) >= n
        };

        wait_until("frame 1 is in the detector", in_detector(1)).await;
        detector.gate.add_permits(1);
        // The loop is sequential: frame 2 in the detector means frame 1 is done.
        wait_until("frame 2 is in the detector", in_detector(2)).await;
        let before = status(&args, &a);
        save(&args, &b_cam).await;
        save(&args, &cam(None)).await;
        detector.gate.add_permits(1);
        wait_until("frame 3 is in the detector", in_detector(3)).await;
        let after_frame_2 = status(&args, &a);
        detector.gate.add_permits(1 << 16);
        wait_until("a later frame applied the new request for A", || {
            status(&args, &a).state == ZoneApplicationState::Applied
        })
        .await;
        let _ = handle.stop.send(true);
        handle.task.await.expect("supervisor stops");

        assert_eq!(
            before.state,
            ZoneApplicationState::Applied,
            "precondition: frame 1 applied A"
        );
        assert_eq!(
            after_frame_2.state,
            ZoneApplicationState::Applying,
            "the frame in flight when A→B→A was saved acknowledged that newer request"
        );
    }

    /// SPEC-088 — frames that never get through the detector never reach the
    /// zone filters, so the generation stays unacknowledged: applying, then
    /// failed once 30 s have passed since its start. A requested stop is not
    /// a failure of its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_detector_that_fails_every_frame_never_applies_the_zones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let detector = Arc::new(FailingDetector::default());
        let handle = spawn_with_detector(&args, detector.clone());
        wait_until("the detector failed on several frames", || {
            detector.calls.load(Ordering::SeqCst) >= 5
        })
        .await;

        let now = std::time::Instant::now();
        let s = zones_status(&args, 7, now).await.expect("reported");
        assert_eq!(
            (s.applied_revision, s.state, s.error_code),
            (None, ZoneApplicationState::Applying, None)
        );
        let late = zones_status(&args, 7, now + APPLICATION_TIMEOUT)
            .await
            .expect("reported");
        assert_eq!(
            (late.applied_revision, late.state, late.error_code),
            (
                None,
                ZoneApplicationState::Failed,
                Some(ZoneApplicationError::ApplicationTimeout)
            )
        );

        let _ = handle.stop.send(true);
        handle.task.await.expect("supervisor stops");
        assert_eq!(
            zones_status(&args, 7, now)
                .await
                .expect("reported")
                .error_code,
            None,
            "a requested stop is not a supervisor that exited"
        );
    }

    /// A supervisor that ends on its own before any frame reads failed at
    /// once rather than applying until the timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_supervisor_that_ends_before_its_first_frame_reads_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NoFrames)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;

        reconcile(&args).await.expect("first pass");
        wait_until("the supervisor ended", || {
            supervisor_ended(&args.handles, 7)
        })
        .await;
        let s = zones_status(&args, 7, std::time::Instant::now())
            .await
            .expect("reported");
        abort_all(&args.handles);
        assert_eq!(
            (s.state, s.error_code),
            (
                ZoneApplicationState::Failed,
                Some(ZoneApplicationError::SupervisorExited)
            )
        );
    }

    /// SPEC-088 Z-5 — a supervisor that applied its zones and then ends on
    /// its own runs them no more: the read says applying from that moment,
    /// failed once the timeout passes with no replacement, not applied until
    /// the next reconcile pass. The replacement generation applies as usual.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_supervisor_that_ends_after_applying_its_zones_stops_reading_applied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::EndsOnce)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let a = zone_revision(&[]);

        reconcile(&args).await.expect("first pass");
        wait_until("the supervisor ended", || {
            supervisor_ended(&args.handles, 7)
        })
        .await;
        let ended = std::time::Instant::now();
        let exited = zones_status(&args, 7, ended).await.expect("reported");
        let late = zones_status(&args, 7, ended + APPLICATION_TIMEOUT)
            .await
            .expect("reported");
        reconcile(&args).await.expect("pass that restarts it");
        wait_until("the replacement applied the zones", || {
            let s = status(&args, &a);
            s.state == ZoneApplicationState::Applied && s.generation > exited.generation
        })
        .await;
        abort_all(&args.handles);

        assert_eq!(
            (exited.applied_revision, exited.state, exited.error_code),
            (Some(a.clone()), ZoneApplicationState::Applying, None),
            "a supervisor that applied its zones and ended on its own still reads applied"
        );
        assert_eq!(
            (late.state, late.error_code),
            (
                ZoneApplicationState::Failed,
                Some(ZoneApplicationError::ApplicationTimeout)
            )
        );
    }

    /// SPEC-088 — a restart with the same zones applies nothing until the
    /// new generation processes a frame of its own, and the old generation's
    /// late callbacks, carrying its token, count for nothing afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_same_zones_restart_reads_applying_until_the_new_generation_acknowledges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let registry = args.frame_stats.zone_application();
        let a = zone_revision(&[]);
        reconcile(&args).await.expect("first pass");
        wait_until("the first generation applied A", || {
            status(&args, &a).state == ZoneApplicationState::Applied
        })
        .await;
        let g1 = entry_of(&args.handles, 7).zone_generation;
        let stale = registry.current_request(7, g1).expect("g1 runs A");

        assert!(stop_camera(&args, 7).await, "the supervisor stops");
        let stopped = status(&args, &a);
        registry.acknowledge(7, g1, stale, &a, STALE);
        let after_stale = status(&args, &a);
        reconcile(&args).await.expect("pass that restarts it");
        let g2 = entry_of(&args.handles, 7).zone_generation;
        wait_until("the restarted generation applied A", || {
            status(&args, &a).applied_generation == Some(g2)
        })
        .await;
        let applied = status(&args, &a);
        registry.acknowledge(7, g1, stale, &a, STALE);
        registry.fail(7, g1, stale, ZoneApplicationError::SupervisorExited);
        let after_late = status(&args, &a);
        abort_all(&args.handles);

        assert_eq!(
            (
                stopped.applied_revision.as_deref(),
                stopped.generation,
                stopped.applied_generation,
                stopped.state
            ),
            (
                Some(a.as_str()),
                g1,
                Some(g1),
                ZoneApplicationState::Applying
            ),
            "a stopped generation still read applied"
        );
        assert_eq!(
            after_stale, stopped,
            "a stopped generation's acknowledgement counted"
        );
        assert!(g2 > g1, "the restart is a new generation");
        assert_eq!(
            (applied.generation, applied.state),
            (g2, ZoneApplicationState::Applied)
        );
        assert_eq!(
            after_late, applied,
            "the old generation's late callbacks counted"
        );
    }

    /// SPEC-088 — A→B→A through restarts: while the generation running B is
    /// current, A saved again cannot be satisfied by A's earlier generation
    /// or acknowledged by B's; only the replacement A generation applies it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_to_b_to_a_through_restarts_is_applied_only_by_the_replacement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let registry = args.frame_stats.zone_application();
        let a = zone_revision(&[]);
        let mut b_cam = cam(None);
        b_cam.zones.push(corner());
        let b = zone_revision(&b_cam.zones);
        reconcile(&args).await.expect("first pass");
        wait_until("g1 applied A", || {
            status(&args, &a).state == ZoneApplicationState::Applied
        })
        .await;
        let g1 = entry_of(&args.handles, 7).zone_generation;
        let token_a = registry.current_request(7, g1).expect("g1 runs A");

        save(&args, &b_cam).await;
        reconcile(&args).await.expect("pass that starts B");
        let g2 = entry_of(&args.handles, 7).zone_generation;
        wait_until("g2 applied B", || {
            status(&args, &b).applied_generation == Some(g2)
        })
        .await;
        let token_b = registry.current_request(7, g2).expect("g2 runs B");

        save(&args, &cam(None)).await;
        let requested = status(&args, &a);
        registry.acknowledge(7, g1, token_a, &a, STALE);
        registry.acknowledge(7, g2, token_b, &b, STALE);
        let after_stale = status(&args, &a);
        reconcile(&args).await.expect("pass that starts A again");
        let g3 = entry_of(&args.handles, 7).zone_generation;
        wait_until("g3 applied A", || {
            status(&args, &a).applied_generation == Some(g3)
        })
        .await;
        let applied = status(&args, &a);
        registry.acknowledge(7, g1, token_a, &a, STALE);
        registry.acknowledge(7, g2, token_b, &b, STALE);
        registry.fail(7, g1, token_a, ZoneApplicationError::SupervisorExited);
        registry.fail(7, g2, token_b, ZoneApplicationError::SupervisorExited);
        let after_late = status(&args, &a);
        abort_all(&args.handles);

        assert_eq!(
            (
                requested.applied_revision.as_deref(),
                requested.generation,
                requested.applied_generation,
                requested.state
            ),
            (
                Some(b.as_str()),
                g2,
                Some(g2),
                ZoneApplicationState::Applying
            ),
            "A saved again read applied while B's generation runs"
        );
        assert_eq!(
            after_stale, requested,
            "A/g1 or B/g2 acknowledged the new A request"
        );
        assert!(g3 > g2 && g2 > g1);
        assert_eq!(
            (
                applied.applied_revision.as_deref(),
                applied.generation,
                applied.state
            ),
            (Some(a.as_str()), g3, ZoneApplicationState::Applied)
        );
        assert_eq!(
            after_late, applied,
            "an earlier generation's late callback counted"
        );
    }

    /// SPEC-088 — A→B→A saved between two passes, with the generation
    /// running A never stopped: the acknowledgement taken before B no longer
    /// counts, and that same generation's next frame applies A again,
    /// without a restart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_to_b_to_a_between_passes_reapplies_on_the_running_generations_next_frame() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let registry = args.frame_stats.zone_application();
        let a = zone_revision(&[]);
        let mut b_cam = cam(None);
        b_cam.zones.push(corner());
        reconcile(&args).await.expect("first pass");
        wait_until("g1 applied A", || {
            status(&args, &a).state == ZoneApplicationState::Applied
        })
        .await;
        let before = entry_of(&args.handles, 7);
        let g1 = before.zone_generation;
        let first = registry.current_request(7, g1).expect("g1 runs A");

        let saved_at = chrono::Utc::now();
        save(&args, &b_cam).await;
        save(&args, &cam(None)).await;
        let again = registry.current_request(7, g1);
        registry.acknowledge(7, g1, first, &a, STALE);
        wait_until("g1's next frame applied the new request for A", || {
            let s = status(&args, &a);
            s.state == ZoneApplicationState::Applied && s.applied_at.is_some_and(|t| t >= saved_at)
        })
        .await;
        let applied = status(&args, &a);
        reconcile(&args).await.expect("pass after A→B→A");
        let unchanged = Arc::ptr_eq(&entry_of(&args.handles, 7).task, &before.task);
        abort_all(&args.handles);

        assert!(
            again.is_some_and(|t| t != first),
            "A→B→A is not a new request: {again:?} after {first:?}"
        );
        assert_eq!(
            (applied.generation, applied.applied_generation),
            (g1, Some(g1)),
            "a restart applied it, not the running generation"
        );
        assert!(unchanged, "A→B→A between passes restarted the camera");
    }

    /// SPEC-088 — disabled is inactive at once, from the saved flag a read
    /// sees, with no deadline and no late callback able to change it; a save
    /// while disabled starts nothing; enabling needs a fresh generation's
    /// acknowledgement; deletion alone forgets the camera.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disabled_camera_reads_inactive_and_enabling_needs_a_fresh_acknowledgement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let registry = args.frame_stats.zone_application();
        let a = zone_revision(&[]);
        let later = || std::time::Instant::now() + 2 * APPLICATION_TIMEOUT;
        let read = |s: ZonesRuntime| {
            (
                s.desired_revision,
                s.applied_revision,
                s.generation,
                s.applied_generation,
                s.state,
                s.error_code,
            )
        };
        reconcile(&args).await.expect("first pass");
        wait_until("g1 applied A", || {
            status(&args, &a).state == ZoneApplicationState::Applied
        })
        .await;
        let g1 = entry_of(&args.handles, 7).zone_generation;
        let token = registry.current_request(7, g1).expect("g1 runs A");
        let inactive_a = (
            a.clone(),
            Some(a.clone()),
            g1,
            Some(g1),
            ZoneApplicationState::Inactive,
            None,
        );

        let mut disabled = cam(None);
        disabled.ingest.enabled = false;
        args.store.upsert_camera(&disabled).await.expect("disable");
        let at_once = zones_status(&args, 7, std::time::Instant::now()).await;
        registry.acknowledge(7, g1, token, &a, STALE);
        registry.fail(7, g1, token, ZoneApplicationError::SupervisorExited);
        let after_late = zones_status(&args, 7, std::time::Instant::now()).await;
        reconcile(&args).await.expect("pass after the disable");
        let stopped = !args.handles.lock().contains_key(&7);
        let long_after = zones_status(&args, 7, later()).await;

        let mut disabled_b = disabled.clone();
        disabled_b.zones.push(corner());
        let b = zone_revision(&disabled_b.zones);
        save(&args, &disabled_b).await;
        reconcile(&args)
            .await
            .expect("pass after a save while disabled");
        let started = args.handles.lock().contains_key(&7);
        let saved_disabled = zones_status(&args, 7, later()).await;

        let mut enabled_b = disabled_b.clone();
        enabled_b.ingest.enabled = true;
        save(&args, &enabled_b).await;
        let enabled = zones_status(&args, 7, std::time::Instant::now()).await;
        reconcile(&args).await.expect("pass after the enable");
        let g2 = entry_of(&args.handles, 7).zone_generation;
        wait_until("g2 applied B", || {
            status(&args, &b).applied_generation == Some(g2)
        })
        .await;
        let applied = zones_status(&args, 7, std::time::Instant::now()).await;

        args.store.delete_camera(7).await.expect("delete");
        reconcile(&args).await.expect("pass after the delete");
        let deleted = zones_status(&args, 7, std::time::Instant::now()).await;
        args.store
            .upsert_camera(&cam(None))
            .await
            .expect("re-create");
        let recreated = zones_status(&args, 7, std::time::Instant::now()).await;
        abort_all(&args.handles);

        assert_eq!(
            at_once.map(read),
            Some(inactive_a.clone()),
            "a disable read before the pass"
        );
        assert_eq!(
            after_late.map(read),
            Some(inactive_a.clone()),
            "a late callback counted"
        );
        assert!(stopped, "the disabled camera still runs");
        assert_eq!(
            long_after.map(read),
            Some(inactive_a),
            "inactive is not timed out"
        );
        assert!(!started, "a save while disabled started a generation");
        assert_eq!(
            saved_disabled.map(read),
            Some((
                b.clone(),
                Some(a.clone()),
                g1,
                Some(g1),
                ZoneApplicationState::Inactive,
                None
            ))
        );
        assert_eq!(
            enabled.map(|s| (s.state, s.error_code)),
            Some((ZoneApplicationState::Applying, None)),
            "an enable read applied or failed before any generation ran B"
        );
        assert!(g2 > g1);
        assert_eq!(
            applied.map(|s| (s.applied_revision, s.state)),
            Some((Some(b), ZoneApplicationState::Applied))
        );
        assert_eq!(deleted, None, "a deleted camera is reported");
        assert_eq!(
            recreated.map(|s| (s.applied_revision, s.applied_generation)),
            Some((None, None)),
            "the deleted camera's history outlived it"
        );
    }

    /// SPEC-088 — a pass is a request too: zones saved by a writer that
    /// bypasses the API's save, and read by nobody, still let the generation
    /// the pass starts for them acknowledge, rather than time out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pass_requests_the_zones_it_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let registry = args.frame_stats.zone_application();
        reconcile(&args).await.expect("first pass");
        wait_until("g1 applied A", || {
            status(&args, &zone_revision(&[])).state == ZoneApplicationState::Applied
        })
        .await;
        let mut b_cam = cam(None);
        b_cam.zones.push(corner());
        args.store
            .upsert_camera(&b_cam)
            .await
            .expect("save B behind the API");
        reconcile(&args).await.expect("pass that starts B");
        let g2 = entry_of(&args.handles, 7).zone_generation;
        wait_until("g2 can acknowledge B with no read", || {
            registry.current_request(7, g2).is_some()
        })
        .await;
        abort_all(&args.handles);
    }

    /// SPEC-088 — a running camera deleted and re-created with the same id
    /// and config before any pass saw the delete: the registry forgot the
    /// running supervisor's generation, so the next pass restarts it rather
    /// than keep a supervisor nothing it processes can acknowledge, and the
    /// camera reads applied again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_camera_deleted_and_re_created_before_a_pass_applies_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let a = zone_revision(&[]);
        reconcile(&args).await.expect("first pass");
        wait_until("g1 applied A", || {
            status(&args, &a).state == ZoneApplicationState::Applied
        })
        .await;
        let g1 = entry_of(&args.handles, 7).zone_generation;

        // What the API's delete does, then the same row saved again.
        args.store.delete_camera(7).await.expect("delete");
        args.frame_stats.zone_application().remove(7);
        save(&args, &cam(None)).await;
        reconcile(&args).await.expect("pass after the re-create");
        let g2 = entry_of(&args.handles, 7).zone_generation;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while status(&args, &a).state != ZoneApplicationState::Applied
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let read = status(&args, &a);
        abort_all(&args.handles);

        assert!(g2 > g1, "the pass kept the supervisor the registry forgot");
        assert_eq!(
            (read.state, read.generation, read.applied_generation),
            (ZoneApplicationState::Applied, g2, Some(g2)),
            "the re-created camera never applied its zones"
        );
    }

    /// What [`GatedOpenRecorder`] saw, in the order it saw it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum GateCall {
        /// A supervisor called `open` and is waiting on the gate.
        OpenBlocked,
        /// An `open` got through the gate and returned.
        OpenCompleted,
        /// The test's stand-in for `main`'s `recorder.shutdown()`.
        RecorderShutdown,
    }

    /// Holds every `open` at a gate the test releases, and logs when one
    /// completes, so a test can park a supervisor mid-frame the way a slow
    /// `recorder.open` does and see whether it outlives the recorder's
    /// shutdown. Frames come from a `VirtualSource` that never ends.
    struct GatedOpenRecorder {
        calls: Mutex<Vec<GateCall>>,
        /// No permits until [`Self::release`]: an `open` waits for one.
        gate: tokio::sync::Semaphore,
    }

    impl GatedOpenRecorder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                gate: tokio::sync::Semaphore::new(0),
            })
        }

        fn calls(&self) -> Vec<GateCall> {
            self.calls.lock().clone()
        }

        /// Lets every `open`, this one and later ones, through.
        fn release(&self) {
            self.gate.add_permits(1 << 16);
        }

        fn shutdown(&self) {
            self.calls.lock().push(GateCall::RecorderShutdown);
        }
    }

    #[async_trait::async_trait]
    impl ClipRecorder for GatedOpenRecorder {
        async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
            self.calls.lock().push(GateCall::OpenBlocked);
            self.gate
                .acquire()
                .await
                .expect("gate is never closed")
                .forget();
            self.calls.lock().push(GateCall::OpenCompleted);
            Ok(ClipHandle {
                clip_id: 1,
                camera_id: args.camera_id,
            })
        }
        async fn close(
            &self,
            handle: ClipHandle,
            _args: ClipFinal,
        ) -> Result<ClipMeta, RecorderError> {
            Ok(ClipMeta {
                clip_id: handle.clip_id,
                camera_id: handle.camera_id,
                path: PathBuf::new(),
                duration_ms: 0,
                size_bytes: 0,
                codec: "h264".into(),
                container: "mp4".into(),
                discarded: false,
            })
        }
        fn set_panic(&self, _panic: bool) {}
        fn is_panic(&self) -> bool {
            false
        }
        fn kind(&self) -> &'static str {
            "gated-open"
        }
        fn remove_camera_ingester(&self, _camera_id: CameraId) {}
        fn shared_frame_source(
            &self,
            camera_id: CameraId,
        ) -> Option<Box<dyn nexus_pipeline::FrameSource + Send>> {
            Some(Box::new(nexus_pipeline::VirtualSource {
                camera_id,
                width: 512,
                height: 288,
                fps: 10,
            }))
        }
    }

    /// BUG-285 review — a shutdown that lands while `stop_camera` waits for a
    /// supervisor to stop. `main` aborts the reconciler without joining it,
    /// aborts the supervisors it finds in the handle map, then shuts the
    /// recorder down. `stop_camera` used to take the entry out of the map
    /// first, so the supervisor it was stopping, a frame paused in
    /// `recorder.open` here, was in nobody's sweep: it kept running past the
    /// recorder's shutdown and could open a clip nothing would finalize. The
    /// steps below replay `main.rs`'s shutdown order — abort the reconciler,
    /// drain and abort the handle map, shut the recorder down — and do not
    /// join the reconciler, the stricter case; keep them in step with it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shutdown_during_a_behavior_restart_aborts_the_stopping_supervisor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = GatedOpenRecorder::new();
        let args = Arc::new(reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await);

        reconcile(&args).await.expect("initial pass");
        // The mock detector reports a person on every frame, so the first
        // frame to reach the supervisor's loop calls `open`, which waits.
        wait_until(
            "precondition: the supervisor is blocked inside recorder.open",
            || recorder.calls() == [GateCall::OpenBlocked],
        )
        .await;
        let original = args
            .handles
            .lock()
            .get(&7)
            .cloned()
            .expect("camera 7 spawned");

        let mut toggled = cam(None);
        toggled.behavior.parking_lot_mode = true;
        args.store
            .upsert_camera(&toggled)
            .await
            .expect("toggle parking-lot mode");
        let reconciling = tokio::spawn({
            let args = args.clone();
            async move { reconcile(&args).await }
        });
        // The supervisor cannot honour the stop while it waits on the gate,
        // so the reconciler stays inside `stop_camera` from here on.
        wait_until("the reconciler asked the supervisor to stop", || {
            *original.stop.borrow()
        })
        .await;

        // `main`'s shutdown, in its order: the reconciler is aborted and not
        // awaited, every supervisor still in the map is aborted, and only
        // then does the recorder shut down.
        reconciling.abort();
        for (_, entry) in args.handles.lock().drain() {
            entry.task.abort();
        }
        recorder.shutdown();

        wait_until(
            "shutdown never aborted the supervisor that stop_camera was stopping",
            || original.task.is_finished(),
        )
        .await;
        recorder.release();
        // Guards against a second supervisor the sweep missed: once the
        // stopped one is gone, only an escapee could still complete an
        // `open`, so this is a bounded wait for one that must not happen.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        abort_all(&args.handles);

        let calls = recorder.calls();
        let after_shutdown = &calls[calls
            .iter()
            .position(|c| *c == GateCall::RecorderShutdown)
            .expect("the shutdown marker was logged")..];
        assert!(
            !after_shutdown.contains(&GateCall::OpenCompleted),
            "a clip was opened after the recorder shut down: {calls:?}"
        );
        assert!(
            original.task.is_finished(),
            "the supervisor outlived shutdown"
        );
    }

    /// SPEC-088 Z-4 — the forced stop. A supervisor parked inside
    /// `recorder.open` cannot honour a stop, so after
    /// [`SUPERVISOR_STOP_TIMEOUT`] `stop_camera` aborts it. The abort lands at
    /// the supervisor's next await, and its entry used to leave the map at
    /// once, out of shutdown's sweep while a supervisor still mid-poll could
    /// finish that open. `stop_camera` must return only once the aborted
    /// supervisor has ended, and its open must never complete.
    #[tokio::test]
    async fn a_forced_stop_ends_the_supervisor_before_it_leaves_the_map() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = GatedOpenRecorder::new();
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;

        reconcile(&args).await.expect("initial pass");
        wait_until(
            "precondition: the supervisor is blocked inside recorder.open",
            || recorder.calls() == [GateCall::OpenBlocked],
        )
        .await;
        let original = entry_of(&args.handles, 7);

        // Nothing below waits on I/O, so the stop's timeout can elapse on the
        // runtime's paused clock.
        tokio::time::pause();
        stop_camera(&args, 7).await;
        let ended = original.task.is_finished();
        let in_map = args.handles.lock().contains_key(&7);
        recorder.release();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(
            ended,
            "stop_camera returned before the supervisor it aborted had ended"
        );
        assert!(!in_map, "a stopped camera stays out of the map");
        assert_eq!(
            recorder.calls(),
            [GateCall::OpenBlocked],
            "the aborted supervisor's open completed after its stop"
        );
    }

    /// SPEC-088 review — a supervisor wedged in synchronous code (blocking
    /// storage work, say) outlives both of `stop_camera`'s waits, its abort
    /// included. It must stay in the map, where shutdown's sweep finds it,
    /// with no replacement started beside it, and later passes must not wait
    /// for it again; once it has ended, one pass starts exactly one
    /// replacement.
    #[tokio::test]
    async fn a_supervisor_that_outlives_its_abort_stays_tracked_and_unreplaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let mut toggled = cam(None);
        toggled.behavior.parking_lot_mode = true;
        let args = reconciler_args(recorder, dir.path(), &[toggled]).await;
        // Started for the camera before the toggle, on a runtime of its own so
        // it can block a thread where no abort lands until the test lets it go.
        let wedge_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let (unwedge, wedged) = std::sync::mpsc::channel::<()>();
        let (entering, entered) = std::sync::mpsc::channel::<()>();
        let original = RunningCameraEntry {
            task: Arc::new(wedge_rt.spawn(async move {
                let _stop_rx = stop_rx;
                let _ = entering.send(());
                let _ = wedged.recv();
                std::future::pending::<()>().await
            })),
            stop: Arc::new(stop),
            key: EntryKey::wanted(&args, &cam(None)),
            sessions: Sessions {
                main_pending: None,
                analysis: None,
            },
            zone_generation: 0,
        };
        args.handles.lock().insert(7, original.clone());
        let is_original = |h: &HandleMap| {
            h.lock()
                .get(&7)
                .is_some_and(|e| Arc::ptr_eq(&e.task, &original.task))
        };

        // No await is left between its signal and its block: an abort sent
        // from here cannot land before it.
        entered
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the supervisor never reached its blocking receive");
        // Both waits elapse on the paused clock; the abort cannot land.
        tokio::time::pause();
        let stopped = stop_camera(&args, 7).await;
        tokio::time::resume();
        let kept = !stopped && is_original(&args.handles);
        // Real time from here: a pass that waited for it again would take 40 s.
        let pass = tokio::time::timeout(std::time::Duration::from_secs(5), reconcile(&args)).await;
        let unreplaced = is_original(&args.handles);

        unwedge.send(()).expect("wedged task waits");
        wait_until("the aborted supervisor ended", || {
            original.task.is_finished()
        })
        .await;
        reconcile(&args).await.expect("pass after it ended");
        let replacement = entry_of(&args.handles, 7);
        reconcile(&args).await.expect("pass with nothing to change");
        let after = entry_of(&args.handles, 7);
        abort_all(&args.handles);
        wedge_rt.shutdown_background();

        assert!(kept, "a supervisor that outlived its abort left the map");
        assert!(
            matches!(pass, Ok(Ok(()))),
            "a later pass waited for the aborted supervisor again"
        );
        assert!(unreplaced, "a replacement started beside a live supervisor");
        assert!(
            !Arc::ptr_eq(&replacement.task, &original.task)
                && !replacement.task.is_finished()
                && Arc::ptr_eq(&after.task, &replacement.task),
            "once it ended, exactly one replacement must start"
        );
    }

    /// One named `CameraBehavior` edit, for the tables below.
    type BehaviorMutation = (&'static str, fn(&mut CameraBehavior));

    /// The key a camera's entry records, with `behavior` in place of the
    /// default. The ingest half is held fixed so only behavior can differ.
    fn key_with(behavior: &CameraBehavior) -> EntryKey {
        let c = cam(None);
        EntryKey {
            url: c.ingest.url.to_string(),
            supervisor_dims: (512, 288),
            codec: c.ingest.codec,
            analysis_url: None,
            score_threshold: None,
            behavior: behavior.clone(),
            zones: vec![],
        }
    }

    /// The defect is a class, not one field: each of these is consumed at
    /// spawn and then frozen in the task's `cfg` snapshot, so each needs a
    /// restart to take effect. The second assertion pins the name the
    /// restart log reports — a mis-paired arm in a fourteen-way chain of
    /// same-typed `Option<u32>` fields is an easy copy-paste and otherwise
    /// invisible.
    #[test]
    fn every_spawn_frozen_behavior_field_changes_the_key() {
        let before = CameraBehavior::default();
        let mutations: Vec<BehaviorMutation> = vec![
            ("parking_lot_mode", |b| b.parking_lot_mode = true),
            ("anchor_ttl_secs", |b| b.anchor_ttl_secs = Some(900)),
            ("detector_skip_crowded_threshold", |b| {
                b.detector_skip_crowded_threshold = Some(25)
            }),
            ("detector_skip_every_n_frames", |b| {
                b.detector_skip_every_n_frames = Some(3)
            }),
            ("detector_downscale_crowded_threshold", |b| {
                b.detector_downscale_crowded_threshold = Some(30)
            }),
            ("detector_downscale_sustained_secs", |b| {
                b.detector_downscale_sustained_secs = Some(60)
            }),
            ("supervisor_downscale_crowded_threshold", |b| {
                b.supervisor_downscale_crowded_threshold = Some(40)
            }),
            ("supervisor_downscale_sustained_secs", |b| {
                b.supervisor_downscale_sustained_secs = Some(60)
            }),
            ("supervisor_upscale_clear_secs", |b| {
                b.supervisor_upscale_clear_secs = Some(300)
            }),
            ("supervisor_downscale_to_width", |b| {
                b.supervisor_downscale_to_width = Some(640)
            }),
            ("tile_enabled", |b| b.tile_enabled = Some(true)),
            ("tile_trigger", |b| b.tile_trigger = Some(12)),
            ("tile_max_per_frame", |b| b.tile_max_per_frame = Some(4)),
            ("tile_grid", |b| b.tile_grid = Some(TileGridConfig::G3x3)),
            ("tile_discovery_cells", |b| b.tile_discovery_cells = vec![2]),
            ("tile_discovery_revisit_secs", |b| {
                b.tile_discovery_revisit_secs = Some(8)
            }),
        ];

        for (field, mutate) in mutations {
            let mut after = before.clone();
            mutate(&mut after);
            assert!(
                key_with(&before) != key_with(&after),
                "changing behavior.{field} must restart the supervisor — it is \
                 consumed once at spawn and then frozen in the task's cfg snapshot"
            );
            assert_eq!(
                changed_behavior_field(&before, &after),
                Some(field),
                "the restart log must name the field that actually changed"
            );
        }
    }

    /// The exclusions in [`changed_behavior_field`] are a decision, not an
    /// oversight, so they are pinned like any other behaviour. A restart
    /// cannot apply these three; the two downscale targets would come back
    /// on the high-res detector, because the router builds its layer set
    /// once at boot. If a future change makes any of them restart-applicable,
    /// delete its arm here in the same commit.
    #[test]
    fn the_deliberately_excluded_behavior_fields_do_not_change_the_key() {
        let before = CameraBehavior {
            detector_downscale_to_width: Some(640),
            detector_downscale_to_height: Some(640),
            ..CameraBehavior::default()
        };
        let exclusions: Vec<BehaviorMutation> = vec![
            ("supervisor_width", |b| b.supervisor_width = Some(512)),
            ("detector_downscale_to_width", |b| {
                b.detector_downscale_to_width = Some(320)
            }),
            ("detector_downscale_to_height", |b| {
                b.detector_downscale_to_height = Some(320)
            }),
        ];

        for (field, mutate) in exclusions {
            let mut after = before.clone();
            mutate(&mut after);
            assert!(
                key_with(&before) == key_with(&after),
                "behavior.{field} must NOT restart the supervisor on its own — a \
                 restart cannot apply it, so the gap would buy nothing"
            );
        }
    }

    /// Publishes through a real bus; subscribes the way `refuse` says.
    ///
    /// No `Bus` in the workspace does either today: `BroadcastBus`
    /// subscribes infallibly and its stream cannot end while the reconciler
    /// holds the bus, and `NatsBus` cannot be constructed. The periodic pass
    /// must not depend on that staying true.
    struct BrokenSubscriptions {
        bus: nexus_bus::BroadcastBus,
        /// `true`: every subscribe fails. `false`: it succeeds with a stream
        /// that has already ended.
        refuse: bool,
    }

    #[async_trait::async_trait]
    impl Bus for BrokenSubscriptions {
        async fn publish_raw(
            &self,
            topic: &str,
            payload: Arc<serde_json::Value>,
        ) -> Result<(), nexus_bus::BusError> {
            self.bus.publish_raw(topic, payload).await
        }
        async fn subscribe_raw(
            &self,
            _topic: &str,
        ) -> Result<nexus_bus::DynStream, nexus_bus::BusError> {
            if self.refuse {
                Err(nexus_bus::BusError::BackendUnavailable("test: refused"))
            } else {
                Ok(Box::pin(futures::stream::empty()))
            }
        }
    }

    /// F3 put supervision on the same loop as the `config.changed`
    /// subscription, so losing the subscription also stopped the pass that
    /// restarts an exited supervisor. Losing hot-add is one thing; losing
    /// supervision with it is not.
    async fn assert_supervision_survives(refuse: bool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::EndsOnce)]);
        let mut args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;
        args.bus = Arc::new(BrokenSubscriptions {
            bus: nexus_bus::BroadcastBus::new(64),
            refuse,
        });
        let handles = args.handles.clone();

        reconcile(&args).await.expect("initial pass");
        wait_until(
            "precondition: the supervisor should end once its frame source returns",
            || supervisor_ended(&handles, 7),
        )
        .await;

        let reconciler = tokio::spawn(run(args, std::time::Duration::from_millis(50)));
        wait_until(
            "the reconciler lost its config.changed subscription and stopped supervising: \
             camera 7's exited supervisor was never restarted",
            || recorder.sources_built(7) == 2 && !supervisor_ended(&handles, 7),
        )
        .await;
        reconciler.abort();
        abort_all(&handles);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_subscription_does_not_stop_supervision() {
        assert_supervision_survives(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ended_subscription_does_not_stop_supervision() {
        assert_supervision_survives(false).await;
    }

    /// `stop_camera` stops the supervisor and shuts the ingesters down, and
    /// neither is synchronous: the tap can be mid-frame and a pipeline's
    /// probes keep firing until its detached NULL lands. Writes from either
    /// after the stop must not make a stopped camera read as running to the
    /// heartbeat, the roster or the decode-health census (SPEC-075 F2).
    #[tokio::test]
    async fn a_stopped_cameras_late_writers_cannot_bring_it_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let tap = args.frame_stats.begin_session(7);
        let now = chrono::Utc::now();
        args.frame_stats.observe_frame(7, tap, now, 512, 288);
        args.decode_health.observe_decoder_output(7);

        stop_camera(&args, 7).await;
        args.frame_stats.observe_frame(7, tap, now, 512, 288);
        args.decode_health.observe_decoder_output(7);

        assert!(
            args.frame_stats.snapshot_all().is_empty(),
            "the stopped session's tap brought camera 7 back into the frame stats"
        );
        assert!(
            args.decode_health.snapshot_all().is_empty(),
            "the stopped ingester's probe brought camera 7 back into the decode-health census"
        );
    }

    /// An `http://` substream keeps the codec probe off the network:
    /// `apply_analysis_session` only probes `rtsp` / `rtsps`.
    const SUBSTREAM: &str = "http://10.0.0.5/sub";

    /// What `build_gst_recorder` hands [`register_boot_sessions`] when every
    /// main ingester built: each enabled camera with a substream, at the
    /// codec and dims its main ingester was built with. The test cameras pin
    /// their codec, so boot would not probe.
    fn pending_like_build_gst_recorder<'a>(
        args: &ReconcilerArgs,
        cams: &'a [CameraConfig],
    ) -> Vec<(&'a CameraConfig, CodecKind, (u32, u32), bool)> {
        cams.iter()
            .filter(|c| c.ingest.enabled && c.ingest.analysis_url.is_some())
            .map(|c| {
                let codec = c.ingest.codec.expect("test cameras pin their codec");
                (
                    c,
                    codec,
                    supervisor_dims_for(c, args.default_detector_width),
                    true,
                )
            })
            .collect()
    }

    /// Seeds `handles` the way `main`'s boot loop does (`for cam in cameras`
    /// … `running.lock().insert`): each enabled camera's entry is the real
    /// [`EntryKey::at_boot`], spawned by the real [`spawn_supervisor`]. It
    /// registers nothing: `build_recorder` did that before the loop.
    async fn seed_like_boot(args: &ReconcilerArgs, mut boot: BootSessions) {
        for cam in args.store.list_cameras().await.expect("list cameras") {
            if !cam.ingest.enabled {
                continue;
            }
            let cam_id = cam.id;
            let start = EntryKey::at_boot(args, &cam, &mut boot);
            let entry = spawn_supervisor(args, cam, start, Vec::new());
            args.handles.lock().insert(cam_id, entry);
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

    /// `GstClipRecorder`'s registrations without GStreamer.
    /// `set_camera_analysis_ingester(Some)` is a no-op while the camera has
    /// a live substream session, as `GstClipRecorder`'s is; otherwise it
    /// counts a try and returns `Err` for the first `refuse_first` tries,
    /// then starts a session and returns `Ok`. `None` ends the session.
    /// [`Self::fall_back`] ends it the way the SPEC-069 fallback's shutdown
    /// does. `add_camera_ingester` counts a try and returns `Err` for the
    /// first `refuse_main_first`. Counts what each restart asks of the main
    /// ingester and the frame source. The tests using it have one camera, so
    /// counts are not keyed.
    struct SubstreamRecorder {
        refuse_first: AtomicUsize,
        refuse_main_first: AtomicUsize,
        live: std::sync::atomic::AtomicBool,
        registrations_tried: AtomicUsize,
        mains_tried: AtomicUsize,
        ingesters_removed: AtomicUsize,
        sources_built: AtomicUsize,
        codec_pins: Mutex<Vec<(CameraId, Option<CodecKind>)>>,
    }

    impl SubstreamRecorder {
        fn new(refuse_first: usize) -> Arc<Self> {
            Arc::new(Self {
                refuse_first: refuse_first.into(),
                refuse_main_first: Default::default(),
                live: Default::default(),
                registrations_tried: Default::default(),
                mains_tried: Default::default(),
                ingesters_removed: Default::default(),
                sources_built: Default::default(),
                codec_pins: Default::default(),
            })
        }

        /// Accepts every substream, and refuses the main ingester's first
        /// `refuse_main_first` registrations.
        fn refusing_main(refuse_main_first: usize) -> Arc<Self> {
            let recorder = Self::new(0);
            recorder
                .refuse_main_first
                .store(refuse_main_first, Ordering::SeqCst);
            recorder
        }

        /// What the SPEC-069 fallback leaves: the session is shut down, so
        /// the next registration starts a new one.
        fn fall_back(&self) {
            self.live.store(false, Ordering::SeqCst);
        }

        fn registrations_tried(&self) -> usize {
            self.registrations_tried.load(Ordering::SeqCst)
        }
        fn mains_tried(&self) -> usize {
            self.mains_tried.load(Ordering::SeqCst)
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
        fn set_camera_codec_pin(&self, camera_id: CameraId, codec: Option<CodecKind>) {
            self.codec_pins.lock().push((camera_id, codec));
        }
        fn add_camera_ingester(
            &self,
            _camera_id: CameraId,
            _url: &str,
            _pre_roll_secs: u32,
            _max_fps: u32,
            _rgb_w: u32,
            _rgb_h: u32,
            _codec: CodecKind,
        ) -> Result<(), RecorderError> {
            self.mains_tried.fetch_add(1, Ordering::SeqCst);
            // `try_update` replaces this in rustc 1.99, above the 1.88 MSRV.
            #[allow(deprecated)]
            let refused = self
                .refuse_main_first
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            if refused {
                return Err(RecorderError::Io(std::io::Error::other(
                    "ingester: scripted refusal",
                )));
            }
            Ok(())
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
                self.live.store(false, Ordering::SeqCst);
                return Ok(());
            }
            if self.live.load(Ordering::SeqCst) {
                return Ok(());
            }
            self.registrations_tried.fetch_add(1, Ordering::SeqCst);
            // `try_update` replaces this in rustc 1.99, above the 1.88 MSRV.
            #[allow(deprecated)]
            let refused = self
                .refuse_first
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            if refused {
                return Err(RecorderError::Io(std::io::Error::other(
                    "analysis ingester: scripted refusal",
                )));
            }
            self.live.store(true, Ordering::SeqCst);
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
        let boot = crate::register_stub_analysis_sessions(args.recorder.as_ref(), &cams).await;
        seed_like_boot(&args, boot).await;
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
            entry_7.sessions.analysis.map(|a| a.registered),
            Some(true),
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
    ) -> (Arc<dyn ClipRecorder>, BootSessions) {
        build_recorder_at_width(kind, dir, cameras, 512).await
    }

    /// [`build_recorder_like_main`], sizing the boot RGB taps from
    /// `default_detector_width`.
    async fn build_recorder_at_width(
        kind: &RecorderKind,
        dir: &std::path::Path,
        cameras: &[CameraConfig],
        default_detector_width: u32,
    ) -> (Arc<dyn ClipRecorder>, BootSessions) {
        let store = Arc::new(
            Store::open(&nexus_config::StoreConfig {
                url: format!("sqlite://{}?mode=rwc", dir.join("recorder.db").display()),
                ..nexus_config::StoreConfig::default()
            })
            .await
            .expect("recorder store"),
        );
        let (recorder, _webrtc, boot) = crate::build_recorder(
            kind,
            store,
            &dir.join("clips"),
            cameras,
            default_detector_width,
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
        (recorder, boot)
    }

    /// `at_boot` takes a camera's frame from its boot tap, when it has one,
    /// over the guard's; a camera with no boot tap gets the guard's.
    #[tokio::test]
    async fn a_boot_entry_takes_its_frame_from_its_tap_when_it_has_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cams = [cam_with_id(7), cam_with_id(8)];
        let recorder = Arc::new(RecordingRecorder::default());
        let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;
        let mut boot = register_boot_sessions(recorder.as_ref(), Vec::new())
            .await
            .with_taps(HashMap::from([(7, (1024, 576))]));
        assert_eq!(
            EntryKey::at_boot(&args, &cams[0], &mut boot)
                .key
                .supervisor_dims,
            (1024, 576),
            "a camera with a boot tap records the tap's frame"
        );
        assert_eq!(
            EntryKey::at_boot(&args, &cams[1], &mut boot)
                .key
                .supervisor_dims,
            (512, 288),
            "a camera with no boot tap records the guard's frame"
        );
    }

    /// A boot entry records the frame its RGB tap was built at, so the two
    /// cannot disagree whatever width `main` hands `build_recorder`. Here
    /// the taps are sized from a 1024 px default and the guard from 512:
    /// each entry must still match its tap, and the guard then sees a
    /// changed frame and restarts the camera at the width it wants. The
    /// oracle is the recorder's own resize, which reports no rebuild only
    /// when the tap already has the asked-for dims.
    #[cfg(feature = "gstreamer")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_boot_entry_records_the_frame_its_rgb_tap_was_built_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A closed local port: every ingester builds, none connects.
        let with = |id: CameraId, shape: &dyn Fn(&mut CameraConfig)| {
            let mut c = cam_with_id(id);
            c.ingest.url = Url::parse(&format!("rtsp://127.0.0.1:1/cam-{id}")).unwrap();
            shape(&mut c);
            c
        };
        let cams = [
            with(7, &|_| {}),
            with(8, &|c| c.behavior.supervisor_width = Some(256)),
            with(9, &|c| c.behavior.supervisor_width = Some(1536)),
            with(10, &|c| {
                c.detector.model_override = Some(nexus_config::ModelConfig {
                    kind: "mock".into(),
                    input_width: 512,
                    ..Default::default()
                })
            }),
        ];
        let (recorder, mut boot) =
            build_recorder_at_width(&RecorderKind::Gstreamer, dir.path(), &cams, 1024).await;
        let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;
        for cam in &cams {
            let start = EntryKey::at_boot(&args, cam, &mut boot);
            let (w, h) = start.key.supervisor_dims;
            assert!(
                recorder.shared_frame_source(cam.id).is_some(),
                "camera {} has no boot RGB tap",
                cam.id
            );
            assert!(
                !recorder
                    .resize_camera_rgb_tap(cam.id, w, h)
                    .expect("resize"),
                "camera {}: the entry records {w}x{h}, which is not its tap's frame",
                cam.id
            );
            // Cameras 9 and 10 size the same at either default width.
            assert_eq!(
                start.key == EntryKey::wanted(&args, cam),
                matches!(cam.id, 9 | 10),
                "camera {}: the guard must see a frame the widths split",
                cam.id
            );
        }
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
        let want = HashMap::from([(
            7,
            Sessions {
                main_pending: None,
                analysis: Some(AnalysisSession {
                    codec: CodecKind::H265,
                    registered: true,
                }),
            },
        )]);

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
        let boot = register_boot_sessions(
            args.recorder.as_ref(),
            pending_like_build_gst_recorder(&args, &cams),
        )
        .await;
        seed_like_boot(&args, boot).await;
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

    /// A refused substream registration is retried by each pass on its own,
    /// never by restarting the camera: a restart tears the main recording
    /// ingester down with it, and a registration that failed on every call
    /// would do that every 30 s (BUG-224). The recorder refuses twice, boot's
    /// try and the first pass's retry, and accepts the second pass's retry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_substream_registration_is_retried_by_each_pass_without_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = SubstreamRecorder::new(2);
        let cams = [cam(Some(SUBSTREAM))];
        let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;
        let boot = register_boot_sessions(
            args.recorder.as_ref(),
            pending_like_build_gst_recorder(&args, &cams),
        )
        .await;
        seed_like_boot(&args, boot).await;
        wait_until(
            "precondition: the boot supervisor should build its frame source",
            || recorder.sources_built() == 1,
        )
        .await;

        let at_boot = sessions_of(&args.handles, 7);

        let restarts = restarts_per_pass(&args, 3, || recorder.sources_built()).await;

        let after = sessions_of(&args.handles, 7);
        abort_all(&args.handles);
        assert_eq!(
            restarts,
            vec![false, false, false],
            "a pass restarted the camera, main recording ingester included, to retry its \
             substream registration"
        );
        let session = |registered| {
            Some(AnalysisSession {
                codec: CodecKind::H265,
                registered,
            })
        };
        assert_eq!(
            at_boot.analysis,
            session(false),
            "boot must record the refused registration as not registered"
        );
        assert_eq!(
            after.analysis,
            session(true),
            "the accepted retry must be recorded in the entry"
        );
        assert_eq!(
            recorder.registrations_tried(),
            3,
            "boot's refused try, the first pass's refused retry, then the second pass's \
             accepted one; the third pass finds the session running"
        );
        assert_eq!(
            recorder.ingesters_removed(),
            0,
            "a retry tore down the camera's main recording ingester"
        );
        assert_eq!(
            recorder.sources_built(),
            1,
            "a retry rebuilt the camera's frame source"
        );
    }

    /// SPEC-069: after the fallback shuts a substream session down, analysis
    /// returns to it "on the long backoff already used for session rebuilds":
    /// the next pass registers it again, without a restart. Before, the entry
    /// still matched the configured URL, so no pass did anything, and the
    /// camera decoded its main stream until an unrelated restart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_substream_session_the_fallback_shut_down_is_registered_again_by_the_next_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = SubstreamRecorder::new(0);
        let cams = [cam(Some(SUBSTREAM))];
        let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;
        let boot = register_boot_sessions(
            args.recorder.as_ref(),
            pending_like_build_gst_recorder(&args, &cams),
        )
        .await;
        seed_like_boot(&args, boot).await;
        wait_until(
            "precondition: the boot supervisor should build its frame source",
            || recorder.sources_built() == 1,
        )
        .await;

        let mut restarts = restarts_per_pass(&args, 1, || recorder.sources_built()).await;
        let tried_while_live = recorder.registrations_tried();
        recorder.fall_back();
        restarts.extend(restarts_per_pass(&args, 2, || recorder.sources_built()).await);

        abort_all(&args.handles);
        assert_eq!(
            tried_while_live, 1,
            "a pass rebuilt a substream session that was still running"
        );
        assert_eq!(
            recorder.registrations_tried(),
            2,
            "the pass after the fallback must register the substream again, once"
        );
        assert_eq!(
            restarts,
            vec![false, false, false],
            "registering the substream again restarted the camera"
        );
        assert_eq!(
            recorder.ingesters_removed(),
            0,
            "registering the substream again tore down the main recording ingester"
        );
    }

    /// A refused main-ingester registration at `start_camera` is retried by
    /// each pass without a restart, and the camera's substream is not
    /// registered meanwhile: with no main ingester the supervisor builds its
    /// own frame source, which reads no substream, and boot registers a
    /// substream only for a camera whose main ingester built. Before, nothing
    /// recorded the refusal, so no pass retried it, and `start_camera`
    /// registered the substream regardless.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_main_ingester_is_retried_by_each_pass_without_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = SubstreamRecorder::refusing_main(2);
        let cams = [cam(Some(SUBSTREAM))];
        let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;
        reconcile(&args).await.expect("start pass");
        wait_until(
            "precondition: the started supervisor should build its frame source",
            || recorder.sources_built() == 1,
        )
        .await;
        let at_start = sessions_of(&args.handles, 7);
        assert!(
            recorder.codec_pins.lock().is_empty(),
            "failed registration must not set a pin"
        );

        let restarts = restarts_per_pass(&args, 3, || recorder.sources_built()).await;

        let after = sessions_of(&args.handles, 7);
        abort_all(&args.handles);
        assert_eq!(
            *recorder.codec_pins.lock(),
            [(7, Some(CodecKind::H265))],
            "successful retry must preserve the configured codec pin exactly once"
        );
        assert_eq!(
            at_start,
            Sessions {
                main_pending: Some(CodecKind::H265),
                analysis: None,
            },
            "the start must record the refused main ingester, and no substream"
        );
        assert_eq!(
            after,
            Sessions {
                main_pending: None,
                analysis: None,
            },
            "the accepted retry must be recorded in the entry"
        );
        assert_eq!(
            recorder.mains_tried(),
            3,
            "the start's refused try, the first pass's refused retry, then the second pass's \
             accepted one; the third pass has nothing to retry"
        );
        assert_eq!(
            restarts,
            vec![false, false, false],
            "retrying the main ingester restarted the camera"
        );
        assert_eq!(
            recorder.registrations_tried(),
            0,
            "a substream was registered for a supervisor spawned without its main ingester"
        );
    }

    /// Boot's side of the same retry: a camera whose main ingester did not
    /// build at boot must be retried by the next pass, without a restart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_main_ingester_that_failed_at_boot_is_retried_by_the_next_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = SubstreamRecorder::refusing_main(1);
        let cams = [cam(None)];
        let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;
        // What `build_gst_recorder` registers when camera 7's main ingester
        // did not build.
        let boot = register_boot_sessions(
            args.recorder.as_ref(),
            vec![(&cams[0], CodecKind::H265, (512, 288), false)],
        )
        .await;
        seed_like_boot(&args, boot).await;
        wait_until(
            "precondition: the boot supervisor should build its frame source",
            || recorder.sources_built() == 1,
        )
        .await;
        let at_boot = sessions_of(&args.handles, 7).main_pending;
        assert!(
            recorder.codec_pins.lock().is_empty(),
            "failed registration must not set a pin"
        );

        let restarts = restarts_per_pass(&args, 3, || recorder.sources_built()).await;

        let after = sessions_of(&args.handles, 7).main_pending;
        abort_all(&args.handles);
        assert_eq!(
            *recorder.codec_pins.lock(),
            [(7, Some(CodecKind::H265))],
            "successful retry must preserve the configured codec pin exactly once"
        );
        assert_eq!(
            (at_boot, after),
            (Some(CodecKind::H265), None),
            "boot must record the main ingester that did not build, and the accepted retry \
             must clear it"
        );
        assert_eq!(
            recorder.mains_tried(),
            2,
            "the first pass's refused retry, then the second pass's accepted one"
        );
        assert_eq!(
            restarts,
            vec![false, false, false],
            "retrying the main ingester restarted the camera"
        );
    }

    /// A registration as the recorder received it: the URL, `max_fps`, RGB
    /// frame and codec it was called with.
    type Registration = (String, u32, (u32, u32), CodecKind);

    /// Records, by camera, every main-ingester and substream registration
    /// call and the frame each clip open asked for, and how many main
    /// ingesters and substream sessions had been registered when the
    /// supervisor built its frame source. Each registration sleeps for
    /// `delay`, the way a codec probe or a pipeline build takes time, so a
    /// supervisor spawned before its registrations would build its source
    /// first. A camera in `refuse_main_first` has that many main-ingester
    /// calls refused before one is accepted.
    #[derive(Default)]
    struct StartRecorder {
        delay: std::time::Duration,
        refuse_main_first: Mutex<HashMap<CameraId, usize>>,
        ingesters: Mutex<HashMap<CameraId, Vec<Registration>>>,
        substreams: Mutex<HashMap<CameraId, Vec<Registration>>>,
        clip_dims: Mutex<HashMap<CameraId, Vec<(u32, u32)>>>,
        registered_at_source_build: Mutex<HashMap<CameraId, Vec<(usize, usize)>>>,
    }

    impl StartRecorder {
        fn calls<T: Clone>(map: &Mutex<HashMap<CameraId, Vec<T>>>, id: CameraId) -> Vec<T> {
            map.lock().get(&id).cloned().unwrap_or_default()
        }
        fn frames(
            map: &Mutex<HashMap<CameraId, Vec<Registration>>>,
            id: CameraId,
        ) -> Vec<(u32, u32)> {
            Self::calls(map, id).into_iter().map(|r| r.2).collect()
        }
        fn count<T>(map: &Mutex<HashMap<CameraId, Vec<T>>>, id: CameraId) -> usize {
            map.lock().get(&id).map_or(0, Vec::len)
        }
    }

    #[async_trait::async_trait]
    impl ClipRecorder for StartRecorder {
        async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
            self.clip_dims
                .lock()
                .entry(args.camera_id)
                .or_default()
                .push((args.frame_width, args.frame_height));
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
            "start"
        }
        fn add_camera_ingester(
            &self,
            camera_id: CameraId,
            url: &str,
            _pre_roll_secs: u32,
            max_fps: u32,
            rgb_w: u32,
            rgb_h: u32,
            codec: CodecKind,
        ) -> Result<(), RecorderError> {
            std::thread::sleep(self.delay);
            self.ingesters.lock().entry(camera_id).or_default().push((
                url.to_string(),
                max_fps,
                (rgb_w, rgb_h),
                codec,
            ));
            if let Some(n) = self.refuse_main_first.lock().get_mut(&camera_id) {
                if *n > 0 {
                    *n -= 1;
                    return Err(RecorderError::Io(std::io::Error::other(
                        "ingester: scripted refusal",
                    )));
                }
            }
            Ok(())
        }
        fn set_camera_analysis_ingester(
            &self,
            camera_id: CameraId,
            analysis_url: Option<&str>,
            max_fps: u32,
            rgb_w: u32,
            rgb_h: u32,
            codec: CodecKind,
        ) -> Result<(), RecorderError> {
            if let Some(url) = analysis_url {
                std::thread::sleep(self.delay);
                self.substreams.lock().entry(camera_id).or_default().push((
                    url.to_string(),
                    max_fps,
                    (rgb_w, rgb_h),
                    codec,
                ));
            }
            Ok(())
        }
        fn shared_frame_source(
            &self,
            camera_id: CameraId,
        ) -> Option<Box<dyn nexus_pipeline::FrameSource + Send>> {
            let registered = (
                Self::count(&self.ingesters, camera_id),
                Self::count(&self.substreams, camera_id),
            );
            self.registered_at_source_build
                .lock()
                .entry(camera_id)
                .or_default()
                .push(registered);
            Some(Box::new(nexus_pipeline::VirtualSource {
                camera_id,
                width: 512,
                height: 288,
                fps: 10,
            }))
        }
    }

    /// A camera's supervisor, its main ingester, its substream session and
    /// its entry must all use one supervisor frame, or the guard compares one
    /// frame while the camera runs another: the detector and every clip's
    /// recorded geometry at one size, the entry at a second. Two cameras whose
    /// frame is not the default, each started by a pass and seeded as at boot.
    /// The supervisor's frame is read off the clips it opens.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_camera_runs_at_the_frame_its_entry_records() {
        let mut wide = cam_with_id(7);
        wide.ingest.analysis_url = Some(Url::parse(SUBSTREAM).unwrap());
        wide.behavior.supervisor_width = Some(1024);
        let mut model = cam_with_id(8);
        model.ingest.analysis_url = Some(Url::parse(SUBSTREAM).unwrap());
        model.detector.model_override = Some(nexus_config::ModelConfig {
            kind: "mock".into(),
            input_width: 640,
            ..Default::default()
        });
        let cams = [wide, model];
        let want = [(7, (1024, 576)), (8, (640, 360))];

        for phase in ["a start", "boot"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let recorder = Arc::new(StartRecorder::default());
            let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;
            if phase == "boot" {
                let boot = register_boot_sessions(
                    args.recorder.as_ref(),
                    pending_like_build_gst_recorder(&args, &cams),
                )
                .await;
                seed_like_boot(&args, boot).await;
            } else {
                reconcile(&args).await.expect("start pass");
            }
            wait_until("every supervisor should open a clip", || {
                want.iter()
                    .all(|(id, _)| StartRecorder::count(&recorder.clip_dims, *id) > 0)
            })
            .await;
            let entries: HashMap<CameraId, RunningCameraEntry> = args.handles.lock().clone();
            abort_all(&args.handles);
            for (id, dims) in want {
                assert_eq!(
                    entries[&id].key.supervisor_dims, dims,
                    "{phase}: camera {id}'s entry"
                );
                let ingesters = StartRecorder::frames(&recorder.ingesters, id);
                // Boot's main ingesters are built by `build_gst_recorder`, not
                // through the recorder trait.
                if phase == "a start" {
                    assert_eq!(
                        ingesters,
                        vec![dims],
                        "{phase}: camera {id}'s main ingester"
                    );
                }
                assert_eq!(
                    StartRecorder::frames(&recorder.substreams, id),
                    vec![dims],
                    "{phase}: camera {id}'s substream session"
                );
                let clips = StartRecorder::calls(&recorder.clip_dims, id);
                assert!(
                    clips.iter().all(|c| *c == dims),
                    "{phase}: camera {id}'s supervisor opened clips at {clips:?}, its entry \
                     records {dims:?}"
                );
            }
        }
    }

    /// `start_camera` registers a camera's main ingester and substream
    /// session before it spawns the supervisor. The supervisor builds its
    /// frame source once, from what the recorder holds at that moment, so a
    /// spawn first would build the source from the main stream alone while
    /// the entry records the substream as registered: a camera that never
    /// analyses its substream, with nothing to retry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_supervisor_builds_its_frame_source_after_its_sessions_are_registered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = Arc::new(StartRecorder {
            delay: std::time::Duration::from_millis(200),
            ..Default::default()
        });
        let cams = [cam(Some(SUBSTREAM))];
        let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;

        reconcile(&args).await.expect("start pass");
        wait_until("the supervisor should build its frame source", || {
            recorder.registered_at_source_build.lock().contains_key(&7)
        })
        .await;

        abort_all(&args.handles);
        assert_eq!(
            recorder.registered_at_source_build.lock()[&7],
            vec![(1, 1)],
            "(main ingesters, substream sessions) registered when the supervisor built its \
             frame source"
        );
    }

    /// An RTSP server on loopback that answers `DESCRIBE` on any path with
    /// one H.265 video track, as a camera answers the codec probe. Returns
    /// its `rtsp://host:port` and the number of probes (connections) it has
    /// answered.
    async fn an_rtsp_server_answering_h265() -> (String, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("rtsp://{}", listener.local_addr().expect("addr"));
        let probes = Arc::new(AtomicUsize::new(0));
        let answered = probes.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                answered.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while let Ok(n @ 1..) = sock.read(&mut chunk).await {
                        buf.extend_from_slice(&chunk[..n]);
                        while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                            buf.drain(..end + 4);
                            let cseq = head
                                .lines()
                                .find_map(|l| l.strip_prefix("CSeq: "))
                                .unwrap_or("0");
                            let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H265/90000\r\n";
                            let reply = if head.starts_with("DESCRIBE") {
                                format!(
                                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: \
                                     application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
                                    sdp.len()
                                )
                            } else {
                                format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n")
                            };
                            if sock.write_all(reply.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        (base, probes)
    }

    /// Each pass registers again exactly what the camera's start registered,
    /// and probes nothing: the URL, `max_fps`, frame and codec of a main
    /// ingester that is outstanding, and of the substream session.
    /// `GstClipRecorder`'s registration is a no-op for a running session
    /// only when the URL and codec match, so any other argument rebuilds the
    /// running session, and clears the camera's decode health, on every pass;
    /// and a session rebuilt at another frame feeds the supervisor frames of
    /// a size its entry does not record. Camera 7's main stream is pinned to
    /// H.264 and its substream answers the probe with H.265, so a retry with
    /// the main codec, the configured one or a literal cannot pass for the
    /// probed one. Camera 8's main stream is auto-codec, probed as H.265, and
    /// its first two main-ingester registrations are refused. Both run at
    /// frames that are not the default. Each is started by a pass, then
    /// seeded as at boot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn each_pass_registers_again_exactly_what_the_cameras_start_registered() {
        let (server, probes) = an_rtsp_server_answering_h265().await;
        let mut sub = cam_with_id(7);
        sub.ingest.codec = Some(CodecKind::H264);
        sub.ingest.analysis_url = Some(Url::parse(&format!("{server}/sub")).unwrap());
        sub.behavior.supervisor_width = Some(1024);
        let mut auto = cam_with_id(8);
        auto.ingest.url = Url::parse(&format!("{server}/main")).unwrap();
        auto.ingest.codec = None;
        auto.detector.model_override = Some(nexus_config::ModelConfig {
            kind: "mock".into(),
            input_width: 640,
            ..Default::default()
        });
        let main_7: Registration = (sub.ingest.url.to_string(), 15, (1024, 576), CodecKind::H264);
        let sub_7: Registration = (format!("{server}/sub"), 15, (1024, 576), CodecKind::H265);
        let main_8: Registration = (auto.ingest.url.to_string(), 15, (640, 360), CodecKind::H265);
        let cams = [sub, auto];

        for phase in ["a start", "boot"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let recorder = Arc::new(StartRecorder::default());
            recorder.refuse_main_first.lock().insert(8, 2);
            let args = reconciler_args(recorder.clone(), dir.path(), &cams).await;
            if phase == "boot" {
                // What `build_gst_recorder` registers: camera 7's main
                // ingester built, and camera 8's, probed as H.265, did not.
                let boot = register_boot_sessions(
                    args.recorder.as_ref(),
                    vec![
                        (&cams[0], CodecKind::H264, (1024, 576), true),
                        (&cams[1], CodecKind::H265, (640, 360), false),
                    ],
                )
                .await;
                seed_like_boot(&args, boot).await;
            } else {
                reconcile(&args).await.expect("start pass");
            }
            let probed_at_start = probes.load(Ordering::SeqCst);
            let started: HashMap<CameraId, RunningCameraEntry> = args.handles.lock().clone();

            for _ in 0..3 {
                reconcile(&args).await.expect("reconcile pass");
            }

            let restarted: Vec<CameraId> = started
                .iter()
                .filter(|(id, entry)| {
                    !args
                        .handles
                        .lock()
                        .get(id)
                        .is_some_and(|e| Arc::ptr_eq(&e.task, &entry.task))
                })
                .map(|(id, _)| *id)
                .collect();
            let main_pending_8 = sessions_of(&args.handles, 8).main_pending;
            abort_all(&args.handles);
            assert!(
                restarted.is_empty(),
                "{phase}: a pass restarted {restarted:?}"
            );
            assert_eq!(
                probes.load(Ordering::SeqCst),
                probed_at_start,
                "{phase}: a pass probed the camera, which only its start does"
            );
            assert_eq!(
                StartRecorder::calls(&recorder.substreams, 7),
                vec![sub_7.clone(); 4],
                "{phase}: camera 7's substream, registered by its start and again by each of \
                 three passes, with the codec the start probed"
            );
            // Boot's main ingesters are built by `build_gst_recorder`, not
            // through the recorder trait.
            let mains_7 = if phase == "boot" {
                vec![]
            } else {
                vec![main_7.clone()]
            };
            assert_eq!(
                StartRecorder::calls(&recorder.ingesters, 7),
                mains_7,
                "{phase}: camera 7's main ingester registered, so no pass registers it again"
            );
            assert_eq!(
                StartRecorder::calls(&recorder.ingesters, 8),
                vec![main_8.clone(); 3],
                "{phase}: camera 8's main ingester, refused twice and then accepted, with the \
                 codec its start resolved"
            );
            assert_eq!(
                main_pending_8, None,
                "{phase}: the accepted retry must be recorded in the entry"
            );
            assert!(
                StartRecorder::calls(&recorder.substreams, 8).is_empty(),
                "{phase}: camera 8 has no substream"
            );
        }
    }

    /// Every field the no-change guard compares, for camera shapes no other
    /// test builds: an auto codec, a `supervisor_width` below and above the
    /// detector input, a `model_override`, and behavior fields the guard
    /// compares (BUG-285) — a boot seed that dropped them would restart that
    /// camera on the first pass. Each camera is seeded by
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
            shaped(11, &|c| {
                c.behavior.parking_lot_mode = true;
                c.behavior.tile_trigger = Some(5);
            }),
        ];
        // (id, configured codec, supervisor frame) each entry must record.
        let want = [
            (7, None, (512, 288)),
            (8, Some(CodecKind::H265), (512, 288)),
            (9, Some(CodecKind::H265), (1024, 576)),
            (10, Some(CodecKind::H265), (640, 360)),
            (11, Some(CodecKind::H265), (512, 288)),
        ];
        let recorder = ScriptedRecorder::new(&[
            (7, SourceScript::NeverEnds),
            (8, SourceScript::NeverEnds),
            (9, SourceScript::NeverEnds),
            (10, SourceScript::NeverEnds),
            (11, SourceScript::NeverEnds),
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
        let boot = register_boot_sessions(
            args.recorder.as_ref(),
            pending_like_build_gst_recorder(&args, &cams),
        )
        .await;
        seed_like_boot(&args, boot).await;
        left_alone_by_two_passes("after boot").await;

        // Drop every entry so the next pass starts each camera through
        // `start_camera`, the other writer.
        abort_all(&args.handles);
        reconcile(&args).await.expect("start pass");
        left_alone_by_two_passes("after a start").await;
        abort_all(&args.handles);
    }

    /// A supervisor that ended on its own analyses nothing until a pass
    /// restarts it, and the only thing that announced it, a
    /// `PIPELINE_STATUS` `Stopped` with no subscriber, reached no surface.
    /// The health roll-up both surfaces report is built from these args, so
    /// it reads the handle map this module keeps and names the camera until
    /// the restart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_exited_supervisor_is_on_the_health_roll_up_until_it_is_restarted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::EndsOnce)]);
        let args = reconciler_args(recorder.clone(), dir.path(), &[cam(None)]).await;
        let health = crate::cloud_tunnel::EngineHealth::new(&args);
        let stopped = |h: nexus_cloud_protocol::v1::EdgeHealth| {
            h.issues
                .into_iter()
                .flatten()
                .find(|i| i.code == "camera_pipeline_stopped")
        };

        reconcile(&args).await.expect("first pass");
        wait_until(
            "precondition: the supervisor should end once its frame source returns",
            || supervisor_ended(&args.handles, 7),
        )
        .await;
        let issue = stopped(health.rollup().await);

        reconcile(&args).await.expect("the pass that restarts it");
        wait_until("the restarted supervisor never delivered a frame", || {
            args.frame_stats
                .snapshot(7)
                .is_some_and(|s| s.frames_emitted > 0)
        })
        .await;
        let after_restart = stopped(health.rollup().await);
        abort_all(&args.handles);

        let issue = issue.expect("a supervisor that exited on its own must be on the roll-up");
        assert_eq!(issue.component, "pipeline");
        assert!(
            issue.detail.ends_with(": 7"),
            "names the camera: {}",
            issue.detail
        );
        assert_eq!(
            after_restart, None,
            "a restarted supervisor is not reported"
        );
    }

    /// `stop_camera` keeps an entry in the map while it stops the task, so a
    /// finished task in the map is no longer proof of a crash. A supervisor
    /// the reconciler asked to stop, found finished before its entry is
    /// removed, must not be reported as "exited without being stopped".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_supervisor_that_finished_after_a_stop_request_is_not_on_the_health_roll_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[(7, SourceScript::NeverEnds)]);
        let args = reconciler_args(recorder, dir.path(), &[cam(None)]).await;
        let health = crate::cloud_tunnel::EngineHealth::new(&args);
        let stopped = |h: nexus_cloud_protocol::v1::EdgeHealth| {
            h.issues
                .into_iter()
                .flatten()
                .find(|i| i.code == "camera_pipeline_stopped")
        };

        reconcile(&args).await.expect("first pass");
        let entry = args
            .handles
            .lock()
            .get(&7)
            .cloned()
            .expect("camera 7 spawned");
        assert!(!entry.exited_on_its_own(), "a running supervisor");

        // What `stop_camera` does first; the entry stays in the map after.
        let _ = entry.stop.send(true);
        wait_until("the stopped supervisor should end", || {
            entry.task.is_finished()
        })
        .await;
        assert!(
            supervisor_ended(&args.handles, 7),
            "precondition: the finished supervisor is still in the map"
        );
        let issue = stopped(health.rollup().await);
        let on_its_own = entry.exited_on_its_own();
        abort_all(&args.handles);

        assert_eq!(
            issue, None,
            "a stop the reconciler asked for is not a crash"
        );
        assert!(!on_its_own, "it finished because it was asked to");
    }

    /// The roll-up built from these args reads their live-view manager, the
    /// one the cloud's subscriptions reach: a subscribed camera that
    /// produces no frame stalls there and is on the roll-up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_health_roll_up_reports_a_stall_on_the_reconcilers_live_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = reconciler_args(ScriptedRecorder::new(&[]), dir.path(), &[]).await;
        let health = crate::cloud_tunnel::EngineHealth::new(&args);
        args.live_view
            .on_subscribe(&nexus_cloud_protocol::v1::LbrSubscribePayload {
                camera_id: 7,
                tile_w: Some(320),
                tile_h: Some(180),
                fps_tier: Some("grid".to_string()),
            });
        wait_until("precondition: the subscribed camera should stall", || {
            args.live_view.stalled_cameras() == vec![7]
        })
        .await;

        let issue = health
            .rollup()
            .await
            .issues
            .into_iter()
            .flatten()
            .find(|i| i.code == "camera_source_stalled")
            .expect("a stall on the reconciler's live view must be on the roll-up");
        assert!(
            issue.detail.ends_with(": 7"),
            "names the camera: {}",
            issue.detail
        );
    }

    /// The feature gate, stated per build, on the roll-up `main` builds from
    /// these args: a stub behind an enabled camera is reported only by a
    /// `gstreamer` build, where a real recorder was available. The recorder
    /// read is the one these args run, so a recorder that is not the stub is
    /// reported by no build.
    #[tokio::test]
    async fn only_a_gstreamer_build_raises_the_recorder_issue() {
        async fn recorder_issue(
            args: &ReconcilerArgs,
        ) -> Option<nexus_cloud_protocol::v1::EdgeDegradation> {
            crate::cloud_tunnel::EngineHealth::new(args)
                .rollup()
                .await
                .issues
                .unwrap_or_default()
                .into_iter()
                .find(|i| i.component == "recorder")
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let mut args = reconciler_args(ScriptedRecorder::new(&[]), dir.path(), &[cam(None)]).await;
        assert_eq!(
            recorder_issue(&args).await,
            None,
            "a recorder that is not the stub must not be reported",
        );

        args.recorder = Arc::new(nexus_pipeline::StubClipRecorder::new(
            args.store.clone(),
            dir.path().join("clips"),
        ));
        assert_eq!(
            recorder_issue(&args).await.is_some(),
            cfg!(feature = "gstreamer"),
            "a gstreamer build must report a stub recorder behind an enabled camera, \
             and a build without the feature must not",
        );
    }

    /// A camera row this build cannot read, such as one a newer release
    /// wrote with a codec this build has no variant for, must not stop the
    /// others. The pass runs every camera it can read, the health roll-up
    /// names each one it cannot, and once the rows are saved again the next
    /// pass runs them and the issue clears. The pass used to fail at its read,
    /// so no camera was added, removed or restarted while the row was there.
    /// A camera whose row stops reading while it runs is stopped, as boot
    /// would leave it: what runs is what the store can read, so a restart
    /// does not change the answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_camera_row_this_build_cannot_read_is_reported_and_the_rest_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder = ScriptedRecorder::new(&[
            (7, SourceScript::NeverEnds),
            (8, SourceScript::NeverEnds),
            (9, SourceScript::NeverEnds),
        ]);
        let cameras = [cam_with_id(7), cam_with_id(8), cam_with_id(9)];
        let args = reconciler_args(recorder, dir.path(), &cameras).await;
        let break_row = |id: CameraId| {
            sqlx::query(
                "UPDATE cameras SET config_json = json_set(config_json, '$.codec', 'av1') \
                 WHERE id = ?",
            )
            .bind(id)
            .execute(args.store.pool())
        };
        break_row(8).await.expect("rewrite camera 8's row");
        break_row(9).await.expect("rewrite camera 9's row");
        let health = crate::cloud_tunnel::EngineHealth::new(&args);
        let unreadable = |h: nexus_cloud_protocol::v1::EdgeHealth| {
            h.issues
                .into_iter()
                .flatten()
                .find(|i| i.code == "camera_config_unreadable")
        };
        let running = || {
            let mut ids: Vec<CameraId> = args.handles.lock().keys().copied().collect();
            ids.sort_unstable();
            ids
        };

        let pass = reconcile(&args).await;
        let running_with_the_row = running();
        let issue = unreadable(health.rollup().await);
        for id in [8, 9] {
            args.store
                .upsert_camera(&cam_with_id(id))
                .await
                .expect("save the camera again");
        }
        reconcile(&args)
            .await
            .expect("the pass after the rows are saved");
        let running_after = running();
        let after = unreadable(health.rollup().await);
        break_row(7).await.expect("rewrite camera 7's row");
        reconcile(&args)
            .await
            .expect("the pass after a running camera's row stops reading");
        let running_after_break = running();
        let running_camera_issue = unreadable(health.rollup().await);
        abort_all(&args.handles);

        pass.expect("a pass over a store with two unreadable rows");
        assert_eq!(running_with_the_row, vec![7], "the readable camera runs");
        let issue = issue.expect("the unreadable rows must be on the roll-up");
        assert_eq!(issue.component, "store");
        assert!(
            issue.detail.ends_with(": 8,9"),
            "names both cameras: {}",
            issue.detail
        );
        assert_eq!(running_after, vec![7, 8, 9], "the saved rows run");
        assert_eq!(after, None, "rows that read again are not reported");
        assert_eq!(
            running_after_break,
            vec![8, 9],
            "a running camera whose row stops reading is stopped, as at boot",
        );
        let issue = running_camera_issue.expect("its row is on the roll-up");
        assert!(
            issue.detail.ends_with(": 7"),
            "names the camera: {}",
            issue.detail
        );
    }
}

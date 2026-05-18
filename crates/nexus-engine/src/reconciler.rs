//! Camera hot-reload reconciler — subscribes to
//! `topic::CONFIG_CHANGED` and diffs the live `cameras` table
//! against the set of supervisor tasks + pre-roll ingesters currently
//! running in this process. Any delta (new camera, deleted camera,
//! disabled→enabled toggle, URL change) is converged without
//! restarting the engine.
//!
//! Why this exists: every camera mutation in the admin API
//! (`PUT /api/cameras/{id}`, `DELETE /api/cameras/{id}`, including
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
//! Restart trigger: today only "URL changed" forces a respawn.
//! Detector / threshold / rule changes do not — those still require
//! a process restart (or a future, finer-grained hot-reload path).
//! This matches the UX where the admin UI surfaces camera ingest
//! edits as the primary live operation.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use nexus_bus::{topic, Bus, BusExt};
use nexus_config::{AnnotatorConfig, CameraConfig, ClipsConfig, StaticObjectConfig};
use nexus_inference::InferenceRouter;
use nexus_pipeline::{spawn_camera, ClipRecorder, LatestFrameCache};
use nexus_rules::RuleEvaluator;
use nexus_store::Store;
use nexus_tracker::Tracker;
use nexus_types::CameraId;
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
}

/// Bundle of every dependency `spawn_camera()` needs. Constructed
/// once at engine boot and moved into the reconciler task; the task
/// keeps it for its entire lifetime.
pub struct ReconcilerArgs {
    pub router: Arc<InferenceRouter>,
    pub tracker: Arc<dyn Tracker>,
    pub annotator: AnnotatorConfig,
    pub static_object: StaticObjectConfig,
    pub clips: ClipsConfig,
    pub state_dir: PathBuf,
    pub evaluator: Arc<RuleEvaluator>,
    pub store: Arc<Store>,
    pub recorder: Arc<dyn ClipRecorder>,
    pub bus: Arc<dyn Bus>,
    pub cache: Arc<LatestFrameCache>,
    pub pre_roll_secs: u32,
    pub handles: HandleMap,
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
        }
    }

    // 2. Add or restart anything that is enabled in the DB.
    for cam in live.into_iter().filter(|c| c.ingest.enabled) {
        let cam_id = cam.id;
        let url = cam.ingest.url.to_string();
        match current.get(&cam_id) {
            Some(entry) if entry.url == url => {
                // No change — supervisor still alive, URL still the
                // same. Skip (we deliberately do not respawn on
                // unrelated config edits today).
                continue;
            }
            Some(_) => {
                info!(
                    camera_id = cam_id,
                    "camera reconciler: ingest URL changed; restarting supervisor"
                );
                stop_camera(args, cam_id);
            }
            None => {}
        }
        start_camera(args, cam, &url);
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
}

fn start_camera(args: &ReconcilerArgs, cam: CameraConfig, url: &str) {
    let cam_id = cam.id;
    // Pre-roll ingester first so the recorder is ready by the time
    // the supervisor opens its first motion clip. Failure is logged
    // but non-fatal: detection still runs; clip opens for this
    // camera return Refused until the next reconcile pass.
    if let Err(e) = args
        .recorder
        .add_camera_ingester(cam_id, url, args.pre_roll_secs)
    {
        error!(
            camera_id = cam_id,
            %url,
            error = %e,
            "camera reconciler: ingester hot-add failed; clips will be refused for this camera"
        );
    }

    let detector = args.router.detector_for_camera(&cam);
    let handle = spawn_camera(
        cam,
        detector,
        args.tracker.clone(),
        args.annotator.clone(),
        args.static_object.clone(),
        args.clips.clone(),
        args.state_dir.clone(),
        args.evaluator.clone(),
        args.store.clone(),
        args.recorder.clone(),
        args.bus.clone(),
        args.cache.clone(),
    );
    args.handles.lock().insert(
        cam_id,
        RunningCameraEntry {
            task: Arc::new(handle.task),
            url: url.to_string(),
        },
    );
    info!(
        camera_id = cam_id,
        %url,
        "camera reconciler: spawned supervisor + ingester"
    );
}

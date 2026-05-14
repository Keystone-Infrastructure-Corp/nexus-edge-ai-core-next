use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use nexus_bus::build_bus;
use nexus_config::{CameraConfig, Config, InferenceConfig, RecorderKind};
use nexus_inference::InferenceRouter;
use nexus_pipeline::{spawn_camera, LatestFrameCache};
use nexus_rules::RuleEvaluator;
use nexus_store::Store;
use nexus_tracker::build_tracker;
use tracing::{info, warn};

mod api;
mod retention;
mod storage_safety;

#[derive(Debug, Parser)]
#[command(
    name = "nexus-engine",
    version,
    about = "Nexus edge engine — pipeline + API + UI in one process"
)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(
        short,
        long,
        env = "NEXUS_CONFIG",
        default_value = "config/single-camera.toml"
    )]
    config: PathBuf,

    /// Override `inference.backend` from the config (mock|in_process|pool).
    /// Convenience for smoke tests so we don't need a separate config.
    #[arg(long)]
    mock_detector: bool,

    /// Skip starting the HTTP server. Useful for headless soak runs.
    #[arg(long)]
    no_api: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg =
        Config::load(&cli.config).with_context(|| format!("loading config {:?}", cli.config))?;
    if cli.mock_detector {
        cfg.inference.model.kind = "mock".into();
    }

    let runtime = build_runtime(&cfg.runtime)?;
    runtime.block_on(run(cfg, cli))
}

fn build_runtime(cfg: &nexus_config::RuntimeConfig) -> Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if cfg.worker_threads > 0 {
        builder.worker_threads(cfg.worker_threads);
    }
    builder.max_blocking_threads(cfg.blocking_threads.max(1));
    Ok(builder.build()?)
}

async fn run(cfg: Config, cli: Cli) -> Result<()> {
    let _telemetry = nexus_telemetry::init(&cfg.telemetry)?;
    info!(version = env!("CARGO_PKG_VERSION"), "nexus-engine starting");

    let store = Arc::new(Store::open(&cfg.store).await?);
    if cfg.store.seed_from_config {
        store.seed_from_config_if_empty(&cfg).await?;
    }

    let rules = store.list_rules().await?;
    let evaluator = Arc::new(RuleEvaluator::new(&cfg.rules, &rules)?);
    info!(
        kind = ?evaluator.engine_kind(),
        rule_count = rules.len(),
        "rule evaluator built"
    );

    let bus = build_bus(&cfg.bus);

    let cameras = store.list_cameras().await?;

    // Router builds one InferenceLayer per kind referenced by any
    // camera (default + each unique override). Keeping disabled cameras
    // in the build set means re-enabling at runtime doesn't require a
    // process restart.
    let router =
        InferenceRouter::build(&cfg.inference, &cameras).context("building inference router")?;
    let pool = router.default_pool();
    log_inference_summary(&cfg.inference, pool.is_some(), &router);

    let tracker: Arc<dyn nexus_tracker::Tracker> = Arc::from(build_tracker(&cfg.tracker));
    let cache = Arc::new(LatestFrameCache::new());

    // Recorder is a per-process singleton: the watermark sampler
    // (storage_safety) and every per-camera supervisor share the
    // same Arc so panic-flag flips affect everything atomically.
    // Constructed BEFORE the per-camera spawn loop so the loop can
    // pass it in.
    let clips_dir = cfg.runtime.clips.clips_dir.clone();
    if let Err(e) = tokio::fs::create_dir_all(&clips_dir).await {
        warn!(path = %clips_dir.display(), error = %e, "could not pre-create clips_dir");
    }
    let recorder: Arc<dyn nexus_pipeline::ClipRecorder> = build_recorder(
        &cfg.runtime.clips.recorder,
        store.clone(),
        &clips_dir,
        &cameras,
        cfg.runtime.clips.pre_roll_secs,
    )?;
    info!(kind = recorder.kind(), "clip recorder constructed");

    let mut handles = Vec::new();
    for cam in cameras {
        if !cam.enabled {
            warn!(camera_id = cam.id, "camera disabled — skipping");
            continue;
        }
        let detector = router.detector_for_camera(&cam);
        let h = spawn_camera(
            cam,
            detector,
            tracker.clone(),
            cfg.tracker.annotator.clone(),
            cfg.tracker.static_object.clone(),
            cfg.runtime.clips.clone(),
            cfg.runtime.state_dir.clone(),
            evaluator.clone(),
            store.clone(),
            recorder.clone(),
            bus.clone(),
            cache.clone(),
        );
        handles.push(h);
    }

    // Storage safety floor (M2.1 Stage A PR 4). Watermark sampler
    // shares the same recorder Arc as the per-camera supervisors
    // above so panic-mode flips propagate atomically.
    let safety_cfg = storage_safety::StorageSafetyConfig {
        clips_dir: clips_dir.clone(),
        low_watermark_pct: cfg.runtime.clips.low_watermark_pct,
        panic_watermark_pct: cfg.runtime.clips.panic_watermark_pct,
        sample_interval: std::time::Duration::from_secs(
            cfg.runtime.clips.watermark_sample_interval_secs.max(1) as u64,
        ),
    };
    let probe: std::sync::Arc<dyn storage_safety::FreeSpaceProbe> =
        std::sync::Arc::new(storage_safety::StatvfsProbe {
            path: clips_dir.clone(),
        });
    let safety_handle = {
        let recorder = recorder.clone();
        let store = store.clone();
        let bus = bus.clone();
        tokio::spawn(async move {
            if let Err(e) =
                storage_safety::run_storage_safety(safety_cfg, probe, recorder, store, bus).await
            {
                tracing::error!(error = %e, "storage safety loop exited");
            }
        })
    };

    // Retention sweeper + orphan-file scan. Runs daily in production
    // (24h interval); shares the same shutdown channel as the
    // safety task so a Ctrl-C between sweep ticks doesn't have to
    // wait the full interval.
    let (retention_shutdown_tx, retention_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let retention_cfg = retention::RetentionConfig {
        clips_dir: clips_dir.clone(),
        retention_days: cfg.runtime.clips.motion_clips_retention_days,
        interval: std::time::Duration::from_secs(24 * 60 * 60),
    };
    let retention_handle = {
        let store = store.clone();
        tokio::spawn(async move {
            retention::run_retention(retention_cfg, store, async {
                let _ = retention_shutdown_rx.await;
            })
            .await;
        })
    };

    let api_state = api::ApiState {
        store: store.clone(),
        bus: bus.clone(),
        cache: cache.clone(),
        pool: pool.clone(),
        ui_root: cfg.server.ui_root.clone(),
        recorder: recorder.clone(),
        clips_dir: clips_dir.clone(),
    };

    if !cli.no_api {
        let bind = cfg.server.api_bind.clone();
        let app = api::router(api_state);
        let listener = tokio::net::TcpListener::bind(&bind).await?;
        info!(bind = %bind, "HTTP API + UI listening");
        let server = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app.into_make_service()).await {
                tracing::error!("axum error: {e}");
            }
        });
        wait_for_signal().await;
        info!("shutdown signal received");
        server.abort();
    } else {
        wait_for_signal().await;
    }

    safety_handle.abort();
    let _ = retention_shutdown_tx.send(());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), retention_handle).await;
    for h in handles {
        h.task.abort();
    }
    Ok(())
}

fn log_inference_summary(cfg: &InferenceConfig, has_pool: bool, router: &InferenceRouter) {
    let kinds: Vec<String> = router.detectors().into_iter().map(|(k, _)| k).collect();
    info!(
        backend = ?cfg.backend,
        workers = cfg.workers,
        default_kind = %cfg.model.kind,
        active_kinds = ?kinds,
        ep_priority = ?cfg.ep_priority,
        fail_soft = cfg.fail_soft,
        pool = has_pool,
        "inference router built"
    );
}

/// Build the per-process clip recorder according to
/// `cfg.runtime.clips.recorder`. The watermark sampler + every
/// per-camera supervisor share this single Arc so panic-flag flips
/// affect everything atomically.
///
/// `Stub` is always available. `Gstreamer` requires the `gstreamer`
/// cargo feature on `nexus-pipeline`; on a build without the feature
/// the engine logs an error + falls back to `Stub` so a misconfigured
/// box still records *something* (0-byte placeholder files) instead of
/// failing to boot.
fn build_recorder(
    kind: &RecorderKind,
    store: Arc<nexus_store::Store>,
    clips_dir: &std::path::Path,
    cameras: &[CameraConfig],
    pre_roll_secs: u32,
) -> Result<Arc<dyn nexus_pipeline::ClipRecorder>> {
    match kind {
        RecorderKind::Stub => Ok(Arc::new(nexus_pipeline::StubClipRecorder::new(
            store, clips_dir,
        ))),
        RecorderKind::Gstreamer => build_gst_recorder(store, clips_dir, cameras, pre_roll_secs),
    }
}

#[cfg(feature = "gstreamer")]
fn build_gst_recorder(
    store: Arc<nexus_store::Store>,
    clips_dir: &std::path::Path,
    cameras: &[CameraConfig],
    pre_roll_secs: u32,
) -> Result<Arc<dyn nexus_pipeline::ClipRecorder>> {
    // Build one always-on PreRollIngester per enabled camera. The
    // ingester holds the only RTSP connection for that camera; the
    // recorder consumes from its broadcast channel + ring snapshot.
    let mut ingesters: std::collections::HashMap<i64, Arc<nexus_pipeline::PreRollIngester>> =
        std::collections::HashMap::new();
    for cam in cameras {
        if !cam.enabled {
            continue;
        }
        match nexus_pipeline::PreRollIngester::new(cam.id, cam.url.to_string(), pre_roll_secs) {
            Ok(ing) => {
                tracing::info!(
                    camera_id = cam.id,
                    pre_roll_secs,
                    "pre-roll ingester started"
                );
                ingesters.insert(cam.id, ing);
            }
            Err(e) => {
                tracing::error!(
                    camera_id = cam.id,
                    error = %e,
                    "failed to start pre-roll ingester; this camera will refuse clips"
                );
            }
        }
    }
    let rec = nexus_pipeline::GstClipRecorder::new(store, clips_dir, ingesters)
        .map_err(|e| anyhow::anyhow!("GstClipRecorder::new: {e}"))?;
    Ok(Arc::new(rec))
}

#[cfg(not(feature = "gstreamer"))]
fn build_gst_recorder(
    store: Arc<nexus_store::Store>,
    clips_dir: &std::path::Path,
    _cameras: &[CameraConfig],
    _pre_roll_secs: u32,
) -> Result<Arc<dyn nexus_pipeline::ClipRecorder>> {
    tracing::error!(
        "config selected RecorderKind::Gstreamer but this build was compiled without \
         --features gstreamer; falling back to StubClipRecorder. Rebuild nexus-engine with \
         the gstreamer feature to record real video."
    );
    Ok(Arc::new(nexus_pipeline::StubClipRecorder::new(
        store, clips_dir,
    )))
}

async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

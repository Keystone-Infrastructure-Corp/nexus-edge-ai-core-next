use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use nexus_bus::build_bus;
use nexus_config::{Config, InferenceConfig};
use nexus_inference::InferenceRouter;
use nexus_pipeline::{spawn_camera, LatestFrameCache};
use nexus_rules::RuleEvaluator;
use nexus_store::{EventStore, Store};
use nexus_tracker::build_tracker;
use tracing::{info, warn};

mod api;

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
            evaluator.clone(),
            store.clone() as Arc<dyn EventStore>,
            bus.clone(),
            cache.clone(),
        );
        handles.push(h);
    }

    let api_state = api::ApiState {
        store: store.clone(),
        bus: bus.clone(),
        cache: cache.clone(),
        pool: pool.clone(),
        ui_root: cfg.server.ui_root.clone(),
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

async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

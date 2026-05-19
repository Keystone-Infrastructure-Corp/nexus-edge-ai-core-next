use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use nexus_bus::build_bus;
use nexus_config::{CameraConfig, Config, InferenceConfig, RecorderKind};
use nexus_inference::InferenceRouter;
use nexus_pipeline::{spawn_camera, LatestFrameCache};
use nexus_rules::RuleEvaluator;
use nexus_store::Store;
use nexus_tracker::build_tracker;
use tracing::{info, warn};

mod admin_auth;
mod api;
mod auth;
mod auth_bootstrap;
mod audit_retention;
mod cold_read_cache;
mod cold_replicator;
mod delivery_reload;
mod discovery;
#[cfg(unix)]
mod fd_limit;
mod models_catalog;
mod oauth_sessions;
mod reconciler;
mod retention;
mod storage_safety;
// M7 Step 6F2 — only compiled when the `test-injection` feature
// is on (off in any production build). Wires the dev-only
// `POST /api/v1/_test/inject_event` handler.
#[cfg(feature = "test-injection")]
mod test_inject;
mod usb_watch;

/// Default config path used when neither `--config` nor `--tier` is given.
/// Matches the M0/M1 dev-loop expectation (run from repo root).
const DEFAULT_CONFIG: &str = "config/single-camera.toml";

#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum TierChoice {
    /// Run `nexus-probe` in-process and load the recommended tier file.
    Auto,
    T10,
    T24,
    T36,
    T36s,
    T64,
    /// Apple-silicon / fallback dev profile (`config/single-camera.toml`).
    Dev,
}

impl TierChoice {
    /// Resolve a named tier (anything other than `Auto`) to its
    /// canonical config-file path. `Auto` is handled by the caller
    /// because it requires a probe round-trip.
    fn config_path(self) -> Option<PathBuf> {
        match self {
            TierChoice::Auto => None,
            TierChoice::T10 => Some(PathBuf::from("config/tiers/t10.toml")),
            TierChoice::T24 => Some(PathBuf::from("config/tiers/t24.toml")),
            TierChoice::T36 => Some(PathBuf::from("config/tiers/t36.toml")),
            TierChoice::T36s => Some(PathBuf::from("config/tiers/t36s.toml")),
            TierChoice::T64 => Some(PathBuf::from("config/tiers/t64.toml")),
            TierChoice::Dev => Some(PathBuf::from("config/single-camera.toml")),
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "nexus-engine",
    version,
    about = "Nexus edge engine — pipeline + API + UI in one process"
)]
struct Cli {
    /// Path to the TOML config file. When set, takes precedence over
    /// `--tier`. Falls back to `config/single-camera.toml` if neither
    /// `--config` nor `--tier` is provided.
    #[arg(short, long, env = "NEXUS_CONFIG")]
    config: Option<PathBuf>,

    /// Hardware tier to load (M-Install Checkpoint 1). `auto` runs
    /// `nexus-probe` in-process and picks the matching
    /// `config/tiers/<tier>.toml`. Ignored when `--config` is given.
    #[arg(long, env = "NEXUS_TIER", value_enum)]
    tier: Option<TierChoice>,

    /// Override `inference.backend` from the config (mock|in_process|pool).
    /// Convenience for smoke tests so we don't need a separate config.
    #[arg(long)]
    mock_detector: bool,

    /// Skip starting the HTTP server. Useful for headless soak runs.
    #[arg(long)]
    no_api: bool,
}

impl Cli {
    /// Resolve the config path according to the precedence rules:
    /// `--config` > `--tier` > built-in default.
    ///
    /// Emits status to stderr because tracing is not initialised yet
    /// at this point in startup; operators get immediate feedback on
    /// which file the engine is about to load.
    fn resolved_config_path(&self) -> PathBuf {
        if let Some(path) = self.config.clone() {
            return path;
        }
        if let Some(tier) = self.tier {
            match tier {
                TierChoice::Auto => {
                    let path = nexus_probe::recommend_tier_config_path();
                    eprintln!(
                        "nexus-engine: --tier auto -> probe recommends {}",
                        path.display()
                    );
                    return path;
                }
                other => {
                    let path = other
                        .config_path()
                        .expect("non-Auto TierChoice always returns Some(path)");
                    eprintln!("nexus-engine: --tier {:?} -> {}", other, path.display());
                    return path;
                }
            }
        }
        PathBuf::from(DEFAULT_CONFIG)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.resolved_config_path();
    let (mut cfg, compat) = Config::load_with_compat(&config_path)
        .with_context(|| format!("loading config {:?}", config_path))?;
    if cli.mock_detector {
        cfg.inference.model.kind = "mock".into();
    }

    // Raise the per-process file-descriptor cap BEFORE tokio spins
    // up any I/O. The LAN discovery sweep can open 100s of sockets
    // in parallel; the macOS default `ulimit -n` of 256 caused
    // GLib's GWakeup helper (used internally by gstreamer pipelines)
    // to call `g_error("Creating pipes for GWakeup: …")` and abort
    // the entire process with SIGTRAP partway through a scan. The
    // bump is best-effort — on failure we keep the original limit
    // and let `discovery::scan` clamp its concurrency accordingly.
    #[cfg(unix)]
    {
        let _ = fd_limit::raise_fd_soft_limit();
    }

    // Apply M-Install Checkpoint 2 auth posture rules:
    // - WARN about grandfathered missing-[auth]-section configs
    // - auto-provision dev_token at <state_dir>/dev-token when needed
    // - refuse to boot if mode = none + non-loopback bind
    //
    // Tracing isn't initialised yet, so the WARN+INFO emitted by
    // auth_bootstrap reach stderr through the global default
    // subscriber (no JSON formatting, no OTLP). That is intentional
    // — the operator-visible secret-token line MUST land before
    // anything else can swallow it.
    let state_dir = auth_bootstrap::state_dir(&cfg);
    auth_bootstrap::apply(&mut cfg, &state_dir, compat)?;

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

    // M6 Phase 2 Step 2.7 — first-boot admin bootstrap. Runs
    // exactly once across the lifetime of the database file;
    // subsequent boots see `SkippedAlreadyBootstrapped` and
    // emit no log line. Mode-gated: only fires for `Local` and
    // `Hybrid`. When it does fire, the one-time password is
    // surfaced via a single `warn!` line — operators MUST grab
    // it from the journal and rotate via `/change-password` on
    // first login. The plaintext is dropped from the engine's
    // memory immediately after this match.
    match auth::bootstrap::bootstrap_if_needed(&store, cfg.auth.mode).await {
        Ok(auth::bootstrap::BootstrapOutcome::AdminCreated {
            user_id,
            username,
            one_time_password,
        }) => {
            tracing::warn!(
                user_id,
                username = %username,
                one_time_password = %one_time_password,
                "FIRST-BOOT ADMIN PROVISIONED — log in once, change password, then redact this log line",
            );
        }
        Ok(auth::bootstrap::BootstrapOutcome::SkippedAlreadyBootstrapped) => {
            tracing::debug!("bootstrap skipped: users table already populated");
        }
        Ok(auth::bootstrap::BootstrapOutcome::SkippedModeDisallowsLocal) => {
            tracing::debug!(mode = ?cfg.auth.mode, "bootstrap skipped: mode does not allow local users");
        }
        Err(e) => {
            return Err(anyhow::anyhow!("first-boot admin bootstrap failed: {e}"));
        }
    }

    // M6 Phase 3 Step 3.3 — discover the OIDC IdP when the
    // configured auth mode allows OIDC AND an `[auth.oidc]`
    // block exists. Discovery is an async HTTP hop to the
    // IdP's `/.well-known/openid-configuration`; we tolerate
    // failure here only by *logging* (boot still proceeds) so
    // a transient IdP outage doesn't keep cameras offline.
    // If discovery fails the OIDC routes simply aren't mounted
    // and the UI falls back to local-only login (assuming
    // hybrid mode). For pure-OIDC mode this means no one can
    // log in until the IdP recovers + the engine is bounced.
    let oidc_login_state = match (cfg.auth.mode.allows_oidc(), cfg.auth.oidc.as_ref()) {
        (true, Some(oidc_cfg)) => {
            match auth::oidc::OidcClient::discover(oidc_cfg.clone()).await {
                Ok(client) => {
                    let client = std::sync::Arc::new(client);
                    // Start the 1h background refresh loop so
                    // JWKS rotations land without a bounce.
                    let _refresh_handle = std::sync::Arc::clone(&client).spawn_refresh();
                    tracing::info!(
                        issuer = %oidc_cfg.issuer,
                        "OIDC discovery succeeded; auth-code routes will be mounted"
                    );
                    Some(auth::oidc_login::OidcLoginState {
                        store: store.clone(),
                        admin_auth: std::sync::Arc::new(
                            admin_auth::AdminAuthState::from_config(&cfg.auth)
                                .context("building admin-auth state for oidc login")?,
                        ),
                        oidc_client: client,
                        cfg: oidc_cfg.clone(),
                        sessions: auth::oidc_login::OidcLoginSessions::new(),
                    })
                }
                Err(e) => {
                    tracing::warn!(
                        issuer = %oidc_cfg.issuer,
                        error = %e,
                        "OIDC discovery failed at boot; auth-code routes will NOT be mounted",
                    );
                    None
                }
            }
        }
        (true, None) => {
            tracing::warn!(
                "auth.mode allows OIDC but no [auth.oidc] block is configured; auth-code routes will NOT be mounted",
            );
            None
        }
        _ => None,
    };

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
    let router = Arc::new(
        InferenceRouter::build(&cfg.inference, &cameras).context("building inference router")?,
    );
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

    // M2.2 Phase 3: USB hot-plug registry + watcher. Constructed
    // BEFORE the recorder so the recorder builder can clone the
    // registry. The watcher polls `<clips_dir>/usb/` for `NEXUS_*`
    // mounts and updates the registry the recorder consults at
    // open() time. None preferred_label means USB tiering is
    // observable (the API surfaces attached volumes) but no clip
    // is ever routed to USB until the operator sets a preferred
    // label in `nexus.toml`.
    let usb_registry = usb_watch::UsbRegistry::new();
    let usb_resolver: Arc<dyn nexus_pipeline::recorder::UsbResolver> =
        Arc::new(usb_registry.clone());
    // The preferred USB label is now a runtime-mutable handle:
    //   1. `engine_runtime_settings` (admin override) takes
    //      precedence; a NULL row means "explicitly cleared".
    //   2. Falling back to `cfg.runtime.clips.preferred_usb_label`
    //      preserves TOML-only deployments that haven't touched
    //      the new admin endpoint.
    //
    // The handle is shared between the recorder (reads on every
    // `open()`) and the API (mutates via PUT /admin/runtime/usb_preferred).
    let initial_label = match store.read_runtime_setting("preferred_usb_label").await {
        Ok(Some(stored)) => stored, // Some(Some(s)) -> Some(s); Some(None) -> None (explicit clear)
        Ok(None) => cfg.runtime.clips.preferred_usb_label.clone(),
        Err(e) => {
            warn!(error = %e, "could not read preferred_usb_label from engine_runtime_settings; falling back to TOML");
            cfg.runtime.clips.preferred_usb_label.clone()
        }
    };
    let preferred_usb_label =
        nexus_pipeline::recorder::PreferredUsbLabel::new(initial_label.clone());

    let recorder: Arc<dyn nexus_pipeline::ClipRecorder> = build_recorder(
        &cfg.runtime.clips.recorder,
        store.clone(),
        &clips_dir,
        &cameras,
        cfg.runtime.clips.pre_roll_secs,
        bus.clone(),
        usb_resolver.clone(),
        preferred_usb_label.clone(),
    )?;
    info!(
        kind = recorder.kind(),
        preferred_usb_label = ?initial_label,
        "clip recorder constructed"
    );

    // Shared map of running supervisors keyed by camera id. Populated
    // here for cameras that exist at boot and mutated by the
    // `reconciler` task in response to `topic::CONFIG_CHANGED` events.
    // The shutdown sweep at the bottom of `main()` iterates this map
    // to abort every supervisor — no separate `Vec<CameraHandle>`
    // because the reconciler may add/remove entries at any time.
    let running: reconciler::HandleMap =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    for cam in cameras {
        if !cam.ingest.enabled {
            warn!(camera_id = cam.id, "camera disabled — skipping");
            continue;
        }
        let cam_id = cam.id;
        let cam_url = cam.ingest.url.to_string();
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
        running.lock().insert(
            cam_id,
            reconciler::RunningCameraEntry {
                task: Arc::new(h.task),
                url: cam_url,
            },
        );
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
    let watermark_signal = storage_safety::WatermarkSignal::new();
    let safety_handle = {
        let recorder = recorder.clone();
        let store = store.clone();
        let bus = bus.clone();
        let signal = watermark_signal.clone();
        tokio::spawn(async move {
            if let Err(e) =
                storage_safety::run_storage_safety(safety_cfg, probe, recorder, store, bus, signal)
                    .await
            {
                tracing::error!(error = %e, "storage safety loop exited");
            }
        })
    };

    // M2.2 cold-mirror replicator. Hydrate the registry from the
    // `storage_backends` table at boot and spawn the follower task.
    // The registry is shared (cheap Arc clone) — the API layer can
    // call `Registry::replace_all` after mutating storage_backends
    // to live-update without restarting the engine.
    let registry = nexus_storage::Registry::new();
    match store.list_storage_backends().await {
        Ok(rows) => {
            let mut backends = Vec::with_capacity(rows.len());
            for row in rows {
                match nexus_storage::build_backend(&row.handle, &row.kind, &row.config_json) {
                    Ok(b) => backends.push(b),
                    Err(e) => warn!(
                        handle = %row.handle,
                        kind = %row.kind,
                        error = %e,
                        "cold replicator: skipping backend that failed to build"
                    ),
                }
            }
            registry.replace_all(backends);
        }
        Err(e) => warn!(error = %e, "cold replicator: list_storage_backends failed at boot"),
    }
    let (cold_shutdown_tx, cold_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let cold_handle = {
        let store = store.clone();
        let bus = bus.clone();
        let registry = registry.clone();
        let cfg = cold_replicator::ColdReplicatorConfig {
            clips_dir: clips_dir.clone(),
        };
        tokio::spawn(async move {
            cold_replicator::run_cold_replicator(cfg, store, bus, registry, async {
                let _ = cold_shutdown_rx.await;
            })
            .await;
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

    // M6 Phase 4 Step 4.4 — audit-log retention sweeper. Daily
    // task that deletes `audit_log` rows older than
    // `runtime.audit.retention_days`. retention_days=0 keeps the
    // task alive but disables the sweep (operators who ship to an
    // external SIEM).
    let (audit_retention_shutdown_tx, audit_retention_shutdown_rx) =
        tokio::sync::oneshot::channel::<()>();
    let audit_retention_cfg = audit_retention::AuditRetentionConfig {
        retention_days: cfg.runtime.audit.retention_days,
        interval: std::time::Duration::from_secs(24 * 60 * 60),
    };
    let audit_retention_handle = {
        let store = store.clone();
        tokio::spawn(async move {
            audit_retention::run_audit_retention(audit_retention_cfg, store, async {
                let _ = audit_retention_shutdown_rx.await;
            })
            .await;
        })
    };

    // M2.2 Phase 3: USB hot-plug watcher. Polls `<clips_dir>/usb/`
    // for `NEXUS_*`-labeled subdirectories every 5s and updates
    // `usb_registry` so the recorder + API see attach/detach
    // events promptly.
    let (usb_shutdown_tx, usb_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let usb_watch_handle = {
        let cfg = usb_watch::UsbWatchConfig::new(&clips_dir);
        let registry = usb_registry.clone();
        let store_for_watch = store.clone();
        let bus = bus.clone();
        tokio::spawn(async move {
            usb_watch::run_usb_watch(cfg, registry, store_for_watch, bus, async {
                let _ = usb_shutdown_rx.await;
            })
            .await;
        })
    };

    // M7 alert-sink dispatcher. Drains `alert_sink_outbox` and
    // ships each row through the registered sinks. The registry
    // is populated from `cfg.sinks` at boot. Step 5 swaps the
    // Step 3 `AllowAllPolicy` stub for the real `CascadingPolicy`
    // (global `delivery_settings` × per-rule
    // `delivery_policy_json`, see `docs/M7_DELIVERY.md`).
    // With no `[[sinks]]` in the config the registry is empty
    // and the dispatcher spins quietly because
    // `record_event_and_enqueue` enqueues nothing.
    let sink_registry = std::sync::Arc::new(nexus_sinks::SinkRegistry::new());
    let configured_sinks = nexus_sinks::build_sinks_from_config(&cfg.sinks)
        .context("M7: build alert-delivery sinks from cfg.sinks")?;
    let n_sinks = sink_registry.replace(configured_sinks);
    if n_sinks > 0 {
        info!(n_sinks, "M7: alert-delivery sinks registered");
    }
    let cascading_policy = std::sync::Arc::new(
        nexus_sinks::policy::CascadingPolicy::hydrate(&store)
            .await
            .context("M7: hydrate delivery policy from store")?,
    );
    let delivery_policy: std::sync::Arc<dyn nexus_sinks::dispatcher::DeliveryPolicy> =
        cascading_policy.clone();
    let (delivery_reload_handle, delivery_reload_shutdown_tx) =
        delivery_reload::spawn(bus.clone(), store.clone(), cascading_policy.clone());
    let (dispatcher_shutdown_tx, dispatcher_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let dispatcher_handle = {
        let store = store.clone();
        let sink_registry = sink_registry.clone();
        let policy = delivery_policy.clone();
        tokio::spawn(async move {
            nexus_sinks::dispatcher::run_dispatcher(
                nexus_sinks::dispatcher::SinkDispatcherConfig::default(),
                store,
                sink_registry,
                policy,
                async {
                    let _ = dispatcher_shutdown_rx.await;
                },
            )
            .await;
        })
    };

    // Camera hot-reload reconciler — subscribes to
    // `topic::CONFIG_CHANGED` and converges the live camera set
    // (DB) with the in-process supervisor / ingester set. Without
    // this, cameras added via the discovery UI (or `PUT /api/cameras/{id}`)
    // persist to disk but never get a pipeline until the next
    // engine restart.
    let reconciler_handle = reconciler::spawn(reconciler::ReconcilerArgs {
        router: router.clone(),
        tracker: tracker.clone(),
        annotator: cfg.tracker.annotator.clone(),
        static_object: cfg.tracker.static_object.clone(),
        clips: cfg.runtime.clips.clone(),
        state_dir: cfg.runtime.state_dir.clone(),
        evaluator: evaluator.clone(),
        store: store.clone(),
        recorder: recorder.clone(),
        bus: bus.clone(),
        cache: cache.clone(),
        pre_roll_secs: cfg.runtime.clips.pre_roll_secs,
        handles: running.clone(),
    });

    let cache_jobs = cold_read_cache::CacheJobs::new(
        store.clone(),
        registry.clone(),
        clips_dir.clone(),
        watermark_signal.clone(),
    );

    // Detector prompt catalog — boot-time snapshot of every kind
    // the router knows about + its vocabulary. The UI calls
    // `GET /api/v1/models/prompts` so the camera + rules forms can
    // render kind-appropriate label pickers (chip strip for
    // closed-vocab COCO, free-text + suggestions for open-vocab
    // yolo_world). See `models_catalog.rs` for details.
    let model_prompts = std::sync::Arc::new(models_catalog::build_catalog(&cfg.inference, &router));

    let api_state = api::ApiState {
        store: store.clone(),
        bus: bus.clone(),
        evaluator: evaluator.clone(),
        cache: cache.clone(),
        pool: pool.clone(),
        ui_root: cfg.server.ui_root.clone(),
        recorder: recorder.clone(),
        clips_dir: clips_dir.clone(),
        low_watermark_pct: cfg.runtime.clips.low_watermark_pct,
        panic_watermark_pct: cfg.runtime.clips.panic_watermark_pct,
        registry: registry.clone(),
        cache_jobs: cache_jobs.clone(),
        usb_registry: usb_registry.clone(),
        preferred_usb_label: preferred_usb_label.clone(),
        model_prompts: model_prompts.clone(),
        // M7 Step 6 — shared with the dispatcher above so
        // `/api/v1/admin/sinks/health` can list configured sinks
        // even if they've never produced an outbox row.
        sink_registry: sink_registry.clone(),
        // M2.2 closeout — in-memory pending-session cache for the
        // OAuth auth-code dance. Empty at boot; lives only as long
        // as the process. Operators who restart mid-consent just
        // retry from the UI.
        oauth_sessions: oauth_sessions::OAuthSessions::new(),
        // HS256 bearer state — see `admin_auth` module docs.
        // Boot fails closed if `auth.admin_secret_path` points at
        // an unreadable file; absence of the path is the expected
        // state for LAN-only / single-box deployments.
        admin_auth: std::sync::Arc::new(
            admin_auth::AdminAuthState::from_config(&cfg.auth)
                .context("building admin-auth state")?,
        ),
        // M-Admin Phase 1B — empty registry at boot; populated by
        // the four `/api/v1/admin/discovery/*` handlers. The
        // eviction sweep spawned below drops stale sessions on a
        // 60 s tick.
        discovery_sessions: discovery::DiscoverySessions::new(),
        // M6 Phase 2 Step 2.7 — snapshot the lockout policy at
        // boot so `auth::login::post_login` doesn't have to
        // re-read it on every request. Hot-reload of the policy
        // requires an engine restart for now (acceptable — these
        // knobs change once a quarter at most).
        lockout: cfg.runtime.auth.lockout.clone(),
        // M6 Phase 2 Step 2.9 — snapshot the auth mode so
        // `GET /api/v1/auth/info` can surface it to the UI
        // without re-reading config on every request.
        auth_mode: cfg.auth.mode,
        // M6 Phase 3 Step 3.3 — OIDC login state populated
        // above. `None` when OIDC isn't enabled or discovery
        // failed; the router skips the auth-code routes in
        // that case.
        oidc_login: oidc_login_state,
    };

    // M-Admin Phase 1B — start the registry eviction sweep. Holds
    // its own Arc on the DashMap; no shutdown signal needed because
    // the task does pure read-then-remove work on a 60 s tick.
    discovery::spawn_eviction_sweep(api_state.discovery_sessions.clone());

    if !cli.no_api {
        let bind = cfg.server.api_bind.clone();
        let app = api::router(api_state);
        let listener = tokio::net::TcpListener::bind(&bind).await?;
        info!(bind = %bind, "HTTP API + UI listening");
        // `into_make_service_with_connect_info::<SocketAddr>()`
        // is required so the admin-auth middleware can pull the
        // peer address out of request extensions for the
        // loopback-fallback check (see admin_auth module).
        let server = tokio::spawn(async move {
            if let Err(e) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            {
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
    let _ = cold_shutdown_tx.send(());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), cold_handle).await;
    let _ = retention_shutdown_tx.send(());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), retention_handle).await;
    let _ = audit_retention_shutdown_tx.send(());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), audit_retention_handle).await;
    let _ = usb_shutdown_tx.send(());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), usb_watch_handle).await;
    let _ = dispatcher_shutdown_tx.send(());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), dispatcher_handle).await;
    let _ = delivery_reload_shutdown_tx.send(());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), delivery_reload_handle).await;
    reconciler_handle.abort();
    // Abort every per-camera supervisor. `drain()` empties the map
    // under one lock acquisition; the reconciler is already aborted
    // above so nothing will re-populate it.
    for (_, entry) in running.lock().drain() {
        entry.task.abort();
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
// Single internal scaffolding fn — pulling the eight wires through a
// struct just to satisfy the lint costs more clarity than it buys.
#[allow(clippy::too_many_arguments)]
fn build_recorder(
    kind: &RecorderKind,
    store: Arc<nexus_store::Store>,
    clips_dir: &std::path::Path,
    cameras: &[CameraConfig],
    pre_roll_secs: u32,
    bus: Arc<dyn nexus_bus::Bus>,
    usb_resolver: Arc<dyn nexus_pipeline::recorder::UsbResolver>,
    preferred_usb_label: nexus_pipeline::recorder::PreferredUsbLabel,
) -> Result<Arc<dyn nexus_pipeline::ClipRecorder>> {
    match kind {
        RecorderKind::Stub => Ok(Arc::new(
            nexus_pipeline::StubClipRecorder::new(store, clips_dir)
                .with_bus(bus)
                .with_usb(usb_resolver, preferred_usb_label),
        )),
        RecorderKind::Gstreamer => build_gst_recorder(
            store,
            clips_dir,
            cameras,
            pre_roll_secs,
            bus,
            usb_resolver,
            preferred_usb_label,
        ),
    }
}

#[cfg(feature = "gstreamer")]
fn build_gst_recorder(
    store: Arc<nexus_store::Store>,
    clips_dir: &std::path::Path,
    cameras: &[CameraConfig],
    pre_roll_secs: u32,
    bus: Arc<dyn nexus_bus::Bus>,
    usb_resolver: Arc<dyn nexus_pipeline::recorder::UsbResolver>,
    preferred_usb_label: nexus_pipeline::recorder::PreferredUsbLabel,
) -> Result<Arc<dyn nexus_pipeline::ClipRecorder>> {
    // Build one always-on PreRollIngester per enabled camera. The
    // ingester holds the only RTSP connection for that camera; the
    // recorder consumes from its broadcast channel + ring snapshot.
    let mut ingesters: std::collections::HashMap<i64, Arc<nexus_pipeline::PreRollIngester>> =
        std::collections::HashMap::new();
    for cam in cameras {
        if !cam.ingest.enabled {
            continue;
        }
        match nexus_pipeline::PreRollIngester::new(
            cam.id,
            cam.ingest.url.to_string(),
            pre_roll_secs,
        ) {
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
        .map_err(|e| anyhow::anyhow!("GstClipRecorder::new: {e}"))?
        .with_bus(bus)
        .with_usb(usb_resolver, preferred_usb_label);
    Ok(Arc::new(rec))
}

#[cfg(not(feature = "gstreamer"))]
fn build_gst_recorder(
    store: Arc<nexus_store::Store>,
    clips_dir: &std::path::Path,
    _cameras: &[CameraConfig],
    _pre_roll_secs: u32,
    bus: Arc<dyn nexus_bus::Bus>,
    usb_resolver: Arc<dyn nexus_pipeline::recorder::UsbResolver>,
    preferred_usb_label: nexus_pipeline::recorder::PreferredUsbLabel,
) -> Result<Arc<dyn nexus_pipeline::ClipRecorder>> {
    tracing::error!(
        "config selected RecorderKind::Gstreamer but this build was compiled without \
         --features gstreamer; falling back to StubClipRecorder. Rebuild nexus-engine with \
         the gstreamer feature to record real video."
    );
    Ok(Arc::new(
        nexus_pipeline::StubClipRecorder::new(store, clips_dir)
            .with_bus(bus)
            .with_usb(usb_resolver, preferred_usb_label),
    ))
}

async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

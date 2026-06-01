use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use nexus_bus::build_bus;
use nexus_config::{CameraConfig, Config, InferenceConfig, RecorderKind};
use nexus_inference::InferenceRouter;
use nexus_pipeline::{
    spawn_camera, FrameStatsRegistry, LatestFrameCache, StaticAnchorClearRegistry,
};
use nexus_rules::RuleEvaluator;
use nexus_store::Store;
use nexus_tracker::build_tracker;
use tracing::{debug, info, warn};

mod admin_auth;
mod admin_cli;
mod admin_cloud;
mod admin_network;
mod admin_runtime;
mod api;
mod audit_retention;
mod auth;
mod auth_bootstrap;
mod cloud_audit;
mod cloud_enroll;
mod cloud_sighting;
mod cloud_tunnel;
mod cold_read_cache;
mod cold_replicator;
mod delivery_reload;
mod discovery;
mod engine_rpc;
mod entity_local_persist;
#[cfg(unix)]
mod fd_limit;
mod gpu;
mod models_catalog;
mod network;
mod npu;
mod oauth_sessions;
mod reconciler;
mod retention;
mod roster;
mod setup;
mod storage_safety;
mod system_metrics;
// M7 Step 6F2 — only compiled when the `test-injection` feature
// is on (off in any production build). Wires the dev-only
// `POST /api/v1/_test/inject_event` handler.
#[cfg(feature = "test-injection")]
mod test_inject;
mod time_sync;
mod tls;
mod usb_watch;
mod visual_prompts_admin;

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
    ///
    /// `global = true` so the flag may appear before OR after a
    /// subcommand name — `install.sh` invokes
    /// `nexus-engine set-admin-password --config <path> ...` and
    /// `nexus-engine tls init --config <path>` with the flag in the
    /// post-subcommand position, and clap would otherwise reject it
    /// with `unexpected argument '--config' found`.
    #[arg(short, long, env = "NEXUS_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Hardware tier to load (M-Install Checkpoint 1). `auto` runs
    /// `nexus-probe` in-process and picks the matching
    /// `config/tiers/<tier>.toml`. Ignored when `--config` is given.
    #[arg(long, env = "NEXUS_TIER", value_enum, global = true)]
    tier: Option<TierChoice>,

    /// Override `inference.backend` from the config (mock|in_process|pool).
    /// Convenience for smoke tests so we don't need a separate config.
    #[arg(long)]
    mock_detector: bool,

    /// Skip starting the HTTP server. Useful for headless soak runs.
    #[arg(long)]
    no_api: bool,

    /// Optional subcommand. When absent, `nexus-engine` runs the
    /// pipeline + API + UI (the default behaviour). When present,
    /// the subcommand runs and exits.
    #[command(subcommand)]
    command: Option<Cmd>,
}

/// Subcommand verbs. `enroll` is the one-shot cloud onboarding
/// flow; `set-admin-password` lets the installer (and the
/// forgot-password recovery path) write the local `admin`
/// password from stdin without spinning up the full pipeline.
/// Future entries will add `rotate-cert`, `factory-reset`, etc.
#[derive(Debug, Subcommand)]
enum Cmd {
    /// Enroll this core against a cloud console using a one-shot code
    /// minted from the "Add Core" flow. Writes the resulting mTLS
    /// bundle into the local store; the next `nexus-engine` boot picks
    /// up the row and starts the WSS tunnel.
    Enroll(cloud_enroll::EnrollArgs),

    /// Set (or create) the local admin password from stdin. Used by
    /// `scripts/install.sh` on first install so the operator picks
    /// the password instead of grepping a one-time OTP out of a
    /// sentinel file, and as the forgot-password recovery path.
    /// Reads the plaintext from stdin (one line) or from
    /// `--password-file <PATH>`.
    SetAdminPassword(admin_cli::SetAdminPasswordArgs),

    /// TLS material management for the engine's in-process HTTPS
    /// listener (M-HTTPS Phase 1). Subcommands:
    /// `init` — generate a self-signed leaf at the configured
    /// `tls_cert_path` / `tls_key_path` if none exists. Idempotent
    /// unless `--force` is passed. Called by the installer on
    /// first boot so an upgraded appliance is HTTPS-ready without
    /// operator action.
    #[command(subcommand)]
    Tls(TlsCmd),
}

#[derive(Debug, Subcommand)]
enum TlsCmd {
    /// Generate a self-signed TLS leaf at
    /// `cfg.server.tls_cert_path` / `cfg.server.tls_key_path` if
    /// no PEM is present. SAN list covers the system hostname,
    /// `<hostname>.local`, `nexus.local`, `localhost`, every
    /// non-loopback IPv4/IPv6 on a local interface, plus
    /// `127.0.0.1` / `::1`. Owner mode `0644` for the cert,
    /// `0640` for the key (root:nexus assumed via systemd).
    Init {
        /// Regenerate the leaf even when valid PEM is already on
        /// disk. Default behaviour preserves the existing pair
        /// so operator-installed or cloud-issued certs are not
        /// trampled by a subsequent install run.
        #[arg(long)]
        force: bool,
    },
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
    let _ = compat; // CompatNotice is currently empty; kept for future shims.
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

    // Apply M-Admin Phase 0 auth posture rules:
    // - auto-provision the admin-secret file under
    //   <state_dir>/admin-secret when mode = local | hybrid and no
    //   path is pinned in nexus.toml
    // - leave OIDC-only deployments alone
    //
    // Tracing isn't initialised yet, so any messages emitted by
    // auth_bootstrap reach stderr through the global default
    // subscriber. That is intentional — the operator-visible
    // bootstrap line MUST land before anything else can swallow it.
    let state_dir = auth_bootstrap::state_dir(&cfg);
    auth_bootstrap::apply(&mut cfg, &state_dir)?;

    let runtime = build_runtime(&cfg.runtime)?;

    // Route subcommands BEFORE the serve path. Each subcommand owns
    // its own tracing / logging story so we don't bring up the full
    // pipeline scaffold for a one-shot operation.
    if let Some(cmd) = cli.command.as_ref() {
        return match cmd {
            Cmd::Enroll(args) => runtime.block_on(cloud_enroll::run_enroll(&cfg, args)),
            Cmd::SetAdminPassword(args) => runtime.block_on(admin_cli::run(&cfg, args)),
            Cmd::Tls(TlsCmd::Init { force }) => run_tls_init(&cfg, *force),
        };
    }

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
    // Phase 1.14 — pre-create the trace uploader channel BEFORE
    // telemetry init so the producer half can be wired into the
    // tracing subscriber straight away (every engine span captured by
    // the layer's EnvFilter from that point on enqueues onto the
    // bounded mpsc). The consumer half is parked here until
    // `cloud_tunnel::spawn_tunnel` reads cloud_enrollment and either
    // (a) spawns the uploader task with the matching mTLS material,
    // or (b) drops the receiver — at which point the bounded channel
    // fills at `queue_capacity` and pushes fail closed per Hard Rule 5
    // (fail-open: the engine continues without cloud trace shipping).
    let (trace_handle, trace_rx) = nexus_cloud_client::trace_uploader::TraceUploader::channel(
        nexus_cloud_client::trace_uploader::DEFAULT_QUEUE_CAPACITY,
    );

    let _telemetry = nexus_telemetry::init(&cfg.telemetry, Some(trace_handle))?;
    info!(
        // See `build.rs` — release-tag at CI build-time, falls back
        // to `CARGO_PKG_VERSION` for local dev builds.
        version = env!("NEXUS_BUILD_VERSION"),
        "nexus-engine starting"
    );

    let store = Arc::new(Store::open(&cfg.store).await?);
    if cfg.store.seed_from_config {
        store.seed_from_config_if_empty(&cfg).await?;
    }

    // v0.1.36 — bootstrap the 20-min idle-timeout pipeline.
    // The bounded channel decouples the request hot path (try_send,
    // never blocks) from the background drain that batches updates
    // into a single UPDATE per chain per 250 ms tick. Capacity 4096
    // absorbs ~16k req/s for a tick before a single bump is dropped;
    // dropping a bump just lags `last_active_at` by one tick, which
    // is acceptable against a 20-min ceiling.
    let (idle_bump_tx, idle_bump_rx) =
        tokio::sync::mpsc::channel::<auth::require_role::IdleBump>(4096);
    {
        let drain_store = store.clone();
        tokio::spawn(idle_bump_drain(drain_store, idle_bump_rx));
    }

    // M-Admin Phase 0 — apply any operator-persisted auth
    // override BEFORE we touch `cfg.auth` for bootstrap, OIDC
    // discovery, or the admin-auth-state builder. Mirrors the
    // `preferred_usb_label` precedence: a stored row wins over
    // `nexus.toml`. Failure to decode an existing row is loud
    // (we want the operator to notice) but non-fatal — we fall
    // back to the on-disk config so a corrupt setting can't
    // hard-brick the engine.
    let mut cfg = cfg;
    match store.read_runtime_setting("auth_config_json").await {
        Ok(Some(Some(json))) => match serde_json::from_str::<nexus_config::AuthConfig>(&json) {
            Ok(parsed) => {
                tracing::warn!(
                    mode = ?parsed.mode,
                    "applying operator-persisted auth.config from engine_runtime_settings (overrides nexus.toml)",
                );
                cfg.auth = parsed;
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "engine_runtime_settings.auth_config_json failed to decode; falling back to nexus.toml",
                );
            }
        },
        Ok(Some(None)) => {
            tracing::debug!("auth_config_json present but NULL; using nexus.toml");
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, "could not read auth_config_json from engine_runtime_settings; using nexus.toml");
        }
    }

    // M-Admin Phase 0 follow-up — apply any operator-persisted
    // inference-model override BEFORE we hand `cfg.inference`
    // to the InferenceRouter / pool / catalog builders. The
    // router is built once at boot from the merged ModelConfig
    // and not rebuilt per-frame, so any value we don't fold in
    // here will never take effect for the duration of the
    // process. See [`admin_runtime::resolve_persisted_inference_model`].
    cfg.inference.model =
        admin_runtime::resolve_persisted_inference_model(&store, &cfg.inference.model).await;

    // First-boot admin: deferred to the UI's first-run-setup
    // form. When `auth.mode` permits local users AND the
    // `users` table is empty, we log a pointer to the UI and
    // proceed; the operator picks the initial admin password
    // via `POST /api/v1/auth/first-run-setup`
    // (see `crate::auth::login::post_first_run_setup`). Any
    // stale OTP sentinel from an older install (back when the
    // engine generated and printed a one-time password) is
    // cleaned up so the installer banner doesn't accidentally
    // surface a now-dead value.
    //
    // Mode-gated: a pure-OIDC deployment has no concept of
    // first-run-setup — admins materialise on first IdP login.
    if cfg.auth.mode.allows_local() {
        match store.count_users().await {
            Ok(0) => {
                tracing::info!(
                    "auth: no admin users yet — open the UI to complete first-run setup \
                     (POST /api/v1/auth/first-run-setup)",
                );
                auth::bootstrap::clear_bootstrap_sentinel(&cfg.runtime.state_dir);
            }
            Ok(n) => {
                tracing::debug!(
                    user_count = n,
                    "auth: admin users present; skipping first-run setup hint",
                );
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "could not count users for first-run check: {e}"
                ));
            }
        }
    } else {
        tracing::debug!(
            mode = ?cfg.auth.mode,
            "auth: mode does not allow local users; skipping first-run check",
        );
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
            // Resolve the optional client_secret ONCE at boot
            // from either a file or an env var (mutually
            // exclusive). Failure here is fatal — the operator
            // asked us to use a confidential-client secret and
            // we can't, so refusing to start is safer than
            // silently downgrading to PKCE-only.
            let client_secret = match (
                oidc_cfg.client_secret_file.as_ref(),
                oidc_cfg.client_secret_env.as_ref(),
            ) {
                (Some(_), Some(_)) => {
                    return Err(anyhow::anyhow!(
                        "auth.oidc.client_secret_file and \
                         auth.oidc.client_secret_env are mutually \
                         exclusive; pick one"
                    ));
                }
                (Some(path), None) => {
                    let raw = std::fs::read_to_string(path).with_context(|| {
                        format!("reading auth.oidc.client_secret_file at {}", path.display())
                    })?;
                    let trimmed = raw.trim().to_string();
                    if trimmed.is_empty() {
                        return Err(anyhow::anyhow!(
                            "auth.oidc.client_secret_file at {} is empty",
                            path.display()
                        ));
                    }
                    tracing::info!(
                        path = %path.display(),
                        "OIDC client_secret loaded from file (length redacted)",
                    );
                    Some(std::sync::Arc::new(trimmed))
                }
                (None, Some(var)) => {
                    let raw = std::env::var(var).map_err(|_| {
                        anyhow::anyhow!(
                            "auth.oidc.client_secret_env points at ${var} but that env var is unset"
                        )
                    })?;
                    let trimmed = raw.trim().to_string();
                    if trimmed.is_empty() {
                        return Err(anyhow::anyhow!(
                            "auth.oidc.client_secret_env (${var}) is empty"
                        ));
                    }
                    tracing::info!(
                        env_var = %var,
                        "OIDC client_secret loaded from env var (length redacted)",
                    );
                    Some(std::sync::Arc::new(trimmed))
                }
                (None, None) => None,
            };
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
                                .context("building admin-auth state for oidc login")?
                                .with_idle_bump_tx(idle_bump_tx.clone()),
                        ),
                        oidc_client: client,
                        cfg: oidc_cfg.clone(),
                        sessions: auth::oidc_login::OidcLoginSessions::new(),
                        client_secret,
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
    //
    // M3.1: pass a Store-backed VisualPromptStore so the `yoloe_visual`
    // arm (text-prompt-free open-vocab via image embeddings) can hydrate
    // per-camera bindings on every `push_camera_config` without a
    // restart. The optional embedding-dim override is None today —
    // detector defaults to 512 (yoloe26_s_image_encoder).
    let visual_prompt_store: Arc<dyn nexus_inference::VisualPromptStore> = Arc::new(
        nexus_inference::StoreBackedVisualPromptStore::new(store.clone()),
    );
    let router = Arc::new(
        InferenceRouter::build_with_visual_store(
            &cfg.inference,
            &cameras,
            Some(visual_prompt_store),
            None,
        )
        .context("building inference router")?,
    );
    let pool = router.default_pool();
    log_inference_summary(&cfg.inference, pool.is_some(), &router);

    // Trackers are stateful (track ids, IoU history) and MUST be
    // instantiated per-camera — a shared `Arc<dyn Tracker>` would
    // merge every camera's detections into one global track table,
    // surfacing camera A's bboxes (and track ids) in camera B's
    // L7-cache entry / `/api/v1/cameras/:id/frames/latest.json`. The
    // reconciler now owns construction; the boot loop below builds
    // its own per spawn from the same `cfg.tracker` snapshot.
    let cache = Arc::new(LatestFrameCache::new());
    // M-Admin Phase 0 closeout: per-camera frame-stats registry.
    // Shared between every supervisor task (writer) and the API
    // layer (reader: `GET /v1/cameras/:id/stats` and the merged
    // health column on `GET /api/v1/cameras`).
    let frame_stats = Arc::new(FrameStatsRegistry::new());

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
        cfg.inference.model.input_width,
        cfg.runtime.clips.pre_roll_secs,
        bus.clone(),
        usb_resolver.clone(),
        preferred_usb_label.clone(),
    )
    .await?;
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
    // Shared bus that lets `DELETE /api/v1/cameras/{id}/static-anchors`
    // signal the per-camera supervisor to wipe its in-memory
    // anchor state + on-disk registry. See `static_clear.rs` in
    // nexus-pipeline for the polled-counter rationale.
    let static_clear = StaticAnchorClearRegistry::new();

    // Phase 2 · Step 2.8 + Phase 5.6 · slice 4c-ii — shared tunnel
    // outbox slot. Hoisted ABOVE the per-camera supervisor spawn
    // loop so the cloud entity-sighting hook (which closes over an
    // `Arc<TunnelOutbox>`) can be constructed before the first
    // `spawn_camera` call. The cloud-tunnel reconnect loop installs
    // the active handle into this slot post-enrollment; the cold
    // replicator + the sighting hook both read it.
    let cloud_outbox = std::sync::Arc::new(nexus_cloud_client::TunnelOutbox::new());

    // Phase 5.6 · slice 4c-ii — build the per-engine entity-sighting
    // hook + scheduler config. Always built, even when the cloud is
    // unreachable: the hook drops snapshots silently when the outbox
    // has no handle, and the supervisor's per-frame tick is cheap
    // when the underlying [`NoopSightingHook`] is installed.
    //
    // Phase 5.6 · R4 — also build the `entity_local_state`
    // persistence sink + load a hydration seed so the scheduler can
    // reuse a prior `entity_local_id` when the tracker re-issues the
    // same `(camera_id, track_id)` within the GC window after a
    // crash + systemd restart. Persistence is wired regardless of
    // `cfg.reid.enabled` — the table is cheap and the operator may
    // flip the flag at runtime without restarting the engine.
    let sighting_persist: Arc<dyn nexus_pipeline::EntityLocalPersist> = Arc::new(
        entity_local_persist::StoreEntityLocalPersist::spawn(store.clone(), 256),
    );
    // Hydration window: 2 × emit_interval, floored at 30 s and
    // capped at 5 min so a long-lived stale row never re-attaches
    // to a fresh track. The scheduler does its own per-camera GC on
    // top of this, so this is just an upper bound on what we ask
    // the DB to return at boot.
    let hydration_window_secs = (cfg.reid.emit_interval_s.saturating_mul(2)).clamp(30, 300);
    let sighting_seed_all = match store
        .load_recent_entity_locals(
            chrono::Utc::now() - chrono::Duration::seconds(hydration_window_secs as i64),
        )
        .await
    {
        Ok(rows) => {
            info!(
                count = rows.len(),
                window_secs = hydration_window_secs,
                "entity_local_state hydrated from store"
            );
            rows
        }
        Err(e) => {
            warn!(
                error = %e,
                "entity_local_state hydration failed; per-camera schedulers will start with empty seed"
            );
            Vec::new()
        }
    };
    // Phase 5.6 · R7 — per-camera re-ID stats registry shared
    // with the worker (writer) and the `/v1/admin/reid/status`
    // admin endpoint (reader). Always built, even when reid is
    // disabled, so the API surface is shape-stable; an empty
    // snapshot is what the UI uses to render "feature is off".
    let reid_stats = Arc::new(cloud_sighting::ReidStatsRegistry::new());

    let (sighting_hook, sighting_cfg): (
        Arc<dyn nexus_pipeline::SightingHook>,
        nexus_pipeline::supervisor::SightingSchedulerConfig,
    ) = if cfg.reid.enabled {
        let extractor = build_reid_extractor(&cfg.reid);
        let hook = cloud_sighting::CloudEntitySightingHook::spawn(
            extractor,
            cloud_outbox.clone(),
            64,
            reid_stats.clone(),
            cfg.reid.min_crop_w_px,
            cfg.reid.min_crop_h_px,
        );
        let scheduler_cfg = nexus_pipeline::supervisor::SightingSchedulerConfig {
            min_track_age_frames: cfg.reid.min_track_age_frames,
            emit_interval: std::time::Duration::from_secs(cfg.reid.emit_interval_s),
            crowded_track_threshold: cfg.reid.crowded_track_threshold,
            crowded_emit_interval: std::time::Duration::from_secs(cfg.reid.crowded_emit_interval_s),
        };
        info!(
            model_id = %cfg.reid.model_id,
            dim = cfg.reid.dim,
            emit_interval_s = cfg.reid.emit_interval_s,
            crowded_track_threshold = cfg.reid.crowded_track_threshold,
            crowded_emit_interval_s = cfg.reid.crowded_emit_interval_s,
            min_track_age_frames = cfg.reid.min_track_age_frames,
            min_crop_w_px = cfg.reid.min_crop_w_px,
            min_crop_h_px = cfg.reid.min_crop_h_px,
            model_path = ?cfg.reid.model_path,
            "reid enabled — entity-sighting hook installed"
        );
        (Arc::new(hook), scheduler_cfg)
    } else {
        info!("reid disabled — noop sighting hook installed");
        (
            Arc::new(nexus_pipeline::NoopSightingHook),
            nexus_pipeline::supervisor::SightingSchedulerConfig::default(),
        )
    };

    for cam in cameras {
        if !cam.ingest.enabled {
            warn!(camera_id = cam.id, "camera disabled — skipping");
            continue;
        }
        let cam_id = cam.id;
        let cam_url = cam.ingest.url.to_string();
        let configured_codec = cam.ingest.codec;
        let detector = router.detector_for_camera(&cam);
        // Fresh per-camera tracker — see the comment on `cfg.tracker`
        // above for why sharing one Arc across cameras is wrong.
        let tracker: Arc<dyn nexus_tracker::Tracker> = Arc::from(build_tracker(&cfg.tracker));
        // Per-camera supervisor (analysis) RGB frame size: matches
        // the camera's resolved detector input width so we don't
        // burn CPU upscaling a 1280-trained net's input to a fixed
        // 960 only to downscale back. See
        // `nexus_pipeline::supervisor_frame_for`.
        let det_w = cam
            .detector
            .model_override
            .as_ref()
            .map(|m| m.input_width)
            .unwrap_or(cfg.inference.model.input_width);
        let (sup_w, sup_h) = nexus_pipeline::supervisor_frame_for(det_w);
        let seed_for_cam: Vec<nexus_pipeline::EntityLocalSeed> = sighting_seed_all
            .iter()
            .filter(|r| r.camera_id == cam_id)
            .map(|r| nexus_pipeline::EntityLocalSeed {
                camera_id: r.camera_id,
                track_id: r.track_id,
                entity_local_id: r.entity_local_id.clone(),
                started_ts: r.started_ts,
                last_seen_at: r.last_seen_at,
            })
            .collect();
        let h = spawn_camera(
            cam,
            detector,
            tracker,
            cfg.tracker.annotator.clone(),
            cfg.tracker.static_object.clone(),
            cfg.runtime.clips.clone(),
            cfg.runtime.state_dir.clone(),
            evaluator.clone(),
            store.clone(),
            recorder.clone(),
            bus.clone(),
            cache.clone(),
            frame_stats.clone(),
            static_clear.clone(),
            sup_w,
            sup_h,
            sighting_hook.clone(),
            sighting_cfg,
            seed_for_cam,
            sighting_persist.clone(),
        );
        running.lock().insert(
            cam_id,
            reconciler::RunningCameraEntry {
                task: Arc::new(h.task),
                url: cam_url,
                supervisor_dims: (sup_w, sup_h),
                codec: configured_codec,
            },
        );
    }

    // Storage safety floor (M2.1 Stage A PR 4). Watermark sampler
    // shares the same recorder Arc as the per-camera supervisors
    // above so panic-mode flips propagate atomically.
    //
    // M-Admin Phase 0 — operator-persisted overrides win over
    // nexus.toml. Mirrors the api_bind / auth_config_json pattern.
    let (effective_low_pct, effective_panic_pct) = admin_runtime::resolve_persisted_watermarks(
        &store,
        cfg.runtime.clips.low_watermark_pct,
        cfg.runtime.clips.panic_watermark_pct,
    )
    .await;
    let safety_cfg = storage_safety::StorageSafetyConfig {
        clips_dir: clips_dir.clone(),
        low_watermark_pct: effective_low_pct,
        panic_watermark_pct: effective_panic_pct,
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
    // Phase 2 Step 2.1b — single `Arc<Notify>` shared between the
    // cold replicator (consumer) and the cloud-tunnel supervisor
    // (producer, post-enrollment). The cold replicator fires once
    // at boot from the internal initial-pulse path AND once per
    // external `notify_one()` thereafter, so the cloud-tunnel
    // supervisor calling `notify_one()` right after installing the
    // Azure backend drains any pre-enrollment clip backlog
    // immediately instead of waiting up to 5 min for the polling
    // backstop.
    let cold_kick = std::sync::Arc::new(tokio::sync::Notify::new());
    // Phase 2 · Step 2.8 — shared tunnel-handle slot. The cloud
    // tunnel reconnect loop publishes the active `Arc<Connection>`
    // into this on connect / clears on disconnect; the cold
    // replicator publishes `clip_replicated` envelopes through it
    // as a best-effort, fire-and-forget side effect after each
    // successful upload + cold-pointer commit.
    //
    // (`cloud_outbox` itself was constructed BEFORE the supervisor
    // spawn loop in Phase 5.6 · slice 4c-ii so the per-camera
    // entity-sighting hook could share it; the cold replicator just
    // clones the existing Arc here.)
    let cold_handle = {
        let store = store.clone();
        let bus = bus.clone();
        let registry = registry.clone();
        let cfg = cold_replicator::ColdReplicatorConfig {
            clips_dir: clips_dir.clone(),
            kick: Some(cold_kick.clone()),
            outbox: Some(cloud_outbox.clone()),
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
    // this, cameras added via the discovery UI (or `PUT /api/v1/cameras/{id}`)
    // persist to disk but never get a pipeline until the next
    // engine restart.
    let reconciler_handle = reconciler::spawn(reconciler::ReconcilerArgs {
        router: router.clone(),
        tracker_cfg: cfg.tracker.clone(),
        annotator: cfg.tracker.annotator.clone(),
        static_object: cfg.tracker.static_object.clone(),
        clips: cfg.runtime.clips.clone(),
        state_dir: cfg.runtime.state_dir.clone(),
        evaluator: evaluator.clone(),
        store: store.clone(),
        recorder: recorder.clone(),
        bus: bus.clone(),
        cache: cache.clone(),
        frame_stats: frame_stats.clone(),
        static_clear: static_clear.clone(),
        pre_roll_secs: cfg.runtime.clips.pre_roll_secs,
        default_detector_width: cfg.inference.model.input_width,
        sighting_hook: sighting_hook.clone(),
        sighting_cfg,
        sighting_persist: sighting_persist.clone(),
        sighting_hydration_window_secs: hydration_window_secs,
        handles: running.clone(),
    });

    // Phase 5.6 · R4 — periodic `entity_local_state` sweeper. Keeps
    // the table from growing unbounded when cameras come and go.
    // Floor of 2 hours is a deliberate over-shoot of the per-camera
    // GC window (10s to a few minutes); any row past 2 h cannot
    // possibly still be in the tracker's re-association window.
    {
        let store = store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
            // First tick fires immediately — skip it.
            tick.tick().await;
            loop {
                tick.tick().await;
                let cutoff = chrono::Utc::now() - chrono::Duration::hours(2);
                match store.prune_entity_local_state(cutoff).await {
                    Ok(0) => {}
                    Ok(n) => {
                        debug!(pruned = n, "entity_local_state sweeper pruned stale rows");
                    }
                    Err(e) => {
                        warn!(error = %e, "entity_local_state sweeper failed");
                    }
                }
            }
        });
    }

    // Phase A — camera-roster publisher. Subscribes to
    // `topic::CONFIG_CHANGED` and pushes a `camera_roster` envelope
    // to the cloud edge-gateway whenever cameras change (plus an
    // initial push on boot, plus a 10s retry tick while the tunnel
    // is down). The cloud upserts the per-core camera list so the
    // site dashboard can show cameras the operator configured
    // locally — even ones that have never produced an alert.
    let roster_handle = roster::spawn(store.clone(), bus.clone(), cloud_outbox.clone());

    // Phase 1.8 — cloud tunnel supervisor. If the local store has a
    // `cloud_enrollment` row (populated by `nexus-engine enroll`),
    // this spawns a long-running task that maintains the WSS+mTLS
    // tunnel to `edge-gateway`, sending heartbeats every 30s. When
    // no enrollment is present, the task parks on
    // `cloud_enrollment_changed` (Phase 1.16) — the engine continues
    // to serve locally (fail-open per Hard Rule 5) and the admin
    // `POST /v1/admin/cloud/enroll` handler fires the Notify after
    // persisting the row so the tunnel activates within seconds
    // without an engine restart.
    //
    // Phase 2 Step 2.1b — also receives the shared `registry` +
    // `cold_kick` so post-enrollment it can install the cloud
    // `AzureBlobBackend` under the reserved handle `"cloud"`, bind
    // `storage_cold_replica` to it if still NULL, and kick the
    // replicator immediately.
    let cloud_enrollment_changed = std::sync::Arc::new(tokio::sync::Notify::new());
    // Phase A Step 5 — the cloud-tunnel admin passthrough
    // (`engine_rpc::EngineRpcHandler::handle_admin_passthrough`)
    // needs to know which port the engine's own admin API ended up
    // listening on so it can `POST http://127.0.0.1:<port>/api/v1
    // /admin/...` for every forwarded cloud→edge envelope. We
    // bootstrap from `cfg.server.api_bind` here so the supervisor
    // has a non-`None` value to capture into the handler, then
    // overwrite below (alongside the `current_bind` thread) after
    // the runtime-setting override resolves.
    let loopback_admin_base = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
        crate::engine_rpc::loopback_admin_url_from_bind(&cfg.server.api_bind),
    ));
    // Snapshot the admin secret once (from the same `cfg.auth`
    // both `admin_auth_layer` and the OIDC login state consume)
    // so the cloud-tunnel passthrough can mint short-lived HS256
    // bearers when forwarding cloud→edge admin envelopes onto
    // the engine's own loopback admin API. Without this, the
    // M6 `auth.mode = "local"` default — which auto-provisions
    // an admin secret to `<state_dir>/admin-secret` — makes
    // strict-mode bearer-or-bust fire on every loopback call,
    // returning 401 for every cloud-initiated admin operation
    // (camera discovery, create, edit, etc.). See
    // `admin_auth::mint_internal_passthrough_bearer`.
    let cloud_passthrough_admin_secret: Option<std::sync::Arc<String>> = {
        let bootstrap_state = admin_auth::AdminAuthState::from_config(&cfg.auth)
            .context("building admin-auth state for cloud passthrough")?;
        bootstrap_state
            .admin_secret()
            .map(|s| std::sync::Arc::new(s.to_string()))
    };
    let (cloud_tunnel_shutdown_tx, cloud_tunnel_handle) = cloud_tunnel::spawn_tunnel(
        store.clone(),
        registry.clone(),
        cold_kick.clone(),
        cloud_enrollment_changed.clone(),
        cloud_outbox.clone(),
        Some(trace_rx),
        loopback_admin_base.clone(),
        cloud_passthrough_admin_secret,
    );

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
        // Provisional — overwritten below when `--no-api` is off
        // with the actual bind we end up listening on (which may
        // be the operator-persisted override). Kept on ApiState
        // so `GET /v1/admin/server/bind` can report the active
        // value without re-reading config.
        current_bind: cfg.server.api_bind.clone(),
        // Provisional too — overwritten below alongside the
        // primary bind after the boot-time precedence sweep for
        // `engine_runtime_settings.ui_bind`. `None` here keeps
        // the GET surface honest even if the second listener
        // boot block panics before resolving the override.
        current_ui_bind: None,
        evaluator: evaluator.clone(),
        cache: cache.clone(),
        frame_stats: frame_stats.clone(),
        pool: pool.clone(),
        ui_root: cfg.server.ui_root.clone(),
        recorder: recorder.clone(),
        clips_dir: clips_dir.clone(),
        low_watermark_pct: effective_low_pct,
        panic_watermark_pct: effective_panic_pct,
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
                .context("building admin-auth state")?
                .with_idle_bump_tx(idle_bump_tx.clone()),
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
        oidc_display_name: oidc_login_state.as_ref().map(|_| {
            // The display-name is sourced from the OIDC
            // config but only surfaced when the routes are
            // actually mounted — otherwise the UI would
            // render a button that 404s on click.
            cfg.auth
                .oidc
                .as_ref()
                .and_then(|o| o.display_name.clone())
                .unwrap_or_else(|| "single sign-on".to_string())
        }),
        oidc_login: oidc_login_state,
        // M3.1 Phase H — visual-prompts admin runtime state.
        // Resolves the image-encoder ONNX path against
        // `inference.model.pack_path` (so the same pack directory
        // that ships the detector also ships the encoder); the
        // upload handler 503s when the operator hasn't set a pack
        // path yet. Encoder session is lazy-init on first POST.
        visual_prompts: visual_prompts_admin::VisualPromptsAdminState::from_config(
            &cfg.runtime,
            &cfg.inference,
        ),
        // M-Admin Phase 0 follow-up — snapshot the effective
        // inference model (after the persisted override has
        // already been merged onto `nexus.toml` further up in
        // this function) so `GET /v1/admin/server/inference`
        // can diff the active value against any newer pending
        // override.
        current_inference_model: std::sync::Arc::new(cfg.inference.model.clone()),
        // Shared with every supervisor + the static-anchors
        // viewer overlay endpoint. Snapshot, not live-reloaded —
        // `runtime.state_dir` is a restart-required setting.
        state_dir: cfg.runtime.state_dir.clone(),
        // Operator-initiated static-anchor wipe signal. Shared with
        // every supervisor; the `DELETE /api/v1/cameras/{id}/static-anchors`
        // handler bumps the per-camera counter and the supervisor
        // notices on its next frame.
        static_clear: static_clear.clone(),
        // Engine-wide fallback for `behavior.anchor_ttl_secs`.
        // Snapshot at boot; surfaced verbatim by
        // `GET /api/v1/system/static-object-defaults` so the camera
        // settings form can hint the inherited value.
        default_anchor_ttl_secs: cfg.tracker.static_object.anchor_ttl_secs,
        // M-Admin Network — single-slot registry holding any
        // in-flight `netplan try` session + its rollback timer.
        // Cheap to clone; the API state and any future
        // background revert task share the same Arc.
        network_apply: network::apply::ApplyRegistry::new(),
        // M-Cloud Phase 1.16 — handed to the tunnel supervisor
        // above so a post-boot admin enrollment hot-activates
        // the WSS tunnel without an engine restart.
        cloud_enrollment_changed: cloud_enrollment_changed.clone(),
        // Shared with the tunnel supervisor so the unauthenticated
        // `GET /api/v1/cloud/status` handler can report whether the
        // WSS session is currently up. The supervisor calls
        // `set_handle(Some(...))` on connect and `set_handle(None)`
        // on every disconnect path, so `is_connected()` tracks the
        // live state without any extra plumbing.
        cloud_outbox: cloud_outbox.clone(),
        // Phase 5.6 · R7 — boot-time [reid] config snapshot +
        // shared per-camera stats registry the worker bumps on
        // every successful extract. Drive `/v1/admin/reid/status`.
        reid_config: Arc::new(cfg.reid.clone()),
        reid_stats: reid_stats.clone(),
    };

    // M-Admin Phase 1B — start the registry eviction sweep. Holds
    // its own Arc on the DashMap; no shutdown signal needed because
    // the task does pure read-then-remove work on a 60 s tick.
    discovery::spawn_eviction_sweep(api_state.discovery_sessions.clone());

    if !cli.no_api {
        // M-Admin Phase 0 — operator-persisted bind override
        // wins over `nexus.toml`. Stored as a plain `host:port`
        // string in `engine_runtime_settings.api_bind`. Same
        // failure shape as the auth override above: loud +
        // non-fatal so a typo in the persisted value falls back
        // to the on-disk config rather than bricking the listener.
        let bind = match store.read_runtime_setting("api_bind").await {
            Ok(Some(Some(persisted))) => {
                tracing::warn!(
                    persisted = %persisted,
                    toml = %cfg.server.api_bind,
                    "applying operator-persisted server.bind from engine_runtime_settings",
                );
                persisted
            }
            Ok(Some(None)) => {
                tracing::debug!("api_bind present but NULL; using nexus.toml");
                cfg.server.api_bind.clone()
            }
            Ok(None) => cfg.server.api_bind.clone(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read api_bind from engine_runtime_settings; using nexus.toml");
                cfg.server.api_bind.clone()
            }
        };
        let mut api_state = api_state;
        api_state.current_bind = bind.clone();
        // Sync the cloud-tunnel admin passthrough's loopback URL
        // with the effective bind so a runtime-setting override
        // (e.g. operator persisted `api_bind` to a non-default
        // port via `PUT /v1/admin/server/bind`) propagates to the
        // cloud→edge proxy without an engine restart.
        loopback_admin_base.store(std::sync::Arc::new(
            crate::engine_rpc::loopback_admin_url_from_bind(&bind),
        ));
        // Resolve the optional UI alias listener with three-state
        // override semantics that diverge from `api_bind` above —
        // `Ok(Some(None))` (row exists, value is SQL NULL) means
        // the operator explicitly TURNED OFF the second listener
        // and we override the TOML default to skip the bind, even
        // if TOML defines `server.ui_bind`. `Ok(None)` (no row)
        // means no override; fall back to TOML. `Ok(Some(Some(s)))`
        // means use the persisted `host:port`. Same loud-non-fatal
        // failure shape as `api_bind`: on read error fall back to
        // TOML so a partial DB doesn't brick the listener.
        let effective_ui_bind: Option<String> = match store.read_runtime_setting("ui_bind").await {
            Ok(Some(Some(persisted))) => {
                tracing::warn!(
                    persisted = %persisted,
                    toml = ?cfg.server.ui_bind,
                    "applying operator-persisted server.ui_bind from engine_runtime_settings",
                );
                Some(persisted)
            }
            Ok(Some(None)) => {
                tracing::warn!(
                    toml = ?cfg.server.ui_bind,
                    "operator-persisted ui_bind = OFF; second listener disabled",
                );
                None
            }
            Ok(None) => cfg.server.ui_bind.clone(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read ui_bind from engine_runtime_settings; using nexus.toml");
                cfg.server.ui_bind.clone()
            }
        };
        api_state.current_ui_bind = effective_ui_bind.clone();
        let app = api::router(api_state);

        // ----------------------------------------------------------
        // M-HTTPS Phase 1 — optional in-process TLS listener.
        //
        // When `[server].https_bind` + `tls_cert_path` + `tls_key_path`
        // are all set, we spin a third listener that terminates TLS
        // using rustls (ring provider) and serves the SAME router
        // as `api_bind` / `ui_bind`. The `ui_bind` listener becomes
        // a 308-redirect shim when `redirect_http_to_https = true`
        // (the default), so an operator's bookmarked
        // `http://nexus/dashboard` arrives at
        // `https://nexus/dashboard` without manual intervention.
        //
        // Cert load failure does NOT block boot — the engine falls
        // back to plain HTTP on `api_bind` (+ `ui_bind` without
        // redirect) and logs loudly, so operators who haven't run
        // `nexus-engine tls init` yet still get a working appliance.
        // ----------------------------------------------------------
        struct TlsListener {
            bind: String,
            addr: std::net::SocketAddr,
            rustls: axum_server::tls_rustls::RustlsConfig,
            cert_path: std::path::PathBuf,
            key_path: std::path::PathBuf,
            /// v0.1.36 \u2014 origin of the leaf. Only `CloudIssued`
            /// turns on HSTS; self-signed never advertises HSTS
            /// because a browser caching HSTS against an
            /// untrusted leaf is trapped for `max-age` seconds.
            cert_source: tls::CertSource,
            /// v0.1.36 \u2014 `true` when the leaf is loaded directly
            /// from in-memory PEM bytes (cloud-issued path). The
            /// cert watcher is skipped in that mode; rotation
            /// happens on engine restart after re-enrollment.
            in_memory: bool,
        }
        // v0.1.36 \u2014 prefer a cloud-issued leaf if the appliance
        // has been enrolled AND enrollment-svc minted a server cert.
        // The DB row carries both the cert PEM and the matching
        // private key (CSR was generated edge-side, so the key never
        // left this process).
        let cloud_tls_pems: Option<(Vec<u8>, Vec<u8>)> = match store.get_cloud_enrollment().await {
            Ok(Some(enr)) => match (enr.server_cert_pem, enr.server_private_key_pem) {
                (Some(c), Some(k)) => Some((c.into_bytes(), k.into_bytes())),
                _ => None,
            },
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, "could not read cloud_enrollment for TLS bootstrap");
                None
            }
        };

        let tls_listener: Option<TlsListener> = match (
            cfg.server.https_bind.as_deref(),
            cfg.server.tls_cert_path.as_deref(),
            cfg.server.tls_key_path.as_deref(),
        ) {
            (Some(b), Some(cert), Some(key)) => match b.parse::<std::net::SocketAddr>() {
                Ok(addr) => {
                    // Path A \u2014 cloud-issued in-memory leaf wins.
                    if let Some((cert_pem, key_pem)) = cloud_tls_pems.as_ref() {
                        match tls::load_rustls_config_from_pems(cert_pem, key_pem).await {
                            Ok(rc) => {
                                tracing::info!(
                                    bind = %b,
                                    "TLS leaf loaded from cloud_enrollment.server_cert_pem (HSTS will be advertised)",
                                );
                                Some(TlsListener {
                                    bind: b.to_string(),
                                    addr,
                                    rustls: rc,
                                    cert_path: cert.to_path_buf(),
                                    key_path: key.to_path_buf(),
                                    cert_source: tls::CertSource::CloudIssued,
                                    in_memory: true,
                                })
                            }
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    "cloud-issued TLS PEM failed to load; falling back to on-disk self-signed leaf",
                                );
                                match tls::load_rustls_config(cert, key).await {
                                    Ok(rc) => Some(TlsListener {
                                        bind: b.to_string(),
                                        addr,
                                        rustls: rc,
                                        cert_path: cert.to_path_buf(),
                                        key_path: key.to_path_buf(),
                                        cert_source: tls::CertSource::SelfSigned,
                                        in_memory: false,
                                    }),
                                    Err(e2) => {
                                        tracing::error!(
                                            bind = %b,
                                            cert = %cert.display(),
                                            key = %key.display(),
                                            error = %e2,
                                            "self-signed fallback also failed; HTTPS listener disabled",
                                        );
                                        None
                                    }
                                }
                            }
                        }
                    } else {
                        // Path B \u2014 no cloud leaf; fall back to the
                        // on-disk self-signed bootstrap cert.
                        match tls::load_rustls_config(cert, key).await {
                            Ok(rc) => Some(TlsListener {
                                bind: b.to_string(),
                                addr,
                                rustls: rc,
                                cert_path: cert.to_path_buf(),
                                key_path: key.to_path_buf(),
                                cert_source: tls::CertSource::SelfSigned,
                                in_memory: false,
                            }),
                            Err(e) => {
                                tracing::error!(
                                    bind = %b,
                                    cert = %cert.display(),
                                    key = %key.display(),
                                    error = %e,
                                    "TLS cert load failed; HTTPS listener disabled, plain HTTP only",
                                );
                                None
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(
                        bind = %b,
                        error = %e,
                        "https_bind is not a valid host:port; HTTPS listener disabled",
                    );
                    None
                }
            },
            (Some(b), _, _) => {
                tracing::warn!(
                    bind = %b,
                    "https_bind set but tls_cert_path / tls_key_path missing; HTTPS listener disabled (run `nexus-engine tls init`)",
                );
                None
            }
            _ => None,
        };
        if tls_listener.is_some() {
            tls::install_default_crypto_provider();
        }

        let listener = tokio::net::TcpListener::bind(&bind).await?;
        info!(bind = %bind, "HTTP API + UI listening");
        // `into_make_service_with_connect_info::<SocketAddr>()`
        // is required so the admin-auth middleware can pull the
        // peer address out of request extensions for the
        // loopback-fallback check (see admin_auth module).
        let server = tokio::spawn({
            let app = app.clone();
            async move {
                if let Err(e) = axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .await
                {
                    tracing::error!("axum error (api): {e}");
                }
            }
        });

        // HTTPS listener (TLS termination in-process).
        let cert_watcher_token = tokio_util::sync::CancellationToken::new();
        let (https_server, cert_watcher) = if let Some(t) = tls_listener.as_ref() {
            let mut tls_app = app.clone();
            // v0.1.36 \u2014 HSTS gate. The middleware only emits a
            // `Strict-Transport-Security` header when the leaf is
            // cloud-issued. Self-signed leaves never advertise HSTS,
            // because a browser that caches HSTS against an
            // untrusted leaf is trapped for `max-age` seconds
            // \u2014 even an operator who later installs a trusted
            // cert can't downgrade their own browser's HSTS cache.
            if let (Some(max_age), tls::CertSource::CloudIssued) =
                (cfg.server.hsts_max_age_seconds, t.cert_source)
            {
                let hsts_value = format!("max-age={max_age}; includeSubDomains");
                tls_app = tls_app.layer(axum::middleware::from_fn(
                    move |req: axum::extract::Request, next: axum::middleware::Next| {
                        let v = hsts_value.clone();
                        async move {
                            let mut resp = next.run(req).await;
                            if let Ok(h) = axum::http::HeaderValue::from_str(&v) {
                                resp.headers_mut().insert("strict-transport-security", h);
                            }
                            resp
                        }
                    },
                ));
            } else if cfg.server.hsts_max_age_seconds.is_some()
                && t.cert_source == tls::CertSource::SelfSigned
            {
                tracing::info!(
                    "hsts_max_age_seconds configured but leaf is self-signed; HSTS NOT emitted (would trap browsers)",
                );
            }
            info!(
                bind = %t.bind,
                cert_source = ?t.cert_source,
                "HTTPS API + UI listening (TLS)",
            );
            let addr = t.addr;
            let rustls = t.rustls.clone();
            let handle = tokio::spawn(async move {
                if let Err(e) = axum_server::bind_rustls(addr, rustls)
                    .serve(tls_app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                    .await
                {
                    tracing::error!("axum-server error (https): {e}");
                }
            });
            let watcher = if t.in_memory {
                // Cloud-issued leaf is loaded from `cloud_enrollment`
                // in memory; rotation requires a re-enrollment (which
                // restarts the engine). Skip the disk-mtime watcher
                // because the on-disk file is the stale self-signed
                // fallback, not the live cert.
                None
            } else {
                Some(tls::spawn_cert_watcher(
                    t.rustls.clone(),
                    t.cert_path.clone(),
                    t.key_path.clone(),
                    cert_watcher_token.clone(),
                ))
            };
            (Some(handle), watcher)
        } else {
            (None, None)
        };

        // Optional second listener so operators can reach the admin
        // console at e.g. `http://<host>/` (port 80) without typing
        // the engine port. When the HTTPS listener is up AND the
        // operator hasn't opted out via `redirect_http_to_https =
        // false`, this becomes a 308 redirect shim instead of a
        // second copy of the app. Binding <1024 on the bare-metal
        // systemd unit needs `CAP_NET_BIND_SERVICE`; Docker already
        // has it.
        let ui_server = if let Some(ui_bind) = effective_ui_bind {
            match tokio::net::TcpListener::bind(&ui_bind).await {
                Ok(ui_listener) => {
                    let redirect_mode = tls_listener.is_some() && cfg.server.redirect_http_to_https;
                    let app_for_ui = if redirect_mode {
                        let https_port =
                            tls_listener.as_ref().map(|t| t.addr.port()).unwrap_or(443);
                        info!(bind = %ui_bind, https_port = https_port, "HTTP UI listening as 308 → HTTPS redirector");
                        tls::redirect_router(https_port)
                    } else {
                        info!(bind = %ui_bind, "HTTP UI alias listening");
                        app.clone()
                    };
                    Some(tokio::spawn(async move {
                        if let Err(e) = axum::serve(
                            ui_listener,
                            app_for_ui
                                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
                        )
                        .await
                        {
                            tracing::error!("axum error (ui): {e}");
                        }
                    }))
                }
                Err(e) => {
                    tracing::error!(
                        ui_bind = %ui_bind,
                        error = %e,
                        "failed to bind server.ui_bind; continuing with api_bind only",
                    );
                    None
                }
            }
        } else {
            None
        };
        wait_for_signal().await;
        info!("shutdown signal received");
        server.abort();
        cert_watcher_token.cancel();
        if let Some(h) = https_server {
            h.abort();
        }
        if let Some(h) = cert_watcher {
            let _ = h.await;
        }
        if let Some(h) = ui_server {
            h.abort();
        }
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
    let _ = cloud_tunnel_shutdown_tx.send(());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), cloud_tunnel_handle).await;
    reconciler_handle.abort();
    roster_handle.abort();
    // Abort every per-camera supervisor. `drain()` empties the map
    // under one lock acquisition; the reconciler is already aborted
    // above so nothing will re-populate it.
    for (_, entry) in running.lock().drain() {
        entry.task.abort();
    }
    Ok(())
}

/// One-shot helper for `nexus-engine tls init`. Mints a self-signed
/// leaf at the configured `tls_cert_path` / `tls_key_path` if none
/// is present (or unconditionally when `force == true`). Exits with
/// a clear error when those paths aren't set in `nexus.toml`, so
/// the installer's invocation surfaces config-vs-code drift loudly.
fn run_tls_init(cfg: &Config, force: bool) -> Result<()> {
    let cert = cfg
        .server
        .tls_cert_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("[server].tls_cert_path is not set in nexus.toml"))?;
    let key = cfg
        .server
        .tls_key_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("[server].tls_key_path is not set in nexus.toml"))?;
    let regenerated = tls::init_self_signed_cert(cert, key, force)?;
    if regenerated {
        eprintln!(
            "nexus-engine tls init: wrote self-signed leaf to {} / {}",
            cert.display(),
            key.display()
        );
    } else {
        eprintln!(
            "nexus-engine tls init: existing cert preserved at {} (pass --force to regenerate)",
            cert.display()
        );
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

/// Build the per-engine appearance-embedding extractor used by the
/// Phase 5.6 entity-sighting hook.
///
/// Selection rules:
///
/// * When the `ort` feature AND a `model_path` are present, attempt
///   to open the real `DinoV2Extractor` (DINOv2-S 224, 384-dim,
///   Apache-2.0 weights). Failure falls back to a `MockExtractor`
///   parameterised with the configured `model_id` + `dim` so the
///   engine still boots — the cloud-side allowlist gate will then
///   reject the mock submissions (which is the correct behaviour
///   when the operator misconfigured the model path).
/// * When the `ort` feature is present but `model_path` is unset,
///   try the canonical release-staged path
///   (`<exe-dir>/../share/models/dinov2_s_224.onnx`, i.e.
///   `/opt/nexus/current/share/models/dinov2_s_224.onnx` on a
///   standard install). This matches the layout assembled by
///   `.github/workflows/release.yml` and lets a vanilla
///   `[reid] enabled = true` block in `nexus.toml` resolve a
///   real extractor without operator path-hunting.
/// * Otherwise (feature off OR no bundled weights found), build a
///   `MockExtractor` directly. The wire-submission path in
///   `cloud_sighting::run_worker` short-circuits when the
///   extractor's `model_id` starts with `"mock_"`, so no cloud
///   round-trip is wasted.
fn build_reid_extractor(cfg: &nexus_config::ReidConfig) -> Arc<dyn nexus_reid::Extractor> {
    #[cfg(feature = "ort")]
    {
        let explicit = cfg.model_path.clone();
        // Auto-discovery is intentionally narrow: only the canonical
        // tarball-staged path is probed. We do NOT walk PWD, $PATH,
        // /usr/local/share, etc. — operators with non-standard
        // layouts must set `model_path` explicitly.
        let candidate = explicit.or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
                .and_then(|bin_dir| bin_dir.parent().map(std::path::Path::to_path_buf))
                .map(|release_dir| release_dir.join("share/models/dinov2_s_224.onnx"))
                .filter(|p| p.exists())
        });
        if let Some(path) = candidate {
            match nexus_reid::ort_dinov2::DinoV2Extractor::open(
                &path,
                cfg.model_id.clone(),
                cfg.ep_priority.as_slice(),
            ) {
                Ok(x) => {
                    info!(
                        model_path = %path.display(),
                        model_id = %cfg.model_id,
                        dim = cfg.dim,
                        auto_discovered = cfg.model_path.is_none(),
                        "reid: DinoV2 extractor opened"
                    );
                    return Arc::new(x);
                }
                Err(e) => {
                    tracing::error!(
                        model_path = %path.display(),
                        error = %e,
                        "reid: DinoV2 extractor failed to open; falling back to mock (cloud will reject)"
                    );
                }
            }
        }
    }
    info!(
        model_id = %cfg.model_id,
        dim = cfg.dim,
        "reid: using MockExtractor (no model_path configured or `ort` feature off); cloud publish disabled"
    );
    Arc::new(nexus_reid::MockExtractor::with_config(
        format!("mock_{}", cfg.model_id),
        cfg.dim,
    ))
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
async fn build_recorder(
    kind: &RecorderKind,
    store: Arc<nexus_store::Store>,
    clips_dir: &std::path::Path,
    cameras: &[CameraConfig],
    default_detector_width: u32,
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
        RecorderKind::Gstreamer => {
            build_gst_recorder(
                store,
                clips_dir,
                cameras,
                default_detector_width,
                pre_roll_secs,
                bus,
                usb_resolver,
                preferred_usb_label,
            )
            .await
        }
    }
}

#[cfg(feature = "gstreamer")]
#[allow(clippy::too_many_arguments)]
async fn build_gst_recorder(
    store: Arc<nexus_store::Store>,
    clips_dir: &std::path::Path,
    cameras: &[CameraConfig],
    default_detector_width: u32,
    pre_roll_secs: u32,
    bus: Arc<dyn nexus_bus::Bus>,
    usb_resolver: Arc<dyn nexus_pipeline::recorder::UsbResolver>,
    preferred_usb_label: nexus_pipeline::recorder::PreferredUsbLabel,
) -> Result<Arc<dyn nexus_pipeline::ClipRecorder>> {
    // Build one always-on PreRollIngester per enabled camera. The
    // ingester holds the only RTSP connection for that camera; the
    // recorder consumes from its broadcast channel + ring snapshot,
    // and the detector consumes decoded RGB frames from the same
    // ingester via the shared frame source (see
    // `crates/nexus-pipeline/src/source.rs::SharedRtspSource`).
    // Collapsing to one session per camera is REQUIRED for cameras
    // whose firmware caps concurrent RTSP sessions at 1 per stream
    // path (e.g. InSight 192.168.1.66).
    let mut ingesters: std::collections::HashMap<i64, Arc<nexus_pipeline::PreRollIngester>> =
        std::collections::HashMap::new();
    for cam in cameras {
        if !cam.ingest.enabled {
            continue;
        }
        // Per-camera supervisor (RGB analysis) frame size, derived
        // from the camera's resolved detector input width via
        // `nexus_pipeline::supervisor_frame_for`. Matches what the
        // engine spawn site will pass to `spawn_camera`.
        let det_w = cam
            .detector
            .model_override
            .as_ref()
            .map(|m| m.input_width)
            .unwrap_or(default_detector_width);
        let (rgb_w, rgb_h) = nexus_pipeline::supervisor_frame_for(det_w);
        // Autodetect codec for cameras stored with `codec=None`
        // (operator picked "auto", or row predates the column).
        // Mirrors what `create_camera` does at create-time; needed
        // at boot for legacy rows + per-boot re-probe of "auto".
        let codec = match cam.ingest.codec {
            Some(c) => c,
            None => {
                let scheme = cam.ingest.url.scheme();
                let probed = if scheme == "rtsp" || scheme == "rtsps" {
                    crate::discovery::rtsp_probe::probe_codec_for_url(&cam.ingest.url).await
                } else {
                    None
                };
                match probed {
                    Some(c) => {
                        tracing::info!(
                            camera_id = cam.id,
                            url = %cam.ingest.url,
                            codec = %c,
                            "codec autodetected at boot"
                        );
                        c
                    }
                    None => {
                        tracing::warn!(
                            camera_id = cam.id,
                            url = %cam.ingest.url,
                            "camera codec unspecified and autodetect probe failed; defaulting to h264 — set `ingest.codec` in the camera config to silence"
                        );
                        nexus_types::CodecKind::H264
                    }
                }
            }
        };
        match nexus_pipeline::PreRollIngester::new_with_rgb(
            cam.id,
            cam.ingest.url.to_string(),
            pre_roll_secs,
            codec,
            cam.ingest.max_fps,
            rgb_w,
            rgb_h,
        ) {
            Ok(ing) => {
                tracing::info!(
                    camera_id = cam.id,
                    pre_roll_secs,
                    max_fps = cam.ingest.max_fps,
                    rgb_w,
                    rgb_h,
                    "pre-roll ingester started (with shared rgb tap)"
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
#[allow(clippy::too_many_arguments)]
async fn build_gst_recorder(
    store: Arc<nexus_store::Store>,
    clips_dir: &std::path::Path,
    _cameras: &[CameraConfig],
    _default_detector_width: u32,
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

/// v0.1.36 — background sliding-window idle-bump drain.
///
/// Receives `(chain_id, activity_ts)` from every authenticated
/// request and coalesces them into one `UPDATE auth_refresh_tokens
/// SET last_active_at = ?` per chain per 250 ms tick. Coalescing
/// keeps a 60 req/s SPA from issuing 60 UPDATEs per second; only
/// the most recent timestamp per chain is forwarded.
///
/// The task lives for the lifetime of the engine process. It exits
/// cleanly when the last sender is dropped (i.e. the engine has
/// already begun shutdown).
async fn idle_bump_drain(
    store: Arc<Store>,
    mut rx: tokio::sync::mpsc::Receiver<auth::require_role::IdleBump>,
) {
    use std::collections::HashMap;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
    loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                if pending.is_empty() { continue; }
                for (chain_id, ts) in pending.drain() {
                    if let Err(e) = store.bump_refresh_chain_active_at(&chain_id, ts).await {
                        tracing::debug!(
                            error = %e,
                            chain_id = %chain_id,
                            "idle-bump UPDATE failed (chain may have been revoked)",
                        );
                    }
                }
            }
            msg = rx.recv() => {
                match msg {
                    Some((chain_id, ts)) => {
                        // Keep only the latest ts per chain. The
                        // entry replaces an older ts if one is
                        // already queued for this tick.
                        let entry = pending.entry(chain_id).or_insert(ts);
                        if ts > *entry { *entry = ts; }
                    }
                    None => {
                        // Sender side dropped \u2014 flush whatever we
                        // have and exit.
                        for (chain_id, ts) in pending.drain() {
                            let _ = store.bump_refresh_chain_active_at(&chain_id, ts).await;
                        }
                        tracing::debug!("idle-bump drain shutting down");
                        return;
                    }
                }
            }
        }
    }
}

//! In-process HTTP API.
//!
//! Routes:
//!
//! * `GET  /api/health`
//! * `GET  /api/cameras`
//! * `PUT  /api/cameras/:id`
//! * `DELETE /api/cameras/:id`
//! * `GET  /api/cameras/:id/frames/latest`        — JPEG snapshot
//! * `GET  /api/cameras/:id/frames/latest.json`   — metadata for that snapshot
//! * `GET  /api/rules`
//! * `PUT  /api/rules/:id`
//! * `DELETE /api/rules/:id`
//! * `POST /api/rules/validate`                  — compile-only CEL check
//! * `POST /api/rules/preview`                   — replay against motion_events
//! * `GET  /api/events?limit=N`
//! * `GET  /api/stream/metadata`                  — SSE
//! * `GET  /api/stream/events`                    — SSE
//! * `GET  /api/backends`                         — DetectorPool slot status (OPS-1)
//!
//! Everything else is served from the UI directory via [`tower_http::services::ServeDir`].

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, put};
use axum::Json;
use axum::Router;
use futures::stream::StreamExt;
use image::ImageEncoder;
use nexus_bus::{topic, Bus, BusExt};
use nexus_config::{CameraConfig, RuleConfig};
use nexus_inference::{BackendStatus, DetectorPool};
use nexus_pipeline::{LatestFrameCache, StaticAnchorClearRegistry};
use nexus_rules::{CelEngine, RuleEngine, RuleEvaluator, RulesError};
use nexus_store::Store;
use nexus_types::{
    AlertEvent, CameraId, FrameMetadata, PixelFormat, RuleId, StaticAnchor, StaticAnchorsResponse,
};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use crate::admin_auth::{self, AdminAuthState};
use crate::cold_read_cache::CacheJobs;
use crate::discovery::{self, DiscoverySessions};

#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<Store>,
    pub bus: Arc<dyn Bus>,
    /// The `host:port` the HTTP server is *currently* bound to
    /// (after the boot-time precedence sweep — see
    /// `crate::admin_runtime::resolve_persisted_bind`). Used by
    /// `GET /v1/admin/server/bind` to surface the active value
    /// alongside any pending override that hasn't been picked
    /// up yet because the operator hasn't restarted.
    pub current_bind: String,
    /// What the optional second listener (`server.ui_bind` in
    /// `nexus.toml`, override key `ui_bind` in
    /// `engine_runtime_settings`) is currently bound to.
    /// `None` = no second listener was started at boot, either
    /// because TOML didn't define it or because the operator
    /// persisted an explicit "off" via the admin surface.
    /// Surfaced verbatim by `GET /v1/admin/server/bind` so the UI
    /// can render "currently off / will be on after restart"
    /// transitions without inferring state from
    /// `engine_runtime_settings` shape.
    pub current_ui_bind: Option<String>,
    /// Shared with every per-camera supervisor. The admin
    /// `PUT /api/rules/:id` + `DELETE /api/rules/:id` handlers
    /// call `reload()` on this after the DB write so rule edits
    /// take effect on the next frame without an engine restart.
    /// Without this wire-up the engine kept evaluating with the
    /// rules it compiled at boot, silently ignoring every edit
    /// the operator made through the UI.
    pub evaluator: Arc<RuleEvaluator>,
    pub cache: Arc<LatestFrameCache>,
    /// M-Admin Phase 0 closeout — per-camera frame statistics
    /// (fps EMA, last-frame timestamp, source dims). Shared with
    /// every supervisor task; updated on every received frame.
    /// Read by `GET /v1/cameras/:id/stats` and surfaced on the
    /// merged camera list response.
    pub frame_stats: Arc<nexus_pipeline::FrameStatsRegistry>,
    pub pool: Option<Arc<DetectorPool>>,
    pub ui_root: PathBuf,
    /// Shared with the per-camera supervisors + the storage_safety
    /// loop. The /api/v1/storage/local endpoint reads `is_panic()` +
    /// `kind()` to surface the recorder state in the UI; /api/v1/clips/:id
    /// uses `kind()` to decide whether to return a 503 stub error
    /// (Stage A) or stream the file (Stage B).
    pub recorder: Arc<dyn nexus_pipeline::ClipRecorder>,
    /// Filesystem root that `motion_clips.path` is relative to.
    /// Used by /api/v1/storage/local for the StatvfsProbe + by
    /// /api/v1/clips/:id to compute the absolute path.
    pub clips_dir: PathBuf,
    /// Configured watermark thresholds — surfaced verbatim by
    /// /api/v1/storage/local so the UI can render the same gauge
    /// the engine is using.
    pub low_watermark_pct: u8,
    pub panic_watermark_pct: u8,
    /// M2.2 cold-mirror registry. Shared with the cold replicator;
    /// `Registry::replace_all()` is called by the admin
    /// `PUT|DELETE /api/v1/admin/storage/backends` handlers so
    /// runtime changes take effect on the next replicator tick
    /// without an engine restart.
    pub registry: nexus_storage::Registry,
    /// M2.2 Phase 4 — cold-read transient cache. When a soft-evicted
    /// clip is requested the API streams from cold AND fires a
    /// background rehydrate so the next request hits the local
    /// fast path again. The `CacheJobs` instance internally holds
    /// its own clone of the watermark signal and refuses to start
    /// jobs while the safety FSM is at Low or Panic.
    pub cache_jobs: CacheJobs,
    /// M2.2 Phase 3 — USB hot-plug registry. Shared with the
    /// `usb_watch` task and the recorder. The
    /// `GET /api/v1/storage` handler reads `list()` to surface
    /// attached `NEXUS_*`-labeled volumes so the UI can show what
    /// the recorder is about to write to.
    pub usb_registry: crate::usb_watch::UsbRegistry,
    /// Live handle on `cfg.runtime.clips.preferred_usb_label` that
    /// is shared with the recorder. The recorder reads the current
    /// value at every `open()` call; the new admin endpoint
    /// `PUT /api/v1/admin/runtime/usb_preferred` mutates it. Holds
    /// an `Arc<ArcSwapOption<String>>` internally so updates are
    /// lock-free.
    pub preferred_usb_label: nexus_pipeline::recorder::PreferredUsbLabel,
    /// HS256 bearer verifier shared with the core-next UI's admin
    /// tabs (M2.2 Phase 2 step 12). Gates every write under
    /// `/api/v1/admin/*` (except the OAuth `/callback` redirect
    /// from Google / Microsoft, which authenticates via the
    /// unguessable `state` token instead). Built once at engine
    /// boot from `auth.admin_secret_path` (or the
    /// `NEXUS_ADMIN_BEARER_ALLOW_REMOTE` env-var fallback). Cheap
    /// to clone — `Arc` shares the underlying `DecodingKey`.
    pub admin_auth: Arc<AdminAuthState>,
    /// M2.2 closeout — in-memory cache for pending OAuth
    /// auth-code sessions. The `start`, `callback`, and `status`
    /// handlers under `/api/v1/admin/oauth/*` read+write this so
    /// the three-leg consent flow can hand state between requests
    /// without ever putting a refresh token in the browser.
    pub oauth_sessions: crate::oauth_sessions::OAuthSessions,
    /// M-Admin Phase 1B — in-memory registry of camera-discovery
    /// sessions (ONVIF WS-Discovery + CIDR sweep). The four
    /// `/api/v1/admin/discovery/*` handlers all read from this;
    /// a background sweep evicts entries older than
    /// [`crate::discovery::SESSION_TTL`]. Cheap to clone — wraps
    /// an `Arc<DashMap<Uuid, _>>` internally.
    pub discovery_sessions: DiscoverySessions,
    /// M-Admin Phase 5 — detector prompt catalog. Boot-time
    /// snapshot of every kind the [`InferenceRouter`] knows about
    /// plus its vocabulary (read from `models-manifest.json` for
    /// open-vocab detectors, hard-coded for closed-vocab COCO).
    /// Served verbatim by `GET /api/v1/models/prompts`; the UI
    /// uses it so the camera + rules forms show the labels the
    /// active detector actually emits instead of a stale
    /// hard-coded list. Cheap to clone — wraps the whole catalog
    /// in a single Arc.
    pub model_prompts: Arc<crate::models_catalog::ModelPromptsCatalog>,
    /// M7 Step 6 — alert sink registry shared with the
    /// dispatcher. The `GET /api/v1/admin/sinks/health` handler
    /// uses [`nexus_sinks::SinkRegistry::ids`] to ensure the
    /// response carries a card per *configured* sink, even when
    /// that sink hasn't seen traffic in the requested window
    /// (so the UI doesn't appear to forget a freshly-added sink
    /// just because no alerts have fired yet).
    pub sink_registry: Arc<nexus_sinks::SinkRegistry>,
    /// M6 Phase 2 Step 2.7 — failed-login lockout policy.
    /// Snapshot of `runtime.auth.lockout` at engine boot;
    /// consumed by `auth::login::post_login` via the
    /// `LoginState::from_ref` bridge. Cheap to clone — three
    /// u32 fields.
    pub lockout: nexus_config::LockoutConfig,
    /// M6 Phase 2 Step 2.9 — auth-mode snapshot. Surfaced by
    /// the unauthenticated `GET /api/v1/auth/info` endpoint so
    /// the UI can decide which login form to render (paste-a-
    /// dev-token vs. username+password vs. OIDC redirect).
    /// Snapshot at boot — mode changes require a restart.
    pub auth_mode: nexus_config::AuthMode,
    /// M6 Phase 3 Step 3.3 — OIDC login state. `Some` iff
    /// `cfg.auth.mode` allows OIDC AND `cfg.auth.oidc` is set
    /// AND `OidcClient::discover` succeeded at boot. The two
    /// `/v1/auth/oidc/{start,callback}` handlers extract this
    /// via `FromRef`; the router only mounts those routes when
    /// this Option is `Some`, so the handlers can `.expect()`.
    pub oidc_login: Option<crate::auth::oidc_login::OidcLoginState>,
    /// M6 Phase 3 Step 3.3 UI — display label for the
    /// "Sign in with X" button on the login overlay.
    /// `Some(label)` only when `oidc_login.is_some()` (i.e.
    /// the OIDC routes are actually mounted); falls back to
    /// `"single sign-on"` when the operator left
    /// `[auth.oidc.display_name]` unset. `None` whenever the
    /// SPA must NOT render the OIDC button — discovery
    /// failed, no `[auth.oidc]` block, or mode disallows OIDC.
    pub oidc_display_name: Option<String>,
    /// M3.1 Phase H — visual-prompts admin runtime state
    /// (upload dir, encoder ONNX path, encoder lazy-init
    /// handle). Built at boot from `cfg.runtime` +
    /// `cfg.inference`; see
    /// [`crate::visual_prompts_admin::VisualPromptsAdminState`].
    pub visual_prompts: crate::visual_prompts_admin::VisualPromptsAdminState,
    /// M-Admin Phase 0 follow-up — boot-time effective
    /// inference model configuration (after any
    /// `engine_runtime_settings.inference_model_json`
    /// override has been merged onto `nexus.toml`). Returned
    /// by `GET /v1/admin/server/inference` so the admin UI
    /// can render the active values + diff against any
    /// pending override the operator has saved since.
    /// Restart-required: this value never changes for the
    /// lifetime of the process.
    pub current_inference_model: Arc<nexus_config::ModelConfig>,
    /// `cfg.runtime.state_dir` snapshot — root directory the
    /// engine writes per-camera anchor registries
    /// (`static_objects/cam-<id>.json`) and other long-lived
    /// state under. Read by
    /// `GET /api/v1/cameras/:id/static-anchors` so the viewer
    /// can overlay the persisted static-object map on top of
    /// the live JPEG without round-tripping through the
    /// supervisor.
    pub state_dir: PathBuf,
    /// Operator-initiated static-anchor wipe signal shared with
    /// every camera supervisor. Bumped by
    /// `DELETE /api/cameras/:id/static-anchors`; the supervisor
    /// polls per-frame and invokes
    /// `StaticObjectFilter::clear` (in-memory + on-disk) on the
    /// next iteration after a delta. See
    /// `nexus_pipeline::static_clear` for the rationale.
    pub static_clear: Arc<StaticAnchorClearRegistry>,
    /// Boot-time snapshot of
    /// `cfg.tracker.static_object.anchor_ttl_secs` — the engine-wide
    /// fallback used when a camera has
    /// `behavior.anchor_ttl_secs = None`. Surfaced verbatim by
    /// `GET /api/v1/system/static-object-defaults` so the camera
    /// settings form can render "Engine default: Ns" next to the
    /// per-camera override input.
    pub default_anchor_ttl_secs: u32,
    /// M-Admin Network — in-flight `netplan try`-style apply
    /// session registry. Holds at most one pending apply at any
    /// time; the rollback timer auto-reverts after
    /// [`crate::network::apply::ROLLBACK_TIMEOUT`] unless the
    /// operator calls `POST /v1/admin/network/plan/confirm`.
    /// Cheap to clone (`Arc<Mutex<...>>` internally).
    pub network_apply: crate::network::apply::ApplyRegistry,
    /// M-Cloud Phase 1.16 — wake the
    /// [`crate::cloud_tunnel::spawn_tunnel`] supervisor when the
    /// admin `POST /v1/admin/cloud/enroll` handler persists a fresh
    /// `cloud_enrollment` row. The supervisor parks on this Notify
    /// while no enrollment is present, so a first-time enrollment
    /// activates the WSS tunnel within seconds without an engine
    /// restart. Re-enrollment of an *already-connected* engine
    /// still requires a restart to swap the live cert material —
    /// the Notify is only consulted in the supervisor's
    /// pre-connection wait state.
    pub cloud_enrollment_changed: Arc<tokio::sync::Notify>,
}

pub fn router(state: ApiState) -> Router {
    // Admin writes — gated by HS256 bearer JWT (or loopback /
    // env-var fallback when no admin secret is configured).
    // Split into its own sub-router so the middleware fires only
    // on these routes; `route_layer` (vs `layer`) keeps 404s
    // outside the gate.
    let admin = Router::new()
        .route("/v1/admin/storage/cold", put(put_storage_cold))
        .route(
            "/v1/admin/storage/backends/{handle}",
            put(put_storage_backend).delete(delete_storage_backend),
        )
        // M2.2 closeout: live USB preferred-label editor. Persists
        // to `engine_runtime_settings` and updates the shared
        // PreferredUsbLabel handle in one go so the next clip
        // honours the change without a restart.
        .route("/v1/admin/runtime/usb_preferred", put(put_usb_preferred))
        // M7 Step 6 — global delivery settings (singleton). GET
        // is admin-gated for symmetry with PUT (the schedule
        // shape is mildly sensitive operational info). PUT
        // emits `delivery.settings.changed` on the bus; the
        // `delivery_reload` task in main.rs re-hydrates the
        // dispatcher's policy cache without a restart.
        .route(
            "/v1/admin/delivery",
            get(get_admin_delivery).put(put_admin_delivery),
        )
        // M7 Step 6 — sinks health card. Per-sink status counts
        // over a 1h + 24h window, plus the registry list so
        // configured-but-quiet sinks still get a card.
        .route("/v1/admin/sinks/health", get(get_admin_sinks_health))
        // M2.2 closeout: core-next-native OAuth auth-code dance for
        // cloud cold backends. `start` and `status` are gated; the
        // `callback` route is registered outside the gate (the
        // browser hitting it after consent has no admin bearer; it
        // authenticates via the unguessable `state` token from
        // `start`).
        .route(
            "/v1/admin/oauth/{provider}/start",
            axum::routing::post(start_oauth),
        )
        .route("/v1/admin/oauth/status", get(oauth_status))
        // M-Admin Phase 1B — camera discovery (ONVIF + CIDR sweep).
        // All four routes spawn / read from the shared
        // `discovery_sessions` registry on `ApiState`.
        //
        // Path layout note: the session-poll + probe-rtsp routes
        // live under `…/discovery/sessions/{session_id}` (not
        // `…/discovery/{session_id}`). With the literals `onvif`
        // and `scan` as siblings of a same-depth `{session_id}`
        // param, axum's matchit router treated an incoming
        // `POST /v1/admin/discovery/onvif` as matching the param
        // route (which only has GET registered) and returned 405
        // — silently breaking the Discover dialog. The `sessions/`
        // prefix removes the overlap; see
        // `discovery_post_routes_do_not_405()` regression test.
        .route(
            "/v1/admin/discovery/onvif",
            axum::routing::post(discovery::post_discovery_onvif),
        )
        .route(
            "/v1/admin/discovery/scan",
            axum::routing::post(discovery::post_discovery_scan),
        )
        .route(
            "/v1/admin/discovery/sessions/{session_id}",
            get(discovery::get_discovery_session),
        )
        .route(
            "/v1/admin/discovery/sessions/{session_id}/probe-rtsp",
            axum::routing::post(discovery::post_probe_rtsp),
        )
        .route(
            "/v1/admin/discovery/sessions/{session_id}/onvif-streams",
            axum::routing::post(discovery::post_probe_onvif),
        )
        // M6 Phase 2 Step 2.8 — local-user roster admin. Lives
        // behind both the admin_auth gate AND the per-handler
        // `AdminContext` extractor: a valid HS256 JWT signed
        // with the configured secret passes the gate, but a
        // bearer whose `role` claim is not `admin` is 403'd by
        // the extractor before the handler body runs. Defense
        // in depth — the gate authenticates, the extractor
        // authorises.
        .route(
            "/v1/admin/users",
            get(crate::auth::users_admin::list_users).post(crate::auth::users_admin::create_user),
        )
        .route(
            "/v1/admin/users/{id}",
            put(crate::auth::users_admin::update_user)
                .delete(crate::auth::users_admin::delete_user),
        )
        .route(
            "/v1/admin/users/{id}/reset-password",
            axum::routing::post(crate::auth::users_admin::reset_password),
        )
        .route(
            "/v1/admin/users/{id}/unlock",
            axum::routing::post(crate::auth::users_admin::unlock_user),
        )
        // M6 Phase 4 Step 4.2 + 4.3 — read access to the audit
        // log. Per-resource history powers the "History" panel in
        // the camera / rule / sink / user detail views; the
        // global filtered feed powers the `/admin/audit` table.
        // Both behind the admin gate AND the per-handler
        // `AdminContext` extractor (Phase 2 pattern: gate
        // authenticates, extractor authorises). No new migration
        // — the `audit_log` table + `idx_audit_resource` index
        // are present since Phase 1.
        .route(
            "/v1/admin/audit",
            get(crate::auth::audit_admin::get_global_audit),
        )
        .route(
            "/v1/admin/audit/resource/{kind}/{id}",
            get(crate::auth::audit_admin::get_resource_audit),
        )
        // M3.1 Phase H — visual-prompts CRUD + per-camera
        // attach/detach. All gated by the same admin middleware
        // below. The upload route uses multipart/form-data; the
        // others are pure JSON.
        .route(
            "/v1/admin/visual-prompts",
            get(crate::visual_prompts_admin::list_visual_prompts)
                .post(crate::visual_prompts_admin::post_visual_prompt),
        )
        .route(
            "/v1/admin/visual-prompts/{id}",
            get(crate::visual_prompts_admin::get_visual_prompt)
                .delete(crate::visual_prompts_admin::delete_visual_prompt),
        )
        .route(
            "/v1/admin/cameras/{camera_id}/visual-prompts",
            get(crate::visual_prompts_admin::list_camera_visual_prompts),
        )
        .route(
            "/v1/admin/cameras/{camera_id}/visual-prompts/{visual_prompt_id}",
            axum::routing::post(crate::visual_prompts_admin::attach_camera_visual_prompt)
                .delete(crate::visual_prompts_admin::detach_camera_visual_prompt),
        )
        // ----- M-Admin Phase 0 — runtime knobs that today require
        // an engine restart to take effect.
        //
        // All three follow the same shape: validate the change
        // up-front (probe-bind a TCP socket; run an OIDC
        // discovery dry-run; etc.), persist the requested value
        // to `engine_runtime_settings`, audit-log, and return
        // `{ restart_required: true }`. The engine consults
        // these settings at boot (see
        // `crate::admin_runtime::resolve_persisted_*`) so the
        // operator's choice survives the bounce.
        .route(
            "/v1/admin/server/bind",
            get(crate::admin_runtime::get_server_bind).put(crate::admin_runtime::put_server_bind),
        )
        .route(
            "/v1/admin/auth/config",
            get(crate::admin_runtime::get_auth_config).put(crate::admin_runtime::put_auth_config),
        )
        .route(
            "/v1/admin/auth/oidc/test-discovery",
            axum::routing::post(crate::admin_runtime::post_test_discovery),
        )
        // Streaming gzipped-tar diagnostics export. Generates the
        // tar entries inside a `spawn_blocking` worker that writes
        // through a `GzEncoder` wrapping a bounded mpsc; axum
        // streams the receiver half. Memory stays O(buffer size)
        // regardless of bundle size.
        .route(
            "/v1/admin/diagnostics/export",
            get(crate::admin_runtime::get_diagnostics_export),
        )
        // M-Admin Phase 0 cleanup — storage watermark editor.
        // Persists to `engine_runtime_settings.low_watermark_pct`
        // and `panic_watermark_pct`; restart required (the
        // `storage_safety` FSM reads the snapshot from
        // `ApiState` at boot).
        .route(
            "/v1/admin/server/watermarks",
            get(crate::admin_runtime::get_watermarks).put(crate::admin_runtime::put_watermarks),
        )
        // M-Admin Phase 0 follow-up — default inference model
        // editor (kind / preset / input dims / score threshold
        // / pack_path). Persists the merged ModelConfig as JSON
        // in `engine_runtime_settings.inference_model_json`;
        // restart required because the `InferenceRouter` is
        // built once at boot and not rebuilt per-frame.
        .route(
            "/v1/admin/server/inference",
            get(crate::admin_runtime::get_inference_model)
                .put(crate::admin_runtime::put_inference_model),
        )
        // M-Cloud Phase 1 — UI-driven cloud enrollment. GET returns
        // a redacted status view (no PEMs, no JWT); POST runs the
        // same `cloud_enroll::perform_enrollment` flow as the
        // `nexus-engine enroll` CLI subcommand. Phase 1.16: POST
        // fires `ApiState::cloud_enrollment_changed` so the tunnel
        // supervisor activates the WSS tunnel within seconds
        // without an engine restart (first-time enrollment only;
        // re-enrolling an already-connected engine still requires
        // a restart to swap the live cert/key material).
        .route(
            "/v1/admin/cloud/enrollment",
            get(crate::admin_cloud::get_cloud_enrollment),
        )
        .route(
            "/v1/admin/cloud/enroll",
            axum::routing::post(crate::admin_cloud::post_cloud_enroll),
        )
        // M-Admin Phase 0 follow-up — graceful self-restart.
        // Returns 202 immediately, then `execv()`s a fresh
        // copy of the same binary (preserving PID + argv) so
        // every persisted `engine_runtime_settings` row takes
        // effect without a separate supervisor bounce.
        .route(
            "/v1/admin/server/restart",
            axum::routing::post(crate::admin_runtime::post_restart),
        )
        // M-Admin Network — physical NIC enumeration + netplan
        // YAML editor + lockout-safe apply. Read endpoints are
        // cross-platform; write endpoints surface 501 on
        // non-Linux dev boxes (the UI hides the apply controls
        // when the OS reports unsupported). Apply runs
        // `netplan try` semantics behind the scenes — see
        // [`crate::admin_network`] module docs.
        .route(
            "/v1/admin/network/interfaces",
            get(crate::admin_network::get_interfaces),
        )
        .route(
            "/v1/admin/network/plan",
            get(crate::admin_network::get_plan).put(crate::admin_network::put_plan),
        )
        .route(
            "/v1/admin/network/plan/apply",
            axum::routing::post(crate::admin_network::post_apply),
        )
        .route(
            "/v1/admin/network/plan/confirm",
            axum::routing::post(crate::admin_network::post_confirm),
        )
        .route(
            "/v1/admin/network/plan/rollback",
            axum::routing::post(crate::admin_network::post_rollback),
        )
        .route(
            "/v1/admin/network/apply/status",
            get(crate::admin_network::get_apply_status),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.admin_auth.clone(),
            admin_auth::admin_auth_layer,
        ));

    let api = Router::new()
        .route("/health", get(health))
        .route("/cameras", get(list_cameras).post(create_camera))
        .route("/cameras/{id}", put(upsert_camera).delete(delete_camera))
        .route("/cameras/{id}/frames/latest", get(get_latest_frame_jpeg))
        .route(
            "/cameras/{id}/frames/latest.json",
            get(get_latest_frame_meta),
        )
        // M-Admin Phase 0 closeout — per-camera frame stats
        // (fps EMA, last_frame_age_ms, frames_emitted/dropped,
        // source dims). Used by the dashboard health column.
        .route("/cameras/{id}/stats", get(get_camera_stats))
        // Static-object map for the live viewer overlay. Reads
        // the on-disk per-camera anchor registry written by the
        // `StaticObjectFilter` running inside each supervisor.
        // `DELETE` signals the supervisor to wipe both in-memory
        // anchors and the on-disk file — used by the viewer's
        // "Clear anchors" button when an operator notices stale
        // entries (e.g. a vehicle drove off occluded).
        .route(
            "/cameras/{id}/static-anchors",
            get(get_static_anchors).delete(delete_static_anchors),
        )
        // Engine-wide defaults for tracker.static_object. Read by
        // the camera-settings form so the per-camera TTL input can
        // display the fallback value the engine will use when the
        // override is left blank.
        .route(
            "/v1/system/static-object-defaults",
            get(get_static_object_defaults),
        )
        .route("/rules", get(list_rules).post(create_rule))
        .route("/rules/{id}", put(upsert_rule))
        .route("/rules/{id}", delete(delete_rule))
        .route("/rules/validate", axum::routing::post(validate_rule))
        .route("/rules/preview", axum::routing::post(preview_rule))
        // CEL editor schema — labels emittable by the loaded detector
        // kinds plus the canonical attribute keys the annotator stamps.
        // The UI's CodeMirror completion source merges this with its
        // static fallback so newly-added annotator attributes show up
        // without a UI rebuild.
        .route("/v1/rules/schema", get(get_rules_schema))
        .route("/events", get(list_events))
        .route("/stream/metadata", get(stream_metadata))
        .route("/stream/events", get(stream_events))
        .route("/backends", get(get_backends))
        // M2.1 Stage A — motion + clips + storage health.
        .route("/v1/storage/local", get(get_storage_local))
        .route("/v1/cameras/{id}/motion", get(list_motion_for_camera))
        .route(
            "/v1/cameras/{id}/motion/histogram",
            get(list_motion_histogram_for_camera),
        )
        .route("/v1/clips/{id}", get(get_clip))
        .route("/v1/clips/{id}/thumbnail", get(get_clip_thumbnail))
        .route("/v1/clips/{id}/tracks", get(get_clip_tracks))
        .route("/v1/events/{event_id}/clip", get(get_event_clip_lookup))
        // M7 Step 6 — per-event delivery history. Read-only.
        // Returns every outbox row for the event (sink × attempt
        // × status). Powers the per-event badge strip in the alert
        // detail view.
        .route("/v1/events/{event_id}/delivery", get(get_event_delivery))
        // M7 Step 6 — per-rule delivery policy. GET returns the
        // override (if any) + the resolved effective policy after
        // the global cascade. PUT sets or clears the override and
        // emits `rule.delivery_policy.changed` so the dispatcher
        // re-hydrates the in-memory cache. Mirrors the existing
        // `/api/rules/{id}` shape — not gated, since the rules
        // CRUD it lives next to isn't either.
        .route(
            "/v1/rules/{id}/delivery",
            get(get_rule_delivery).put(put_rule_delivery),
        )
        // M2.2 cold-mirror — combined hot+cold view (read-only).
        .route("/v1/storage", get(get_storage))
        // M-Admin Phase 5 — detector prompt catalog (read-only).
        // Lets the UI render kind-appropriate label pickers in
        // the camera + rules forms.
        .route("/v1/models/prompts", get(get_model_prompts))
        // M-Dashboard Phase 2 — host metrics snapshot for the
        // operator dashboard. Any authenticated viewer can read
        // it; cached for 1s server-side. See
        // [`crate::system_metrics`] for the response shape.
        .route(
            "/v1/system/metrics",
            get(crate::system_metrics::get_system_metrics),
        )
        // M2.2 closeout: OAuth callback for the auth-code dance.
        // Registered OUTSIDE the admin gate (provider redirects a
        // browser here; authentication is via the unguessable
        // `state` token from the matching `start` request).
        .route("/v1/admin/oauth/{provider}/callback", get(oauth_callback))
        // M6 Phase 2 Step 2.7 — auth endpoints. Live OUTSIDE
        // the admin gate (anyone can attempt to log in; the
        // change-password handler checks the bearer itself via
        // the `SessionContext` extractor).
        .route(
            "/v1/auth/login",
            axum::routing::post(crate::auth::login::post_login),
        )
        .route(
            "/v1/auth/refresh",
            axum::routing::post(crate::auth::login::post_refresh),
        )
        .route(
            "/v1/auth/logout",
            axum::routing::post(crate::auth::login::post_logout),
        )
        .route(
            "/v1/auth/change-password",
            axum::routing::post(crate::auth::login::post_change_password),
        )
        // First-run admin setup. UNAUTHENTICATED by design —
        // the operator can't authenticate yet because no admin
        // exists. The handler enforces the precondition
        // (`users` table empty AND mode allows local) server-
        // side; any abuse attempt after the initial admin is
        // provisioned returns 409. The UI presents the setup
        // form only when `GET /auth/info` reports
        // `first_run_pending: true`.
        .route(
            "/v1/auth/first-run-setup",
            axum::routing::post(crate::auth::login::post_first_run_setup),
        )
        // M6 Phase 2 Step 2.9 — public auth-mode probe. The UI
        // hits this on first paint to decide which login form
        // to render (paste-a-dev-token vs. username+password
        // vs. OIDC redirect). Unauthenticated by design.
        .route("/v1/auth/info", get(get_auth_info))
        // M-Install Checkpoint 3c — first-boot setup wizard.
        // `status` is read by every authenticated request the
        // SPA router makes (it gates the `/setup` redirect);
        // `complete` is the one-shot Finish button. Both live
        // OUTSIDE the admin-gate sub-router because they
        // authenticate via `SessionContext` / `AdminContext`
        // extractors instead of the admin-bearer middleware.
        .route("/v1/setup/status", get(crate::setup::get_status))
        .route(
            "/v1/setup/complete",
            axum::routing::post(crate::setup::post_complete),
        )
        // Admin writes (gated) merged in last so they share state.
        .merge(admin);

    // M6 Phase 3 Step 3.3 — mount the OIDC auth-code routes
    // ONLY when an `OidcLoginState` is wired in. The handlers
    // extract that state via `FromRef::expect`, so registering
    // the routes here when it's `None` would panic on first
    // hit; gating on the Option keeps `auth.mode = local`
    // deployments from accidentally exposing a 500'ing OIDC
    // endpoint.
    let api = if state.oidc_login.is_some() {
        api.route(
            "/v1/auth/oidc/start",
            axum::routing::post(crate::auth::oidc_login::post_start),
        )
        .route(
            "/v1/auth/oidc/callback",
            get(crate::auth::oidc_login::get_callback),
        )
    } else {
        api
    };

    // M7 Step 6F2 — dev-only event-injection endpoint. Lives
    // OUTSIDE the admin gate by design (the e2e fixture runs
    // on loopback). Compiled out entirely unless the
    // `test-injection` cargo feature is on.
    #[cfg(feature = "test-injection")]
    let api = api.route(
        "/v1/_test/inject_event",
        axum::routing::post(crate::test_inject::post_inject_event),
    );

    // M-Install Checkpoint 3c — dev-only setup-latch reset.
    // Used by the wizard e2e spec to exercise the empty-state
    // redirect path without spawning a second engine. Same
    // gate as inject_event.
    #[cfg(feature = "test-injection")]
    let api = api.route(
        "/v1/_test/setup_reset",
        axum::routing::post(crate::test_inject::post_setup_reset),
    );

    // SPA hosting: ServeDir returns 404 for unknown paths like /dashboard
    // or /admin/users (TanStack Router uses history-mode paths). Configure
    // index.html as the not-found fallback so a hard refresh on any SPA
    // route still loads the app, which then runs its own client router.
    let index_html = state.ui_root.join("index.html");
    let static_dir = ServeDir::new(state.ui_root.clone())
        .append_index_html_on_directories(true)
        .not_found_service(ServeFile::new(index_html));

    Router::new()
        .nest("/api", api)
        .fallback_service(static_dir)
        .layer(CorsLayer::permissive())
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<nexus_store::StoreError> for ApiError {
    fn from(e: nexus_store::StoreError) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// `GET /api/v1/auth/info` — public auth-mode probe. Returns
/// the engine's configured authentication backend so the UI
/// can pick the right login form on first paint without
/// needing to know which deployment it's pointing at.
///
/// Wire shape:
///
/// ```json
/// {
///   "mode": "local" | "oidc" | "hybrid",
///   "allows_local": bool,
///   "allows_oidc": bool
/// }
/// ```
///
/// Deliberately unauthenticated: anonymous visitors need to
/// know how to authenticate. No other fields are exposed — in
/// particular nothing about the OIDC issuer (which would leak
/// internal IdP topology), no lockout policy, no user counts.
async fn get_auth_info(State(s): State<ApiState>) -> Json<serde_json::Value> {
    // `first_run_pending` is the signal the UI's login page
    // uses to render the "Set up the initial admin password"
    // form instead of the normal sign-in form. True iff the
    // engine's `auth.mode` allows local users AND the `users`
    // table is empty (incl. tombstones). count_users() is a
    // single indexed `COUNT(*)` so calling it on every
    // /auth/info request is fine. If the DB is unreachable we
    // fall back to `false` — the operator will see the
    // "engine unreachable" banner from the broader error
    // surface anyway.
    let first_run_pending = if s.auth_mode.allows_local() {
        match s.store.count_users().await {
            Ok(n) => n == 0,
            Err(e) => {
                tracing::warn!(error = %e, "get_auth_info: count_users failed; defaulting first_run_pending=false");
                false
            }
        }
    } else {
        false
    };
    Json(serde_json::json!({
        "mode": match s.auth_mode {
            nexus_config::AuthMode::Local => "local",
            nexus_config::AuthMode::Oidc => "oidc",
            nexus_config::AuthMode::Hybrid => "hybrid",
        },
        "allows_local": s.auth_mode.allows_local(),
        "allows_oidc": s.auth_mode.allows_oidc(),
        // M6 Phase 3 Step 3.3 UI — non-null only when the
        // OIDC routes are actually mounted (discovery
        // succeeded). The SPA uses this as the single signal
        // to render the "Sign in with X" button, so a
        // misconfigured / unreachable IdP doesn't surface a
        // button that 404s on click.
        "oidc_display_name": s.oidc_display_name,
        // First-run setup signal — see comment above.
        "first_run_pending": first_run_pending,
    }))
}

async fn list_cameras(State(s): State<ApiState>) -> Result<Json<Vec<CameraConfig>>, ApiError> {
    Ok(Json(s.store.list_cameras().await?))
}

async fn upsert_camera(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
    Json(mut cam): Json<CameraConfig>,
) -> Result<Json<CameraConfig>, ApiError> {
    cam.id = id;
    // M6 Phase 4 Step 4.1 — capture pre-state for the audit row so
    // operators can diff before/after on the per-resource history
    // panel. `None` on a create; `Some(prev)` on update. The list
    // walk is cheap (rules / cameras are tens of rows), but if it
    // becomes hot we'd switch to a `get_camera(id)` shortcut.
    let before = s
        .store
        .list_cameras()
        .await
        .ok()
        .and_then(|all| all.into_iter().find(|c| c.id == id));
    let after_str = serde_json::to_string(&cam).ok();
    let before_str = before.as_ref().and_then(|b| serde_json::to_string(b).ok());
    let resource_id = id.to_string();
    // M6 Phase 4 Step 4.1 (tx-merge) — the domain mutation
    // and the success audit row commit together in one SQLite
    // tx. If the audit write fails, the tx drops and the camera
    // upsert is rolled back too — so the audit log can never
    // be missing a row for a mutation that actually landed.
    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store.upsert_camera_tx(&mut tx, &cam).await?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            session.as_ref(),
            &headers,
            peer.ip(),
            "camera.upsert",
            "camera",
            Some(resource_id.as_str()),
            before_str.as_deref(),
            after_str.as_deref(),
        )
        .await?;
        nexus_store::Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        // Failure-path audit — standalone tx, so it survives even
        // if the in-tx audit was what failed above.
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            session.as_ref(),
            &headers,
            peer.ip(),
            "camera.upsert",
            "camera",
            Some(resource_id.as_str()),
            nexus_store::audit::AuditOutcome::Failure,
            before_str.as_deref(),
            None,
        )
        .await;
        return Err(e.into());
    }
    // Fire-and-forget: the engine's `config.changed` reconciler
    // listens for this and hot-starts a supervisor + pre-roll
    // ingester for the new (or modified) camera. `action` lets the
    // reconciler skip a DB roundtrip for the trivial cases.
    let _ = s
        .bus
        .publish(
            topic::CONFIG_CHANGED,
            &serde_json::json!({ "kind": "camera", "action": "upsert", "camera_id": id }),
        )
        .await;
    Ok(Json(cam))
}

/// `POST /cameras` — create a new camera with a server-assigned
/// `id`. The engine `CameraId` is `i64`, so the UI has no way to
/// pick a stable id at create time (a derived string like
/// `cam-<ip>` won't deserialise as i64 and the path extractor
/// rejects it with 400 before we ever reach the handler). This
/// endpoint accepts the camera body verbatim, ignores any `id`
/// the caller passed, lets SQLite's `INTEGER PRIMARY KEY` rowid
/// alias assign one, and returns the populated config so the UI
/// can update its local state with the new id.
async fn create_camera(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
    Json(mut cam): Json<CameraConfig>,
) -> Result<Json<CameraConfig>, ApiError> {
    // Force the id to zero so a careless body field can't trick
    // us into update-via-create — the store helper binds NULL
    // regardless, but zeroing here keeps the placeholder JSON
    // honest before the post-insert rewrite.
    cam.id = 0;
    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store.create_camera_tx(&mut tx, &mut cam).await?;
        let after_str = serde_json::to_string(&cam).ok();
        let resource_id = cam.id.to_string();
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            session.as_ref(),
            &headers,
            peer.ip(),
            "camera.create",
            "camera",
            Some(resource_id.as_str()),
            None,
            after_str.as_deref(),
        )
        .await?;
        nexus_store::Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        // Failure-path audit (standalone tx) — see upsert_camera
        // for the same pattern. `before` is unconditionally
        // `None` on a create, so we just record the attempted
        // payload as the failed "after".
        let attempted_str = serde_json::to_string(&cam).ok();
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            session.as_ref(),
            &headers,
            peer.ip(),
            "camera.create",
            "camera",
            None,
            nexus_store::audit::AuditOutcome::Failure,
            None,
            attempted_str.as_deref(),
        )
        .await;
        return Err(e.into());
    }
    let _ = s
        .bus
        .publish(
            topic::CONFIG_CHANGED,
            &serde_json::json!({
                "kind": "camera",
                "action": "create",
                "camera_id": cam.id,
            }),
        )
        .await;
    Ok(Json(cam))
}

async fn delete_camera(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
) -> Result<StatusCode, ApiError> {
    let before = s
        .store
        .list_cameras()
        .await
        .ok()
        .and_then(|all| all.into_iter().find(|c| c.id == id));
    let before_str = before.as_ref().and_then(|b| serde_json::to_string(b).ok());
    let resource_id = id.to_string();
    // M6 Phase 4 Step 4.1 (tx-merge) — see upsert_camera.
    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store.delete_camera_tx(&mut tx, id).await?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            session.as_ref(),
            &headers,
            peer.ip(),
            "camera.delete",
            "camera",
            Some(resource_id.as_str()),
            before_str.as_deref(),
            None,
        )
        .await?;
        nexus_store::Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            session.as_ref(),
            &headers,
            peer.ip(),
            "camera.delete",
            "camera",
            Some(resource_id.as_str()),
            nexus_store::audit::AuditOutcome::Failure,
            before_str.as_deref(),
            None,
        )
        .await;
        return Err(e.into());
    }
    // Same channel as upsert — the reconciler diffs the live set
    // against the DB and aborts the supervisor for any removed id.
    let _ = s
        .bus
        .publish(
            topic::CONFIG_CHANGED,
            &serde_json::json!({ "kind": "camera", "action": "delete", "camera_id": id }),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_rules(State(s): State<ApiState>) -> Result<Json<Vec<RuleConfig>>, ApiError> {
    Ok(Json(s.store.list_rules().await?))
}

async fn upsert_rule(
    State(s): State<ApiState>,
    Path(id): Path<RuleId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
    Json(mut rule): Json<RuleConfig>,
) -> Result<Json<RuleConfig>, ApiError> {
    rule.id = id.clone();
    // M-Admin Phase 5 — compile the CEL before we touch the store.
    // Pre-Phase-5 a bad `when` field was silently accepted and only
    // crashed the engine on next restart (compile happens at load).
    // Returning 400 here lets the admin UI surface the precise
    // parser error instead of a generic 500/timeout. The upstream
    // `cel-interpreter` parser panics on some malformed inputs
    // (e.g. trailing operators), so the call is wrapped in
    // `catch_unwind` to convert that into a clean 400 instead of
    // tearing down the worker.
    if let Err(msg) = compile_cel_safely(&rule) {
        // M6 Phase 4 Step 4.1 — record validation failures too so
        // operators can see "user tried to ship a broken CEL"
        // events in the audit log without scraping engine logs.
        let rule_id_str = id.to_string();
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            session.as_ref(),
            &headers,
            peer.ip(),
            "rule.upsert",
            "rule",
            Some(rule_id_str.as_str()),
            nexus_store::audit::AuditOutcome::Failure,
            None,
            None,
        )
        .await;
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("invalid CEL in `when`: {msg}"),
        ));
    }
    let before = s
        .store
        .list_rules()
        .await
        .ok()
        .and_then(|all| all.into_iter().find(|r| r.id == id));
    let before_str = before.as_ref().and_then(|b| serde_json::to_string(b).ok());
    let after_str = serde_json::to_string(&rule).ok();
    let rule_id_str = id.to_string();
    // M6 Phase 4 Step 4.1 (tx-merge) — see upsert_camera.
    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store.upsert_rule_tx(&mut tx, &rule).await?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            session.as_ref(),
            &headers,
            peer.ip(),
            "rule.upsert",
            "rule",
            Some(rule_id_str.as_str()),
            before_str.as_deref(),
            after_str.as_deref(),
        )
        .await?;
        nexus_store::Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            session.as_ref(),
            &headers,
            peer.ip(),
            "rule.upsert",
            "rule",
            Some(rule_id_str.as_str()),
            nexus_store::audit::AuditOutcome::Failure,
            before_str.as_deref(),
            None,
        )
        .await;
        return Err(e.into());
    }
    reload_rules_into_evaluator(&s, "upsert", &id).await;
    Ok(Json(rule))
}

/// `POST /rules` — create a new rule with a server-assigned id.
/// Mirrors `POST /cameras`: any `id` the caller passed in the body
/// is discarded and replaced with the next available `rule-<N>`
/// sequence (allocated inside the same tx that performs the
/// INSERT so concurrent creates can't collide). Returns the
/// populated [`RuleConfig`] so the UI can refresh its local state
/// with the new id without a second `list_rules` roundtrip.
async fn create_rule(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
    Json(mut rule): Json<RuleConfig>,
) -> Result<Json<RuleConfig>, ApiError> {
    // Same defence-in-depth as `create_camera` — zero the id field
    // before the tx so a careless body field can't trick us into
    // update-via-create. The tx will overwrite it with the freshly
    // allocated `rule-<N>` id before INSERT.
    rule.id.clear();
    // Compile the CEL up-front so a bad `when` produces a 400 with
    // the precise parser error, not a transactional INSERT-then-
    // rollback-without-context. Mirrors the upsert path.
    if let Err(msg) = compile_cel_safely(&rule) {
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            session.as_ref(),
            &headers,
            peer.ip(),
            "rule.create",
            "rule",
            None,
            nexus_store::audit::AuditOutcome::Failure,
            None,
            None,
        )
        .await;
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("invalid CEL in `when`: {msg}"),
        ));
    }
    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        // Allocate the id INSIDE the tx so two concurrent
        // `POST /rules` calls can't both pick the same suffix.
        rule.id = s.store.next_rule_id_tx(&mut tx).await?;
        s.store.upsert_rule_tx(&mut tx, &rule).await?;
        let after_str = serde_json::to_string(&rule).ok();
        let resource_id = rule.id.clone();
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            session.as_ref(),
            &headers,
            peer.ip(),
            "rule.create",
            "rule",
            Some(resource_id.as_str()),
            None,
            after_str.as_deref(),
        )
        .await?;
        nexus_store::Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        // Failure-path audit — `before` is always None on create.
        let attempted_str = serde_json::to_string(&rule).ok();
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            session.as_ref(),
            &headers,
            peer.ip(),
            "rule.create",
            "rule",
            None,
            nexus_store::audit::AuditOutcome::Failure,
            None,
            attempted_str.as_deref(),
        )
        .await;
        return Err(e.into());
    }
    reload_rules_into_evaluator(&s, "create", &rule.id).await;
    Ok(Json(rule))
}

async fn delete_rule(
    State(s): State<ApiState>,
    Path(id): Path<RuleId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
) -> Result<StatusCode, ApiError> {
    let before = s
        .store
        .list_rules()
        .await
        .ok()
        .and_then(|all| all.into_iter().find(|r| r.id == id));
    let before_str = before.as_ref().and_then(|b| serde_json::to_string(b).ok());
    let rule_id_str = id.to_string();
    // M6 Phase 4 Step 4.1 (tx-merge) — see upsert_camera.
    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store.delete_rule_tx(&mut tx, &id).await?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            session.as_ref(),
            &headers,
            peer.ip(),
            "rule.delete",
            "rule",
            Some(rule_id_str.as_str()),
            before_str.as_deref(),
            None,
        )
        .await?;
        nexus_store::Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            session.as_ref(),
            &headers,
            peer.ip(),
            "rule.delete",
            "rule",
            Some(rule_id_str.as_str()),
            nexus_store::audit::AuditOutcome::Failure,
            before_str.as_deref(),
            None,
        )
        .await;
        return Err(e.into());
    }
    reload_rules_into_evaluator(&s, "delete", &id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Push the current rule set from the store into the shared
/// [`RuleEvaluator`] so the next frame evaluated by every
/// per-camera supervisor sees the edit. Best-effort: we already
/// returned success to the admin caller (DB write + audit
/// succeeded), so a reload failure here is logged at WARN but
/// does not fail the response. On next engine restart the rules
/// are recompiled from the store regardless, so this is
/// strictly a hot-reload accelerator — never the source of
/// truth.
///
/// Failure modes:
///   * `store.list_rules` fails — almost certainly a DB-level
///     issue that the caller's previous write would also have
///     surfaced; log it.
///   * `evaluator.reload` fails — a *different* rule in the
///     store has an invalid `when` clause that `compile_cel_safely`
///     never had a chance to catch (e.g. it was written by an
///     older build that didn't validate). Log the offending
///     rule id so the operator can find and fix it.
async fn reload_rules_into_evaluator(s: &ApiState, op: &str, rule_id: &RuleId) {
    match s.store.list_rules().await {
        Ok(rules) => {
            if let Err(e) = s.evaluator.reload(&rules) {
                tracing::warn!(
                    op,
                    rule = %rule_id,
                    "rule {op} persisted but evaluator reload failed: {e}; \
                     supervisors will keep using the previously-compiled \
                     rule set until next engine restart"
                );
            } else {
                tracing::info!(
                    op,
                    rule = %rule_id,
                    count = rules.len(),
                    "rule {op}: evaluator reloaded"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                op,
                rule = %rule_id,
                "rule {op} persisted but re-reading rules from store \
                 for evaluator reload failed: {e}"
            );
        }
    }
}

/// M-Admin Phase 5 — compile-only CEL validation endpoint.
///
/// Lets the admin UI surface "is this `when` expression syntactically
/// valid + references only known fields" on textarea blur, without
/// having to PUT the whole rule (which would also persist + audit).
///
/// Always returns 200 with a `{ok, error?}` body so the UI can render
/// the error inline; a 4xx would force the caller to special-case
/// "valid response" vs "invalid input" vs "transport failure".
#[derive(serde::Deserialize)]
struct ValidateRuleReq {
    when: String,
}

#[derive(serde::Serialize)]
struct ValidateRuleResp {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn validate_rule(Json(req): Json<ValidateRuleReq>) -> Json<ValidateRuleResp> {
    // Build a stub RuleConfig — only `when` is read by the compiler;
    // every other field is filled with a harmless default so we can
    // reuse the exact `CelEngine::compile()` path the loader uses.
    let stub = RuleConfig {
        id: "__validate__".into(),
        name: "validate".into(),
        predicate: nexus_config::RulePredicate {
            when: req.when,
            severity: "low".into(),
        },
        gates: nexus_config::RuleGates::default(),
        debounce: nexus_config::RuleDebounce {
            min_track_age_ms: 0,
            consecutive_frames: 1,
            cooldown_ms: 0,
        },
        enabled: true,
    };
    match compile_cel_safely(&stub) {
        Ok(()) => Json(ValidateRuleResp {
            ok: true,
            error: None,
        }),
        Err(msg) => Json(ValidateRuleResp {
            ok: false,
            error: Some(msg),
        }),
    }
}

/// `GET /api/v1/rules/schema` — what the CEL editor's completion source
/// can suggest from a live engine instance.
///
/// Two slices:
///
/// * `labels` — every detector label the currently-loaded model kinds
///   are known to emit. Sourced from the prompt catalog the camera
///   form already consumes.
/// * `attribute_keys` — every `object.attributes['...']` key the
///   annotator stamps. Hardcoded for now (this matches the static
///   list in `crates/nexus-tracker/src/annotator.rs`); future
///   annotators can extend this without a UI rebuild.
///
/// Both are advisory: the UI keeps its own static fallback so the
/// editor still completes when the engine is unreachable.
#[derive(serde::Serialize)]
struct RulesSchemaResp {
    labels: Vec<String>,
    attribute_keys: Vec<&'static str>,
}

async fn get_rules_schema(State(s): State<ApiState>) -> Json<RulesSchemaResp> {
    let mut labels: Vec<String> = s
        .model_prompts
        .kinds
        .iter()
        .flat_map(|k| k.prompts.iter().cloned())
        .collect();
    labels.sort();
    labels.dedup();
    Json(RulesSchemaResp {
        labels,
        attribute_keys: vec![
            "motion.speed_class",
            "motion.direction",
            "motion.parked_vehicle",
            "motion.dwell_seconds",
            "motion.zone_state",
            "motion.zone_ids",
            "group.size",
        ],
    })
}

/// Wrap [`CelEngine::compile`] in `catch_unwind` so a user-supplied
/// CEL string can't take down a worker thread. The upstream
/// `cel-interpreter` parser (built on `antlr4rust`) panics on some
/// malformed-but-balanced inputs (e.g. trailing operators); we want
/// those to land as a clean validation error, not a 500/hung
/// connection. Returns the parser's error message on `Err`.
fn compile_cel_safely(rule: &RuleConfig) -> Result<(), String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    match catch_unwind(AssertUnwindSafe(|| CelEngine::new().compile(rule))) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(RulesError::Compile(_, m))) => Err(m),
        Ok(Err(other)) => Err(other.to_string()),
        Err(_) => Err("CEL parser panicked on this input (malformed expression).".into()),
    }
}

// ---------------------------------------------------------------------------
// POST /rules/preview — "what would this rule have fired on?"
//
// Replays the candidate rule against the last N hours of
// `motion_events` (the per-track lifecycle table from M2.1) and
// returns the rows whose synthetic TrackedObject matches the
// rule's CEL predicate AND zone gate. Lets operators tune a rule
// against real data before saving it.
//
// Approximations vs. the live pipeline:
//   * Debounce gates (min_track_age_ms, consecutive_frames,
//     cooldown_ms) are NOT applied — the preview wants to surface
//     every raw match so the operator can see what gets filtered.
//     The "Debounce" panel still lets them tune those numbers
//     separately; the preview is for the predicate + zone work.
//   * `age_ms` on the synthetic object is set to 0 because we
//     can't reconstruct true track-age from a single row. Rules
//     that read `object.age_ms` will see 0 in preview — call out
//     in the UI hint.
//   * `now.*` reflects the wall-clock at preview time, not the
//     historical timestamp the row came from. Time-of-day rules
//     (e.g. "after 22:00") preview against the operator's current
//     hour, which matches what they're about to deploy.
// ---------------------------------------------------------------------------

/// Request body for `POST /rules/preview`.
#[derive(serde::Deserialize)]
struct PreviewRuleReq {
    /// The candidate rule. Compiled fresh per request; never
    /// persisted. The pipeline's loaded ruleset is unaffected.
    rule: RuleConfig,
    /// Window start, milliseconds since the Unix epoch. Defaults
    /// to `until_ms - 24h` when omitted.
    #[serde(default)]
    since_ms: Option<i64>,
    /// Window end, milliseconds since the Unix epoch. Defaults to
    /// "now" when omitted.
    #[serde(default)]
    until_ms: Option<i64>,
    /// Hard cap on rows scanned + returned. Default 500; clamped
    /// to `[1, 5000]`. The UI shows "stopped at N — widen the
    /// window to see more" rather than paginating.
    #[serde(default)]
    limit: Option<u32>,
}

/// One past detection that would have matched the candidate rule.
/// Fields mirror the columns of `motion_events` plus the camera
/// name (joined client-side via the cameras list the caller
/// already has cached). `clip_id` lets the UI deep-link into the
/// existing clip playback view.
#[derive(serde::Serialize)]
struct PreviewMatch {
    motion_event_id: nexus_store::MotionEventId,
    camera_id: CameraId,
    clip_id: nexus_store::ClipId,
    track_id: nexus_types::TrackId,
    captured_at: String,
    label: String,
    confidence: f32,
    bbox: nexus_types::BBox,
}

/// Response body for `POST /rules/preview`.
#[derive(serde::Serialize)]
struct PreviewRuleResp {
    /// Matching rows, most-recent-first.
    matches: Vec<PreviewMatch>,
    /// Total `motion_events` rows scanned (i.e. the SQL result
    /// length before predicate filtering). Lets the UI show
    /// "scanned 500 of 12,431 in the last 24h".
    scanned: u32,
    /// The window the scan actually used, echoed back as ISO-8601
    /// so the UI can render "from <X> to <Y>" without re-parsing
    /// the operator's input.
    window_start: String,
    window_end: String,
    /// `true` if `scanned == limit` — i.e. there were more rows
    /// in the window than the cap allowed. The UI nudges the
    /// operator to either widen the window or accept the truncation.
    limit_hit: bool,
    /// Histogram of the distinct labels in the scanned window,
    /// most-frequent first. Lets the UI show a "saw these
    /// labels in the window" hint when the rule returned zero
    /// matches — the single most useful diagnostic for the
    /// common foot-gun of writing `object.label == 'vehicle'`
    /// against a YOLO/COCO pipeline that emits namespaced
    /// labels like `vehicle.car`, `vehicle.truck`, etc.
    /// Truncated to the top 32 labels so a noisy pipeline can't
    /// blow up the response body.
    scanned_labels: Vec<ScannedLabel>,
    /// Number of scanned rows whose per-row CEL evaluation
    /// returned an `Err` (e.g. missing attribute, type mismatch,
    /// `startsWith` arity wrong). These are silently skipped by
    /// the matcher so a single malformed row can't poison the
    /// whole preview, but we still need to surface the count
    /// because a non-zero value here is almost always the cause
    /// of "my rule should match this label but it doesn't".
    eval_errors: u32,
    /// First per-row eval error message (deduped). Mirrors the
    /// shape of `error` but at the per-row layer instead of
    /// compile-time. Lets the UI show "15 of 49 rows errored:
    /// <msg>" so the operator can fix the predicate.
    #[serde(skip_serializing_if = "Option::is_none")]
    eval_first_error: Option<String>,
    /// Total number of rows rejected by the zone gate before
    /// reaching the CEL matcher. Non-zero here when the rule
    /// has `zones` configured AND some rows' bbox-centres fall
    /// outside every resolved zone polygon. The fourth silent
    /// rejection path (after compile-error, eval-error, and
    /// CEL-returns-false) — surfaced because it's invisible
    /// from the predicate alone: the rule looks correct but
    /// zones are filtering everything.
    zone_filtered: u32,
    /// Echo of the CEL `when` string the engine actually compiled.
    /// Verbatim from `req.rule.when` — lets the UI sanity-check
    /// that the form really sent what the operator typed (the
    /// most common cause of "my rule doesn't match" complaints
    /// turns out to be a stale form state sending the wrong
    /// expression entirely).
    effective_when: String,
    /// Compile / validation error if the rule's CEL didn't parse.
    /// When set, `matches` is empty and the UI shows the error
    /// inline (same pattern as `/rules/validate`).
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// One bucket in `PreviewRuleResp::scanned_labels`. Kept as a
/// tuple-ish struct rather than `HashMap<String, u32>` so the JSON
/// preserves the most-frequent-first ordering the UI displays.
///
/// `matched` lets the UI render the per-label scoreboard the
/// operator needs to see when a rule "should" match a known
/// label but doesn't: "vehicle.car: 49 scanned / 49 matched,
/// person: 12 scanned / 0 matched" — the latter line points the
/// finger directly at the broken predicate.
#[derive(serde::Serialize)]
struct ScannedLabel {
    label: String,
    count: u32,
    /// Subset of `count` that satisfied the rule's CEL predicate
    /// (post zone-gate). 0 = none, == count = all.
    matched: u32,
    /// Subset of `count` rejected by the zone gate BEFORE
    /// reaching the CEL matcher. Surfacing this per-label is
    /// crucial: it's how an operator distinguishes "my
    /// predicate is wrong" (matched=0 with zone_filtered=0)
    /// from "my zones don't cover where this label appears"
    /// (zone_filtered == count).
    zone_filtered: u32,
    /// Byte-level representation of the label, as a JSON array of
    /// u8 values, attached ONLY when `matched == 0 && count > 0
    /// && zone_filtered == 0`. This is the tiebreaker for the
    /// very-confusing case where the chip text reads identical
    /// to the operator's literal but `==` still returns false
    /// AND no zone filtering happened — invisible characters
    /// (zero-width-joiner, BOM, NBSP, smart quotes) are the
    /// usual culprit, and only a byte dump reveals them.
    #[serde(skip_serializing_if = "Option::is_none")]
    label_bytes: Option<Vec<u8>>,
}

async fn preview_rule(
    State(s): State<ApiState>,
    Json(req): Json<PreviewRuleReq>,
) -> Result<Json<PreviewRuleResp>, ApiError> {
    use chrono::TimeZone;

    // Default window: last 24h. Caller may override either bound.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let until_ms = req.until_ms.unwrap_or(now_ms);
    let since_ms = req.since_ms.unwrap_or(until_ms - 24 * 3600 * 1000);
    if since_ms >= until_ms {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "since_ms must be < until_ms".into(),
        ));
    }
    let limit = req.limit.unwrap_or(500).clamp(1, 5000);

    // Compile first so a bad CEL string is reported immediately
    // (and we avoid scanning motion_events on a doomed request).
    if let Err(msg) = compile_cel_safely(&req.rule) {
        let window_start = chrono::Utc
            .timestamp_millis_opt(since_ms)
            .single()
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc3339();
        let window_end = chrono::Utc
            .timestamp_millis_opt(until_ms)
            .single()
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc3339();
        return Ok(Json(PreviewRuleResp {
            matches: vec![],
            scanned: 0,
            window_start,
            window_end,
            limit_hit: false,
            scanned_labels: vec![],
            eval_errors: 0,
            eval_first_error: None,
            zone_filtered: 0,
            effective_when: req.rule.predicate.when.clone(),
            error: Some(msg),
        }));
    }

    // Build a one-rule RuleEvaluator. We bypass it on purpose for
    // the per-object loop (we want raw predicate matches, no
    // debounce), but the compile path is the same.
    let engine = CelEngine::new();
    let compiled = engine
        .compile(&req.rule)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;

    // Resolve the camera scope: `camera_filter` on the rule wins.
    // We also need every camera's zones so the per-row zone gate
    // can look them up by id (same logic as RuleEvaluator).
    let all_cameras = s.store.list_cameras().await?;
    let camera_scope: Option<Vec<CameraId>> = req.rule.gates.camera_filter.clone();

    let from = chrono::Utc
        .timestamp_millis_opt(since_ms)
        .single()
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "since_ms out of range".into()))?;
    let to = chrono::Utc
        .timestamp_millis_opt(until_ms)
        .single()
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "until_ms out of range".into()))?;

    let rows = s
        .store
        .list_motion_events_across_cameras(camera_scope.as_deref(), from, to, limit as i64)
        .await?;
    let scanned = rows.len() as u32;
    let limit_hit = scanned == limit;

    // Build a camera_id → &[ZoneConfig] lookup once so the
    // per-row zone gate is O(1) instead of O(cameras).
    let zones_by_camera: std::collections::HashMap<CameraId, &[nexus_config::ZoneConfig]> =
        all_cameras
            .iter()
            .map(|c| (c.id, c.zones.as_slice()))
            .collect();

    let mut matches: Vec<PreviewMatch> = Vec::new();
    // Track silently-swallowed CEL eval errors so the response
    // can surface them. A non-zero count here is almost always
    // the explanation when a rule "should" match but doesn't:
    // e.g. predicate references `object.attributes['x']` on a
    // synthetic preview object whose attributes are empty.
    let mut eval_errors: u32 = 0;
    let mut eval_first_error: Option<String> = None;
    // Per-label scoreboards. `matched` counts rows that
    // produced `Ok(true)`. `zone_filtered` counts rows that
    // were rejected by the zone gate BEFORE reaching the CEL
    // matcher — the third silent rejection path that
    // produces "label scanned 12 times, matched 0" with no
    // other explanation. Surfacing the two separately is the
    // only way the operator can tell "my CEL is wrong" from
    // "my zones reject this label's bboxes".
    let mut matched_by_label: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let mut zone_filtered_by_label: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let mut zone_filtered_total: u32 = 0;
    for row in &rows {
        let synthetic = nexus_types::TrackedObject {
            track_id: row.track_id,
            label: row.label.clone(),
            confidence: row.confidence,
            bbox: row.bbox,
            age_frames: 0,
            age_ms: 0,
            attributes: Default::default(),
        };

        // Apply the zone gate the same way RuleEvaluator does —
        // we want preview parity for the gate path even though we
        // deliberately skip the debounce/cooldown gates.
        if let Some(zone_ids) = req.rule.gates.zones.as_ref().filter(|ids| !ids.is_empty()) {
            let cam_zones = zones_by_camera.get(&row.camera_id).copied().unwrap_or(&[]);
            let resolved: Vec<&nexus_config::ZoneConfig> = zone_ids
                .iter()
                .filter_map(|id| cam_zones.iter().find(|z| &z.id == id))
                .collect();
            // Mirror the "all-unresolved ⇒ suppress everywhere"
            // semantics from RuleEvaluator. We need the frame
            // dims to normalise; motion_events doesn't carry
            // them, so fall back to the camera's most recent
            // cached frame (almost always present once the
            // pipeline has been running for a few seconds). If
            // no cache entry exists yet, default to (1920,1080)
            // — operator-facing preview, not security-critical
            // path; the only cost of a wrong default is a
            // slight bbox-centre offset.
            let (fw, fh) = s
                .cache
                .get(row.camera_id)
                .map(|e| (e.frame.width.max(1), e.frame.height.max(1)))
                .unwrap_or((1920, 1080));
            let (cx, cy) = synthetic.bbox.center();
            let nx = (cx / fw as f32).clamp(0.0, 1.0);
            let ny = (cy / fh as f32).clamp(0.0, 1.0);
            let inside_any = resolved
                .iter()
                .any(|z| preview_point_in_polygon(nx, ny, &z.polygon));
            if !inside_any {
                *zone_filtered_by_label.entry(row.label.clone()).or_insert(0) += 1;
                zone_filtered_total += 1;
                continue;
            }
        }

        match engine.matches(&compiled, &synthetic, row.camera_id) {
            Ok(true) => {
                *matched_by_label.entry(row.label.clone()).or_insert(0) += 1;
                matches.push(PreviewMatch {
                    motion_event_id: row.id,
                    camera_id: row.camera_id,
                    clip_id: row.clip_id,
                    track_id: row.track_id,
                    captured_at: row.captured_at.to_rfc3339(),
                    label: row.label.clone(),
                    confidence: row.confidence,
                    bbox: row.bbox,
                });
            }
            Ok(false) => {}
            // Per-row errors are intentionally swallowed for the
            // matcher (a single malformed attribute shouldn't
            // poison the whole result set), but we count them
            // and capture the first message so the response can
            // surface them. The UI shows "X of N rows errored:
            // <msg>" — the missing piece that turns a silent
            // zero-match into an actionable error.
            Err(e) => {
                eval_errors += 1;
                if eval_first_error.is_none() {
                    let msg = e.to_string();
                    tracing::warn!(
                        rule_id = %req.rule.id,
                        label = %row.label,
                        error = %msg,
                        "preview: CEL eval error on row (counted, swallowed)"
                    );
                    eval_first_error = Some(msg);
                }
            }
        }
    }

    if eval_errors > 0 {
        tracing::warn!(
            rule_id = %req.rule.id,
            eval_errors,
            scanned,
            "preview: {} of {} scanned rows errored during CEL eval (first: {:?})",
            eval_errors,
            scanned,
            eval_first_error.as_deref().unwrap_or(""),
        );
    }

    Ok(Json(PreviewRuleResp {
        matches,
        scanned,
        window_start: from.to_rfc3339(),
        window_end: to.to_rfc3339(),
        limit_hit,
        scanned_labels: tally_scanned_labels(&rows, 32, &matched_by_label, &zone_filtered_by_label),
        eval_errors,
        eval_first_error,
        zone_filtered: zone_filtered_total,
        effective_when: req.rule.predicate.when.clone(),
        error: None,
    }))
}

/// Bucket the scanned rows by `label`, return the top-N most
/// frequent (descending). Powers the "saw these labels in the
/// window" hint the UI shows when a rule returns zero matches —
/// the single fastest way to spot the common foot-gun of writing
/// `object.label == 'vehicle'` against a COCO pipeline that emits
/// `vehicle.car`, `vehicle.truck`, etc. (see
/// `nexus-inference/src/yolo.rs::map_coco_to_domain_label`).
///
/// `top_n` caps the response so a noisy pipeline (e.g. an
/// open-vocab detector with 200 distinct prompts) can't make the
/// preview JSON huge. 32 is enough headroom for every label the
/// COCO-domain mapper emits plus typical open-vocab prompt sets.
///
/// `matched_by_label` is the per-label scoreboard from the
/// matcher loop, used to populate `ScannedLabel.matched` (and
/// `label_bytes` for the byte-level diagnostic when a label was
/// scanned but matched zero times).
fn tally_scanned_labels(
    rows: &[nexus_store::MotionEventRow],
    top_n: usize,
    matched_by_label: &std::collections::HashMap<String, u32>,
    zone_filtered_by_label: &std::collections::HashMap<String, u32>,
) -> Vec<ScannedLabel> {
    use std::collections::HashMap;
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for row in rows {
        *counts.entry(row.label.as_str()).or_insert(0) += 1;
    }
    let mut v: Vec<ScannedLabel> = counts
        .into_iter()
        .map(|(label, count)| {
            let matched = matched_by_label.get(label).copied().unwrap_or(0);
            let zone_filtered = zone_filtered_by_label.get(label).copied().unwrap_or(0);
            // Only attach the byte dump for the genuinely
            // surprising case ("label appears N times, matched
            // 0, AND none were zone-filtered"); when
            // zone_filtered > 0 the explanation is the zone
            // gate, not the predicate, so a byte dump would
            // just be noise.
            let label_bytes = if matched == 0 && count > 0 && zone_filtered == 0 {
                Some(label.as_bytes().to_vec())
            } else {
                None
            };
            ScannedLabel {
                label: label.to_string(),
                count,
                matched,
                zone_filtered,
                label_bytes,
            }
        })
        .collect();
    // Descending by count, then ascending by label for stable
    // ordering when two labels tie (otherwise HashMap iteration
    // order makes the response flap between requests).
    v.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));
    v.truncate(top_n);
    v
}

/// Even-odd winding on a normalised polygon. Inlined here (copy of
/// the helper inside `nexus-rules`) so the preview path doesn't
/// have to expose its internals or pull in nexus-tracker.
fn preview_point_in_polygon(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    if poly.len() < 3 {
        return false;
    }
    let mut inside = false;
    let n = poly.len();
    let xd = x as f64;
    let yd = y as f64;
    for i in 0..n {
        let (p1x, p1y) = poly[i];
        let (p2x, p2y) = poly[(i + 1) % n];
        let p1x = p1x as f64;
        let p1y = p1y as f64;
        let p2x = p2x as f64;
        let p2y = p2y as f64;
        let intersects = ((p1y > yd) != (p2y > yd))
            && (xd < ((p2x - p1x) * (yd - p1y) / ((p2y - p1y) + 1e-9) + p1x));
        if intersects {
            inside = !inside;
        }
    }
    inside
}

#[derive(serde::Deserialize)]
struct EventsQuery {
    limit: Option<i64>,
}

async fn list_events(
    State(s): State<ApiState>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Vec<AlertEvent>>, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let evs = nexus_store::EventStore::list_recent_events(&*s.store, limit).await?;
    Ok(Json(evs))
}

async fn get_latest_frame_meta(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
) -> Result<Json<FrameMetadata>, ApiError> {
    let entry = s
        .cache
        .get(id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no frame for camera".into()))?;
    let f = &entry.frame;
    Ok(Json(FrameMetadata {
        camera_id: f.camera_id,
        frame_id: f.frame_id,
        captured_at: f.captured_at,
        width: f.width,
        height: f.height,
        trace_id: f.trace_id.clone(),
        objects: (*entry.objects).clone(),
    }))
}

/// M-Admin Phase 0 closeout — per-camera frame stats response.
/// Returned by `GET /v1/cameras/:id/stats`. `last_frame_age_ms`
/// is computed at request time so the UI doesn't have to maintain
/// its own wall-clock offset; `null` means no frame has ever been
/// observed for this camera (supervisor hasn't started or source
/// hasn't yet produced a frame).
#[derive(serde::Serialize)]
struct CameraFrameStatsView {
    camera_id: CameraId,
    last_frame_at: Option<chrono::DateTime<chrono::Utc>>,
    last_frame_age_ms: Option<i64>,
    fps_ema: f64,
    frames_emitted: u64,
    frames_dropped: u64,
    source_width: u32,
    source_height: u32,
}

async fn get_camera_stats(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
) -> Result<Json<CameraFrameStatsView>, ApiError> {
    let snap = s
        .frame_stats
        .snapshot(id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no stats for camera".into()))?;
    let now = chrono::Utc::now();
    Ok(Json(CameraFrameStatsView {
        camera_id: id,
        last_frame_at: snap.last_frame_at,
        last_frame_age_ms: snap.last_frame_age_ms(now),
        fps_ema: snap.fps_ema,
        frames_emitted: snap.frames_emitted,
        frames_dropped: snap.frames_dropped,
        source_width: snap.source_width,
        source_height: snap.source_height,
    }))
}

/// On-disk shape of `<state_dir>/static_objects/cam-<id>.json` —
/// mirrors `nexus_tracker::static_object::RegistryFile` without
/// pulling that crate (private struct) into the engine. The
/// `version` field is read-and-ignored for forward compatibility;
/// only `anchors` is surfaced on the wire.
#[derive(serde::Deserialize)]
struct StaticAnchorsFile {
    #[serde(default)]
    anchors: Vec<StaticAnchor>,
}

/// `GET /api/cameras/:id/static-anchors` — returns the persisted
/// static-object map for the camera. Missing file, empty list, or
/// a parse error all collapse to `{ camera_id, anchors: [] }` (no
/// 404 / 500) so the UI overlay can poll without bothering the
/// operator about a registry that simply hasn't been written yet
/// (e.g. camera has `behavior.parking_lot_mode = false`, or the
/// supervisor hasn't promoted any vehicle to "static" yet).
async fn get_static_anchors(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
) -> Result<Json<StaticAnchorsResponse>, ApiError> {
    let path = s
        .state_dir
        .join("static_objects")
        .join(format!("cam-{id}.json"));
    let anchors = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<StaticAnchorsFile>(&bytes) {
            Ok(doc) => doc.anchors,
            Err(e) => {
                tracing::warn!(
                    camera_id = id,
                    path = %path.display(),
                    "static-anchors registry parse failed (returning empty): {e}"
                );
                Vec::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            tracing::warn!(
                camera_id = id,
                path = %path.display(),
                "static-anchors registry read failed (returning empty): {e}"
            );
            Vec::new()
        }
    };
    Ok(Json(StaticAnchorsResponse {
        camera_id: id,
        anchors,
    }))
}

/// `DELETE /api/cameras/:id/static-anchors` — operator-initiated
/// wipe of the persisted + in-memory static-object map for one
/// camera. Bumps the per-camera entry in the shared
/// [`StaticAnchorClearRegistry`]; the camera's supervisor task
/// notices the delta on its next frame and calls
/// `StaticObjectFilter::clear`, which empties the in-memory anchor
/// vector + per-track state and removes
/// `<state_dir>/static_objects/cam-<id>.json` from disk.
///
/// Idempotent — calling it on a camera with no anchors (or on a
/// camera with `behavior.parking_lot_mode = false` where no
/// filter is even running) is a no-op that still returns
/// `204 No Content`. The actual disk file removal happens
/// asynchronously inside the supervisor; callers that immediately
/// re-`GET` may briefly see the pre-clear state on a quiet camera.
async fn delete_static_anchors(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
) -> Result<StatusCode, ApiError> {
    let seq = s.static_clear.request_clear(id);
    tracing::info!(
        camera_id = id,
        seq,
        "operator requested static-anchor clear",
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Wire shape for `GET /api/v1/system/static-object-defaults`.
/// Engine-wide fallback values used by every camera that hasn't
/// set its own override. Restart-required \u2014 the snapshot is taken
/// at boot.
#[derive(serde::Serialize)]
struct StaticObjectDefaultsView {
    anchor_ttl_secs: u32,
}

async fn get_static_object_defaults(State(s): State<ApiState>) -> Json<StaticObjectDefaultsView> {
    Json(StaticObjectDefaultsView {
        anchor_ttl_secs: s.default_anchor_ttl_secs,
    })
}

async fn get_latest_frame_jpeg(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
) -> Result<Response, ApiError> {
    let entry = s
        .cache
        .get(id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no frame for camera".into()))?;
    let frame = &entry.frame;

    // Convert NV12/I420 → RGB on demand for the snapshot. M0 supports RGB24.
    let rgb = match frame.format {
        PixelFormat::Rgb24 => frame.data.as_ref().clone(),
        PixelFormat::Bgr24 => bgr_to_rgb(frame.data.as_ref()),
        _ => {
            return Err(ApiError(
                StatusCode::NOT_IMPLEMENTED,
                format!("snapshot for {:?} not yet implemented", frame.format),
            ));
        }
    };

    let mut out = Vec::with_capacity(rgb.len() / 4);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80)
        .write_image(
            &rgb,
            frame.width,
            frame.height,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        out,
    )
        .into_response())
}

fn bgr_to_rgb(buf: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; buf.len()];
    for (i, chunk) in buf.chunks_exact(3).enumerate() {
        let off = i * 3;
        out[off] = chunk[2];
        out[off + 1] = chunk[1];
        out[off + 2] = chunk[0];
    }
    out
}

async fn stream_metadata(
    State(s): State<ApiState>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let mut sub = s
        .bus
        .subscribe::<FrameMetadata>(topic::FRAME_METADATA)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let stream = async_stream::stream! {
        while let Some(item) = sub.next().await {
            match item {
                Ok(meta) => {
                    if let Ok(ev) = Event::default().json_data(&meta) {
                        yield Ok(ev);
                    }
                }
                Err(_) => break,
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn stream_events(
    State(s): State<ApiState>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let mut sub = s
        .bus
        .subscribe::<AlertEvent>(topic::ALERT_EVENT)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let stream = async_stream::stream! {
        while let Some(item) = sub.next().await {
            match item {
                Ok(ev) => {
                    if let Ok(e) = Event::default().json_data(&ev) {
                        yield Ok(e);
                    }
                }
                Err(_) => break,
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(serde::Serialize)]
struct BackendsResponse {
    mode: &'static str,
    slots: Vec<BackendStatus>,
}

async fn get_backends(State(s): State<ApiState>) -> Json<BackendsResponse> {
    match &s.pool {
        Some(p) => Json(BackendsResponse {
            mode: "pool",
            slots: p.snapshot(),
        }),
        None => Json(BackendsResponse {
            mode: "in_process",
            slots: vec![],
        }),
    }
}

// ---------------------------------------------------------------------------
// M-Admin Phase 5 — detector prompt catalog (read-only).
// ---------------------------------------------------------------------------

/// `GET /api/v1/models/prompts` — surface the prompt vocabulary the
/// engine's currently-loaded detector kinds will actually emit. The
/// UI uses this so the camera + rules forms render a kind-appropriate
/// chip strip (closed-vocab COCO) or free-text + suggestions box
/// (open-vocab yolo_world). See `models_catalog.rs` for how the
/// catalog is built.
async fn get_model_prompts(
    State(s): State<ApiState>,
) -> Json<crate::models_catalog::ModelPromptsCatalog> {
    Json((*s.model_prompts).clone())
}

// ---------------------------------------------------------------------------
// M2.1 Stage A — storage / motion / clips endpoints
// ---------------------------------------------------------------------------

/// Spec'd response shape for `GET /api/v1/storage/local` per
/// `docs/M2_STORAGE.md`. The UI's Storage tab renders the global gauge
/// + per-camera occupancy strip directly off this body.
#[derive(serde::Serialize)]
struct StorageLocalResponse {
    /// `stub` until the GStreamer recorder lands in Stage B.
    recorder_kind: &'static str,
    /// True iff the watermark sampler has the recorder paused. UI
    /// uses this to render the "evicting / no new clips" banner.
    /// Aliases `watermark_state == "panic"`; kept for backwards
    /// compatibility with early Stage A consumers.
    panic: bool,
    clips_dir: PathBuf,

    // --- filesystem ---
    /// Total bytes on `clips_dir`'s mount, per `statvfs`. None on
    /// platforms without `statvfs` (currently: windows in Stage A).
    fs_total_bytes: Option<u64>,
    /// Bytes in use on `clips_dir`'s mount (`total - free`).
    fs_used_bytes: Option<u64>,
    /// User-available free bytes on `clips_dir`'s mount
    /// (`bavail * frsize`, NOT raw `bfree` — matches what the
    /// watermark sampler observes).
    fs_free_bytes: Option<u64>,
    /// Free-pct under clips_dir, 0..=100.
    free_pct: Option<f32>,

    // --- watermark FSM snapshot ---
    /// Current watermark level: `"ok" | "low" | "panic"`. Derived
    /// from `recorder.is_panic()` + the latest `free_pct` against
    /// the configured thresholds. May briefly disagree with the
    /// FSM during a sample-interval window because the FSM has
    /// hysteresis and this snapshot does not — UI badges should
    /// poll once per second and treat the value as advisory.
    watermark_state: &'static str,
    watermark_low_pct: u8,
    watermark_panic_pct: u8,

    // --- per-camera occupancy strip ---
    /// One entry per camera that currently owns at least one clip.
    /// Cameras with zero clips are omitted; the UI may render them
    /// as zero-rows on its own. Sorted by `camera_id`.
    per_camera: Vec<nexus_store::PerCameraClipStats>,
}

async fn get_storage_local(
    State(s): State<ApiState>,
) -> Result<Json<StorageLocalResponse>, ApiError> {
    let stats = compute_fs_stats(&s.clips_dir).await;
    let panic = s.recorder.is_panic();
    let watermark_state = derive_watermark_state(
        panic,
        stats.free_pct,
        s.low_watermark_pct,
        s.panic_watermark_pct,
    );
    let per_camera = s.store.per_camera_clip_stats().await?;
    Ok(Json(StorageLocalResponse {
        recorder_kind: s.recorder.kind(),
        panic,
        clips_dir: s.clips_dir.clone(),
        fs_total_bytes: stats.total_bytes,
        fs_used_bytes: stats.used_bytes,
        fs_free_bytes: stats.free_bytes,
        free_pct: stats.free_pct,
        watermark_state,
        watermark_low_pct: s.low_watermark_pct,
        watermark_panic_pct: s.panic_watermark_pct,
        per_camera,
    }))
}

/// Filesystem stats snapshot consumed by `get_storage_local`.
/// All fields are `None` on platforms without `statvfs`.
#[derive(Default)]
struct FsStats {
    total_bytes: Option<u64>,
    used_bytes: Option<u64>,
    free_bytes: Option<u64>,
    free_pct: Option<f32>,
}

#[cfg(unix)]
async fn compute_fs_stats(path: &std::path::Path) -> FsStats {
    let path = path.to_path_buf();
    let r = tokio::task::spawn_blocking(move || nix::sys::statvfs::statvfs(path.as_path())).await;
    match r {
        Ok(Ok(stat)) => {
            // `fragment_size` is already `u64` on every platform we
            // support; `blocks`/`blocks_available` may be either
            // `u32` (older glibc) or `u64` (macOS/musl), so the
            // explicit casts are still needed there. The allow
            // suppresses the macOS-only lint without removing
            // portability on Linux.
            let frag = stat.fragment_size();
            #[allow(clippy::unnecessary_cast)]
            let blocks = stat.blocks() as u64;
            #[allow(clippy::unnecessary_cast)]
            let avail = stat.blocks_available() as u64;
            let total_bytes = blocks.saturating_mul(frag);
            let free_bytes = avail.saturating_mul(frag);
            let used_bytes = total_bytes.saturating_sub(free_bytes);
            let free_pct = if blocks == 0 {
                Some(0.0)
            } else {
                Some(((avail as f64 / blocks as f64) * 100.0) as f32)
            };
            FsStats {
                total_bytes: Some(total_bytes),
                used_bytes: Some(used_bytes),
                free_bytes: Some(free_bytes),
                free_pct,
            }
        }
        _ => FsStats::default(),
    }
}

#[cfg(not(unix))]
async fn compute_fs_stats(_path: &std::path::Path) -> FsStats {
    FsStats::default()
}

/// Derive a watermark-state label from the recorder panic flag + a
/// fresh `free_pct` reading. Mirrors the order
/// [`nexus_engine::storage_safety::WatermarkController`] uses, minus
/// the hysteresis (which only the FSM owns).
fn derive_watermark_state(
    panic: bool,
    free_pct: Option<f32>,
    low_pct: u8,
    panic_pct: u8,
) -> &'static str {
    if panic {
        return "panic";
    }
    match free_pct {
        Some(pct) if pct <= panic_pct as f32 => "panic",
        Some(pct) if pct <= low_pct as f32 => "low",
        Some(_) => "ok",
        None => "unknown",
    }
}

#[derive(serde::Deserialize)]
struct MotionQuery {
    /// RFC3339, inclusive lower bound. Defaults to now-1h.
    from: Option<String>,
    /// RFC3339, inclusive upper bound. Defaults to now.
    to: Option<String>,
    /// Cap the result page. Defaults to 1000, max 5000.
    limit: Option<i64>,
}

async fn list_motion_for_camera(
    State(s): State<ApiState>,
    Path(camera_id): Path<CameraId>,
    Query(q): Query<MotionQuery>,
) -> Result<Json<Vec<nexus_store::MotionEventRow>>, ApiError> {
    let now = chrono::Utc::now();
    let from = match q.from.as_deref() {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("from: {e}")))?
            .with_timezone(&chrono::Utc),
        None => now - chrono::Duration::hours(1),
    };
    let to = match q.to.as_deref() {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("to: {e}")))?
            .with_timezone(&chrono::Utc),
        None => now,
    };
    if to < from {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "`to` must be >= `from`".into(),
        ));
    }
    let limit = q.limit.unwrap_or(1000).clamp(1, 5000);
    let rows = s
        .store
        .list_motion_events_for_camera(camera_id, from, to, limit)
        .await?;
    Ok(Json(rows))
}

#[derive(serde::Deserialize)]
struct MotionHistogramQuery {
    /// RFC3339, inclusive lower bound. Defaults to now-24h.
    from: Option<String>,
    /// RFC3339, inclusive upper bound. Defaults to now.
    to: Option<String>,
    /// Bucket width in seconds. Defaults to 3600 (one hour).
    /// Clamped to [60, 86400] so the UI can't blow up sqlite with
    /// per-second buckets over a multi-day window.
    bucket_seconds: Option<i64>,
}

async fn list_motion_histogram_for_camera(
    State(s): State<ApiState>,
    Path(camera_id): Path<CameraId>,
    Query(q): Query<MotionHistogramQuery>,
) -> Result<Json<Vec<nexus_store::MotionHistogramBucket>>, ApiError> {
    let now = chrono::Utc::now();
    let from = match q.from.as_deref() {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("from: {e}")))?
            .with_timezone(&chrono::Utc),
        None => now - chrono::Duration::hours(24),
    };
    let to = match q.to.as_deref() {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("to: {e}")))?
            .with_timezone(&chrono::Utc),
        None => now,
    };
    if to < from {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "`to` must be >= `from`".into(),
        ));
    }
    let bucket_seconds = q.bucket_seconds.unwrap_or(3600).clamp(60, 86_400);
    let buckets = s
        .store
        .list_motion_histogram_for_camera(camera_id, from, to, bucket_seconds)
        .await?;
    Ok(Json(buckets))
}

/// M2.2 Phase 4 — serve a soft-evicted (cold-only) clip directly
/// from the cold backend. Range header honoured; if absent the
/// full clip is fetched. Always fires a fire-and-forget rehydrate
/// so the next request hits the local fast path.
///
/// Returns 404 only when both the hot AND cold pointers are
/// missing (legacy row, can't recover).
async fn serve_from_cold(
    s: &ApiState,
    clip: &nexus_store::ClipRow,
    headers: &axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    serve_from_cold_inner(&s.registry, &s.cache_jobs, clip, headers).await
}

/// Inner implementation broken out from [`serve_from_cold`] so
/// tests can exercise it without spinning up the full `ApiState`
/// (which requires a Bus, a recorder, a frame cache, etc.).
async fn serve_from_cold_inner(
    registry: &nexus_storage::Registry,
    cache_jobs: &CacheJobs,
    clip: &nexus_store::ClipRow,
    headers: &axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let cold_handle = clip.cold_handle.as_deref().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            format!("clip {} has no hot or cold pointer; cannot serve", clip.id),
        )
    })?;
    let cold_path = clip.cold_path.as_deref().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            format!(
                "clip {} has cold_handle but no cold_path; row is corrupt",
                clip.id
            ),
        )
    })?;
    if clip.size_bytes <= 0 {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "clip size is zero; cold playback unavailable".to_string(),
        ));
    }
    let file_size = clip.size_bytes as u64;

    let backend = registry.get(cold_handle).ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "cold backend '{cold_handle}' is not registered; cannot serve clip {}",
                clip.id
            ),
        )
    })?;

    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| parse_byte_range(s, file_size));

    // M2.2 perf P1.5 — only kick a rehydrate when this is a
    // full-clip fetch. A viewer scrubbing the timeline issues a
    // sequence of short Range requests for the SAME clip; without
    // this gate each one would start (and the dedup map would
    // promptly cancel) a fresh download — doubling LAN reads / API
    // quota / cloud egress for a clip the operator may never
    // finish watching. Full-clip fetches (no Range header) are the
    // signal that the operator wants the whole file local, so
    // that's where we pay the rehydrate cost.
    if range.is_none() {
        cache_jobs.spawn(clip.id);
    }

    let (start, end_inclusive, status) = match range {
        Some((s, e)) => (s, e, StatusCode::PARTIAL_CONTENT),
        None => (0u64, file_size - 1, StatusCode::OK),
    };

    // M2.2 perf P2 — stream the cold-tier bytes directly to the
    // HTTP client instead of buffering the whole range as
    // `Vec<u8>`. Eliminates the 4 × clip-size transient buffer
    // that 4 concurrent viewers used to cost.
    let stream = backend
        .get_range_stream(cold_path, start, end_inclusive)
        .await
        .map_err(|e| {
            ApiError(
                StatusCode::BAD_GATEWAY,
                format!("cold backend '{cold_handle}' get_range_stream: {e}"),
            )
        })?;
    let len = end_inclusive - start + 1;

    let content_type = match clip.container.as_str() {
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        _ => "application/octet-stream",
    };

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len);
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end_inclusive}/{file_size}"),
        );
    }
    builder
        .body(axum::body::Body::from_stream(stream))
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn get_clip(
    State(s): State<ApiState>,
    headers: axum::http::HeaderMap,
    Path(clip_id): Path<i64>,
) -> Result<Response, ApiError> {
    let clip = s
        .store
        .get_clip(clip_id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("clip {clip_id} not found")))?;

    // Stage A: recorder is `stub` and the on-disk file is 0 bytes —
    // serving it would be misleading. Return 503 with an explicit
    // body so the UI can render "playback unavailable" instead of
    // a broken video element. Stage B (this PR) switches non-stub
    // recorders to a streaming 200 response with HTTP Range support.
    if s.recorder.kind() == "stub" {
        let body = serde_json::json!({
            "error": "playback unavailable",
            "reason": "recorder=stub",
            "clip_id": clip.id,
            "camera_id": clip.camera_id,
            "started_at": clip.started_at,
            "ended_at": clip.ended_at,
            "size_bytes": clip.size_bytes,
            "duration_ms": clip.duration_ms,
            "hot_path": clip.hot_path,
            "cold_handle": clip.cold_handle,
        });
        return Ok((StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response());
    }

    // Reject header-only stub clips up-front. When `mp4mux` opens
    // the file it immediately writes a `ftyp + moov` header (~800
    // bytes) and only adds sample tables on EOS. If the source
    // stalled or every sample was dropped (e.g. missing PTS — see
    // memory note on mp4mux/qtmux PTS handling), the closed clip
    // ends up at ~1 KiB on disk while `duration_ms` is still set
    // from wall-clock (started → ended), so size_bytes — not
    // duration_ms — is the only trustworthy signal. Serving such
    // a file makes Chrome's MP4 demuxer raise `FFmpegDemuxer:
    // demuxer seek failed`. Returning 503 + a structured body
    // lets the UI render a clear "no playable data" affordance
    // instead of a generic broken-video icon.
    //
    // 4 KiB threshold: a 720p H.264 keyframe alone is ≥ 20 KiB;
    // any closed clip with real samples will be many times
    // larger. We only enforce the guard once the clip is closed
    // (`ended_at IS NOT NULL`) so an in-progress recording that
    // legitimately hasn't grown past the header yet still gets a
    // chance to serve once it finishes.
    const STUB_CLIP_BYTE_THRESHOLD: i64 = 4096;
    if clip.ended_at.is_some() && clip.size_bytes > 0 && clip.size_bytes < STUB_CLIP_BYTE_THRESHOLD
    {
        let body = serde_json::json!({
            "error": "playback unavailable",
            "reason": "no_samples",
            "clip_id": clip.id,
            "camera_id": clip.camera_id,
            "started_at": clip.started_at,
            "ended_at": clip.ended_at,
            "size_bytes": clip.size_bytes,
            "duration_ms": clip.duration_ms,
        });
        return Ok((StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response());
    }

    // M2.2 Phase 4 — soft-evicted (cold-only) playback. When the
    // hot pointer is NULL but a cold pointer exists, stream the
    // requested byte range straight from the cold backend AND
    // spawn a background rehydrate so the second request hits
    // the local fast path. The rehydrate is fire-and-forget; it
    // is also a no-op when the storage watermark is not Ok (we
    // refuse to fight the eviction sweeper).
    let hot_path = match clip.hot_path.as_deref() {
        Some(p) => p,
        None => {
            return serve_from_cold(&s, &clip, &headers).await;
        }
    };

    // Resolve the clip path. `motion_clips.hot_path` is stored relative
    // to `clips_dir`; reject any traversal attempt before touching
    // the filesystem (clips_dir is the security boundary).
    let rel = std::path::PathBuf::from(hot_path);
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("clip path contains '..': {hot_path}"),
        ));
    }
    let abs = s.clips_dir.join(&rel);
    let canonical_root = std::fs::canonicalize(&s.clips_dir).map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("canonicalize clips_dir: {e}"),
        )
    })?;
    let canonical_clip = match std::fs::canonicalize(&abs) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("clip file missing on disk: {}", abs.display()),
            ));
        }
        Err(e) => {
            return Err(ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("canonicalize clip: {e}"),
            ));
        }
    };
    if !canonical_clip.starts_with(&canonical_root) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "clip path escapes clips_dir".to_string(),
        ));
    }

    let file_size = match tokio::fs::metadata(&canonical_clip).await {
        Ok(m) => m.len(),
        Err(e) => {
            return Err(ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("stat clip: {e}"),
            ));
        }
    };
    if file_size == 0 {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "clip file is empty (recorder may still be opening it)".to_string(),
        ));
    }

    // Parse `Range:` header. Only `bytes=` units are honoured; missing
    // or malformed headers fall through to a 200 full-body response.
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| parse_byte_range(s, file_size));

    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(&canonical_clip)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("open clip: {e}")))?;

    let content_type = match clip.container.as_str() {
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        _ => "application/octet-stream",
    };

    if let Some((start, end)) = range {
        // RFC 7233 partial content. end is INCLUSIVE.
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return Err(ApiError(
                StatusCode::RANGE_NOT_SATISFIABLE,
                format!("seek failed for range {start}-{end}"),
            ));
        }
        let len = end - start + 1;
        let limited = file.take(len);
        let stream = tokio_util::io::ReaderStream::new(limited);
        let body = axum::body::Body::from_stream(stream);
        let resp = Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, len)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{file_size}"),
            )
            .body(body)
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        return Ok(resp);
    }

    // Full-body 200.
    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, file_size)
        .body(body)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(resp)
}

/// Parse a single-range `bytes=START-END` value, clamped to the
/// file size. Returns `(start, end_inclusive)`. Multi-range is
/// intentionally unsupported (we honour only the first range).
///
/// Supports:
/// * `bytes=START-END` — explicit, inclusive both ends
/// * `bytes=START-` — open-ended, clamped to EOF
/// * `bytes=-N` — suffix form, last N bytes (clamped to whole file
///   when `N >= file_size`). Chrome's MP4 demuxer issues these to
///   probe trailing index boxes (`mfra`/`sidx`); refusing them with
///   a 200 full-body response triggers `FFmpegDemuxer: demuxer seek
///   failed` on clip playback.
fn parse_byte_range(raw: &str, file_size: u64) -> Option<(u64, u64)> {
    if file_size == 0 {
        return None;
    }
    let raw = raw.trim();
    let rest = raw.strip_prefix("bytes=")?;
    // First range only.
    let first = rest.split(',').next()?.trim();
    let (start_str, end_str) = first.split_once('-')?;
    let start_str = start_str.trim();
    let end_str = end_str.trim();
    if start_str.is_empty() {
        // Suffix form `bytes=-N`: return the last N bytes.
        // RFC 7233 §2.1: a suffix length of zero is invalid.
        let suffix: u64 = end_str.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = file_size.saturating_sub(suffix);
        return Some((start, file_size - 1));
    }
    let start: u64 = start_str.parse().ok()?;
    if start >= file_size {
        return None;
    }
    let end: u64 = if end_str.is_empty() {
        file_size - 1
    } else {
        end_str.parse().ok()?
    };
    let end = end.min(file_size - 1);
    if end < start {
        return None;
    }
    Some((start, end))
}

async fn get_clip_thumbnail(
    State(s): State<ApiState>,
    Path(clip_id): Path<i64>,
) -> Result<Response, ApiError> {
    let clip = s
        .store
        .get_clip(clip_id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("clip {clip_id} not found")))?;

    if s.recorder.kind() == "stub" {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "thumbnails unavailable for stub recorder".to_string(),
        ));
    }

    // M2.2: thumbnail generation requires the hot file. Soft-evicted
    // clips return 404 — the UI keeps the cached thumbnail it
    // already has (thumbnails are sticky and survive eviction).
    let hot_path = clip.hot_path.as_deref().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            format!(
                "clip {} is soft-evicted (cold-only); thumbnail unavailable",
                clip.id
            ),
        )
    })?;

    let rel = std::path::PathBuf::from(hot_path);
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("clip path contains '..': {hot_path}"),
        ));
    }
    let clip_path = s.clips_dir.join(&rel);
    if !clip_path.is_file() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("clip file missing on disk: {}", clip_path.display()),
        ));
    }
    // Co-locate thumbnail next to the clip with `.jpg` suffix so the
    // retention sweeper deletes both atoms together.
    let thumb_path = clip_path.with_extension("mp4.jpg");

    let thumb = generate_thumbnail_or_err(&clip_path, &thumb_path).await?;
    let bytes = tokio::fs::read(&thumb).await.map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read thumb: {e}"),
        )
    })?;
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "public, max-age=300")
        .body(axum::body::Body::from(bytes))
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(resp)
}

#[cfg(feature = "gstreamer")]
async fn generate_thumbnail_or_err(
    clip_path: &std::path::Path,
    thumb_path: &std::path::Path,
) -> Result<std::path::PathBuf, ApiError> {
    let clip_owned = clip_path.to_path_buf();
    let thumb_owned = thumb_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        nexus_pipeline::thumbnail::ensure_thumbnail(&clip_owned, &thumb_owned)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")))?
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("thumbnail: {e}")))
}

#[cfg(not(feature = "gstreamer"))]
async fn generate_thumbnail_or_err(
    _clip_path: &std::path::Path,
    _thumb_path: &std::path::Path,
) -> Result<std::path::PathBuf, ApiError> {
    Err(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "thumbnails require the 'gstreamer' feature".to_string(),
    ))
}

/// Response body for `GET /api/v1/clips/:id/tracks`.
///
/// Powers the per-clip bbox overlay on the UI clip-modal player.
/// We bundle a small `clip` summary (`id`, `camera_id`, `started_at`,
/// `ended_at`, `duration_ms`) alongside the per-event rows so the
/// UI can map each event's wall-clock `captured_at` to a
/// `<video>.currentTime` offset without a second round-trip.
///
/// Bbox coordinates in each event are in **supervisor-frame
/// pixels** (currently a hardcoded 960×540 set by `RtspSource`'s
/// videoscale caps), NOT the MP4 clip's native resolution. The
/// `source_width`/`source_height` fields publish those dimensions
/// so the UI can scale per draw call:
///   `pixelOnVideo = bbox.x * (videoWidth / source_width)`.
/// Without this rescale the boxes would render at half-size in
/// the top-left quadrant on any camera whose RTSP feed is wider
/// than 960×540 (i.e. essentially all of them).
///
/// `trigger_track_ids` is the de-duplicated set of `events.track_id`
/// values whose alert rows were stamped against this `clip_id`.
/// The UI filters the draw loop to ONLY these tracks so reviewers
/// see the object that triggered the alert, not every passing car
/// or shadow that the tracker happened to label during the clip
/// window. Empty when the clip has no linked alerts (motion-only
/// recording, or all alert rows have NULL track_id) — the UI
/// shows the bare video with no overlay in that case.
#[derive(serde::Serialize)]
struct ClipTracksResponse {
    clip: ClipTracksSummary,
    /// Pixel width of the coordinate space the bbox values live
    /// in. See struct-level doc for why this is NOT the MP4's
    /// `videoWidth`.
    source_width: u32,
    /// See [`ClipTracksResponse::source_width`].
    source_height: u32,
    /// Track ids that triggered an alert linked to this clip.
    /// See struct-level doc for the UI's filtering contract.
    trigger_track_ids: Vec<i64>,
    events: Vec<nexus_store::MotionEventRow>,
}

#[derive(serde::Serialize)]
struct ClipTracksSummary {
    id: i64,
    camera_id: CameraId,
    started_at: chrono::DateTime<chrono::Utc>,
    ended_at: Option<chrono::DateTime<chrono::Utc>>,
    duration_ms: i64,
}

/// `GET /api/v1/clips/:id/tracks` — return the per-track lifecycle
/// rows recorded against this clip (`born` / `updated` / `died`),
/// in `captured_at ASC` order. Used by the UI clip player to draw
/// bounding boxes on a transparent `<canvas>` synced to the video
/// timeline; see [`ClipTracksResponse`] above for the wire shape.
///
/// 404 when the clip row is missing. We do NOT 404 on "clip exists
/// but has zero motion_events" — the UI still wants the summary
/// so it can render the bare video without overlays.
async fn get_clip_tracks(
    State(s): State<ApiState>,
    Path(clip_id): Path<i64>,
) -> Result<Json<ClipTracksResponse>, ApiError> {
    let clip = s
        .store
        .get_clip(clip_id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("clip {clip_id} not found")))?;

    let events = s.store.list_motion_events_for_clip(clip_id).await?;

    let trigger_track_ids = s
        .store
        .list_event_track_ids_for_clip(clip_id)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(ClipTracksResponse {
        clip: ClipTracksSummary {
            id: clip.id,
            camera_id: clip.camera_id,
            started_at: clip.started_at,
            ended_at: clip.ended_at,
            duration_ms: clip.duration_ms,
        },
        source_width: clip.frame_width,
        source_height: clip.frame_height,
        trigger_track_ids,
        events,
    }))
}

/// Response body for `GET /api/v1/events/:event_id/clip`.
///
/// Tiny lookup the live-alert ticker calls when the user hits the
/// "play" button on a card. The SSE alert payload itself can't
/// carry `clip_id` because the supervisor links events to clips
/// AFTER the bus broadcast (see `link_event_to_clip` in
/// `supervisor.rs`); this endpoint closes the loop on demand.
#[derive(serde::Serialize)]
struct EventClipResponse {
    clip_id: i64,
}

/// `GET /api/v1/events/:event_id/clip` — return the clip id the
/// supervisor stamped against this alert, if any.
///
/// Status codes:
///   * 200 + `{clip_id}` — event exists AND has a linked clip.
///   * 404 — event doesn't exist, OR exists but `clip_id IS NULL`
///     (the link race lost OR the alert fired on a frame with no
///     open recorder). The UI treats both the same way: "no clip
///     to open right now".
async fn get_event_clip_lookup(
    State(s): State<ApiState>,
    Path(event_id): Path<String>,
) -> Result<Json<EventClipResponse>, ApiError> {
    let clip_id = s
        .store
        .get_event_clip_id(&event_id)
        .await
        .map_err(|e| match e {
            nexus_store::StoreError::NotFound(_) => {
                ApiError(StatusCode::NOT_FOUND, format!("event {event_id} not found"))
            }
            other => ApiError(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        })?
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                format!("event {event_id} has no linked clip"),
            )
        })?;
    Ok(Json(EventClipResponse { clip_id }))
}

// ---------------------------------------------------------------------------
// M2.2 — combined storage view + admin mutations
// ---------------------------------------------------------------------------

/// Response shape for `GET /api/v1/storage`. Always includes a
/// `hot` section (re-using the M2.1 [`StorageLocalResponse`] body
/// verbatim so the UI can keep its existing watermark / per-camera
/// rendering paths). The `cold` section is `null` when no cold
/// backend is configured (`storage_cold_replica.backend_handle IS
/// NULL`); when set, it carries the active handle, throttle, and
/// the full [`nexus_store::ColdReplicaStats`] counter set.
///
/// `cold_only_count` is also surfaced at top-level so the UI's
/// storage tab can render the "N clips cold-only" subtitle even
/// when cold replication is currently disabled (the count reflects
/// previously-replicated clips that have since been soft-evicted).
#[derive(serde::Serialize)]
struct StorageResponse {
    hot: StorageLocalResponse,
    cold: Option<ColdStatus>,
    backends: Vec<StorageBackendOut>,
    /// Clips with `hot_path IS NULL AND cold_handle IS NOT NULL`.
    /// First-request playback for these incurs a cold round-trip
    /// and triggers the M2.2 Phase 4 background rehydrate. Always
    /// present (independent of whether cold replication is
    /// currently configured).
    cold_only_count: i64,
    /// M2.2 Phase 3 — USB hot-plug visibility. Surfaces the live
    /// `usb_watch::UsbRegistry` snapshot + the configured
    /// `preferred_usb_label` so the UI can show the operator
    /// which USB volumes are attached and whether the recorder
    /// will route new clips to one of them.
    usb: UsbSection,
}

#[derive(serde::Serialize)]
struct UsbSection {
    /// Currently-attached `NEXUS_*`-labeled volumes the watcher
    /// has seen under `<clips_dir>/usb/` (Linux production layout
    /// shipped via the udev rule, or `/Volumes` symlinked into
    /// `<clips_dir>/usb` on macOS dev). Sorted by label.
    attached: Vec<UsbVolumeOut>,
    /// `cfg.runtime.clips.preferred_usb_label` echoed back. When
    /// non-null AND the matching label appears in `attached`, the
    /// recorder routes new clips under that volume's mount path
    /// and stamps `motion_clips.hot_handle = "usb-<label>"`.
    /// Editing requires `nexus.toml` + a restart in this build —
    /// runtime mutation is a follow-up.
    preferred_label: Option<String>,
    /// Convenience: `true` iff `preferred_label` is set AND the
    /// matching volume is currently attached. The UI uses this
    /// to color the preferred row green vs. amber ("configured
    /// but not currently mounted").
    preferred_active: bool,
}

#[derive(serde::Serialize)]
struct UsbVolumeOut {
    label: String,
    /// Mount path **relative to `clips_dir`** (e.g.
    /// `"usb/NEXUS_VAULT"`). Joining with `clips_dir` gives the
    /// absolute mount root the recorder writes under.
    mount_relpath: std::path::PathBuf,
}

#[derive(serde::Serialize)]
struct ColdStatus {
    /// Handle of the active cold backend (matches a row in
    /// `storage_backends`).
    handle: String,
    /// Backend kind (`"lan"`, etc.). Convenience field — same as
    /// `backends[].kind` for the same handle, surfaced here so the
    /// UI doesn't need a join.
    kind: String,
    throttle_bps: i64,
    /// Last time the cold-replica policy row was updated.
    updated_at: chrono::DateTime<chrono::Utc>,
    /// Count of clips with `cold_handle IS NULL AND sha256 IS NOT
    /// NULL`. The replicator drains this on every tick; a
    /// persistent non-zero number with the backend `Ok` is the
    /// signal to widen `BATCH_SIZE` or check throttle config.
    pending_count: i64,
    /// Count of clips with `cold_handle IS NOT NULL`. Includes
    /// both still-hot replicated clips and soft-evicted (cold-only)
    /// clips. Strictly monotonic for a given backend.
    replicated_count: i64,
    /// Count of clips that are cold-only (soft-evicted). First
    /// request rehydrates from cold via the Phase 4 cache job.
    /// Mirrors the top-level [`StorageResponse::cold_only_count`]
    /// for clients that read only the cold section.
    cold_only_count: i64,
    /// Lifetime bytes uploaded to cold across all clips that
    /// currently carry a cold pointer. Cumulative — the replicator
    /// never deletes from cold so this only grows.
    lifetime_uploaded_bytes: i64,
    /// Backend health pill. Probed inline by the handler so the UI
    /// has fresh truth rather than a cached value; kept fast (no
    /// I/O beyond the backend's own `health()`).
    health: ColdHealthOut,
}

#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ColdHealthOut {
    Ok,
    ReadOnly {
        reason: String,
    },
    Unreachable {
        reason: String,
    },
    /// The configured backend handle is not in the runtime registry
    /// (e.g. it failed to construct at boot from its `config_json`).
    /// Distinct from `Unreachable` because the fix is operator
    /// re-config, not waiting for a transient outage to recover.
    NotRegistered,
}

#[derive(serde::Serialize)]
struct StorageBackendOut {
    handle: String,
    kind: String,
    /// Opaque per-kind config (e.g. `{"root":"/mnt/lan-archive"}`
    /// for `lan`). Parsed as JSON for easier client consumption.
    /// Validated at write time, so an invalid blob here means an
    /// out-of-band edit happened.
    config: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

async fn get_storage(State(s): State<ApiState>) -> Result<Json<StorageResponse>, ApiError> {
    // Reuse the M2.1 hot-section computation verbatim so the two
    // endpoints stay in sync.
    let stats = compute_fs_stats(&s.clips_dir).await;
    let panic = s.recorder.is_panic();
    let watermark_state = derive_watermark_state(
        panic,
        stats.free_pct,
        s.low_watermark_pct,
        s.panic_watermark_pct,
    );
    let per_camera = s.store.per_camera_clip_stats().await?;
    let hot = StorageLocalResponse {
        recorder_kind: s.recorder.kind(),
        panic,
        clips_dir: s.clips_dir.clone(),
        fs_total_bytes: stats.total_bytes,
        fs_used_bytes: stats.used_bytes,
        fs_free_bytes: stats.free_bytes,
        free_pct: stats.free_pct,
        watermark_state,
        watermark_low_pct: s.low_watermark_pct,
        watermark_panic_pct: s.panic_watermark_pct,
        per_camera,
    };

    let policy = s.store.read_cold_replica().await?;
    let stats = s.store.cold_replica_stats().await?;
    let cold = match policy.backend_handle.as_deref() {
        None => None,
        Some(handle) => {
            let (kind, health) = match s.registry.get(handle) {
                Some(b) => {
                    // M2.2 closeout — bound the worst-case page-load
                    // blocking time on a hung backend. reqwest's
                    // default connect timeout is ~30s; we don't want
                    // the Storage tab to hang on a flaky LAN mount
                    // or a stalled OAuth refresh against Drive /
                    // OneDrive. 2s is well above any healthy probe
                    // latency (LAN stat ~ µs, cloud /about ~ 50-300
                    // ms) and below a human's patience threshold.
                    // On timeout we surface Unreachable; the
                    // replicator will continue probing on its own
                    // tick and the next page load will reflect
                    // the recovered state.
                    let probe =
                        tokio::time::timeout(std::time::Duration::from_secs(2), b.health()).await;
                    let h = match probe {
                        Ok(nexus_storage::HealthStatus::Ok) => ColdHealthOut::Ok,
                        Ok(nexus_storage::HealthStatus::ReadOnly { reason }) => {
                            ColdHealthOut::ReadOnly { reason }
                        }
                        Ok(nexus_storage::HealthStatus::Unreachable { reason }) => {
                            ColdHealthOut::Unreachable { reason }
                        }
                        Err(_elapsed) => ColdHealthOut::Unreachable {
                            reason: "health probe timed out (>2s)".into(),
                        },
                    };
                    (b.kind().to_string(), h)
                }
                None => ("unknown".to_string(), ColdHealthOut::NotRegistered),
            };
            Some(ColdStatus {
                handle: handle.to_string(),
                kind,
                throttle_bps: policy.throttle_bps,
                updated_at: policy.updated_at,
                pending_count: stats.pending_count,
                replicated_count: stats.replicated_count,
                cold_only_count: stats.cold_only_count,
                lifetime_uploaded_bytes: stats.lifetime_uploaded_bytes,
                health,
            })
        }
    };

    let backends_rows = s.store.list_storage_backends().await?;
    let backends = backends_rows
        .into_iter()
        .map(|r| StorageBackendOut {
            handle: r.handle,
            kind: r.kind,
            config: serde_json::from_str(&r.config_json).unwrap_or(serde_json::Value::Null),
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();

    let attached: Vec<UsbVolumeOut> = s
        .usb_registry
        .list()
        .into_iter()
        .map(|v| UsbVolumeOut {
            label: v.label,
            mount_relpath: v.mount_relpath,
        })
        .collect();
    let preferred_label = s.preferred_usb_label.get();
    let preferred_active = preferred_label
        .as_deref()
        .map(|l| attached.iter().any(|v| v.label == l))
        .unwrap_or(false);
    let usb = UsbSection {
        attached,
        preferred_label,
        preferred_active,
    };

    Ok(Json(StorageResponse {
        hot,
        cold,
        backends,
        cold_only_count: stats.cold_only_count,
        usb,
    }))
}

// --- Admin mutations ----------------------------------------------------

/// `PUT /api/v1/admin/storage/cold` — switch the active cold
/// backend (or disable cold replication by passing `handle: null`).
#[derive(serde::Deserialize)]
struct PutColdReq {
    /// Backend handle to point cold replication at, or `null` to
    /// disable. The handle MUST exist in `storage_backends` (the
    /// FK on `storage_cold_replica.backend_handle` is `ON DELETE
    /// RESTRICT` — a 4xx surfaces if it doesn't).
    handle: Option<String>,
    /// Per-second throttle for the replicator's token bucket. `0`
    /// disables throttling. Defaults to the current value if
    /// omitted.
    throttle_bps: Option<i64>,
}

async fn put_storage_cold(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: crate::auth::require_role::SessionContext,
    Json(req): Json<PutColdReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Default throttle: keep whatever's already set so the caller
    // can switch handles without re-specifying bandwidth.
    let current = s.store.read_cold_replica().await?;
    let throttle = req.throttle_bps.unwrap_or(current.throttle_bps);
    let after_json = serde_json::json!({
        "handle": req.handle,
        "throttle_bps": throttle,
    });
    let after_str = serde_json::to_string(&after_json).ok();
    let before_str = serde_json::to_string(&serde_json::json!({
        "handle": current.backend_handle,
        "throttle_bps": current.throttle_bps,
    }))
    .ok();
    // M6 Phase 4 Step 4.1 (tx-merge) — the cold-replica UPDATE
    // and its audit row commit together. Mapping FK errors to
    // 400 has to happen inside the async block so the error
    // surfaces from the same `?` chain.
    let tx_res: Result<(), ApiError> = async {
        let mut tx = s.store.begin_tx().await.map_err(ApiError::from)?;
        s.store
            .write_cold_replica_tx(&mut tx, req.handle.as_deref(), throttle)
            .await
            .map_err(|e| match e {
                nexus_store::StoreError::Sqlx(ref se) if se.to_string().contains("FOREIGN KEY") => {
                    ApiError(
                        StatusCode::BAD_REQUEST,
                        format!("cold backend handle does not exist: {e}"),
                    )
                }
                other => other.into(),
            })?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            Some(&session),
            &headers,
            peer.ip(),
            "storage.cold.put",
            "admin/storage/cold",
            Some("singleton"),
            before_str.as_deref(),
            after_str.as_deref(),
        )
        .await
        .map_err(ApiError::from)?;
        nexus_store::Store::commit_tx(tx)
            .await
            .map_err(ApiError::from)?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            Some(&session),
            &headers,
            peer.ip(),
            "storage.cold.put",
            "admin/storage/cold",
            Some("singleton"),
            nexus_store::audit::AuditOutcome::Failure,
            before_str.as_deref(),
            None,
        )
        .await;
        return Err(e);
    }
    let _ = s
        .bus
        .publish(
            topic::STORAGE_BACKENDS_CHANGED,
            &serde_json::json!({ "reason": "cold_replica_updated" }),
        )
        .await;
    Ok(Json(after_json))
}

/// `PUT /api/v1/admin/storage/backends/:handle` — register or
/// update a backend. Body shape:
/// ```json
/// { "kind": "lan", "config": { "root": "/mnt/lan-archive" } }
/// ```
/// On success the backend is built via
/// [`nexus_storage::build_backend`] and inserted into the runtime
/// [`Registry`], so the cold replicator picks it up on the next
/// tick without an engine restart.
#[derive(serde::Deserialize)]
struct PutBackendReq {
    kind: String,
    config: serde_json::Value,
}

async fn put_storage_backend(
    State(s): State<ApiState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: crate::auth::require_role::SessionContext,
    Json(req): Json<PutBackendReq>,
) -> Result<Json<StorageBackendOut>, ApiError> {
    // Validate the URL-path handle BEFORE touching the DB. Same
    // shape `start_oauth` enforces on its body field: lowercase
    // ASCII alnum + [_-], must not be empty, must not collide with
    // the engine-owned `'local'` row. Without this an operator
    // could PUT `/v1/admin/storage/backends/local` and silently
    // rewrite the implicit local backend's config_json, or PUT a
    // handle containing `../` and create a row whose handle would
    // confuse downstream string-matching (eviction sweeper, audit
    // log). DB-level CHECK constraints don't exist on this column
    // because the M2.1 migration predates the regex.
    if !is_valid_handle(&handle) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("handle '{handle}' must match ^[a-z0-9][a-z0-9_-]*$ and not be 'local'"),
        ));
    }
    // For cloud kinds, the API accepts either:
    //   1. `refresh_token: "<cleartext>"` (synthesised by the
    //      engine's own OAuth callback handler, or supplied by an
    //      external admin tool) — we encrypt before persist.
    //   2. `refresh_token: { ciphertext, nonce, ... }` — already-
    //      encrypted from a prior round-trip (e.g. re-PUT of an
    //      unchanged config) — we leave it alone.
    //
    // Cleartext is never persisted: encryption happens BEFORE
    // `upsert_storage_backend` so a `SELECT config_json` from disk
    // can never expose a refresh token even if the encryption step
    // panics mid-way (the panic surfaces as a 500, not a half-write).
    let mut config = req.config.clone();
    if matches!(req.kind.as_str(), "gdrive" | "onedrive") {
        encrypt_cloud_refresh_token_in_place(&s, &mut config)?;
    }
    let config_json = config.to_string();

    // Build first so we never insert a row we can't actually
    // construct an impl for. This catches missing config keys
    // (e.g. `lan` without `root`, or a cloud config whose
    // already-encrypted token won't decrypt with the current
    // admin secret) at the API boundary.
    let _probe = build_any_backend(
        &handle,
        &req.kind,
        &config_json,
        s.admin_auth.admin_secret(),
    )
    .map_err(|e| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("invalid backend config: {e}"),
        )
    })?;
    let audited_config = redacted_config_for_audit(&config);
    let after_str =
        serde_json::to_string(&serde_json::json!({ "kind": req.kind, "config": audited_config }))
            .ok();
    // M6 Phase 4 Step 4.1 (tx-merge) — upsert + audit commit
    // together. `rebuild_registry` stays outside the tx because
    // it only updates in-memory state; if it fails after commit
    // the reconciler picks it up on next tick.
    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store
            .upsert_storage_backend_tx(&mut tx, &handle, &req.kind, &config_json)
            .await?;
        // Audit log: redact the encrypted refresh token blob even
        // though it's only ciphertext — it's still operator
        // credential material and ops logs should not carry it.
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            Some(&session),
            &headers,
            peer.ip(),
            "storage.backend.put",
            "admin/storage/backend",
            Some(handle.as_str()),
            None,
            after_str.as_deref(),
        )
        .await?;
        nexus_store::Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            Some(&session),
            &headers,
            peer.ip(),
            "storage.backend.put",
            "admin/storage/backend",
            Some(handle.as_str()),
            nexus_store::audit::AuditOutcome::Failure,
            None,
            None,
        )
        .await;
        return Err(e.into());
    }
    rebuild_registry(&s).await?;
    let _ = s
        .bus
        .publish(
            topic::STORAGE_BACKENDS_CHANGED,
            &serde_json::json!({ "reason": "backend_upserted", "handle": handle }),
        )
        .await;
    let row = s
        .store
        .list_storage_backends()
        .await?
        .into_iter()
        .find(|r| r.handle == handle)
        .ok_or_else(|| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "upsert succeeded but row not found".to_string(),
            )
        })?;
    Ok(Json(StorageBackendOut {
        handle: row.handle,
        kind: row.kind,
        config: serde_json::from_str(&row.config_json).unwrap_or(serde_json::Value::Null),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

async fn delete_storage_backend(
    State(s): State<ApiState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: crate::auth::require_role::SessionContext,
) -> Result<StatusCode, ApiError> {
    // M6 Phase 4 Step 4.1 (tx-merge) — the DELETE and its audit
    // commit together. The pre-checks (local-handle reject,
    // active-cold replica, in-use motion_clips) run inside the
    // tx so a concurrent change can't slip past us between
    // check and delete. `rebuild_registry` stays outside the
    // tx — see put_storage_backend.
    let tx_res: Result<(), ApiError> = async {
        let mut tx = s.store.begin_tx().await.map_err(ApiError::from)?;
        s.store
            .delete_storage_backend_tx(&mut tx, &handle)
            .await
            .map_err(|e| match e {
                nexus_store::DeleteBackendError::InUse(h) => ApiError(
                    StatusCode::CONFLICT,
                    format!(
                        "backend '{h}' is referenced by motion_clips; clear cold pointers first"
                    ),
                ),
                nexus_store::DeleteBackendError::ActiveCold(h) => ApiError(
                    StatusCode::CONFLICT,
                    format!(
                        "backend '{h}' is the active cold replica; PUT /admin/storage/cold {{handle:null}} first"
                    ),
                ),
                nexus_store::DeleteBackendError::Local(h) => ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("backend '{h}' is the implicit local backend and cannot be deleted"),
                ),
                nexus_store::DeleteBackendError::Store(e) => e.into(),
            })?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            Some(&session),
            &headers,
            peer.ip(),
            "storage.backend.delete",
            "admin/storage/backend",
            Some(handle.as_str()),
            None,
            None,
        )
        .await
        .map_err(ApiError::from)?;
        nexus_store::Store::commit_tx(tx)
            .await
            .map_err(ApiError::from)?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            Some(&session),
            &headers,
            peer.ip(),
            "storage.backend.delete",
            "admin/storage/backend",
            Some(handle.as_str()),
            nexus_store::audit::AuditOutcome::Failure,
            None,
            None,
        )
        .await;
        return Err(e);
    }
    rebuild_registry(&s).await?;
    let _ = s
        .bus
        .publish(
            topic::STORAGE_BACKENDS_CHANGED,
            &serde_json::json!({ "reason": "backend_deleted", "handle": handle }),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Rehydrate the runtime [`Registry`] from the `storage_backends`
/// table. Called after every admin write so the cold replicator's
/// next tick sees the new state without an engine restart.
async fn rebuild_registry(s: &ApiState) -> Result<(), ApiError> {
    let rows = s.store.list_storage_backends().await?;
    let mut backends = Vec::with_capacity(rows.len());
    for row in rows {
        match build_any_backend(
            &row.handle,
            &row.kind,
            &row.config_json,
            s.admin_auth.admin_secret(),
        ) {
            Ok(b) => backends.push(b),
            Err(e) => {
                tracing::warn!(
                    handle = %row.handle,
                    kind = %row.kind,
                    error = %e,
                    "rebuild_registry: skipping backend that failed to build"
                );
            }
        }
    }
    s.registry.replace_all(backends);
    Ok(())
}

/// Cross-crate dispatcher: pick between [`nexus_storage::build_backend`]
/// (LAN) and [`nexus_storage_cloud::build_from_config_json`] (cloud)
/// based on the discriminator. The engine is the only place that
/// knows both crates exist; the trait + factories live in the leaf
/// crates and don't know about each other.
///
/// `admin_secret` is required for cloud kinds so the encrypted
/// refresh-token in `config_json` can be decrypted in-memory at
/// backend construction. None for `lan` because that backend has
/// nothing secret to decrypt.
///
/// Phase 2 Step 2.1b: `azure_blob` is special — the cloud-tunnel
/// supervisor owns the backend's lifecycle (it has the mTLS cert
/// material already in hand from the enrollment artefact and
/// constructs `GatewaySasIssuer` + `AzureBlobBackend` once
/// post-enrollment, then installs them via
/// [`nexus_storage::Registry::insert_reserved`]). The rebuild path
/// MUST therefore refuse to construct an `azure_blob` backend so an
/// admin update to an unrelated LAN/Drive row does not race the
/// cloud-tunnel supervisor by trying to (re)build a backend without
/// the cert material. The `Other` error is logged as a warning by
/// [`rebuild_registry`]; the reserved entry the cloud tunnel
/// installed survives the [`Registry::replace_all`] swap untouched.
fn build_any_backend(
    handle: &str,
    kind: &str,
    config_json: &str,
    admin_secret: Option<&str>,
) -> Result<std::sync::Arc<dyn nexus_storage::ColdBackend>, nexus_storage::BackendError> {
    match kind {
        "lan" | "local" => nexus_storage::build_backend(handle, kind, config_json),
        "gdrive" | "onedrive" => {
            let secret = admin_secret.ok_or_else(|| {
                nexus_storage::BackendError::Other(
                    "cloud backends require auth.admin_secret_path configured (used to \
                     encrypt/decrypt the OAuth refresh token at rest)"
                        .to_string(),
                )
            })?;
            nexus_storage_cloud::build_from_config_json(handle, kind, config_json, secret)
        }
        "azure_blob" => Err(nexus_storage::BackendError::Other(
            "azure_blob backends are owned by the cloud-tunnel supervisor and registered \
             as reserved entries; rebuild_registry must skip them"
                .to_string(),
        )),
        other => Err(nexus_storage::BackendError::Other(format!(
            "unknown backend kind '{other}'"
        ))),
    }
}

/// If `config["refresh_token"]` is a plain string, encrypt it using
/// [`nexus_storage::token_crypto::encrypt`] and replace it with the
/// JSON-serialised [`nexus_storage::token_crypto::EncryptedToken`].
/// If it's already a JSON object (already encrypted or operator
/// supplied), leave it alone. Missing key → 400.
fn encrypt_cloud_refresh_token_in_place(
    s: &ApiState,
    config: &mut serde_json::Value,
) -> Result<(), ApiError> {
    let admin_secret = s.admin_auth.admin_secret().ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "cloud backend writes require auth.admin_secret_path to be configured \
             (used to encrypt the OAuth refresh token at rest)"
                .to_string(),
        )
    })?;

    let obj = config.as_object_mut().ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "cloud backend `config` must be a JSON object".to_string(),
        )
    })?;

    let Some(rt) = obj.get("refresh_token") else {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "cloud backend `config.refresh_token` is required".to_string(),
        ));
    };

    if let Some(cleartext) = rt.as_str() {
        if cleartext.is_empty() {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "cloud backend `config.refresh_token` must be non-empty".to_string(),
            ));
        }
        let encrypted =
            nexus_storage::token_crypto::encrypt(admin_secret, cleartext).map_err(|e| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("refresh-token encryption failed: {e}"),
                )
            })?;
        let serialised = serde_json::to_value(&encrypted).map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("refresh-token serialise: {e}"),
            )
        })?;
        obj.insert("refresh_token".to_string(), serialised);
    }
    // Already-object case: trust the caller's pre-encrypted blob;
    // build_any_backend will reject it at the probe step if the
    // ciphertext is malformed or signed under a different secret.
    Ok(())
}

/// Strip the ciphertext blob from the audit-log surface. The
/// encrypted refresh token is already AES-GCM, but operators' log
/// pipelines (Splunk, journald, etc.) routinely ship audit rows to
/// long-term archives where any token-shaped value is a liability.
/// Replace with `"<redacted>"`.
fn redacted_config_for_audit(config: &serde_json::Value) -> serde_json::Value {
    let mut copy = config.clone();
    if let Some(obj) = copy.as_object_mut() {
        if obj.contains_key("refresh_token") {
            obj.insert(
                "refresh_token".to_string(),
                serde_json::Value::String("<redacted>".to_string()),
            );
        }
        if obj.contains_key("client_secret") {
            obj.insert(
                "client_secret".to_string(),
                serde_json::Value::String("<redacted>".to_string()),
            );
        }
    }
    copy
}

/// `PUT /api/v1/admin/runtime/usb_preferred` — flip the preferred
/// USB label live. Persists to `engine_runtime_settings` so the
/// next engine boot also picks up the new value, and updates the
/// in-memory [`PreferredUsbLabel`] handle so the recorder honours
/// it on the very next clip without waiting for a restart.
///
/// Body shape: `{ "label": "NEXUS_VAULT" }` to set; `{ "label": null }`
/// to clear (and the recorder falls back to the implicit local clips
/// directory). A persisted NULL row is distinct from a missing row
/// — the missing-row path falls back to `nexus.toml`; the NULL row
/// is an explicit "do not use USB even though toml says so".
#[derive(serde::Deserialize)]
struct UsbPreferredReq {
    /// `None` = clear (no USB tiering). `Some(s)` must be
    /// non-empty after trimming.
    label: Option<String>,
}

#[derive(serde::Serialize)]
struct UsbPreferredOut {
    label: Option<String>,
}

async fn put_usb_preferred(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: crate::auth::require_role::SessionContext,
    Json(req): Json<UsbPreferredReq>,
) -> Result<Json<UsbPreferredOut>, ApiError> {
    let normalised = match req.label {
        Some(raw) => {
            let trimmed = raw.trim().to_string();
            if trimmed.is_empty() {
                crate::auth::admin_audit::audit_admin_action(
                    &s.store,
                    Some(&session),
                    &headers,
                    peer.ip(),
                    "runtime.usb_preferred.put",
                    "admin/runtime/usb_preferred",
                    Some("singleton"),
                    nexus_store::audit::AuditOutcome::Failure,
                    None,
                    None,
                )
                .await;
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "label must be non-empty (send null to clear)".to_string(),
                ));
            }
            Some(trimmed)
        }
        None => None,
    };

    let before_label = s
        .store
        .read_runtime_setting("preferred_usb_label")
        .await
        .ok()
        .flatten();
    let before_str = serde_json::to_string(&serde_json::json!({ "label": before_label })).ok();
    let after_str = serde_json::to_string(&serde_json::json!({ "label": normalised })).ok();

    // Persist first so a crash between the in-memory flip and the
    // SQLite write doesn't leave the recorder pointed at a label
    // the next boot won't reconstruct.
    //
    // M6 Phase 4 Step 4.1 (tx-merge) — setting + audit commit
    // together. The in-memory `preferred_usb_label.set(...)` stays
    // OUTSIDE the tx so a commit failure cannot leave the cache
    // pointing at a value the DB doesn't agree with.
    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store
            .write_runtime_setting_tx(&mut tx, "preferred_usb_label", normalised.as_deref())
            .await?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            Some(&session),
            &headers,
            peer.ip(),
            "runtime.usb_preferred.put",
            "admin/runtime/usb_preferred",
            Some("singleton"),
            before_str.as_deref(),
            after_str.as_deref(),
        )
        .await?;
        nexus_store::Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            Some(&session),
            &headers,
            peer.ip(),
            "runtime.usb_preferred.put",
            "admin/runtime/usb_preferred",
            Some("singleton"),
            nexus_store::audit::AuditOutcome::Failure,
            before_str.as_deref(),
            None,
        )
        .await;
        return Err(e.into());
    }
    s.preferred_usb_label.set(normalised.clone());
    Ok(Json(UsbPreferredOut { label: normalised }))
}

// ===========================================================================
// M7 Step 6 — delivery-policy admin surface.
//
// Five HTTP endpoints wire the `CascadingPolicy` (steps 1–5) to the
// admin UI:
//
//   GET  /api/v1/admin/delivery                 — global settings + schedule
//   PUT  /api/v1/admin/delivery                 — atomic update; bus signal
//   GET  /api/v1/admin/sinks/health             — per-sink counts (1h, 24h)
//   GET  /api/v1/rules/{id}/delivery            — override + effective policy
//   PUT  /api/v1/rules/{id}/delivery            — set/clear override
//   GET  /api/v1/events/{event_id}/delivery     — per-sink × attempt history
//
// All writes round-trip through `Store::*` (the source of truth)
// and publish on the bus so `delivery_reload.rs` picks them up;
// the API handler never touches `CascadingPolicy` directly.
// ===========================================================================

/// `PUT /api/v1/admin/delivery` request body. We accept exactly
/// what the GET returns minus `updated_at` (server-stamped). Both
/// `schedule` and `timezone` are optional in the wire shape so the
/// UI can ship a "toggle only" PUT without re-sending the grid;
/// missing fields collapse to the obvious defaults
/// (schedule=None, timezone="UTC").
#[derive(serde::Deserialize)]
struct PutAdminDeliveryReq {
    enabled: bool,
    #[serde(default)]
    schedule: Option<nexus_types::DeliverySchedule>,
    #[serde(default)]
    timezone: Option<String>,
}

async fn get_admin_delivery(
    State(s): State<ApiState>,
) -> Result<Json<nexus_types::DeliverySettings>, ApiError> {
    let settings = s.store.delivery_settings_get().await?;
    Ok(Json(settings))
}

async fn put_admin_delivery(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: crate::auth::require_role::SessionContext,
    Json(req): Json<PutAdminDeliveryReq>,
) -> Result<Json<nexus_types::DeliverySettings>, ApiError> {
    let timezone = req
        .timezone
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "UTC".to_string());
    // Defend the dispatcher: a tz string we can't parse would
    // silently fall back to UTC inside the policy with a warn.
    // 400'ing here surfaces operator typos at form-submit time.
    if timezone.parse::<chrono_tz::Tz>().is_err() {
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            Some(&session),
            &headers,
            peer.ip(),
            "delivery.settings.put",
            "admin/delivery",
            Some("singleton"),
            nexus_store::audit::AuditOutcome::Failure,
            None,
            None,
        )
        .await;
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown IANA timezone: {timezone:?}"),
        ));
    }
    let before = s.store.delivery_settings_get().await.ok();
    let before_str = before.as_ref().and_then(|b| serde_json::to_string(b).ok());
    let settings = nexus_types::DeliverySettings {
        enabled: req.enabled,
        schedule: req.schedule,
        timezone,
        updated_at: chrono::Utc::now(),
    };
    let after_str = serde_json::to_string(&settings).ok();
    // `delivery_settings_put` re-validates the schedule grid
    // (7 × 48) and surfaces a 500 on shape mismatch via the
    // default StoreError conversion — that's fine because the
    // caller has no way to produce a malformed grid through a
    // well-formed JSON body unless they bypassed the UI.
    //
    // M6 Phase 4 Step 4.1 (tx-merge) — settings + audit commit
    // together.
    let tx_res: Result<(), nexus_store::StoreError> = async {
        let mut tx = s.store.begin_tx().await?;
        s.store.delivery_settings_put_tx(&mut tx, &settings).await?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            Some(&session),
            &headers,
            peer.ip(),
            "delivery.settings.put",
            "admin/delivery",
            Some("singleton"),
            before_str.as_deref(),
            after_str.as_deref(),
        )
        .await?;
        nexus_store::Store::commit_tx(tx).await?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            Some(&session),
            &headers,
            peer.ip(),
            "delivery.settings.put",
            "admin/delivery",
            Some("singleton"),
            nexus_store::audit::AuditOutcome::Failure,
            before_str.as_deref(),
            None,
        )
        .await;
        return Err(e.into());
    }
    // Sentinel payload — the reload task always re-reads the
    // store. `_ =`d so a saturated bus doesn't fail the write.
    let _ = s
        .bus
        .publish(topic::DELIVERY_SETTINGS_CHANGED, &serde_json::json!({}))
        .await;
    Ok(Json(settings))
}

/// Response body for `GET /api/v1/rules/{id}/delivery`. The
/// `effective` block is the same shape regardless of inheritance,
/// so the UI can render it without a conditional. `inherited =
/// true` ⇔ `policy = null`.
#[derive(serde::Serialize)]
struct RuleDeliveryResp {
    /// The per-rule override exactly as stored. `None` means the
    /// rule inherits global.
    policy: Option<nexus_types::RuleDeliveryPolicy>,
    /// Resolved policy after the cascade: rule.enabled (or
    /// global.enabled if no override), rule.schedule (or
    /// global.schedule if no rule schedule).
    effective: nexus_types::RuleDeliveryPolicy,
    /// True ⇔ `policy is None`. Convenience so the UI doesn't
    /// have to introspect.
    inherited: bool,
}

async fn get_rule_delivery(
    State(s): State<ApiState>,
    Path(rule_id): Path<RuleId>,
) -> Result<Json<RuleDeliveryResp>, ApiError> {
    let policy = s.store.rule_delivery_policy_get(&rule_id).await?;
    let settings = s.store.delivery_settings_get().await?;
    let effective = nexus_types::RuleDeliveryPolicy {
        // Both gates must be open for delivery; the dispatcher's
        // cascade lives in `nexus_sinks::policy::evaluate_cascade`.
        // Here we just project the same logic into a UI shape.
        enabled: settings.enabled && policy.as_ref().map(|p| p.enabled).unwrap_or(true),
        // Rule schedule REPLACES global (does not intersect),
        // matching the dispatcher's resolution order.
        schedule: policy
            .as_ref()
            .and_then(|p| p.schedule.clone())
            .or_else(|| settings.schedule.clone()),
    };
    let inherited = policy.is_none();
    Ok(Json(RuleDeliveryResp {
        policy,
        effective,
        inherited,
    }))
}

/// `PUT /api/v1/rules/{id}/delivery` body. Use `{"policy": null}`
/// to clear the override and revert the rule to inheriting global;
/// use `{"policy": { ... }}` to set/replace it. A bare-null body
/// is technically valid JSON but tends to surprise callers, so we
/// require the wrapper.
#[derive(serde::Deserialize)]
struct PutRuleDeliveryReq {
    #[serde(default)]
    policy: Option<nexus_types::RuleDeliveryPolicy>,
}

async fn put_rule_delivery(
    State(s): State<ApiState>,
    Path(rule_id): Path<RuleId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
    Json(req): Json<PutRuleDeliveryReq>,
) -> Result<StatusCode, ApiError> {
    let rule_id_str = rule_id.to_string();
    let before = s.store.rule_delivery_policy_get(&rule_id).await.ok();
    let before_str = before.as_ref().and_then(|b| serde_json::to_string(b).ok());
    let after_str = serde_json::to_string(&req.policy).ok();
    // M6 Phase 4 Step 4.1 (tx-merge) — policy + audit commit
    // together. `NotFound` from the store still maps to 404.
    let tx_res: Result<(), ApiError> = async {
        let mut tx = s.store.begin_tx().await.map_err(ApiError::from)?;
        s.store
            .rule_delivery_policy_put_tx(&mut tx, &rule_id, req.policy.as_ref())
            .await
            .map_err(|e| match e {
                nexus_store::StoreError::NotFound(msg) => ApiError(StatusCode::NOT_FOUND, msg),
                other => other.into(),
            })?;
        crate::auth::admin_audit::audit_admin_action_in_tx(
            &s.store,
            &mut tx,
            session.as_ref(),
            &headers,
            peer.ip(),
            "rule.delivery.put",
            "rule/delivery",
            Some(rule_id_str.as_str()),
            before_str.as_deref(),
            after_str.as_deref(),
        )
        .await
        .map_err(ApiError::from)?;
        nexus_store::Store::commit_tx(tx)
            .await
            .map_err(ApiError::from)?;
        Ok(())
    }
    .await;
    if let Err(e) = tx_res {
        crate::auth::admin_audit::audit_admin_action(
            &s.store,
            session.as_ref(),
            &headers,
            peer.ip(),
            "rule.delivery.put",
            "rule/delivery",
            Some(rule_id_str.as_str()),
            nexus_store::audit::AuditOutcome::Failure,
            before_str.as_deref(),
            None,
        )
        .await;
        return Err(e);
    }
    let _ = s
        .bus
        .publish(
            topic::RULE_DELIVERY_POLICY_CHANGED,
            &serde_json::json!({ "rule_id": rule_id }),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_event_delivery(
    State(s): State<ApiState>,
    Path(event_id): Path<String>,
) -> Result<Json<Vec<nexus_store::OutboxRow>>, ApiError> {
    let rows = s.store.outbox_for_event(&event_id).await?;
    Ok(Json(rows))
}

/// Response body for `GET /api/v1/admin/sinks/health`. The window
/// labels (`1h`, `24h`) double as object keys so the UI can index
/// directly without a switch on numeric seconds.
#[derive(serde::Serialize)]
struct SinksHealthResp {
    /// Window definitions for the UI's tab strip / column headers.
    /// Order matches the `last_*` keys on `SinkHealthRow.counts`
    /// so a future "last 7d" tab is a one-line add.
    windows: Vec<SinksHealthWindow>,
    /// One row per sink. Rows are union of (configured sinks,
    /// historical sinks present in the outbox) so the UI shows a
    /// card for a freshly-added sink that hasn't seen traffic AND
    /// for an orphan from a deleted sink (so operators can
    /// reconcile).
    sinks: Vec<SinkHealthRow>,
}

#[derive(serde::Serialize)]
struct SinksHealthWindow {
    label: &'static str,
    secs: i64,
}

#[derive(serde::Serialize)]
struct SinkHealthRow {
    sink_id: String,
    /// True ⇔ this sink id is present in the live `SinkRegistry`.
    /// False ⇔ the rows belong to a deleted sink (still listed so
    /// operators can see the orphan and run the dead-letter purge).
    configured: bool,
    /// Counts keyed by window label.
    counts: std::collections::BTreeMap<&'static str, nexus_store::OutboxSinkCounts>,
}

const HEALTH_WINDOWS: &[(&str, i64)] = &[("1h", 3_600), ("24h", 86_400)];

async fn get_admin_sinks_health(
    State(s): State<ApiState>,
) -> Result<Json<SinksHealthResp>, ApiError> {
    let now = chrono::Utc::now();
    // Per-window stats keyed by sink_id.
    let mut by_sink: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<&'static str, nexus_store::OutboxSinkCounts>,
    > = std::collections::BTreeMap::new();
    for (label, secs) in HEALTH_WINDOWS {
        let since = now - chrono::Duration::seconds(*secs);
        let rows = s.store.outbox_counts_since(since).await?;
        for r in rows {
            by_sink
                .entry(r.sink_id.clone())
                .or_default()
                .insert(*label, r);
        }
    }
    // Union with the live registry so configured-but-quiet sinks
    // still appear. `SinkId::to_string()` matches the format the
    // outbox stores.
    let configured: std::collections::BTreeSet<String> = s
        .sink_registry
        .ids()
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    for id in &configured {
        by_sink.entry(id.clone()).or_default();
    }

    let sinks = by_sink
        .into_iter()
        .map(|(sink_id, mut counts)| {
            // Backfill every window so the UI doesn't have to
            // null-check — a window with no rows shows as zeros.
            for (label, _) in HEALTH_WINDOWS {
                counts
                    .entry(*label)
                    .or_insert_with(|| nexus_store::OutboxSinkCounts {
                        sink_id: sink_id.clone(),
                        ..Default::default()
                    });
            }
            SinkHealthRow {
                configured: configured.contains(&sink_id),
                sink_id,
                counts,
            }
        })
        .collect();

    let windows = HEALTH_WINDOWS
        .iter()
        .map(|(label, secs)| SinksHealthWindow { label, secs: *secs })
        .collect();

    Ok(Json(SinksHealthResp { windows, sinks }))
}

// ===========================================================================
// M2.2 closeout — OAuth auth-code dance for cloud cold backends.
//
// The three handlers below replace the previous "register an OAuth
// app in a sibling service and paste the refresh token here"
// step. They run end-to-end inside nexus-engine + the core-next UI:
//
//   POST /api/v1/admin/oauth/{provider}/start       (admin-gated)
//   GET  /api/v1/admin/oauth/{provider}/callback    (state-gated)
//   GET  /api/v1/admin/oauth/status?state=...       (admin-gated)
//
// `start` stashes the form fields in an in-memory cache, returns
// the consent URL the UI must `window.open`. The provider redirects
// the popup to `callback`, which exchanges the auth code for a
// refresh token, encrypts it, and writes the backend row in the
// same shape `put_storage_backend` would have. The UI polls
// `status` until it sees `Complete { handle }`, then reloads its
// backend list.
//
// The OAuth primitives (auth URL builder + code-exchange POST)
// live in `nexus-storage-cloud::oauth`; this module is just the
// HTTP glue + state machine.
// ===========================================================================

#[derive(serde::Deserialize)]
struct OAuthStartReq {
    /// Backend handle the resulting `storage_backends` row will
    /// take. Must satisfy `^[a-z0-9][a-z0-9_-]*$` and not be the
    /// reserved name `local`.
    handle: String,
    client_id: String,
    client_secret: String,
    /// Operator-visible label surfaced in the admin UI's "connect
    /// status" string. Free-form but typically the email of the
    /// account that will consent in the popup.
    account_email: Option<String>,
    /// gdrive-only optional knob. Ignored for onedrive.
    root_folder_id: Option<String>,
    /// Where the provider must redirect the browser after consent.
    /// MUST match a redirect URI registered on the OAuth app at
    /// the provider AND must end in the engine's own
    /// `/api/v1/admin/oauth/{provider}/callback` path. The UI
    /// computes this from `location.origin` so dev / staging /
    /// prod all work without a config knob.
    redirect_uri: String,
}

#[derive(serde::Serialize)]
struct OAuthStartResp {
    authorize_url: String,
    state: String,
    expires_in_secs: u64,
}

async fn start_oauth(
    State(s): State<ApiState>,
    Path(provider_str): Path<String>,
    Json(req): Json<OAuthStartReq>,
) -> Result<Json<OAuthStartResp>, ApiError> {
    // 1. Provider validation. Anything other than the two known
    //    strings is a 404 so callers don't accidentally rely on
    //    the engine to discover provider names.
    let provider = nexus_storage_cloud::Provider::from_kind(&provider_str).ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            format!("unknown OAuth provider '{provider_str}' — supported: gdrive, onedrive"),
        )
    })?;

    // 2. Handle validation. Same regex as `put_storage_backend` so
    //    the caller can't smuggle "../" or `local` past us. We
    //    reject early before stashing in the cache to keep error
    //    surfaces small.
    let handle = req.handle.trim().to_string();
    if !is_valid_handle(&handle) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "handle must match ^[a-z0-9][a-z0-9_-]*$ and not be 'local'".to_string(),
        ));
    }

    // 3. Required-field validation. We don't want to send a
    //    consent URL the operator will only discover is broken
    //    after the popup opens.
    if req.client_id.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "client_id is required".to_string(),
        ));
    }
    if req.client_secret.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "client_secret is required".to_string(),
        ));
    }
    if !req.redirect_uri.ends_with(&format!(
        "/api/v1/admin/oauth/{}/callback",
        provider.as_str()
    )) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "redirect_uri must end with '/api/v1/admin/oauth/{}/callback'",
                provider.as_str()
            ),
        ));
    }
    // Admin-secret presence is a hard precondition: without it the
    // callback can't encrypt the refresh token before persisting.
    // Fail fast in `start` so the operator sees a clean error in
    // the form rather than after consent.
    if s.admin_auth.admin_secret().is_none() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "cloud backend OAuth requires auth.admin_secret_path to be configured \
             (used to encrypt the OAuth refresh token at rest)"
                .to_string(),
        ));
    }

    // 4. Mint state + stash the session.
    let state_token = nexus_storage_cloud::new_state();
    let session = crate::oauth_sessions::PendingSession {
        provider: provider.as_str().to_string(),
        handle: handle.clone(),
        client_id: req.client_id.trim().to_string(),
        client_secret: req.client_secret,
        account_email: req
            .account_email
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty()),
        root_folder_id: req
            .root_folder_id
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty()),
        redirect_uri: req.redirect_uri.clone(),
        created_at: std::time::Instant::now(),
        status: crate::oauth_sessions::SessionStatus::Pending,
    };
    s.oauth_sessions.insert(state_token.clone(), session);

    // 5. Build the consent URL + return.
    let authorize_url = nexus_storage_cloud::authorize_url(
        provider,
        req.client_id.trim(),
        &req.redirect_uri,
        &state_token,
    );

    tracing::info!(
        provider = provider.as_str(),
        handle = %handle,
        "OAuth auth-code flow: started; awaiting callback"
    );

    Ok(Json(OAuthStartResp {
        authorize_url,
        state: state_token,
        expires_in_secs: crate::oauth_sessions::SESSION_TTL.as_secs(),
    }))
}

#[derive(serde::Deserialize)]
struct OAuthCallbackQuery {
    /// Set on success. Present in 100% of successful redirects
    /// from both Google and Microsoft.
    code: Option<String>,
    /// Always present (in both success and error redirects) —
    /// echoed back verbatim from the consent URL. We use this to
    /// look up the matching pending session.
    state: Option<String>,
    /// Set when the operator clicked "Cancel" or the provider
    /// refused for any reason (e.g. consent screen
    /// misconfiguration). Surface to the UI as a session Error.
    error: Option<String>,
    /// Optional verbose description from the provider. Logged but
    /// NOT shown to the operator (often contains internal URLs).
    error_description: Option<String>,
}

/// Returns an HTML page the operator's popup tab renders after
/// consent. Always status 200 — the browser landed here via a
/// trusted redirect from the provider, and any error in our
/// processing is communicated through the page body + the
/// matching `oauth_status` poll.
async fn oauth_callback(
    State(s): State<ApiState>,
    Path(provider_str): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(q): Query<OAuthCallbackQuery>,
) -> Response {
    // The callback runs without an admin bearer; the unguessable
    // `state` token is the proof-of-authorisation. Every branch
    // below must therefore validate the state before touching
    // anything else.
    let Some(state_token) = q.state else {
        return oauth_html_response(
            "Missing state",
            "OAuth callback missing required `state` parameter.",
            false,
        );
    };
    let Some(mut session) = s.oauth_sessions.get(&state_token) else {
        return oauth_html_response(
            "Unknown or expired session",
            "This OAuth session is unknown to the engine. It may have expired (10 min) or already been used. \
             Retry the Connect button in the Storage Admin tab.",
            false,
        );
    };

    let provider = match nexus_storage_cloud::Provider::from_kind(&provider_str) {
        Some(p) => p,
        None => {
            let msg = format!("unknown provider '{provider_str}'");
            s.oauth_sessions.set_status(
                &state_token,
                crate::oauth_sessions::SessionStatus::Error {
                    message: msg.clone(),
                },
            );
            return oauth_html_response("Unknown provider", &msg, false);
        }
    };

    // Defence-in-depth: the operator could in principle hand-edit
    // the redirect_uri before consent to land on the *other*
    // provider's callback route. The state token would still
    // resolve, but the kind would mismatch. Reject early before
    // we try to exchange a Drive code at a Microsoft token
    // endpoint (or vice versa).
    if session.provider != provider.as_str() {
        let msg = format!(
            "provider mismatch: session was started for '{}', callback hit '{}'",
            session.provider,
            provider.as_str()
        );
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Provider mismatch", &msg, false);
    }

    // Provider-side error (operator clicked Cancel, scope
    // mismatch, etc.). Mark the session Error and short-circuit
    // before any token-endpoint round-trip.
    if let Some(err_code) = q.error.as_deref() {
        let desc = q.error_description.as_deref().unwrap_or("");
        tracing::warn!(
            provider = provider.as_str(),
            handle = %session.handle,
            error_code = %err_code,
            error_desc = %desc,
            "OAuth callback: provider returned error"
        );
        let msg = format!("OAuth provider returned error: {err_code}");
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Consent declined or failed", &msg, false);
    }

    let Some(code) = q.code else {
        let msg =
            "OAuth callback missing both `code` and `error` — provider misbehaved".to_string();
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Malformed callback", &msg, false);
    };

    // Exchange the code for a refresh + access token pair.
    // `exchange_code` builds its own short-lived reqwest client
    // internally (20 s timeout) so the engine doesn't depend on
    // reqwest. Already maps every error to BackendError so we can
    // surface a clean status string without leaking the
    // provider's raw body.
    let tokens = match nexus_storage_cloud::exchange_code(
        provider,
        &code,
        &session.redirect_uri,
        &session.client_id,
        &session.client_secret,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("token exchange failed: {e}");
            tracing::warn!(
                provider = provider.as_str(),
                handle = %session.handle,
                error = %e,
                "OAuth callback: exchange_code failed"
            );
            s.oauth_sessions.set_status(
                &state_token,
                crate::oauth_sessions::SessionStatus::Error {
                    message: msg.clone(),
                },
            );
            return oauth_html_response("Token exchange failed", &msg, false);
        }
    };

    // Build the same JSON shape `put_storage_backend` accepts and
    // route through the existing encrypt + upsert pipeline.
    // Centralising on `put_storage_backend`'s helpers keeps the
    // crypto + audit + bus-publish surface in one place.
    let mut extra = serde_json::Map::new();
    if matches!(provider, nexus_storage_cloud::Provider::Gdrive) {
        if let Some(root) = session.root_folder_id.clone() {
            extra.insert(
                "root_folder_id".to_string(),
                serde_json::Value::String(root),
            );
        }
    }
    let mut config = serde_json::json!({
        "client_id": session.client_id,
        "client_secret": session.client_secret,
        "refresh_token": tokens.refresh_token,
        "account_email": session.account_email,
        "extra": serde_json::Value::Object(extra),
    });

    if let Err(e) = encrypt_cloud_refresh_token_in_place(&s, &mut config) {
        let msg = format!("refresh-token encryption failed: {}", e.1);
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Persist failed", &msg, false);
    }

    let config_json = config.to_string();
    let kind = provider.as_str();
    if let Err(e) = build_any_backend(
        &session.handle,
        kind,
        &config_json,
        s.admin_auth.admin_secret(),
    ) {
        let msg = format!("invalid backend config after exchange: {e}");
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Probe failed", &msg, false);
    }
    if let Err(e) = s
        .store
        .upsert_storage_backend(&session.handle, kind, &config_json)
        .await
    {
        let msg = format!("upsert failed: {e}");
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Persist failed", &msg, false);
    }
    if let Err(e) = rebuild_registry(&s).await {
        let msg = format!("registry rebuild failed: {}", e.1);
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Persist failed", &msg, false);
    }

    let audited = redacted_config_for_audit(&config);
    let resource_id = format!("{kind}/{}", session.handle);
    let after_str = serde_json::to_string(&serde_json::json!({
        "handle": session.handle,
        "kind": kind,
        "config": audited,
        "scope": tokens.scope,
    }))
    .ok();
    // No SessionContext: this endpoint runs outside the admin
    // gate (state token is the proof-of-authorisation). Helper
    // will record actor as `system:unknown`. Audit failures are
    // logged but do not fail the request.
    crate::auth::admin_audit::audit_admin_action(
        &s.store,
        None,
        &headers,
        peer.ip(),
        "oauth.callback",
        "admin/oauth",
        Some(resource_id.as_str()),
        nexus_store::audit::AuditOutcome::Success,
        None,
        after_str.as_deref(),
    )
    .await;
    let _ = s
        .bus
        .publish(
            topic::STORAGE_BACKENDS_CHANGED,
            &serde_json::json!({ "reason": "backend_oauth_completed", "handle": session.handle }),
        )
        .await;

    tracing::info!(
        provider = kind,
        handle = %session.handle,
        "OAuth auth-code flow: completed; backend persisted"
    );

    // Mark the session Complete so the UI's status poll picks it
    // up on the next tick.
    session.status = crate::oauth_sessions::SessionStatus::Complete {
        handle: session.handle.clone(),
    };
    s.oauth_sessions.set_status(
        &state_token,
        crate::oauth_sessions::SessionStatus::Complete {
            handle: session.handle.clone(),
        },
    );

    oauth_html_response(
        "Connected",
        &format!(
            "Backend `{}` has been connected. You can close this window — \
             the Storage Admin tab will refresh automatically.",
            session.handle
        ),
        true,
    )
}

/// Build the self-contained HTML page returned by `oauth_callback`.
/// Black-on-white, system font, no JS beyond a 2 s `window.close()`
/// on success. `success` controls the colour of the headline
/// dot — the polling UI is the authoritative status source, this
/// page is just operator-friendly chrome.
fn oauth_html_response(title: &str, body: &str, success: bool) -> Response {
    // Trivial sanitisation: the only operator-controlled segment
    // is `body`, and we already build it from constants or
    // `Display` impls. Run a tight HTML-escape anyway so any
    // future caller can't accidentally inject markup.
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    let dot = if success { "#1e9d4f" } else { "#c0392b" };
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Nexus OAuth — {title}</title>
<style>
  body {{ font: 14px/1.4 system-ui, -apple-system, sans-serif; max-width: 480px; margin: 4rem auto; padding: 1rem; color: #222; }}
  h1 {{ font-size: 1.2rem; display: flex; align-items: center; gap: 0.5rem; }}
  .dot {{ width: 10px; height: 10px; border-radius: 50%; background: {dot}; }}
  p.muted {{ color: #888; font-size: 12px; }}
</style>
</head>
<body>
<h1><span class="dot"></span>{title}</h1>
<p>{body}</p>
<p class="muted">This window will close automatically.</p>
<script>setTimeout(function() {{ try {{ window.close(); }} catch (e) {{}} }}, 2000);</script>
</body>
</html>"#,
        title = esc(title),
        body = esc(body),
        dot = dot,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct OAuthStatusQuery {
    state: String,
}

/// Lifecycle status the polling UI sees. `pending` means the
/// callback hasn't run yet; `complete` carries the backend handle
/// the operator just minted; `error` carries a sanitised reason.
///
/// When the state isn't in the cache (either TTL'd out, never
/// existed, or was already cleared after a previous successful
/// poll) the handler returns **404 Not Found** rather than another
/// JSON variant — the UI's existing error-handling path already
/// treats a 404 here as "session expired before consent completed"
/// and surfaces a re-connect prompt. Keeping the wire format
/// strictly `pending | complete | error` mirrors the TS
/// discriminated union in `ui/src/api/types.ts::OAuthStatusResp`.
#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum OAuthStatusResp {
    Pending,
    Complete { handle: String },
    Error { message: String },
}

async fn oauth_status(
    State(s): State<ApiState>,
    Query(q): Query<OAuthStatusQuery>,
) -> Result<Json<OAuthStatusResp>, ApiError> {
    let Some(session) = s.oauth_sessions.get(&q.state) else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "oauth session not found or expired".to_string(),
        ));
    };
    let resp = match session.status {
        crate::oauth_sessions::SessionStatus::Pending => OAuthStatusResp::Pending,
        crate::oauth_sessions::SessionStatus::Complete { ref handle } => {
            // Drop the session AFTER the UI has observed the
            // Complete state so a re-poll doesn't keep returning
            // the same row. Errors stay around for the full TTL
            // so the operator's UI can re-read the message if it
            // re-mounts the tab.
            let h = handle.clone();
            s.oauth_sessions.remove(&q.state);
            OAuthStatusResp::Complete { handle: h }
        }
        crate::oauth_sessions::SessionStatus::Error { ref message } => OAuthStatusResp::Error {
            message: message.clone(),
        },
    };
    Ok(Json(resp))
}

/// Backend-handle validator shared between `put_storage_backend`'s
/// path-param check and `start_oauth`'s body-field check. Mirrors
/// the regex documented in the admin-storage UI.
fn is_valid_handle(s: &str) -> bool {
    if s == "local" || s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return false;
        }
        if c.is_ascii_uppercase() {
            return false;
        }
    }
    !s.chars().next().unwrap().is_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::parse_byte_range;

    #[test]
    fn parse_simple_range() {
        assert_eq!(parse_byte_range("bytes=0-499", 1000), Some((0, 499)));
        assert_eq!(parse_byte_range("bytes=500-999", 1000), Some((500, 999)));
    }

    #[test]
    fn parse_open_ended_range_clamps_to_file_size() {
        assert_eq!(parse_byte_range("bytes=200-", 1000), Some((200, 999)));
    }

    #[test]
    fn parse_clamps_end_to_eof() {
        assert_eq!(parse_byte_range("bytes=900-99999", 1000), Some((900, 999)));
    }

    #[test]
    fn parse_suffix_range_returns_last_n_bytes() {
        // Chrome's MP4 demuxer issues these to find trailing index
        // boxes. Refusing them breaks clip playback in the SPA.
        assert_eq!(parse_byte_range("bytes=-500", 1000), Some((500, 999)));
        assert_eq!(parse_byte_range("bytes=-1", 1000), Some((999, 999)));
    }

    #[test]
    fn parse_suffix_range_larger_than_file_returns_whole_file() {
        // RFC 7233: when suffix-length exceeds file size, return
        // the entire representation.
        assert_eq!(parse_byte_range("bytes=-5000", 1000), Some((0, 999)));
    }

    #[test]
    fn parse_rejects_zero_suffix() {
        // `bytes=-0` is invalid per RFC 7233 §2.1.
        assert!(parse_byte_range("bytes=-0", 1000).is_none());
    }

    #[test]
    fn parse_rejects_any_range_on_empty_file() {
        assert!(parse_byte_range("bytes=0-0", 0).is_none());
        assert!(parse_byte_range("bytes=-100", 0).is_none());
    }

    #[test]
    fn parse_rejects_start_past_eof() {
        assert!(parse_byte_range("bytes=2000-2500", 1000).is_none());
    }

    #[test]
    fn parse_rejects_inverted_range() {
        assert!(parse_byte_range("bytes=500-100", 1000).is_none());
    }

    // M-Admin Phase 5 — POST /api/rules/validate.
    //
    // The handler is intentionally `async fn` over `Json<T>`, so we
    // can drive it directly here without standing up a Router or a
    // Store. Both branches must round-trip 200 OK; the `ok` flag
    // (not the HTTP status) tells the UI whether the CEL parsed.

    #[tokio::test]
    async fn validate_rule_accepts_well_formed_cel() {
        let req = axum::Json(super::ValidateRuleReq {
            when: "object.label == 'person'".into(),
        });
        let resp = super::validate_rule(req).await;
        assert!(
            resp.0.ok,
            "well-formed CEL must validate; got {:?}",
            resp.0.error
        );
        assert!(resp.0.error.is_none());
    }

    #[tokio::test]
    async fn validate_rule_rejects_unclosed_paren_with_inline_error() {
        // Reliably hits the cel-interpreter `Err` path (vs the
        // panic path covered separately below).
        let req = axum::Json(super::ValidateRuleReq {
            when: "(object.label".into(),
        });
        let resp = super::validate_rule(req).await;
        assert!(!resp.0.ok, "unclosed paren must not validate");
        let err = resp.0.error.expect("error message present on !ok");
        assert!(!err.is_empty(), "compile error message must not be empty");
    }

    /// cel-interpreter's antlr4rust grammar panics on some
    /// malformed-but-balanced inputs (e.g. trailing operators,
    /// stray `@@@`). The validate endpoint must convert those to a
    /// clean `ok: false` so a single bad POST can't kill a worker.
    #[tokio::test]
    async fn validate_rule_catches_parser_panics() {
        for input in &["@@@", "object.label ===", "&&&"] {
            let req = axum::Json(super::ValidateRuleReq {
                when: (*input).to_string(),
            });
            let resp = super::validate_rule(req).await;
            assert!(
                !resp.0.ok,
                "panic-inducing input {input:?} must surface as ok=false",
            );
            assert!(
                resp.0.error.is_some(),
                "panic-inducing input {input:?} must carry an error message",
            );
        }
    }

    // POST /api/rules/preview — exercises the full HTTP round-trip
    // (router + state) because the handler reads from `ApiState`
    // (store + frame cache + cameras). Two slices:
    //   * happy path: insert a couple of motion_events; CEL filters
    //     to the one with label=='person'; matches.len() == 1.
    //   * bad CEL: 200 OK with `error` populated, `matches` empty.
    #[tokio::test]
    async fn preview_rule_matches_historical_motion_events() {
        use axum::body::to_bytes;
        use nexus_config::CameraConfig;
        use nexus_types::BBox;

        const ADMIN_SECRET: &[u8] = b"preview-rule-test-secret";
        let (app, store, _dir) = build_test_router(Some(ADMIN_SECRET)).await;

        // Camera + one open clip so motion_events FK is satisfied.
        store
            .upsert_camera(&CameraConfig {
                id: 1,
                name: "front".into(),
                ingest: nexus_config::CameraIngest {
                    url: url::Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                    enabled: true,
                    max_fps: 0,
                },
                detector: nexus_config::CameraDetector {
                    prompts: vec![],
                    visual_prompts: vec![],
                    model_override: None,
                },
                behavior: nexus_config::CameraBehavior {
                    parking_lot_mode: false,
                    anchor_ttl_secs: None,
                },
                zones: vec![],
            })
            .await
            .unwrap();
        let clip_id = store
            .open_clip(&nexus_store::NewClip {
                camera_id: 1,
                started_at: chrono::Utc::now() - chrono::Duration::minutes(5),
                hot_path: "cam1/test.mp4".into(),
                codec: "h264".into(),
                container: "mp4".into(),
                hot_handle: "local".into(),
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();

        // Two events: one matches (person), one doesn't (vehicle).
        let now = chrono::Utc::now();
        for (track, label, when_off) in [(10u64, "person", 30i64), (11, "vehicle", 20)] {
            store
                .insert_motion_event(&nexus_store::NewMotionEvent {
                    camera_id: 1,
                    clip_id,
                    track_id: track,
                    kind: nexus_store::MotionEventKind::Born,
                    captured_at: now - chrono::Duration::seconds(when_off),
                    bbox: BBox {
                        x1: 100.0,
                        y1: 100.0,
                        x2: 200.0,
                        y2: 300.0,
                    },
                    label: label.into(),
                    confidence: 0.9,
                    attributes_json: "{}".into(),
                })
                .await
                .unwrap();
        }

        let body = serde_json::json!({
            "rule": {
                "id": "p",
                "name": "p",
                "when": "object.label == 'person'",
                "severity": "low",
                "enabled": true,
            },
            "limit": 100,
        });
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/rules/preview")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            resp["error"].is_null(),
            "valid CEL must not surface an error: {resp:#}"
        );
        let matches = resp["matches"].as_array().expect("matches array");
        assert_eq!(matches.len(), 1, "exactly one person match: {resp:#}");
        assert_eq!(matches[0]["label"], "person");
        assert_eq!(matches[0]["clip_id"], clip_id);
        assert!(resp["scanned"].as_u64().unwrap() >= 2);
    }

    #[tokio::test]
    async fn preview_rule_surfaces_cel_compile_error_as_ok_with_error_field() {
        use axum::body::to_bytes;

        const ADMIN_SECRET: &[u8] = b"preview-rule-bad-cel-secret";
        let (app, _store, _dir) = build_test_router(Some(ADMIN_SECRET)).await;

        let body = serde_json::json!({
            "rule": {
                "id": "p",
                "name": "p",
                "when": "(object.label",
                "severity": "low",
                "enabled": true,
            }
        });
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/rules/preview")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        // 200 OK + error in body — mirrors /rules/validate posture
        // so the form can render the parser message inline.
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            resp["error"].is_string(),
            "bad CEL must surface error: {resp:#}"
        );
        assert_eq!(resp["matches"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn parse_rejects_unknown_unit() {
        assert!(parse_byte_range("items=0-9", 1000).is_none());
    }

    #[test]
    fn parse_takes_only_first_of_multi_range() {
        // Multi-range gets the first range and drops the rest.
        assert_eq!(parse_byte_range("bytes=0-99,200-299", 1000), Some((0, 99)));
    }

    // ===============================================================
    // M2.2 Phase 4 — soft-evicted (cold-only) playback integration.
    //
    // We exercise [`super::serve_from_cold_inner`] directly so the
    // test doesn't have to fake the unrelated parts of `ApiState`
    // (recorder, frame cache, bus). Coverage:
    //
    // * cold-only clip with no Range header → 200 OK + full bytes
    // * cold-only clip WITH Range header     → 206 + correct slice
    //   + Content-Range header
    // * Reading a cold-only clip schedules a hot rehydrate
    //   (CacheJobs::inflight_count rises to 1, then drains)
    // ===============================================================

    use super::serve_from_cold_inner;
    use crate::cold_read_cache::CacheJobs;
    use crate::storage_safety::{WatermarkLevel, WatermarkSignal};
    use async_trait::async_trait;
    use axum::body::to_bytes;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use chrono::Utc;
    use nexus_config::{CameraConfig, StoreConfig};
    use nexus_storage::{
        BackendError, ColdBackend, HealthStatus, PutReceipt, Registry, VolumeInfo,
    };
    use nexus_store::{ClipClose, ClipColdMark, NewClip, Store};
    use parking_lot::Mutex;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use url::Url;

    /// Mock cold backend mirrored from `cold_read_cache::tests`. Kept
    /// inline here so the test doesn't depend on a sibling test
    /// module's private types.
    struct ColdBackendStub {
        handle: String,
        store: Mutex<HashMap<String, Vec<u8>>>,
    }
    impl ColdBackendStub {
        fn new(handle: &str) -> Arc<Self> {
            Arc::new(Self {
                handle: handle.into(),
                store: Mutex::new(HashMap::new()),
            })
        }
        fn put_bytes(&self, path: &str, bytes: Vec<u8>) {
            self.store.lock().insert(path.into(), bytes);
        }
    }
    #[async_trait]
    impl ColdBackend for ColdBackendStub {
        fn handle(&self) -> &str {
            &self.handle
        }
        fn kind(&self) -> &str {
            "lan"
        }
        async fn put(
            &self,
            _path: &str,
            _bytes: &[u8],
            _expected_sha256: &str,
        ) -> Result<PutReceipt, BackendError> {
            unreachable!()
        }
        async fn get_range(
            &self,
            path: &str,
            start: u64,
            end_inclusive: u64,
        ) -> Result<Vec<u8>, BackendError> {
            let b = self
                .store
                .lock()
                .get(path)
                .cloned()
                .ok_or_else(|| BackendError::Other(format!("no such path {path}")))?;
            let s = start as usize;
            let e = (end_inclusive as usize + 1).min(b.len());
            Ok(b[s..e].to_vec())
        }
        async fn delete(&self, _path: &str) -> Result<bool, BackendError> {
            unreachable!()
        }
        async fn exists(&self, _path: &str, _expected_sha256: &str) -> Result<bool, BackendError> {
            Ok(true)
        }
        async fn volume_info(&self) -> Result<VolumeInfo, BackendError> {
            Ok(VolumeInfo {
                free_bytes: Some(1 << 30),
                total_bytes: Some(1 << 31),
                used_bytes: Some(1 << 30),
            })
        }
        async fn health(&self) -> HealthStatus {
            HealthStatus::Ok
        }
    }

    /// Seed a cold-only clip and return everything the test needs.
    /// Mirrors `cold_read_cache::tests::seed_soft_evicted` but kept
    /// inline so cross-module test imports aren't required.
    async fn seed_cold_only(
        bytes: Vec<u8>,
    ) -> (Arc<Store>, Registry, PathBuf, i64, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nexus.db");
        let store = Arc::new(
            Store::open(&StoreConfig {
                url: format!("sqlite:{}?mode=rwc", db_path.display()),
                seed_from_config: false,
                duckdb_attach: false,
                duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
            })
            .await
            .unwrap(),
        );
        let clips_dir = dir.path().join("clips");
        tokio::fs::create_dir_all(clips_dir.join("3"))
            .await
            .unwrap();

        store
            .upsert_camera(&CameraConfig {
                id: 3,
                name: "cam3".into(),
                ingest: nexus_config::CameraIngest {
                    url: Url::parse("rtsp://127.0.0.1/stream3").unwrap(),
                    enabled: true,
                    max_fps: 0,
                },
                detector: nexus_config::CameraDetector {
                    prompts: vec![],
                    visual_prompts: vec![],
                    model_override: None,
                },
                behavior: nexus_config::CameraBehavior {
                    parking_lot_mode: false,
                    anchor_ttl_secs: None,
                },
                zones: vec![],
            })
            .await
            .unwrap();
        store
            .upsert_storage_backend("api-mock", "lan", "{\"root\":\"/tmp/api-mock\"}")
            .await
            .unwrap();

        let now = Utc::now();
        let rel = "3/clip_0001.mp4".to_string();
        let clip_id = store
            .open_clip(&NewClip {
                camera_id: 3,
                started_at: now - chrono::Duration::seconds(30),
                hot_path: rel.clone(),
                codec: "h264".into(),
                container: "mp4".into(),
                hot_handle: "local".into(),
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();
        let sha256 = {
            let mut h = Sha256::new();
            h.update(&bytes);
            format!("{:x}", h.finalize())
        };
        store
            .close_clip(
                clip_id,
                &ClipClose {
                    ended_at: now,
                    duration_ms: 1000,
                    size_bytes: bytes.len() as i64,
                    hot_path: Some(rel.clone()),
                    sha256: Some(sha256),
                },
            )
            .await
            .unwrap();
        store
            .mark_cold_replicated(
                clip_id,
                &ClipColdMark {
                    cold_handle: "api-mock".into(),
                    cold_path: rel.clone(),
                    cold_uploaded_at: now,
                },
            )
            .await
            .unwrap();
        store.clear_hot_pointer(clip_id).await.unwrap();

        let backend = ColdBackendStub::new("api-mock");
        backend.put_bytes(&rel, bytes);
        let registry = Registry::new();
        registry.replace_all([backend as Arc<dyn ColdBackend>]);

        (store, registry, clips_dir, clip_id, dir)
    }

    #[tokio::test]
    async fn serve_from_cold_returns_full_body_when_no_range_header() {
        let payload = b"some-cold-bytes-from-an-evicted-clip".to_vec();
        let (store, registry, clips_dir, clip_id, _tmp) = seed_cold_only(payload.clone()).await;
        let watermark = WatermarkSignal::new();
        watermark.set(WatermarkLevel::Ok);
        let cache_jobs = CacheJobs::new(store.clone(), registry.clone(), clips_dir, watermark);
        let clip = store.get_clip(clip_id).await.unwrap().unwrap();
        let headers = HeaderMap::new();

        let resp = serve_from_cold_inner(&registry, &cache_jobs, &clip, &headers)
            .await
            .expect("serve_from_cold_inner returns Ok for a cold-only clip");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::ACCEPT_RANGES)
                .unwrap(),
            "bytes"
        );
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), payload.as_slice());

        // Rehydrate must have been kicked off (will complete async).
        // Wait briefly for it to finish.
        for _ in 0..50 {
            if cache_jobs.inflight_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // After it drains, the row should now have hot_path
        // repopulated (the rehydrate succeeded against the same
        // clips_dir + same cold_path).
        let row = store.get_clip(clip_id).await.unwrap().unwrap();
        assert!(
            row.hot_path.is_some(),
            "rehydrate fired by serve_from_cold should repopulate hot_path"
        );
    }

    #[tokio::test]
    async fn serve_from_cold_returns_partial_content_for_range_header() {
        let payload = (0..256u32).map(|i| (i & 0xff) as u8).collect::<Vec<u8>>();
        let (store, registry, clips_dir, clip_id, _tmp) = seed_cold_only(payload.clone()).await;
        let watermark = WatermarkSignal::new();
        watermark.set(WatermarkLevel::Ok);
        let cache_jobs = CacheJobs::new(store.clone(), registry.clone(), clips_dir, watermark);
        let clip = store.get_clip(clip_id).await.unwrap().unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::RANGE,
            HeaderValue::from_static("bytes=10-19"),
        );
        let resp = serve_from_cold_inner(&registry, &cache_jobs, &clip, &headers)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_RANGE)
                .unwrap(),
            &format!("bytes 10-19/{}", payload.len())
        );
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), &payload[10..=19]);
    }

    /// M2.2 perf P1.5 — a partial-range fetch must NOT trigger a
    /// rehydrate. A viewer scrubbing the timeline emits a stream
    /// of short Range requests for the same clip; pre-gate, each
    /// one started + cancelled a fresh download (doubling LAN
    /// read or cloud egress). Post-gate, the spawn is reserved
    /// for full-clip fetches.
    #[tokio::test]
    async fn serve_from_cold_partial_range_does_not_spawn_rehydrate() {
        let payload = (0..256u32).map(|i| (i & 0xff) as u8).collect::<Vec<u8>>();
        let (store, registry, clips_dir, clip_id, _tmp) = seed_cold_only(payload.clone()).await;
        let watermark = WatermarkSignal::new();
        watermark.set(WatermarkLevel::Ok);
        let cache_jobs = CacheJobs::new(store.clone(), registry.clone(), clips_dir, watermark);
        let clip = store.get_clip(clip_id).await.unwrap().unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::RANGE,
            HeaderValue::from_static("bytes=10-19"),
        );
        let resp = serve_from_cold_inner(&registry, &cache_jobs, &clip, &headers)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        // Drain the body so the range read actually completes.
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        // Give any (incorrect) spawned rehydrate a chance to land.
        // If the gate is doing its job, inflight_count() stays at 0
        // and the hot_path stays NULL.
        for _ in 0..20 {
            if cache_jobs.inflight_count() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            cache_jobs.inflight_count(),
            0,
            "partial-range fetches must not spawn a rehydrate job"
        );
        let row = store.get_clip(clip_id).await.unwrap().unwrap();
        assert!(
            row.hot_path.is_none(),
            "partial-range fetches must not repopulate the hot pointer"
        );
    }

    // ===============================================================
    // M2.2 closeout — admin-auth gate + refresh-token at-rest sweep
    // ===============================================================
    //
    // These two tests use the full axum `router(state)` (not the
    // inner handler functions used above) because we specifically
    // want to exercise the middleware tower-layer that gates admin
    // writes.

    use crate::admin_auth::AdminAuthState;
    use crate::usb_watch::UsbRegistry;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Method, Request};
    use jsonwebtoken::{encode as jwt_encode, Algorithm, EncodingKey, Header};
    use nexus_bus::BroadcastBus;
    use nexus_pipeline::LatestFrameCache;
    use std::net::{Ipv4Addr, SocketAddr};
    use tower::ServiceExt;

    /// Build a minimal but real [`super::ApiState`] backed by a
    /// fresh in-tempdir SQLite + stub recorder. Returns the
    /// constructed router, the underlying store handle (so tests
    /// can introspect persisted rows), and the tempdir keep-alive
    /// guard.
    async fn build_test_router(
        admin_secret: Option<&[u8]>,
    ) -> (axum::Router, Arc<Store>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nexus.db");
        let store = Arc::new(
            Store::open(&StoreConfig {
                url: format!("sqlite:{}?mode=rwc", db_path.display()),
                seed_from_config: false,
                duckdb_attach: false,
                duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
            })
            .await
            .unwrap(),
        );
        let clips_dir = dir.path().join("clips");
        tokio::fs::create_dir_all(&clips_dir).await.unwrap();

        let bus: Arc<dyn nexus_bus::Bus> = Arc::new(BroadcastBus::new(8));
        let cache = Arc::new(LatestFrameCache::new());
        let recorder: Arc<dyn nexus_pipeline::ClipRecorder> = Arc::new(
            nexus_pipeline::StubClipRecorder::new(store.clone(), &clips_dir),
        );
        let registry = Registry::new();
        let watermark = WatermarkSignal::new();
        watermark.set(WatermarkLevel::Ok);
        let cache_jobs = CacheJobs::new(
            store.clone(),
            registry.clone(),
            clips_dir.clone(),
            watermark,
        );
        let usb_registry = UsbRegistry::new();
        let preferred_usb_label = nexus_pipeline::recorder::PreferredUsbLabel::new(None);
        let admin_auth = Arc::new(AdminAuthState::from_secret_bytes(admin_secret, false));
        let rules_cfg = nexus_config::RulesConfig {
            backend: nexus_config::RulesBackendKind::Cel,
            inline: vec![],
        };
        let evaluator = Arc::new(
            nexus_rules::RuleEvaluator::new(&rules_cfg, &[])
                .expect("empty rule set always compiles"),
        );
        let state = super::ApiState {
            store: store.clone(),
            bus,
            current_bind: "127.0.0.1:0".into(),
            current_ui_bind: None,
            evaluator,
            cache,
            frame_stats: Arc::new(nexus_pipeline::FrameStatsRegistry::new()),
            pool: None,
            ui_root: dir.path().join("ui-unused"),
            recorder,
            clips_dir,
            low_watermark_pct: 5,
            panic_watermark_pct: 2,
            registry,
            cache_jobs,
            usb_registry,
            preferred_usb_label,
            admin_auth,
            oauth_sessions: crate::oauth_sessions::OAuthSessions::new(),
            discovery_sessions: crate::discovery::DiscoverySessions::new(),
            // Empty catalog — tests don't exercise
            // `GET /v1/models/prompts`; the engine integration
            // tests cover it end-to-end.
            model_prompts: Arc::new(crate::models_catalog::ModelPromptsCatalog {
                default_kind: "mock".into(),
                kinds: vec![],
                by_kind: std::collections::BTreeMap::new(),
            }),
            // M7 Step 6 — empty sink registry by default; the
            // delivery-policy tests below populate it when they
            // exercise the sinks-health endpoint.
            sink_registry: Arc::new(nexus_sinks::SinkRegistry::new()),
            // M6 — default LockoutConfig is fine for every test
            // here; tests that exercise the lockout boundary
            // override via a per-test mutation of `state` before
            // building the app.
            lockout: nexus_config::LockoutConfig::default(),
            // M-Admin Phase 0 closeout — the legacy `None` and
            // `DevToken` variants were retired. Tests that need
            // a "no external state" mode now sit on `Local`;
            // tests that exercise OIDC override this on the
            // constructed state before building the app.
            auth_mode: nexus_config::AuthMode::Local,
            // M6 Phase 3 Step 3.3 — no OIDC backend in unit
            // tests; the dedicated oidc_login tests live in
            // their own module against a wiremock IdP.
            oidc_login: None,
            oidc_display_name: None,
            // M3.1 Phase H — tests don't exercise the
            // visual-prompts upload path; default state with no
            // encoder model path means `POST /visual-prompts`
            // would 503 if a test ever hit it.
            visual_prompts: crate::visual_prompts_admin::VisualPromptsAdminState {
                visual_prompts_dir: dir.path().join("visual_prompts"),
                encoder_model_path: None,
                encoder_model_id: "test-encoder".to_string(),
                encoder_embedding_dim: 4,
                encoder_ep_priority: vec![],
                #[cfg(feature = "ort")]
                encoder: std::sync::Arc::new(tokio::sync::OnceCell::new()),
            },
            // M-Admin Phase 0 follow-up — tests don't exercise
            // the `GET /v1/admin/server/inference` endpoint;
            // default ModelConfig is fine.
            current_inference_model: std::sync::Arc::new(nexus_config::ModelConfig::default()),
            // Static-anchors handler reads `<state_dir>/static_objects/cam-<id>.json`;
            // tests don't write that file so the endpoint returns
            // an empty anchor list, which is the documented behaviour
            // for "no persisted map yet".
            state_dir: dir.path().to_path_buf(),
            // No supervisors running under the unit-test app, so
            // the clear registry is just a satisfied dependency —
            // bumping it is a no-op without a polling consumer.
            static_clear: nexus_pipeline::StaticAnchorClearRegistry::new(),
            // Engine-wide default surfaced by
            // `GET /api/v1/system/static-object-defaults`. Matches
            // `nexus_config::default_static_object_anchor_ttl_secs`.
            default_anchor_ttl_secs: 3600,
            // M-Admin Network — unit tests don't drive the
            // network plan/apply endpoints; an empty registry is
            // the documented "no apply in flight" state.
            network_apply: crate::network::apply::ApplyRegistry::new(),
            // M-Cloud Phase 1.16 — unit tests don't drive the
            // tunnel supervisor; a stand-alone Notify is the
            // satisfied-dependency form (nothing observes it).
            cloud_enrollment_changed: Arc::new(tokio::sync::Notify::new()),
        };
        let app = super::router(state);
        (app, store, dir)
    }

    fn sign_admin_jwt(secret: &[u8]) -> String {
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60;
        let claims = serde_json::json!({ "exp": exp, "sub": "nexus-admin-test" });
        jwt_encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap()
    }

    fn remote_peer() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::new(10, 0, 0, 5), 51234))
    }

    fn loopback_peer() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 8089))
    }

    /// Sanity check: with an admin secret configured, a write
    /// request that lacks `Authorization: Bearer ...` is rejected
    /// with 401 — even from a loopback peer. (When a secret is
    /// configured the bearer is mandatory; loopback bypass only
    /// applies in the no-secret fallback path.)
    #[tokio::test]
    async fn admin_write_without_bearer_returns_401() {
        let (app, _store, _dir) = build_test_router(Some(b"shared-admin-secret-xyz")).await;
        let mut req = Request::builder()
            .method(Method::PUT)
            .uri("/api/v1/admin/storage/backends/foo")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"kind":"lan","config":{"root":"/tmp/x"}}).to_string(),
            ))
            .unwrap();
        // Even from loopback: secret is configured so JWT is mandatory.
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// Same as above but from a remote peer, just to confirm the
    /// non-loopback path also rejects (i.e. the gate isn't accidentally
    /// short-circuited by the loopback check).
    #[tokio::test]
    async fn admin_write_from_remote_without_bearer_returns_401() {
        let (app, _store, _dir) = build_test_router(Some(b"shared-admin-secret-xyz")).await;
        let mut req = Request::builder()
            .method(Method::PUT)
            .uri("/api/v1/admin/storage/backends/foo")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"kind":"lan","config":{"root":"/tmp/x"}}).to_string(),
            ))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(remote_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// The critical at-rest test: PUT a cloud backend with a
    /// sentinel cleartext refresh token, then SELECT the persisted
    /// `config_json` and confirm the sentinel does not appear. This
    /// catches the regression where a future refactor accidentally
    /// stores the cleartext token in the DB.
    #[tokio::test]
    async fn cloud_backend_put_encrypts_refresh_token_at_rest() {
        const ADMIN_SECRET: &[u8] = b"refresh-token-at-rest-sweep-secret";
        const SENTINEL: &str = "SENTINEL_PLAINTEXT_REFRESH_TOKEN_xyz123";
        let (app, store, _dir) = build_test_router(Some(ADMIN_SECRET)).await;
        let token = sign_admin_jwt(ADMIN_SECRET);
        let body = serde_json::json!({
            "kind": "gdrive",
            "config": {
                "client_id": "test-client-id",
                "client_secret": "test-client-secret",
                "refresh_token": SENTINEL,
                "account_email": "ops@example.com",
                "extra": { "root_folder_id": null }
            }
        });
        let mut req = Request::builder()
            .method(Method::PUT)
            .uri("/api/v1/admin/storage/backends/gdrive-vault")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "PUT should succeed (build_any_backend probe constructs a backend; no network IO at probe time)"
        );

        let rows = store.list_storage_backends().await.unwrap();
        let row = rows
            .iter()
            .find(|r| r.handle == "gdrive-vault")
            .expect("gdrive-vault row must exist after PUT");
        assert!(
            !row.config_json.contains(SENTINEL),
            "persisted config_json contains the sentinel cleartext refresh token!\n\
             config_json was: {}",
            row.config_json
        );
        // Belt-and-braces: confirm the persisted config has an
        // encrypted-shape `refresh_token` (object with alg + nonce +
        // ct fields per nexus_storage::token_crypto::EncryptedToken),
        // not a string.
        let cfg: serde_json::Value = serde_json::from_str(&row.config_json).unwrap();
        let rt = cfg
            .get("refresh_token")
            .expect("config has refresh_token field");
        assert!(
            rt.is_object(),
            "refresh_token should be a serialized EncryptedToken object, not a string"
        );
        assert_eq!(
            rt.get("alg").and_then(|v| v.as_str()),
            Some("AES-256-GCM"),
            "EncryptedToken.alg should mark AES-256-GCM v1"
        );
        assert!(
            rt.get("nonce").and_then(|v| v.as_str()).is_some(),
            "EncryptedToken should have a base64 nonce field"
        );
        assert!(
            rt.get("ct").and_then(|v| v.as_str()).is_some(),
            "EncryptedToken should have a base64 ct (ciphertext) field"
        );
    }

    // ===============================================================
    // M-Admin Phase 1B regression — discovery route layout
    // ===============================================================
    //
    // Pre-fix the four discovery routes lived in a single path
    // slot under `/v1/admin/discovery/`: two literals (`onvif`,
    // `scan`) plus a same-depth `{session_id}` GET. axum's matchit
    // routed `POST /v1/admin/discovery/onvif` to the param entry
    // (which only had GET) and returned 405, silently breaking the
    // Discover dialog. Moving the param routes under a `sessions/`
    // prefix eliminates the overlap; these tests pin that down.

    #[tokio::test]
    async fn discovery_onvif_post_does_not_405() {
        const ADMIN_SECRET: &[u8] = b"discovery-route-regression-secret";
        let (app, _store, _dir) = build_test_router(Some(ADMIN_SECRET)).await;
        let token = sign_admin_jwt(ADMIN_SECRET);
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/admin/discovery/onvif")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from("{}"))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        // We don't care whether the probe actually starts (it spawns
        // a multicast socket which may or may not work in the test
        // sandbox); we ONLY care that the router doesn't 405 us.
        assert_ne!(
            res.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "POST /v1/admin/discovery/onvif must not 405 — \
             the literal `onvif` was being shadowed by a sibling \
             {{session_id}} param route. See api::router() docs."
        );
    }

    #[tokio::test]
    async fn discovery_scan_post_does_not_405() {
        const ADMIN_SECRET: &[u8] = b"discovery-route-regression-secret-2";
        let (app, _store, _dir) = build_test_router(Some(ADMIN_SECRET)).await;
        let token = sign_admin_jwt(ADMIN_SECRET);
        let body = serde_json::json!({ "cidr": "192.168.99.0/30" }).to_string();
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/admin/discovery/scan")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_ne!(
            res.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "POST /v1/admin/discovery/scan must not 405"
        );
    }

    #[tokio::test]
    async fn discovery_session_get_is_under_sessions_prefix() {
        // Just confirm the GET path that the client polls is
        // reachable. With no such session we expect 404 (not 405).
        const ADMIN_SECRET: &[u8] = b"discovery-route-regression-secret-3";
        let (app, _store, _dir) = build_test_router(Some(ADMIN_SECRET)).await;
        let token = sign_admin_jwt(ADMIN_SECRET);
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/admin/discovery/sessions/00000000-0000-0000-0000-000000000000")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::NOT_FOUND,
            "unknown session id should be NOT_FOUND, not 405/405"
        );
    }

    // -------------------------------------------------------------------
    // M7 Step 6 — delivery-policy admin surface tests.
    //
    // Each test stands up its own router so the inserted rows
    // (rules, motion_events, outbox entries) don't leak. We exercise
    // the HTTP round-trip rather than the handler fn directly so the
    // admin auth gate, the route layout, and the JSON shapes are all
    // covered end-to-end.
    // -------------------------------------------------------------------

    /// GET /api/v1/admin/delivery returns the seeded singleton row
    /// (migration 0007 seeds `enabled=1, schedule=null, tz=UTC`).
    #[tokio::test]
    async fn admin_delivery_get_returns_seeded_defaults() {
        use axum::body::to_bytes;
        const SECRET: &[u8] = b"m7-admin-delivery-get-secret";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let token = sign_admin_jwt(SECRET);
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/admin/delivery")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["enabled"], serde_json::Value::Bool(true));
        assert_eq!(v["timezone"], serde_json::Value::String("UTC".into()));
        assert!(v["schedule"].is_null());
    }

    /// PUT /api/v1/admin/delivery writes a 7×48 schedule, then the
    /// follow-up GET reads it back. Round-trips through SQLite.
    #[tokio::test]
    async fn admin_delivery_put_then_get_round_trip() {
        use axum::body::to_bytes;
        const SECRET: &[u8] = b"m7-admin-delivery-put-secret";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let token = sign_admin_jwt(SECRET);

        // Build a never-schedule then flip a single slot true so we
        // can detect a misindex in the round-trip.
        let mut grid = vec![vec![false; 48]; 7];
        grid[3][10] = true; // Thursday, 05:00–05:30
        let put_body = serde_json::json!({
            "enabled": true,
            "timezone": "America/Los_Angeles",
            "schedule": { "grid": grid },
        });
        let mut req = Request::builder()
            .method(Method::PUT)
            .uri("/api/v1/admin/delivery")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(put_body.to_string()))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Now GET and confirm.
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/admin/delivery")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["enabled"], serde_json::Value::Bool(true));
        assert_eq!(
            v["timezone"],
            serde_json::Value::String("America/Los_Angeles".into())
        );
        assert_eq!(v["schedule"]["grid"][3][10], serde_json::Value::Bool(true));
        assert_eq!(v["schedule"]["grid"][3][9], serde_json::Value::Bool(false));
    }

    /// PUT /api/v1/admin/delivery rejects an unknown IANA tz with a
    /// 400 so the operator catches typos at form-submit time
    /// (otherwise the policy silently falls back to UTC).
    #[tokio::test]
    async fn admin_delivery_put_rejects_unknown_timezone() {
        const SECRET: &[u8] = b"m7-admin-delivery-bad-tz-secret";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let token = sign_admin_jwt(SECRET);
        let body = serde_json::json!({
            "enabled": true,
            "timezone": "Mars/Olympus_Mons",
            "schedule": null,
        });
        let mut req = Request::builder()
            .method(Method::PUT)
            .uri("/api/v1/admin/delivery")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// Admin gate: PUT without a bearer is 401. The handler must
    /// never run; we don't bother checking the body.
    #[tokio::test]
    async fn admin_delivery_put_requires_bearer() {
        const SECRET: &[u8] = b"m7-admin-delivery-gate-secret";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let mut req = Request::builder()
            .method(Method::PUT)
            .uri("/api/v1/admin/delivery")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"enabled": false}).to_string(),
            ))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// GET /api/v1/rules/{id}/delivery for a rule that has no
    /// override returns `inherited=true` with `policy=null` and an
    /// `effective` block resolved from global. Default seed:
    /// enabled=true, schedule=null → effective enabled=true,
    /// schedule=null.
    #[tokio::test]
    async fn rule_delivery_get_inherits_global_when_no_override() {
        use axum::body::to_bytes;
        const SECRET: &[u8] = b"m7-rule-delivery-inherit-secret";
        let (app, store, _dir) = build_test_router(Some(SECRET)).await;

        // Need an actual rule row so the FK check on
        // rule_delivery_policy_put would pass — but the GET path
        // also works for rule ids that don't exist (treated as
        // "inherit global"). Insert one anyway so the test mirrors
        // the realistic flow.
        store
            .upsert_rule(&nexus_config::RuleConfig {
                id: "rule_test".into(),
                name: "test rule".into(),
                predicate: nexus_config::RulePredicate {
                    when: "true".into(),
                    severity: "low".into(),
                },
                gates: nexus_config::RuleGates::default(),
                debounce: nexus_config::RuleDebounce {
                    min_track_age_ms: 0,
                    consecutive_frames: 1,
                    cooldown_ms: 0,
                },
                enabled: true,
            })
            .await
            .unwrap();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/rules/rule_test/delivery")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["policy"].is_null(), "no override → policy must be null");
        assert_eq!(v["inherited"], serde_json::Value::Bool(true));
        assert_eq!(v["effective"]["enabled"], serde_json::Value::Bool(true));
        assert!(v["effective"]["schedule"].is_null());
    }

    /// PUT /api/v1/rules/{id}/delivery with a payload, then GET
    /// reads it back. `inherited` flips to false. The rule's own
    /// schedule REPLACES the (still-null) global one.
    #[tokio::test]
    async fn rule_delivery_put_then_get_round_trip() {
        use axum::body::to_bytes;
        const SECRET: &[u8] = b"m7-rule-delivery-put-secret";
        let (app, store, _dir) = build_test_router(Some(SECRET)).await;
        store
            .upsert_rule(&nexus_config::RuleConfig {
                id: "rule_put".into(),
                name: "put".into(),
                predicate: nexus_config::RulePredicate {
                    when: "true".into(),
                    severity: "low".into(),
                },
                gates: nexus_config::RuleGates::default(),
                debounce: nexus_config::RuleDebounce {
                    min_track_age_ms: 0,
                    consecutive_frames: 1,
                    cooldown_ms: 0,
                },
                enabled: true,
            })
            .await
            .unwrap();

        let mut grid = vec![vec![false; 48]; 7];
        grid[0][24] = true; // Monday noon
        let put_body = serde_json::json!({
            "policy": { "enabled": true, "schedule": { "grid": grid } }
        });
        let mut req = Request::builder()
            .method(Method::PUT)
            .uri("/api/v1/rules/rule_put/delivery")
            .header("content-type", "application/json")
            .body(Body::from(put_body.to_string()))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/rules/rule_put/delivery")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["inherited"], serde_json::Value::Bool(false));
        assert_eq!(v["policy"]["enabled"], serde_json::Value::Bool(true));
        assert_eq!(
            v["policy"]["schedule"]["grid"][0][24],
            serde_json::Value::Bool(true)
        );
        // Rule schedule replaces global (which is null), so
        // `effective.schedule` mirrors the rule's grid exactly.
        assert_eq!(
            v["effective"]["schedule"]["grid"][0][24],
            serde_json::Value::Bool(true)
        );
    }

    /// PUT /api/v1/rules/{id}/delivery with `{"policy": null}`
    /// clears the override. GET then reports `inherited=true`.
    #[tokio::test]
    async fn rule_delivery_put_null_clears_override() {
        use axum::body::to_bytes;
        const SECRET: &[u8] = b"m7-rule-delivery-clear-secret";
        let (app, store, _dir) = build_test_router(Some(SECRET)).await;
        store
            .upsert_rule(&nexus_config::RuleConfig {
                id: "rule_clear".into(),
                name: "clear".into(),
                predicate: nexus_config::RulePredicate {
                    when: "true".into(),
                    severity: "low".into(),
                },
                gates: nexus_config::RuleGates::default(),
                debounce: nexus_config::RuleDebounce {
                    min_track_age_ms: 0,
                    consecutive_frames: 1,
                    cooldown_ms: 0,
                },
                enabled: true,
            })
            .await
            .unwrap();

        // Set then clear.
        for body in [
            serde_json::json!({"policy":{"enabled":false}}),
            serde_json::json!({"policy": null}),
        ] {
            let mut req = Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/rules/rule_clear/delivery")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            req.extensions_mut().insert(ConnectInfo(loopback_peer()));
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::NO_CONTENT);
        }

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/rules/rule_clear/delivery")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["inherited"], serde_json::Value::Bool(true));
        assert!(v["policy"].is_null());
    }

    /// PUT /api/v1/rules/{id}/delivery for an unknown rule returns
    /// 404. The store layer surfaces NotFound; the handler maps to
    /// the right status (instead of leaking as a 500).
    #[tokio::test]
    async fn rule_delivery_put_unknown_rule_returns_404() {
        const SECRET: &[u8] = b"m7-rule-delivery-404-secret";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let mut req = Request::builder()
            .method(Method::PUT)
            .uri("/api/v1/rules/no_such_rule/delivery")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"policy":{"enabled":false}}).to_string(),
            ))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    /// GET /api/v1/events/{event_id}/delivery returns the outbox
    /// rows for that event. We hand-craft the outbox by going
    /// through the store's `record_event_and_enqueue` path so the
    /// returned rows match exactly what the dispatcher would see.
    #[tokio::test]
    async fn event_delivery_lists_outbox_rows() {
        use axum::body::to_bytes;
        const SECRET: &[u8] = b"m7-event-delivery-secret";
        let (app, store, _dir) = build_test_router(Some(SECRET)).await;

        // Need a camera so the event FK is satisfied.
        store
            .upsert_camera(&nexus_config::CameraConfig {
                id: 1,
                name: "front".into(),
                ingest: nexus_config::CameraIngest {
                    url: url::Url::parse("rtsp://127.0.0.1/s").unwrap(),
                    enabled: true,
                    max_fps: 0,
                },
                detector: nexus_config::CameraDetector {
                    prompts: vec![],
                    visual_prompts: vec![],
                    model_override: None,
                },
                behavior: nexus_config::CameraBehavior {
                    parking_lot_mode: false,
                    anchor_ttl_secs: None,
                },
                zones: vec![],
            })
            .await
            .unwrap();
        let ev = nexus_types::AlertEvent {
            event_id: uuid::Uuid::now_v7(),
            camera_id: 1,
            rule_id: "rule_event_delivery".into(),
            track_id: None,
            label: "person".into(),
            severity: nexus_types::Severity::Low,
            bbox: None,
            frame_id: 1,
            captured_at: chrono::Utc::now(),
            trace_id: "trace-test".into(),
            artifacts: nexus_types::Artifacts::default(),
            context: serde_json::Map::new(),
        };
        store
            .record_event_and_enqueue(&ev, &["webhook:foo"])
            .await
            .unwrap();

        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("/api/v1/events/{}/delivery", ev.event_id))
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let rows = v.as_array().expect("array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["sink_id"], "webhook:foo");
        assert_eq!(rows[0]["status"], "pending");
        assert_eq!(rows[0]["attempts"], 0);
    }

    /// GET /api/v1/admin/sinks/health returns a card for every
    /// configured sink even when the outbox is empty. With no
    /// configured sinks and an empty outbox, `sinks` is `[]`.
    #[tokio::test]
    async fn sinks_health_empty_returns_window_grid() {
        use axum::body::to_bytes;
        const SECRET: &[u8] = b"m7-sinks-health-empty-secret";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let token = sign_admin_jwt(SECRET);
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/admin/sinks/health")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let windows = v["windows"].as_array().expect("windows array");
        // We ship 1h + 24h windows by default.
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0]["label"], "1h");
        assert_eq!(windows[1]["label"], "24h");
        assert_eq!(v["sinks"].as_array().unwrap().len(), 0);
    }

    /// M6 Phase 2 Step 2.9 — public auth-mode probe is reachable
    /// WITHOUT a bearer (anonymous visitors need to know how to
    /// log in) and returns the mode plus the two derived flags.
    /// `build_test_router` defaults to `Local`.
    #[tokio::test]
    async fn auth_info_is_public_and_returns_mode() {
        use axum::body::to_bytes;
        let (app, _store, _dir) = build_test_router(None).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/auth/info")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 4 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["mode"], "local");
        assert_eq!(v["allows_local"], true);
        assert_eq!(v["allows_oidc"], false);
        // M6 Phase 3 Step 3.3 UI — when OIDC isn't wired the
        // field is JSON null, NOT missing, so the SPA can rely
        // on `"oidc_display_name" in info` everywhere.
        assert!(v["oidc_display_name"].is_null());
    }

    // ---------------------------------------------------------------
    // M6 Phase 4 Step 4.2 + 4.3 — audit read endpoints.
    // ---------------------------------------------------------------

    /// `GET /api/v1/admin/audit/resource/{kind}/{id}` is gated by
    /// the admin bearer and returns rows for the requested
    /// (resource_kind, resource_id) pair newest-first.
    #[tokio::test]
    async fn audit_resource_endpoint_returns_rows_for_admin() {
        use axum::body::to_bytes;
        use nexus_store::audit::{AuditActorKind, AuditOutcome, NewAuditEntry};

        const SECRET: &[u8] = b"m6-audit-read-resource-secret";
        let (app, store, _dir) = build_test_router(Some(SECRET)).await;

        // Seed two audit rows against `camera/42` directly via
        // the store (we don't need the full mutation pipeline —
        // we're testing the read path).
        let mut tx = store.pool().begin().await.unwrap();
        for action in ["camera.upsert", "camera.delete"] {
            store
                .record_audit_event(
                    &mut tx,
                    &NewAuditEntry {
                        actor_kind: Some(AuditActorKind::LocalUser),
                        actor_id: Some("7"),
                        actor_label: "user:7",
                        action,
                        resource_kind: Some("camera"),
                        resource_id: Some("42"),
                        before_json: None,
                        after_json: Some("{\"name\":\"cam\"}"),
                        outcome: AuditOutcome::Success,
                        ip: None,
                        user_agent: None,
                    },
                )
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let token = sign_admin_jwt(SECRET);
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/admin/audit/resource/camera/42?limit=10")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let rows: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = rows.as_array().expect("array");
        assert_eq!(arr.len(), 2);
        // Newest-first: camera.delete then camera.upsert.
        assert_eq!(arr[0]["action"], "camera.delete");
        assert_eq!(arr[1]["action"], "camera.upsert");
        assert_eq!(arr[0]["actor_kind"], "local_user");
        assert_eq!(arr[0]["outcome"], "success");
    }

    /// `GET /api/v1/admin/audit/resource/...` without a bearer is
    /// 401 even from loopback (a secret is configured).
    #[tokio::test]
    async fn audit_resource_endpoint_requires_bearer() {
        const SECRET: &[u8] = b"m6-audit-read-resource-no-bearer";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/admin/audit/resource/camera/42")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// `GET /api/v1/admin/audit` filters by outcome.
    #[tokio::test]
    async fn audit_global_endpoint_filters_by_outcome() {
        use axum::body::to_bytes;
        use nexus_store::audit::{AuditActorKind, AuditOutcome, NewAuditEntry};

        const SECRET: &[u8] = b"m6-audit-read-global-outcome";
        let (app, store, _dir) = build_test_router(Some(SECRET)).await;

        let mut tx = store.pool().begin().await.unwrap();
        for outcome in [
            AuditOutcome::Success,
            AuditOutcome::Failure,
            AuditOutcome::Failure,
        ] {
            store
                .record_audit_event(
                    &mut tx,
                    &NewAuditEntry {
                        actor_kind: Some(AuditActorKind::LocalUser),
                        actor_id: Some("1"),
                        actor_label: "user:1",
                        action: "rule.upsert",
                        resource_kind: Some("rule"),
                        resource_id: Some("r1"),
                        before_json: None,
                        after_json: None,
                        outcome,
                        ip: None,
                        user_agent: None,
                    },
                )
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let token = sign_admin_jwt(SECRET);
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/admin/audit?outcome=failure&limit=10")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["limit"], 10);
        assert_eq!(v["offset"], 0);
        let rows = v["rows"].as_array().expect("rows array");
        assert_eq!(rows.len(), 2);
        for r in rows {
            assert_eq!(r["outcome"], "failure");
        }
    }

    /// `GET /api/v1/admin/audit?outcome=bogus` returns 400 so a
    /// typo in the URL surfaces immediately rather than being
    /// silently dropped.
    #[tokio::test]
    async fn audit_global_endpoint_rejects_unknown_outcome() {
        const SECRET: &[u8] = b"m6-audit-read-bad-outcome";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let token = sign_admin_jwt(SECRET);
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/admin/audit?outcome=bogus")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    // ===============================================================
    // M3.1 Phase H — visual prompts admin REST API
    // ===============================================================

    /// Smoke test: `GET /v1/admin/visual-prompts` requires the
    /// admin bearer (loopback bypass does NOT apply once an admin
    /// secret is configured). Confirms the route is reachable
    /// under the admin layer.
    #[tokio::test]
    async fn visual_prompts_list_requires_admin_auth() {
        const SECRET: &[u8] = b"m3-1-visual-prompts-auth";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/admin/visual-prompts")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "no bearer → 401 (loopback bypass disabled when secret configured)"
        );
    }

    /// `GET /v1/admin/visual-prompts` returns an empty JSON array
    /// when no prompts have been uploaded yet. Catches route-
    /// registration regressions where the path resolves but the
    /// handler isn't wired.
    #[tokio::test]
    async fn visual_prompts_list_empty_returns_empty_array() {
        const SECRET: &[u8] = b"m3-1-visual-prompts-list-empty";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let token = sign_admin_jwt(SECRET);
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/admin/visual-prompts")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.is_array(), "response must be a JSON array");
        assert_eq!(v.as_array().unwrap().len(), 0);
    }

    /// `POST /v1/admin/visual-prompts` returns 503 with
    /// `encoder_not_configured` when no encoder model path is
    /// resolvable from `inference.model.pack_path`. The test
    /// harness builds the state with `encoder_model_path = None`,
    /// so this is the default no-encoder path.
    ///
    /// The handler validates required multipart fields (name,
    /// image) BEFORE checking the encoder path, so the test
    /// posts a real (tiny) PNG to make sure the 503 surfaces
    /// rather than a 400 "missing field" error.
    #[tokio::test]
    async fn visual_prompts_post_returns_503_when_encoder_unconfigured() {
        const SECRET: &[u8] = b"m3-1-visual-prompts-no-encoder";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let token = sign_admin_jwt(SECRET);
        // Smallest valid PNG: 1×1 transparent pixel. We don't
        // need the encoder to actually run — the handler must
        // return 503 BEFORE it touches the encoder session, but
        // AFTER it validates the multipart fields.
        let png_1x1: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
            0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, // IDAT chunk
            0x54, 0x78, 0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D,
            0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, // IEND chunk
            0x42, 0x60, 0x82,
        ];
        let boundary = "------------------------testboundary";
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\ntest-prompt\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(png_1x1);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/admin/visual-prompts")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "no encoder configured → 503",
        );
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(
            s.contains("encoder_not_configured"),
            "503 body must mention encoder_not_configured, got: {s}",
        );
    }

    /// `GET /v1/admin/cameras/{cid}/visual-prompts` against a
    /// camera that has no attachments returns an empty array
    /// (NOT 404 — the camera might not even exist in the test
    /// DB; the join-table query just returns no rows).
    #[tokio::test]
    async fn visual_prompts_list_for_unknown_camera_returns_empty() {
        const SECRET: &[u8] = b"m3-1-visual-prompts-unknown-cam";
        let (app, _store, _dir) = build_test_router(Some(SECRET)).await;
        let token = sign_admin_jwt(SECRET);
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/admin/cameras/999/visual-prompts")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(loopback_peer()));
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.as_array().expect("array").len(), 0);
    }
}

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
//! * `GET  /api/events?limit=N`
//! * `GET  /api/stream/metadata`                  — SSE
//! * `GET  /api/stream/events`                    — SSE
//! * `GET  /api/backends`                         — DetectorPool slot status (OPS-1)
//!
//! Everything else is served from the UI directory via [`tower_http::services::ServeDir`].

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
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
use nexus_pipeline::LatestFrameCache;
use nexus_store::Store;
use nexus_types::{AlertEvent, CameraId, FrameMetadata, PixelFormat, RuleId};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::admin_auth::{self, AdminAuthState};
use crate::cold_read_cache::CacheJobs;

#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<Store>,
    pub bus: Arc<dyn Bus>,
    pub cache: Arc<LatestFrameCache>,
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
        .route_layer(axum::middleware::from_fn_with_state(
            state.admin_auth.clone(),
            admin_auth::admin_auth_layer,
        ));

    let api = Router::new()
        .route("/health", get(health))
        .route("/cameras", get(list_cameras))
        .route("/cameras/{id}", put(upsert_camera).delete(delete_camera))
        .route("/cameras/{id}/frames/latest", get(get_latest_frame_jpeg))
        .route(
            "/cameras/{id}/frames/latest.json",
            get(get_latest_frame_meta),
        )
        .route("/rules", get(list_rules))
        .route("/rules/{id}", put(upsert_rule))
        .route("/rules/{id}", delete(delete_rule))
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
        // M2.2 cold-mirror — combined hot+cold view (read-only).
        .route("/v1/storage", get(get_storage))
        // M2.2 closeout: OAuth callback for the auth-code dance.
        // Registered OUTSIDE the admin gate (provider redirects a
        // browser here; authentication is via the unguessable
        // `state` token from the matching `start` request).
        .route("/v1/admin/oauth/{provider}/callback", get(oauth_callback))
        // Admin writes (gated) merged in last so they share state.
        .merge(admin);

    let static_dir = ServeDir::new(state.ui_root.clone()).append_index_html_on_directories(true);

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
struct ApiError(StatusCode, String);

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

async fn list_cameras(State(s): State<ApiState>) -> Result<Json<Vec<CameraConfig>>, ApiError> {
    Ok(Json(s.store.list_cameras().await?))
}

async fn upsert_camera(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    Json(mut cam): Json<CameraConfig>,
) -> Result<Json<CameraConfig>, ApiError> {
    cam.id = id;
    s.store.upsert_camera(&cam).await?;
    s.store
        .write_audit(
            "api",
            "upsert",
            &format!("camera/{id}"),
            &serde_json::to_value(&cam).unwrap_or(serde_json::Value::Null),
        )
        .await?;
    let _ = s
        .bus
        .publish(
            topic::CONFIG_CHANGED,
            &serde_json::json!({ "camera_id": id }),
        )
        .await;
    Ok(Json(cam))
}

async fn delete_camera(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
) -> Result<StatusCode, ApiError> {
    s.store.delete_camera(id).await?;
    s.store
        .write_audit(
            "api",
            "delete",
            &format!("camera/{id}"),
            &serde_json::json!({}),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_rules(State(s): State<ApiState>) -> Result<Json<Vec<RuleConfig>>, ApiError> {
    Ok(Json(s.store.list_rules().await?))
}

async fn upsert_rule(
    State(s): State<ApiState>,
    Path(id): Path<RuleId>,
    Json(mut rule): Json<RuleConfig>,
) -> Result<Json<RuleConfig>, ApiError> {
    rule.id = id.clone();
    s.store.upsert_rule(&rule).await?;
    s.store
        .write_audit(
            "api",
            "upsert",
            &format!("rule/{id}"),
            &serde_json::to_value(&rule).unwrap_or(serde_json::Value::Null),
        )
        .await?;
    Ok(Json(rule))
}

async fn delete_rule(
    State(s): State<ApiState>,
    Path(id): Path<RuleId>,
) -> Result<StatusCode, ApiError> {
    s.store.delete_rule(&id).await?;
    s.store
        .write_audit(
            "api",
            "delete",
            &format!("rule/{id}"),
            &serde_json::json!({}),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
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
            // explicit casts are still needed there.
            let frag = stat.fragment_size();
            let blocks = stat.blocks() as u64;
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
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
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
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
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
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
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
/// file size. Returns `(start, end_inclusive)`. Multi-range and
/// suffix-range (`bytes=-N`) are intentionally unsupported — browsers
/// only need single-byte-range for `<video>` element seeking.
fn parse_byte_range(raw: &str, file_size: u64) -> Option<(u64, u64)> {
    let raw = raw.trim();
    let rest = raw.strip_prefix("bytes=")?;
    // First range only.
    let first = rest.split(',').next()?.trim();
    let (start_str, end_str) = first.split_once('-')?;
    let start_str = start_str.trim();
    let end_str = end_str.trim();
    if start_str.is_empty() {
        // Suffix form `bytes=-N` — not implemented.
        return None;
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
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
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
    Json(req): Json<PutColdReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Default throttle: keep whatever's already set so the caller
    // can switch handles without re-specifying bandwidth.
    let current = s.store.read_cold_replica().await?;
    let throttle = req.throttle_bps.unwrap_or(current.throttle_bps);
    s.store
        .write_cold_replica(req.handle.as_deref(), throttle)
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
    s.store
        .write_audit(
            "api",
            "put",
            "admin/storage/cold",
            &serde_json::json!({
                "handle": req.handle,
                "throttle_bps": throttle,
            }),
        )
        .await?;
    let _ = s
        .bus
        .publish(
            topic::STORAGE_BACKENDS_CHANGED,
            &serde_json::json!({ "reason": "cold_replica_updated" }),
        )
        .await;
    Ok(Json(serde_json::json!({
        "handle": req.handle,
        "throttle_bps": throttle,
    })))
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
    s.store
        .upsert_storage_backend(&handle, &req.kind, &config_json)
        .await?;
    rebuild_registry(&s).await?;
    // Audit log: redact the encrypted refresh token blob even though
    // it's only ciphertext — it's still operator credential material
    // and ops logs should not carry it.
    let audited_config = redacted_config_for_audit(&config);
    s.store
        .write_audit(
            "api",
            "put",
            &format!("admin/storage/backends/{handle}"),
            &serde_json::json!({ "kind": req.kind, "config": audited_config }),
        )
        .await?;
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
) -> Result<StatusCode, ApiError> {
    s.store
        .delete_storage_backend(&handle)
        .await
        .map_err(|e| match e {
            nexus_store::DeleteBackendError::InUse(h) => ApiError(
                StatusCode::CONFLICT,
                format!("backend '{h}' is referenced by motion_clips; clear cold pointers first"),
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
    rebuild_registry(&s).await?;
    s.store
        .write_audit(
            "api",
            "delete",
            &format!("admin/storage/backends/{handle}"),
            &serde_json::json!({}),
        )
        .await?;
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
    Json(req): Json<UsbPreferredReq>,
) -> Result<Json<UsbPreferredOut>, ApiError> {
    let normalised = match req.label {
        Some(raw) => {
            let trimmed = raw.trim().to_string();
            if trimmed.is_empty() {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "label must be non-empty (send null to clear)".to_string(),
                ));
            }
            Some(trimmed)
        }
        None => None,
    };

    // Persist first so a crash between the in-memory flip and the
    // SQLite write doesn't leave the recorder pointed at a label
    // the next boot won't reconstruct.
    s.store
        .write_runtime_setting("preferred_usb_label", normalised.as_deref())
        .await?;
    s.preferred_usb_label.set(normalised.clone());
    s.store
        .write_audit(
            "api",
            "put",
            "admin/runtime/usb_preferred",
            &serde_json::json!({ "label": normalised }),
        )
        .await?;
    Ok(Json(UsbPreferredOut { label: normalised }))
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
    let _ = s
        .store
        .write_audit(
            "api",
            "oauth_callback",
            &format!("admin/oauth/{kind}/callback"),
            &serde_json::json!({
                "handle": session.handle,
                "kind": kind,
                "config": audited,
                "scope": tokens.scope,
            }),
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
    fn parse_rejects_suffix_range() {
        assert!(parse_byte_range("bytes=-500", 1000).is_none());
    }

    #[test]
    fn parse_rejects_start_past_eof() {
        assert!(parse_byte_range("bytes=2000-2500", 1000).is_none());
    }

    #[test]
    fn parse_rejects_inverted_range() {
        assert!(parse_byte_range("bytes=500-100", 1000).is_none());
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
                url: Url::parse("rtsp://127.0.0.1/stream3").unwrap(),
                enabled: true,
                prompts: vec![],
                model_override: None,
                zones: vec![],
                max_fps: 0,
                parking_lot_mode: false,
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
        let state = super::ApiState {
            store: store.clone(),
            bus,
            cache,
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
}

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

#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<Store>,
    pub bus: Arc<dyn Bus>,
    pub cache: Arc<LatestFrameCache>,
    pub pool: Option<Arc<DetectorPool>>,
    pub ui_root: PathBuf,
}

pub fn router(state: ApiState) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/cameras", get(list_cameras))
        .route("/cameras/:id", put(upsert_camera).delete(delete_camera))
        .route(
            "/cameras/:id/frames/latest",
            get(get_latest_frame_jpeg),
        )
        .route(
            "/cameras/:id/frames/latest.json",
            get(get_latest_frame_meta),
        )
        .route("/rules", get(list_rules))
        .route("/rules/:id", put(upsert_rule))
        .route("/rules/:id", delete(delete_rule))
        .route("/events", get(list_events))
        .route("/stream/metadata", get(stream_metadata))
        .route("/stream/events", get(stream_events))
        .route("/backends", get(get_backends));

    let static_dir = ServeDir::new(state.ui_root.clone())
        .append_index_html_on_directories(true);

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

async fn list_cameras(
    State(s): State<ApiState>,
) -> Result<Json<Vec<CameraConfig>>, ApiError> {
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
        .write_audit("api", "upsert", &format!("camera/{id}"), &serde_json::to_value(&cam).unwrap_or(serde_json::Value::Null))
        .await?;
    let _ = s
        .bus
        .publish(topic::CONFIG_CHANGED, &serde_json::json!({ "camera_id": id }))
        .await;
    Ok(Json(cam))
}

async fn delete_camera(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
) -> Result<StatusCode, ApiError> {
    s.store.delete_camera(id).await?;
    s.store
        .write_audit("api", "delete", &format!("camera/{id}"), &serde_json::json!({}))
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
        .write_audit("api", "upsert", &format!("rule/{id}"), &serde_json::to_value(&rule).unwrap_or(serde_json::Value::Null))
        .await?;
    Ok(Json(rule))
}

async fn delete_rule(
    State(s): State<ApiState>,
    Path(id): Path<RuleId>,
) -> Result<StatusCode, ApiError> {
    s.store.delete_rule(&id).await?;
    s.store
        .write_audit("api", "delete", &format!("rule/{id}"), &serde_json::json!({}))
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
        .write_image(&rgb, frame.width, frame.height, image::ExtendedColorType::Rgb8)
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

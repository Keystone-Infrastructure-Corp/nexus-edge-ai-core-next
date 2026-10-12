//! Clip byte-range server: `GET /v1/clips/{id}` (hot and cold-backend
//! playback), thumbnails, per-clip tracks, and the event-to-clip lookup.

use super::*;

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

pub(super) async fn get_clip(
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

pub(super) async fn get_clip_thumbnail(
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
pub(super) struct ClipTracksResponse {
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
pub(super) async fn get_clip_tracks(
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
pub(super) struct EventClipResponse {
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
pub(super) async fn get_event_clip_lookup(
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
                    analysis_url: None,
                    enabled: true,
                    max_fps: 0,
                    codec: None,
                },
                detector: nexus_config::CameraDetector {
                    prompts: vec![],
                    visual_prompts: vec![],
                    model_override: None,
                },
                behavior: nexus_config::CameraBehavior {
                    parking_lot_mode: false,
                    anchor_ttl_secs: None,
                    ..Default::default()
                },
                onvif: Default::default(),
                talk_down: Default::default(),
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
}

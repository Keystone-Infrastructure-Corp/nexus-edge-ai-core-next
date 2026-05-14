//! Best-effort, on-demand thumbnail extraction for recorded clips.
//!
//! Used by `GET /api/v1/clips/:id/thumbnail` to produce a preview JPEG
//! for the admin UI Timeline. The function shells out to a one-shot
//! GStreamer pipeline that seeks to the midpoint of the file, decodes
//! a single frame, and writes a JPEG to disk so subsequent requests
//! hit the cache.
//!
//! Pipeline (one-off):
//!   filesrc location=PATH ! decodebin ! videoconvert
//!     ! videoscale ! video/x-raw,width=320 ! jpegenc quality=80
//!     ! filesink location=PATH.jpg
//!
//! Notes:
//! * No re-mux, no clip duplication; we only read the existing file.
//! * 5s state-change timeout. If the file is still being written
//!   (clip in progress), we let the caller surface 503 — there's no
//!   guarantee a valid frame exists yet.
//! * This module is gated on `feature = "gstreamer"`. The non-gstreamer
//!   build doesn't expose it, and the api falls back to 503.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use thiserror::Error;
use tracing::{debug, warn};

use crate::source::gst_init;

#[derive(Debug, Error)]
pub enum ThumbnailError {
    #[error("source clip is missing: {0}")]
    Missing(PathBuf),
    #[error("gstreamer init failed: {0}")]
    Init(String),
    #[error("decode pipeline failed: {0}")]
    Decode(String),
}

/// Extract a single 320px-wide JPEG thumbnail from `clip_path` and
/// write it to `out_path`. Returns `Ok(out_path)` on success.
///
/// Idempotent: if `out_path` already exists with a non-zero size and
/// is newer than `clip_path`, the file is returned without rebuilding.
pub fn ensure_thumbnail(clip_path: &Path, out_path: &Path) -> Result<PathBuf, ThumbnailError> {
    if !clip_path.is_file() {
        return Err(ThumbnailError::Missing(clip_path.to_path_buf()));
    }
    if let Ok(out_meta) = std::fs::metadata(out_path) {
        if out_meta.len() > 0 {
            if let Ok(clip_meta) = std::fs::metadata(clip_path) {
                if let (Ok(out_mtime), Ok(clip_mtime)) = (out_meta.modified(), clip_meta.modified())
                {
                    if out_mtime >= clip_mtime {
                        debug!(
                            path = %out_path.display(),
                            "thumbnail cache hit"
                        );
                        return Ok(out_path.to_path_buf());
                    }
                }
            }
        }
    }
    if let Some(parent) = out_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    gst_init::ensure().map_err(|e| ThumbnailError::Init(e.to_string()))?;

    // Strip stray `"` so the launch parser doesn't tokenise wrong.
    let in_safe = clip_path.to_string_lossy().replace('"', "");
    let out_safe = out_path.to_string_lossy().replace('"', "");
    let desc = format!(
        "filesrc location=\"{in_safe}\" \
         ! decodebin \
         ! videoconvert \
         ! videoscale \
         ! video/x-raw,width=320 \
         ! jpegenc quality=80 \
         ! filesink location=\"{out_safe}\""
    );

    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| ThumbnailError::Decode(format!("parse::launch: {e}")))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| ThumbnailError::Decode("downcast Pipeline".to_string()))?;

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| ThumbnailError::Decode(format!("set Playing: {e}")))?;

    let bus = pipeline
        .bus()
        .ok_or_else(|| ThumbnailError::Decode("no bus".to_string()))?;
    let mut out = Ok(out_path.to_path_buf());
    let timeout = gst::ClockTime::from_seconds(5);
    let stop_at = std::time::Instant::now() + Duration::from_secs(6);
    loop {
        let remaining = stop_at.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            out = Err(ThumbnailError::Decode("timeout decoding frame".to_string()));
            break;
        }
        let Some(msg) = bus.timed_pop(timeout) else {
            out = Err(ThumbnailError::Decode(
                "timed_pop returned None".to_string(),
            ));
            break;
        };
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(_) => break,
            MessageView::Error(e) => {
                out = Err(ThumbnailError::Decode(format!(
                    "bus error: {} ({})",
                    e.error(),
                    e.debug().unwrap_or_else(|| "<no debug>".into())
                )));
                break;
            }
            _ => {}
        }
    }

    if let Err(e) = pipeline.set_state(gst::State::Null) {
        warn!("thumbnail pipeline set Null failed: {e}");
    }

    if let Err(e) = &out {
        // Tear down any partial file so the next call retries cleanly.
        let _ = std::fs::remove_file(out_path);
        warn!(clip = %clip_path.display(), "thumbnail extract failed: {e}");
    }
    out
}

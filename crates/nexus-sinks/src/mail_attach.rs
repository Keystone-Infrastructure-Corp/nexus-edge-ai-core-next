//! Shared best-effort attachment reader for the SMTP-based sinks
//! ([`crate::email`] and [`crate::sureview_email`]).
//!
//! Alert artifacts (annotated snapshot JPG, motion clip MP4) live on
//! the appliance's disk and are referenced by path on the event. They
//! are attached *best-effort*: a missing, unreadable, or oversized
//! file is logged and omitted so the alert still goes out. An alert
//! that arrives without its picture is far better than one that never
//! arrives at all.

use lettre::message::{header::ContentType, Attachment, SinglePart};
use tracing::{debug, warn};

/// Cap on an attached snapshot (JPG). Snapshots are small; anything
/// larger is almost certainly the wrong file, so skip it rather than
/// risk a 552 "message too large" reply.
pub(crate) const MAX_SNAPSHOT_BYTES: u64 = 5 * 1024 * 1024;

/// Cap on an attached motion clip (MP4). SMTP relays commonly reject
/// messages over ~25 MB; we stay well under and skip oversized clips
/// (the alert still fires, just without the clip attached).
pub(crate) const MAX_CLIP_BYTES: u64 = 20 * 1024 * 1024;

/// MIME content type for an attachment, inferred from its file
/// extension. Falls back to `application/octet-stream` for unknown
/// types (lettre requires a parseable content type; the static
/// strings here always parse).
fn content_type_for(path: &str) -> ContentType {
    let lower = path.to_ascii_lowercase();
    let mime = if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".mp4") {
        "video/mp4"
    } else {
        "application/octet-stream"
    };
    ContentType::parse(mime).unwrap_or(ContentType::TEXT_PLAIN)
}

/// Read one attachment from disk, capped at `max_bytes`. Returns
/// `None` (with a log line) on any problem so the caller can send the
/// message without it.
pub(crate) async fn read_attachment(path: &str, max_bytes: u64) -> Option<SinglePart> {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() > max_bytes => {
            warn!(
                path,
                size = meta.len(),
                max = max_bytes,
                "attachment too large, skipping"
            );
            return None;
        }
        Ok(_) => {}
        Err(e) => {
            debug!(path, error = %e, "attachment not available, skipping");
            return None;
        }
    }
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) => {
            warn!(path, error = %e, "attachment read failed, skipping");
            return None;
        }
    };
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("attachment")
        .to_string();
    Some(Attachment::new(filename).body(bytes, content_type_for(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_is_inferred_from_extension() {
        assert_eq!(
            content_type_for("/a/b.JPG"),
            ContentType::parse("image/jpeg").unwrap()
        );
        assert_eq!(
            content_type_for("/a/b.png"),
            ContentType::parse("image/png").unwrap()
        );
        assert_eq!(
            content_type_for("/a/b.mp4"),
            ContentType::parse("video/mp4").unwrap()
        );
        assert_eq!(
            content_type_for("/a/b.bin"),
            ContentType::parse("application/octet-stream").unwrap()
        );
    }

    #[tokio::test]
    async fn missing_file_is_skipped_not_fatal() {
        assert!(read_attachment("/nope/does-not-exist.jpg", 1024)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn oversized_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.jpg");
        tokio::fs::write(&path, vec![0u8; 4096]).await.unwrap();
        assert!(read_attachment(path.to_str().unwrap(), 1024)
            .await
            .is_none());
        assert!(read_attachment(path.to_str().unwrap(), 8192)
            .await
            .is_some());
    }
}

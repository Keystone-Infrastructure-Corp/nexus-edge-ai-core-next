//! L7 side-channel — the "latest frame per camera" cache.
//!
//! **Why this exists, in one paragraph:** the bus carries metadata for many
//! subscribers. Frame buffers are large (a 1080p RGB24 frame is ~6 MB).
//! Broadcasting them would clone the buffer per subscriber per frame, which
//! is unacceptable on the hot path. The cache keeps a single `Arc<Frame>`
//! per camera; readers (the snapshot HTTP route, the SSE overlay route)
//! get a cheap pointer copy. The cache is documented in `ARCHITECTURE.md`
//! as L7 — it's a first-class architectural element, not a hack.
//!
//! Contention model: writers are pipeline tasks (one per camera). Readers
//! are HTTP handlers. `parking_lot::RwLock` is the right primitive here —
//! the cache is read 100x more often than written.

use std::collections::HashMap;
use std::sync::Arc;

use nexus_types::{CameraId, Frame, TrackedObject};
use parking_lot::RwLock;

#[derive(Clone)]
pub struct LatestEntry {
    pub frame: Arc<Frame>,
    pub objects: Arc<Vec<TrackedObject>>,
}

#[derive(Default)]
pub struct LatestFrameCache {
    inner: RwLock<HashMap<CameraId, LatestEntry>>,
}

impl LatestFrameCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&self, camera_id: CameraId, frame: Arc<Frame>, objects: Arc<Vec<TrackedObject>>) {
        self.inner
            .write()
            .insert(camera_id, LatestEntry { frame, objects });
    }

    pub fn get(&self, camera_id: CameraId) -> Option<LatestEntry> {
        self.inner.read().get(&camera_id).cloned()
    }

    pub fn clear(&self, camera_id: CameraId) {
        self.inner.write().remove(&camera_id);
    }

    pub fn cameras(&self) -> Vec<CameraId> {
        self.inner.read().keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nexus_types::PixelFormat;

    fn frame(id: CameraId) -> Arc<Frame> {
        Arc::new(Frame {
            camera_id: id,
            frame_id: 1,
            captured_at: Utc::now(),
            width: 16,
            height: 16,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![0u8; 16 * 16 * 3]),
            trace_id: "t".into(),
        })
    }

    #[test]
    fn put_then_get_returns_same_arc() {
        let cache = LatestFrameCache::new();
        let f = frame(7);
        cache.put(7, f.clone(), Arc::new(vec![]));
        let got = cache.get(7).unwrap();
        assert!(Arc::ptr_eq(&got.frame, &f));
    }
}

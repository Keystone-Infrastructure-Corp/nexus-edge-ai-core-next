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
    /// `frame_id` the objects were computed on, or `None` when no inference
    /// has completed yet. The frame is published at decode rate and the
    /// objects at inference rate (BUG-136), so the two routinely describe
    /// different frames and callers that pair them must check.
    pub objects_frame_id: Option<u64>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<CameraId, LatestEntry>,
    /// Bumped by `begin_session` and `clear`. A writer holding an older
    /// epoch has been superseded and its writes are dropped.
    epochs: HashMap<CameraId, u64>,
}

#[derive(Default)]
pub struct LatestFrameCache {
    inner: RwLock<Inner>,
}

impl LatestFrameCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the camera for a new pipeline session.
    ///
    /// `abort()` is asynchronous, so a stopped camera's writers can still be
    /// mid-flight when the next session starts. Writes carry the epoch they
    /// were issued under and lose the race deterministically rather than by
    /// timing — the same shape as `is_current_session` (BUG-070).
    pub fn begin_session(&self, camera_id: CameraId) -> u64 {
        let mut g = self.inner.write();
        let e = g.epochs.entry(camera_id).or_insert(0);
        *e += 1;
        *e
    }

    fn is_current(inner: &Inner, camera_id: CameraId, epoch: u64) -> bool {
        inner.epochs.get(&camera_id).copied().unwrap_or(0) == epoch
    }

    /// Publish a decoded frame, leaving any cached objects in place.
    pub fn put_frame(&self, camera_id: CameraId, epoch: u64, frame: Arc<Frame>) {
        let mut g = self.inner.write();
        if !Self::is_current(&g, camera_id, epoch) {
            return;
        }
        match g.entries.get_mut(&camera_id) {
            Some(entry) => entry.frame = frame,
            None => {
                g.entries.insert(
                    camera_id,
                    LatestEntry {
                        frame,
                        objects: Arc::new(Vec::new()),
                        objects_frame_id: None,
                    },
                );
            }
        }
    }

    /// Publish inference results for a frame.
    ///
    /// Deliberately never touches `frame`. The analysed frame is always older
    /// than whatever the tap last published, so writing it back would rewind
    /// the cached `frame_id` and `captured_at` on every completed inference —
    /// which the LBR pump reads as a brand-new frame and as its own content
    /// re-appearing after others, i.e. a manufactured decoder loop.
    ///
    /// No entry means no frame has been published yet; objects without a
    /// frame are not useful to any reader, so they are dropped.
    pub fn put_objects(
        &self,
        camera_id: CameraId,
        epoch: u64,
        frame_id: u64,
        objects: Arc<Vec<TrackedObject>>,
    ) {
        let mut g = self.inner.write();
        if !Self::is_current(&g, camera_id, epoch) {
            return;
        }
        if let Some(entry) = g.entries.get_mut(&camera_id) {
            entry.objects = objects;
            entry.objects_frame_id = Some(frame_id);
        }
    }

    pub fn get(&self, camera_id: CameraId) -> Option<LatestEntry> {
        self.inner.read().entries.get(&camera_id).cloned()
    }

    /// Drop the camera's entry and retire its epoch, so a writer still
    /// draining cannot repopulate it.
    pub fn clear(&self, camera_id: CameraId) {
        let mut g = self.inner.write();
        g.entries.remove(&camera_id);
        let e = g.epochs.entry(camera_id).or_insert(0);
        *e += 1;
    }

    pub fn cameras(&self) -> Vec<CameraId> {
        self.inner.read().entries.keys().copied().collect()
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
        let epoch = cache.begin_session(7);
        let f = frame(7);
        cache.put_frame(7, epoch, f.clone());
        let got = cache.get(7).unwrap();
        assert!(Arc::ptr_eq(&got.frame, &f));
    }

    /// A stopped camera must not keep serving its last frame. Both the admin
    /// frame API and the Phase 10 LBR pump read this cache, so a surviving
    /// entry paints the cloud wall with a dead camera's final image under a
    /// "LIVE" badge — and a camera that went green just before it stalled
    /// stays green on the wall indefinitely. Measured on San Marcos 1: after
    /// disabling 26 cameras, every one still returned a full JPEG frozen at
    /// the moment of teardown.
    #[test]
    fn clear_stops_a_stopped_camera_serving_a_stale_frame() {
        let cache = LatestFrameCache::new();
        let epoch = cache.begin_session(7);
        cache.put_frame(7, epoch, frame(7));
        assert!(cache.get(7).is_some());

        cache.clear(7);

        assert!(
            cache.get(7).is_none(),
            "a stopped camera must not serve its last frame"
        );
        assert!(!cache.cameras().contains(&7));
    }

    /// The decode-rate tap runs in its own task, and `abort()` is
    /// asynchronous — so it can still be holding a frame when `stop_camera`
    /// clears the cache. Without the epoch that write lands *after* the
    /// clear and the wall streams a live feed of a camera the operator just
    /// disabled, which is strictly worse than the frozen frame the test
    /// above locks (BUG-136).
    #[test]
    fn a_write_from_a_retired_session_cannot_repopulate_a_cleared_camera() {
        let cache = LatestFrameCache::new();
        let epoch = cache.begin_session(7);
        cache.put_frame(7, epoch, frame(7));

        cache.clear(7);
        cache.put_frame(7, epoch, frame(7));

        assert!(
            cache.get(7).is_none(),
            "a retired session repopulated a cleared camera"
        );
    }

    /// A camera edit stops and restarts the pipeline while deliberately
    /// leaving the LBR pump running. The old session's tap must not
    /// interleave its own `frame_id` sequence into the new session's cell.
    #[test]
    fn a_previous_session_cannot_write_over_the_current_one() {
        let cache = LatestFrameCache::new();
        let old = cache.begin_session(7);
        let new = cache.begin_session(7);
        assert_ne!(old, new);

        let current = frame(7);
        cache.put_frame(7, new, current.clone());
        cache.put_frame(7, old, frame(7));

        let got = cache.get(7).unwrap();
        assert!(
            Arc::ptr_eq(&got.frame, &current),
            "a superseded session overwrote the live frame"
        );
    }

    /// "No inference has completed yet" and "the detector found nothing" are
    /// different answers, and the frame API reports them to an operator.
    #[test]
    fn a_frame_published_before_any_inference_reports_no_objects_frame_id() {
        let cache = LatestFrameCache::new();
        let epoch = cache.begin_session(7);
        cache.put_frame(7, epoch, frame(7));

        let got = cache.get(7).unwrap();
        assert_eq!(
            got.objects_frame_id, None,
            "a never-inferred camera must not claim its empty objects belong to a frame"
        );

        cache.put_objects(7, epoch, 1, Arc::new(vec![]));
        assert_eq!(cache.get(7).unwrap().objects_frame_id, Some(1));
    }

    /// The analysed frame is always older than the one the tap last
    /// published, so writing it back would rewind `frame_id` on every
    /// completed inference. The LBR pump reads a backwards id as a brand-new
    /// frame, and reads its own already-sent content re-appearing as a
    /// decoder loop — so it would start suppressing sends on a healthy
    /// camera.
    #[test]
    fn inference_results_never_rewind_the_published_frame() {
        let cache = LatestFrameCache::new();
        let epoch = cache.begin_session(7);

        let mut newest = (*frame(7)).clone();
        newest.frame_id = 50;
        cache.put_frame(7, epoch, Arc::new(newest));

        // Inference finishes on a much older frame.
        cache.put_objects(7, epoch, 12, Arc::new(vec![]));

        let got = cache.get(7).unwrap();
        assert_eq!(got.frame.frame_id, 50, "a completed inference rewound the live frame");
        assert_eq!(got.objects_frame_id, Some(12));
    }

    /// Objects with no frame to hang on are not useful to any reader.
    #[test]
    fn objects_for_a_camera_with_no_published_frame_are_dropped() {
        let cache = LatestFrameCache::new();
        let epoch = cache.begin_session(7);
        cache.put_objects(7, epoch, 1, Arc::new(vec![]));
        assert!(cache.get(7).is_none());
    }

    /// The tap publishes at decode rate and leaves objects alone, so the
    /// wall keeps painting while inference is still working on an older
    /// frame.
    #[test]
    fn put_frame_advances_the_frame_without_disturbing_objects() {
        let cache = LatestFrameCache::new();
        let epoch = cache.begin_session(7);
        let objects = Arc::new(vec![]);
        cache.put_frame(7, epoch, frame(7));
        cache.put_objects(7, epoch, 1, objects.clone());

        let mut newer = (*frame(7)).clone();
        newer.frame_id = 99;
        cache.put_frame(7, epoch, Arc::new(newer));

        let got = cache.get(7).unwrap();
        assert_eq!(got.frame.frame_id, 99, "the frame did not advance");
        assert_eq!(
            got.objects_frame_id,
            Some(1),
            "objects must still name the frame they were computed on"
        );
    }
}

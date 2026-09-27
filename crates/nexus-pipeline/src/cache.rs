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
//! Contention model: writers are pipeline tasks (two per camera — the
//! decode-rate tap and the inference loop). Readers are HTTP handlers and
//! the per-camera LBR pump. The camera id already partitions the data, so
//! the lock does too: each camera has its own `Mutex<Slot>`, and a write for
//! one camera never waits on another. The outer map is only write-locked
//! the first time a camera is seen; every hot-path call takes it shared.

use std::collections::HashMap;
use std::sync::Arc;

use nexus_types::{CameraId, Frame, TrackedObject};
use parking_lot::{Mutex, RwLock};

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

/// One camera's state. The entry and its epoch share one lock so the epoch
/// check and the write it guards are atomic with respect to `begin_session`
/// and `clear` for the same camera. A slot is never removed from the map
/// (`clear` empties it), so every call for a camera serialises on the same
/// mutex.
#[derive(Default)]
struct Slot {
    entry: Option<LatestEntry>,
    /// Bumped by `begin_session` and `clear`. A writer holding an older
    /// epoch has been superseded and its writes are dropped.
    epoch: u64,
}

#[derive(Default)]
pub struct LatestFrameCache {
    slots: RwLock<HashMap<CameraId, Arc<Mutex<Slot>>>>,
}

impl LatestFrameCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&self, camera_id: CameraId) -> Option<Arc<Mutex<Slot>>> {
        self.slots.read().get(&camera_id).cloned()
    }

    fn slot_or_insert(&self, camera_id: CameraId) -> Arc<Mutex<Slot>> {
        if let Some(slot) = self.slot(camera_id) {
            return slot;
        }
        self.slots.write().entry(camera_id).or_default().clone()
    }

    /// Claim the camera for a new pipeline session.
    ///
    /// `abort()` is asynchronous, so a stopped camera's writers can still be
    /// mid-flight when the next session starts. Writes carry the epoch they
    /// were issued under and lose the race deterministically rather than by
    /// timing — the same shape as `is_current_session` (BUG-070).
    ///
    /// The last frame is kept — a restarting camera should not blank the
    /// wall — but the objects are dropped. They describe the *previous*
    /// session, and the tap republishes at decode rate the moment the new
    /// source starts, so keeping them would ride stale boxes on a brand-new
    /// frame. That matters most on the supervisor-dims rebuild, which
    /// re-enters the source loop without a `clear` and would otherwise pair
    /// new-dims frames with old-dims coordinates; and `frame_id` restarts at
    /// 1 per source, so a retained `objects_frame_id` can collide with a
    /// live id and claim a match that never happened.
    pub fn begin_session(&self, camera_id: CameraId) -> u64 {
        let slot = self.slot_or_insert(camera_id);
        let mut g = slot.lock();
        if let Some(entry) = g.entry.as_mut() {
            entry.objects = Arc::new(Vec::new());
            entry.objects_frame_id = None;
        }
        g.epoch += 1;
        g.epoch
    }

    /// Publish a decoded frame, leaving any cached objects in place.
    pub fn put_frame(&self, camera_id: CameraId, epoch: u64, frame: Arc<Frame>) {
        let slot = self.slot_or_insert(camera_id);
        let mut g = slot.lock();
        if g.epoch != epoch {
            return;
        }
        match g.entry.as_mut() {
            Some(entry) => entry.frame = frame,
            None => {
                g.entry = Some(LatestEntry {
                    frame,
                    objects: Arc::new(Vec::new()),
                    objects_frame_id: None,
                });
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
        let Some(slot) = self.slot(camera_id) else {
            return;
        };
        let mut g = slot.lock();
        if g.epoch != epoch {
            return;
        }
        if let Some(entry) = g.entry.as_mut() {
            entry.objects = objects;
            entry.objects_frame_id = Some(frame_id);
        }
    }

    pub fn get(&self, camera_id: CameraId) -> Option<LatestEntry> {
        self.slot(camera_id)?.lock().entry.clone()
    }

    /// Drop the camera's entry and retire its epoch, so a writer still
    /// draining cannot repopulate it.
    pub fn clear(&self, camera_id: CameraId) {
        let slot = self.slot_or_insert(camera_id);
        let mut g = slot.lock();
        g.entry = None;
        g.epoch += 1;
    }

    pub fn cameras(&self) -> Vec<CameraId> {
        self.slots
            .read()
            .iter()
            .filter(|(_, slot)| slot.lock().entry.is_some())
            .map(|(id, _)| *id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nexus_types::{BBox, PixelFormat};

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

    fn track(track_id: u64) -> TrackedObject {
        TrackedObject {
            track_id,
            label: "person".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: 1.0,
                y1: 1.0,
                x2: 2.0,
                y2: 2.0,
            },
            detection_bbox: None,
            age_frames: 1,
            age_ms: 100,
            attributes: Default::default(),
        }
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
        assert_eq!(
            got.frame.frame_id, 50,
            "a completed inference rewound the live frame"
        );
        assert_eq!(got.objects_frame_id, Some(12));
    }

    /// A restart keeps the last frame so the wall does not blank, but the
    /// previous session's boxes do not describe the new session's frames —
    /// and since `frame_id` restarts at 1 per source, a retained
    /// `objects_frame_id` could collide with a live id and claim a match
    /// that never happened.
    #[test]
    fn a_new_session_does_not_inherit_the_previous_sessions_objects() {
        let cache = LatestFrameCache::new();
        let epoch = cache.begin_session(7);
        cache.put_frame(7, epoch, frame(7));
        cache.put_objects(7, epoch, 1, Arc::new(vec![track(7)]));
        assert_eq!(cache.get(7).unwrap().objects.len(), 1);

        cache.begin_session(7);

        let got = cache.get(7).unwrap();
        assert_eq!(got.frame.frame_id, 1, "a restart blanked the wall");
        assert!(got.objects.is_empty(), "stale boxes survived a restart");
        assert_eq!(
            got.objects_frame_id, None,
            "a restarted camera claimed inference results it has not produced"
        );
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

    /// 29 cameras at 8 fps each write through this cache. When one lock
    /// covered every camera, camera 3's write excluded camera 17's and the
    /// readers queued behind both. A camera's writes must only ever wait on
    /// that same camera. The check fails by timeout only on regression; on
    /// the passing path camera 9's calls complete without waiting.
    #[test]
    fn a_write_for_one_camera_is_not_blocked_by_another_cameras_lock() {
        let cache = Arc::new(LatestFrameCache::new());
        let a = cache.begin_session(7);
        cache.put_frame(7, a, frame(7));

        let held = cache.slot_or_insert(7);
        let guard = held.lock();
        let (tx, rx) = std::sync::mpsc::channel();
        let c = cache.clone();
        let worker = std::thread::spawn(move || {
            let b = c.begin_session(9);
            c.put_frame(9, b, frame(9));
            c.put_objects(9, b, 1, Arc::new(vec![]));
            let got = c.get(9).map(|e| e.objects_frame_id);
            let _ = tx.send(got);
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(5));
        drop(guard);
        worker.join().unwrap();
        assert_eq!(
            got.expect("camera 9's write waited on camera 7's lock"),
            Some(Some(1))
        );
    }

    /// Partitioning must not reopen BUG-136: a superseded writer racing
    /// `begin_session` and `clear` on the same camera still loses, however
    /// the threads interleave.
    #[test]
    fn superseded_writers_racing_session_changes_never_land() {
        let cache = Arc::new(LatestFrameCache::new());
        let old = cache.begin_session(7);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let (c, stop) = (cache.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut stale = (*frame(7)).clone();
                    stale.frame_id = 1;
                    let stale = Arc::new(stale);
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        c.put_frame(7, old, stale.clone());
                        c.put_objects(7, old, 1, Arc::new(vec![track(1)]));
                    }
                })
            })
            .collect();

        for _ in 0..1_000 {
            let epoch = cache.begin_session(7);
            let mut live = (*frame(7)).clone();
            live.frame_id = 1_000;
            cache.put_frame(7, epoch, Arc::new(live));
            let got = cache.get(7).unwrap();
            assert_eq!(got.frame.frame_id, 1_000, "a superseded frame landed");
            assert_eq!(got.objects_frame_id, None, "superseded objects landed");

            cache.clear(7);
            assert!(cache.get(7).is_none(), "a retired writer repopulated");
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for w in writers {
            w.join().unwrap();
        }
        assert!(cache.get(7).is_none());
    }
}

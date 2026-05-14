//! H.264 NAL ring buffer for the pre-roll feature — M2.1 Stage B PR B8.
//!
//! Pure Rust, GStreamer-free so it unit-tests on any platform.
//! Holds a deque of [`NalSample`] grouped into [`Gop`]s (a GOP starts
//! at a keyframe and runs until the next keyframe). On insert the
//! buffer trims oldest GOPs until the duration from the oldest
//! sample's PTS to the newest sample's PTS is `<=` the configured
//! cap.
//!
//! Design rules:
//!
//! * **GOP-aligned trimming.** The output of the ring buffer is meant
//!   to be appended to a fresh mp4 file via a fresh decoder; the
//!   first sample MUST therefore be a keyframe (delta-frame leads
//!   produce the green-block decode artefacts NVR users hate).
//!   Trimming drops whole GOPs at a time.
//!
//! * **Wall-clock-free.** All durations are derived from PTS
//!   (`Option<Duration>`). A sample with no PTS (rare on RTSP H.264
//!   sources, but possible during stream start-up) is appended but
//!   doesn't advance the trim watermark. This keeps the buffer's
//!   timing logic deterministic in tests and immune to host clock
//!   jitter.
//!
//! * **Bounded memory by *duration*, not byte count.** A camera with
//!   high bitrate gets fewer samples in the same window than a low
//!   bitrate one — both keep the same ~`pre_roll_secs` of history.
//!
//! * **Snapshot semantics.** [`NalRingBuffer::snapshot`] returns a
//!   flat `Vec<NalSample>` covering every sample currently in the
//!   buffer, ordered by arrival. Callers use it to seed an
//!   `appsrc` at clip-open time. The buffer continues filling
//!   independently of any snapshot — readers and the writer don't
//!   block each other.

use std::collections::VecDeque;
use std::time::Duration;

/// One H.264 sample as pulled off the GStreamer pipeline. Owns its
/// payload because the ring buffer outlives the appsink callback's
/// borrowed `gst::Buffer`.
#[derive(Debug, Clone)]
pub struct NalSample {
    /// Presentation timestamp from the source (RTP). `None` is
    /// possible during stream start-up; samples with no PTS are
    /// kept but don't move the trim watermark.
    pub pts: Option<Duration>,
    /// Decode timestamp. Often equal to `pts` on intra-only or
    /// low-latency live streams; preserved verbatim so the muxer
    /// downstream can reconstruct B-frame ordering.
    pub dts: Option<Duration>,
    /// True iff this sample is a keyframe (IDR / non-delta).
    /// Drives GOP boundary detection.
    pub is_keyframe: bool,
    /// Raw H.264 NAL bytes in the source's stream-format. The
    /// pre-roll ingester normalises to byte-stream (`Annex-B`)
    /// before pushing here so all samples share one parser path.
    pub data: Vec<u8>,
}

/// One closed group-of-pictures: keyframe + zero or more
/// delta-frames. The buffer always holds whole GOPs so a snapshot
/// starts on a keyframe.
#[derive(Debug, Clone)]
struct Gop {
    samples: Vec<NalSample>,
}

impl Gop {
    fn new(keyframe: NalSample) -> Self {
        Self {
            samples: vec![keyframe],
        }
    }

    fn first_pts(&self) -> Option<Duration> {
        // The keyframe's PTS, or fall back to the first delta-frame
        // that does have a PTS (defensive against missing PTS on
        // the I-frame).
        self.samples.iter().find_map(|s| s.pts)
    }
}

/// Rolling H.264 NAL window, GOP-aligned, capped at `max_duration`.
#[derive(Debug)]
pub struct NalRingBuffer {
    gops: VecDeque<Gop>,
    /// Inclusive upper bound on (newest PTS - oldest PTS). Trim
    /// pops from the front until this is satisfied.
    max_duration: Duration,
    /// Last PTS we observed across all samples (keyframe or not).
    /// Cached so trim doesn't have to walk the whole buffer.
    newest_pts: Option<Duration>,
}

impl NalRingBuffer {
    pub fn new(max_duration: Duration) -> Self {
        Self {
            gops: VecDeque::new(),
            max_duration,
            newest_pts: None,
        }
    }

    /// Total number of samples currently held.
    pub fn sample_count(&self) -> usize {
        self.gops.iter().map(|g| g.samples.len()).sum()
    }

    /// Number of complete GOPs currently held.
    pub fn gop_count(&self) -> usize {
        self.gops.len()
    }

    /// Approximate buffered duration — the difference between the
    /// oldest and newest PTS we've seen. Returns
    /// `Duration::ZERO` for an empty buffer or one that hasn't
    /// observed any PTS yet.
    pub fn buffered_duration(&self) -> Duration {
        match (self.oldest_pts(), self.newest_pts) {
            (Some(old), Some(new)) if new >= old => new - old,
            _ => Duration::ZERO,
        }
    }

    fn oldest_pts(&self) -> Option<Duration> {
        self.gops.front().and_then(|g| g.first_pts())
    }

    /// Append a sample and trim. Samples arriving before the first
    /// keyframe are dropped — without a keyframe head we have nothing
    /// to anchor a GOP on, and a snapshot would start on a delta
    /// frame and decode to garbage.
    pub fn push(&mut self, sample: NalSample) {
        if let Some(pts) = sample.pts {
            self.newest_pts = Some(self.newest_pts.map_or(pts, |n| n.max(pts)));
        }
        if sample.is_keyframe {
            // New GOP. Pre-trim before pushing so a high-FPS camera
            // can't temporarily spike memory above the cap.
            self.trim();
            self.gops.push_back(Gop::new(sample));
        } else if let Some(g) = self.gops.back_mut() {
            g.samples.push(sample);
        }
        // else: pre-keyframe delta — silently drop.
        self.trim();
    }

    fn trim(&mut self) {
        let Some(newest) = self.newest_pts else {
            return;
        };
        // Pop GOPs from the front while keeping at least one GOP
        // alive (so a snapshot taken right after trim still has
        // something useful to flush).
        while self.gops.len() > 1 {
            let first_pts = match self.gops.front().and_then(|g| g.first_pts()) {
                Some(t) => t,
                None => break,
            };
            // We're trying to keep `newest - first_pts <= max_duration`
            // for the GOP that will become the new front after
            // popping this one. Compute the second GOP's first PTS;
            // if dropping the first GOP would still leave the
            // remainder within the window, pop. Otherwise we're as
            // tight as we can get without violating the
            // "always-keep-one-GOP" invariant.
            let second_pts = match self.gops.get(1).and_then(|g| g.first_pts()) {
                Some(t) => t,
                None => break,
            };
            if newest.saturating_sub(second_pts) > self.max_duration {
                // Even if we drop the front, the remainder is still
                // over budget. Pop and re-evaluate.
                self.gops.pop_front();
            } else if newest.saturating_sub(first_pts) > self.max_duration {
                // Dropping the front brings us back within budget.
                self.gops.pop_front();
                break;
            } else {
                // Already within budget — nothing to do.
                break;
            }
        }
    }

    /// Snapshot every buffered sample, in arrival order. Returned
    /// vec starts on a keyframe (or is empty if the buffer is). The
    /// buffer itself is unchanged — callers can take repeated
    /// snapshots and the live stream keeps flowing.
    pub fn snapshot(&self) -> Vec<NalSample> {
        let mut out = Vec::with_capacity(self.sample_count());
        for g in &self.gops {
            out.extend_from_slice(&g.samples);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(pts_ms: u64, payload: &[u8]) -> NalSample {
        NalSample {
            pts: Some(Duration::from_millis(pts_ms)),
            dts: Some(Duration::from_millis(pts_ms)),
            is_keyframe: true,
            data: payload.to_vec(),
        }
    }

    fn delta(pts_ms: u64, payload: &[u8]) -> NalSample {
        NalSample {
            pts: Some(Duration::from_millis(pts_ms)),
            dts: Some(Duration::from_millis(pts_ms)),
            is_keyframe: false,
            data: payload.to_vec(),
        }
    }

    #[test]
    fn empty_buffer_snapshots_to_empty() {
        let b = NalRingBuffer::new(Duration::from_secs(5));
        assert!(b.snapshot().is_empty());
        assert_eq!(b.sample_count(), 0);
        assert_eq!(b.gop_count(), 0);
        assert_eq!(b.buffered_duration(), Duration::ZERO);
    }

    #[test]
    fn delta_frames_before_first_keyframe_are_dropped() {
        let mut b = NalRingBuffer::new(Duration::from_secs(5));
        b.push(delta(0, b"d0"));
        b.push(delta(33, b"d1"));
        assert_eq!(b.sample_count(), 0, "no keyframe yet -> nothing buffered");
    }

    #[test]
    fn snapshot_starts_on_keyframe_and_preserves_order() {
        let mut b = NalRingBuffer::new(Duration::from_secs(5));
        b.push(key(0, b"k0"));
        b.push(delta(33, b"d0"));
        b.push(delta(66, b"d1"));
        let snap = b.snapshot();
        assert_eq!(snap.len(), 3);
        assert!(snap[0].is_keyframe);
        assert_eq!(snap[0].data, b"k0");
        assert_eq!(snap[1].data, b"d0");
        assert_eq!(snap[2].data, b"d1");
    }

    #[test]
    fn trims_to_max_duration_dropping_whole_gops() {
        // 5s cap; push 5 GOPs of 2s each (PTS 0, 2, 4, 6, 8 seconds).
        // After the 5th GOP the buffer should retain at least the
        // newest GOP and as many older ones as fit in 5s.
        let mut b = NalRingBuffer::new(Duration::from_secs(5));
        for i in 0..5 {
            let pts_ms = i * 2000;
            b.push(key(pts_ms, format!("k{i}").as_bytes()));
            b.push(delta(pts_ms + 1000, format!("d{i}").as_bytes()));
        }
        // Newest PTS = 9000ms (the last delta). The trimmer keeps
        // GOPs whose head is within 5000ms of newest_pts. Heads are
        // at 0,2,4,6,8 seconds — within 5s of 9s are 4,6,8 (the
        // 4-second head means newest - head = 5s exactly which is
        // boundary-allowed; the 2-second head means 7s > 5s so it
        // gets evicted).
        let snap = b.snapshot();
        assert!(
            snap.first().unwrap().is_keyframe,
            "first sample after trim must still be a keyframe"
        );
        // GOP at PTS=2s should be evicted (delta 7s > 5s).
        let starts: Vec<_> = snap
            .iter()
            .filter(|s| s.is_keyframe)
            .map(|s| s.pts.unwrap().as_millis() as u64)
            .collect();
        assert!(
            starts.iter().all(|&t| 9000u64.saturating_sub(t) <= 5000),
            "every retained GOP head must be within 5s of newest, got {starts:?}"
        );
        // And the buffer must NEVER trim away its only remaining GOP.
        assert!(b.gop_count() >= 1);
    }

    #[test]
    fn always_retains_at_least_one_gop_under_pressure() {
        // Cap = 1ms; even after pushing many GOPs spaced 100ms apart
        // (each one over budget on its own), the trimmer must keep
        // the newest GOP alive so a snapshot is decodable.
        let mut b = NalRingBuffer::new(Duration::from_millis(1));
        for i in 0..10 {
            b.push(key(i * 100, format!("k{i}").as_bytes()));
        }
        assert!(b.gop_count() >= 1);
        let snap = b.snapshot();
        assert!(!snap.is_empty());
        assert!(snap[0].is_keyframe);
    }

    #[test]
    fn samples_with_no_pts_are_buffered_but_dont_move_watermark() {
        let mut b = NalRingBuffer::new(Duration::from_secs(5));
        b.push(key(0, b"k0"));
        let no_pts = NalSample {
            pts: None,
            dts: None,
            is_keyframe: false,
            data: b"d0".to_vec(),
        };
        b.push(no_pts);
        b.push(delta(33, b"d1"));
        assert_eq!(b.sample_count(), 3);
        // Newest PTS should be 33ms (the no-PTS sample didn't bump
        // the watermark).
        assert_eq!(b.buffered_duration(), Duration::from_millis(33));
    }

    #[test]
    fn snapshot_does_not_drain_the_buffer() {
        let mut b = NalRingBuffer::new(Duration::from_secs(5));
        b.push(key(0, b"k0"));
        b.push(delta(33, b"d0"));
        let s1 = b.snapshot();
        let s2 = b.snapshot();
        assert_eq!(s1.len(), 2);
        assert_eq!(s2.len(), 2);
        assert_eq!(b.sample_count(), 2);
    }
}

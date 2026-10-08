//! Post-roll deferred-close decision for the per-camera clip recorder.
//!
//! Pre-B3 behaviour: the supervisor closed the open clip the instant
//! `live_track_count` hit zero. That produced a sea of sub-2s clips
//! whenever a tracker briefly dropped a track and re-acquired it
//! (lighting flicker, occlusion, low-confidence frames).
//!
//! Post-roll fixes this by introducing a configurable grace window
//! (`runtime.clips.post_roll_secs`, default 10s) between "no live
//! tracks" and "close the clip". If a new motion event arrives inside
//! the window, the pending close is cancelled and the clip stays open
//! — so two intermittent bursts of motion within `post_roll_secs`
//! produce ONE clip, not two.
//!
//! Pre-roll (recording the N seconds *before* motion starts) is the
//! sibling feature and is intentionally **not** in this PR. Pre-roll
//! requires an always-running shadow appsink ringbuffer per camera and
//! is being shipped as a separate follow-up; see ARCHITECTURE.md.
//!
//! This module is pure: no I/O, no async, no clock — the supervisor
//! pumps it once per frame with `(frame.captured_mono, has_live_motion)`
//! and acts on the returned [`PostRollAction`]. That keeps it trivially
//! testable without a tokio runtime or virtual time.

use std::time::{Duration, Instant};

/// Outcome of a single [`PostRoll::tick`] call. The supervisor uses
/// these to drive `recorder.close` calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostRollAction {
    /// Motion is live (or grace window hasn't elapsed). Keep the clip
    /// open and do nothing.
    KeepOpen,
    /// Grace window elapsed and motion never resumed. Close the clip
    /// using `frame.captured_at` as `ended_at`.
    CloseNow,
}

/// State machine for the deferred close. One instance per camera, owned
/// by the per-camera supervisor task.
///
/// Construct with [`PostRoll::new`]; call [`PostRoll::tick`] every
/// frame. `reset` is called on shutdown / pipeline-end so a freshly
/// reused supervisor doesn't carry over stale pending state.
#[derive(Debug)]
pub struct PostRoll {
    grace: Duration,
    pending_close_at: Option<Instant>,
}

impl PostRoll {
    /// `grace_secs == 0` disables post-roll: every tick with no motion
    /// returns [`PostRollAction::CloseNow`] (matches pre-B3 behaviour).
    pub fn new(grace_secs: u32) -> Self {
        Self {
            grace: Duration::from_secs(u64::from(grace_secs)),
            pending_close_at: None,
        }
    }

    /// Pump one frame's lifecycle decision.
    ///
    /// * `now` — frame's `captured_mono` stamp. Using the frame's own
    ///   stamp (not the time it is processed) keeps the grace window
    ///   correct under stalled streams, and a monotonic one keeps a step
    ///   of the wall clock from ending or extending it.
    /// * `has_live_motion` — true if `live_track_count(camera_id) > 0`
    ///   at this frame. (Born-this-frame counts as live; the supervisor
    ///   already opened the clip before reaching here.)
    pub fn tick(&mut self, now: Instant, has_live_motion: bool) -> PostRollAction {
        if has_live_motion {
            // Cancel any pending close — motion is back inside the
            // grace window.
            self.pending_close_at = None;
            return PostRollAction::KeepOpen;
        }
        // No live motion this frame.
        if self.grace.is_zero() {
            // Post-roll disabled. Close immediately.
            return PostRollAction::CloseNow;
        }
        match self.pending_close_at {
            None => {
                // First "no motion" frame — schedule the close and
                // keep the clip open.
                self.pending_close_at = Some(now + self.grace);
                PostRollAction::KeepOpen
            }
            Some(close_at) if now >= close_at => {
                // Grace window elapsed without motion returning.
                // Clear pending state and tell the caller to close.
                self.pending_close_at = None;
                PostRollAction::CloseNow
            }
            Some(_) => {
                // Still inside grace window.
                PostRollAction::KeepOpen
            }
        }
    }

    /// Drop any pending close. Called on pipeline shutdown / when the
    /// supervisor performs its own final close — prevents the next
    /// resumed supervisor from seeing stale "close pending" state.
    pub fn reset(&mut self) {
        self.pending_close_at = None;
    }

    /// True when the grace timer is armed but hasn't elapsed yet.
    /// Used by the supervisor to gate metrics / debug logging.
    pub fn is_pending(&self) -> bool {
        self.pending_close_at.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> Instant {
        static T0: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
        *T0 + Duration::from_secs(secs)
    }

    #[test]
    fn motion_present_keeps_open_and_clears_pending() {
        let mut p = PostRoll::new(10);
        assert_eq!(p.tick(t(0), true), PostRollAction::KeepOpen);
        assert!(!p.is_pending());
        // No motion arms the timer.
        assert_eq!(p.tick(t(1), false), PostRollAction::KeepOpen);
        assert!(p.is_pending());
        // Motion comes back — pending cleared, clip stays open.
        assert_eq!(p.tick(t(2), true), PostRollAction::KeepOpen);
        assert!(!p.is_pending());
    }

    #[test]
    fn no_motion_arms_timer_first_then_keeps_open_until_grace_elapses() {
        let mut p = PostRoll::new(10);
        // First no-motion frame → arm.
        assert_eq!(p.tick(t(0), false), PostRollAction::KeepOpen);
        // Mid-grace → keep open.
        assert_eq!(p.tick(t(5), false), PostRollAction::KeepOpen);
        // Just before grace expires → still open.
        assert_eq!(p.tick(t(9), false), PostRollAction::KeepOpen);
        // At grace boundary → close.
        assert_eq!(p.tick(t(10), false), PostRollAction::CloseNow);
        assert!(!p.is_pending(), "pending must clear after CloseNow");
    }

    #[test]
    fn motion_resuming_inside_grace_cancels_close() {
        let mut p = PostRoll::new(10);
        // Arm.
        assert_eq!(p.tick(t(0), false), PostRollAction::KeepOpen);
        // Motion returns at t=5 — cancel.
        assert_eq!(p.tick(t(5), true), PostRollAction::KeepOpen);
        // At what would have been the original grace expiry (t=10)
        // we still have motion → no close.
        assert_eq!(p.tick(t(10), true), PostRollAction::KeepOpen);
        // Motion stops again → re-arm with a NEW window from t=11.
        assert_eq!(p.tick(t(11), false), PostRollAction::KeepOpen);
        // Old window (t=10..t=15) must NOT trigger early close.
        assert_eq!(p.tick(t(15), false), PostRollAction::KeepOpen);
        // New window expires at t=21.
        assert_eq!(p.tick(t(21), false), PostRollAction::CloseNow);
    }

    #[test]
    fn zero_grace_closes_immediately_on_no_motion() {
        let mut p = PostRoll::new(0);
        assert_eq!(p.tick(t(0), true), PostRollAction::KeepOpen);
        // No motion → immediate close (pre-B3 behaviour).
        assert_eq!(p.tick(t(1), false), PostRollAction::CloseNow);
        assert!(!p.is_pending());
        // And again — no implicit "re-arm".
        assert_eq!(p.tick(t(2), false), PostRollAction::CloseNow);
    }

    #[test]
    fn reset_drops_pending_window() {
        let mut p = PostRoll::new(10);
        assert_eq!(p.tick(t(0), false), PostRollAction::KeepOpen);
        assert!(p.is_pending());
        p.reset();
        assert!(!p.is_pending());
        // After reset, next no-motion frame must re-arm a fresh window.
        assert_eq!(p.tick(t(1), false), PostRollAction::KeepOpen);
        assert!(p.is_pending());
    }
}

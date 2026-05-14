//! Per-camera static-object filter. Mirrors v1's
//! `EventFilter::staticVehicle*` block in
//! `src/tracking/event_filter.cpp`.
//!
//! Vehicles whose smoothed per-frame movement stays below
//! `significant_movement_pixels` for `dwell_frames` consecutive frames
//! are *promoted* to "static" and dropped from the rule-eval slice
//! (i.e. parked cars stop firing alerts). Promoted tracks are written
//! to a per-camera anchor registry on disk so the suppression survives
//! a restart.
//!
//! A previously-suppressed track that starts moving again
//! (significant-movement EMA crossed for
//! `significant_movement_frames` consecutive frames) gets demoted —
//! the matching anchor is erased from the registry and subsequent
//! frames flow through to the rule layer.
//!
//! State surface: one `StaticObjectFilter` per camera, owned by the
//! supervisor task. Per-track state is keyed by `track_id` only (the
//! filter is already scoped to its camera). The on-disk registry is
//! the only cross-restart state.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use nexus_config::StaticObjectConfig;
use nexus_types::{CameraId, Frame, TrackId, TrackedObject};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// In-memory per-track state for the suppression FSM.
#[derive(Debug, Default, Clone)]
struct PerTrackState {
    last_center: Option<(f32, f32)>,
    /// EMA of per-frame center movement magnitude (px).
    movement_ema: f64,
    /// Frames spent below the significant-movement threshold.
    static_frames: u32,
    /// Consecutive frames at-or-above the threshold (for demotion).
    moving_consecutive_frames: u32,
    /// Whether this track has crossed `dwell_frames` and is currently
    /// being suppressed.
    static_promoted: bool,
}

/// Persisted record of a known-static vehicle location for a camera.
/// `label` is the lowercased TrackedObject label; centers are in
/// pixel coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticAnchor {
    pub label: String,
    pub center_x: f32,
    pub center_y: f32,
}

/// On-disk shape of the per-camera registry. v1's shape was
/// `{cameras: [...]}` — here we split per file (one file per camera)
/// so concurrent supervisors don't race on a shared file. The
/// `version` field exists for forward-compat.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryFile {
    version: u32,
    camera_id: CameraId,
    anchors: Vec<StaticAnchor>,
}

pub struct StaticObjectFilter {
    cfg: StaticObjectConfig,
    camera_id: CameraId,
    /// `None` disables disk persistence (used by tests and by cameras
    /// where `parking_lot_mode = false`).
    persistence_path: Option<PathBuf>,
    state_by_track: HashMap<TrackId, PerTrackState>,
    anchors: Vec<StaticAnchor>,
}

impl StaticObjectFilter {
    /// Build a filter and load any persisted anchors from `persistence_path`.
    /// Missing or unreadable registry files are logged + treated as an
    /// empty registry (no panic — operators may have wiped it).
    pub fn new(
        cfg: StaticObjectConfig,
        camera_id: CameraId,
        persistence_path: Option<PathBuf>,
    ) -> Self {
        let anchors = match &persistence_path {
            Some(path) if cfg.persistence_enabled => Self::load_registry(camera_id, path),
            _ => Vec::new(),
        };
        Self {
            cfg,
            camera_id,
            persistence_path,
            state_by_track: HashMap::new(),
            anchors,
        }
    }

    pub fn name(&self) -> &'static str {
        "static_object"
    }

    /// Read-only view of the current persistent anchor set. Useful for
    /// tests; no production code should mutate the registry directly.
    pub fn anchors(&self) -> &[StaticAnchor] {
        &self.anchors
    }

    /// Drop tracks from `objects` whose smoothed motion has settled
    /// below threshold for `dwell_frames` consecutive frames (or that
    /// match a persisted anchor and aren't moving again yet).
    /// Mutates `objects` in place; mutates internal per-track state
    /// and the persistent anchor registry.
    pub fn filter(&mut self, _frame: &Frame, objects: &mut Vec<TrackedObject>) {
        let mut dirty = false;

        // Walk the object list, classifying each. Borrow-checker: pull
        // values out before the per-track state borrow.
        let cfg_dwell = self.cfg.dwell_frames.max(1);
        let cfg_sig_px = self.cfg.significant_movement_pixels.max(1) as f64;
        let cfg_sig_frames = self.cfg.significant_movement_frames.max(1);
        let cfg_alpha = self.cfg.movement_ema_alpha.clamp(0.01, 1.0) as f64;
        let cfg_match_dist = self.cfg.match_distance_pixels.max(1) as f32;
        let cfg_persistence = self.cfg.persistence_enabled;

        // Build a "suppress?" verdict per-index without removing yet,
        // because we touch `self.anchors` from inside the loop.
        let mut to_drop: Vec<bool> = Vec::with_capacity(objects.len());

        for o in objects.iter() {
            if !is_vehicle_label(&o.label) {
                to_drop.push(false);
                continue;
            }

            let center = o.bbox.center();
            let state = self.state_by_track.entry(o.track_id).or_default();

            // ---- update movement EMA ----
            let instant_movement = match state.last_center {
                Some((px, py)) => {
                    let dx = (center.0 - px) as f64;
                    let dy = (center.1 - py) as f64;
                    (dx * dx + dy * dy).sqrt()
                }
                None => 0.0,
            };
            if state.last_center.is_none() {
                state.movement_ema = instant_movement;
            } else {
                state.movement_ema =
                    cfg_alpha * instant_movement + (1.0 - cfg_alpha) * state.movement_ema;
            }
            state.last_center = Some(center);

            // ---- promote / demote counters ----
            let significant = state.movement_ema >= cfg_sig_px;
            if significant {
                state.moving_consecutive_frames = state.moving_consecutive_frames.saturating_add(1);
                state.static_frames = 0;
            } else {
                state.moving_consecutive_frames = 0;
                state.static_frames = state.static_frames.saturating_add(1);
            }

            if state.static_frames >= cfg_dwell {
                state.static_promoted = true;
            }

            // ---- registry-anchor check ----
            let label_lc = o.label.to_lowercase();
            let matched_anchor_index = if cfg_persistence {
                Self::match_anchor(&self.anchors, &label_lc, center, cfg_match_dist)
            } else {
                None
            };

            // Demote: matched a persistent anchor AND moving again.
            if let Some(idx) = matched_anchor_index {
                if state.moving_consecutive_frames >= cfg_sig_frames {
                    self.anchors.remove(idx);
                    dirty = true;
                }
            }

            // The post-demotion match (we may have just removed it):
            let still_matches_anchor = if cfg_persistence {
                Self::match_anchor(&self.anchors, &label_lc, center, cfg_match_dist).is_some()
            } else {
                false
            };

            let suppress = (still_matches_anchor || state.static_promoted)
                && state.moving_consecutive_frames < cfg_sig_frames;

            // Promote into the registry while we hold the suppression
            // verdict — doing this lazily on the next frame would lose
            // anchors across a fast restart.
            if state.static_promoted
                && cfg_persistence
                && Self::upsert_anchor(&mut self.anchors, &label_lc, center, cfg_match_dist)
            {
                dirty = true;
            }

            to_drop.push(suppress);
        }

        // Apply the verdict.
        let mut idx = 0;
        objects.retain(|_| {
            let keep = !to_drop[idx];
            idx += 1;
            keep
        });

        if dirty {
            self.save_registry();
        }
    }

    // ---------------------------------------------------------------------
    // Anchor helpers
    // ---------------------------------------------------------------------

    fn match_anchor(
        anchors: &[StaticAnchor],
        label_lc: &str,
        center: (f32, f32),
        max_dist_px: f32,
    ) -> Option<usize> {
        let max_sq = (max_dist_px * max_dist_px) as f64;
        for (i, a) in anchors.iter().enumerate() {
            if a.label != label_lc {
                continue;
            }
            let dx = (a.center_x - center.0) as f64;
            let dy = (a.center_y - center.1) as f64;
            if dx * dx + dy * dy <= max_sq {
                return Some(i);
            }
        }
        None
    }

    /// Inserts or merges. Returns true if the registry mutated.
    fn upsert_anchor(
        anchors: &mut Vec<StaticAnchor>,
        label_lc: &str,
        center: (f32, f32),
        max_dist_px: f32,
    ) -> bool {
        if let Some(idx) = Self::match_anchor(anchors, label_lc, center, max_dist_px) {
            // Average toward the new observation — same shape as v1
            // (`(old + new) * 0.5`). Tiny drift; only triggers a save
            // when the average actually moves the centroid.
            let prev = (anchors[idx].center_x, anchors[idx].center_y);
            let new_cx = (anchors[idx].center_x + center.0) * 0.5;
            let new_cy = (anchors[idx].center_y + center.1) * 0.5;
            if (new_cx - prev.0).abs() < 0.01 && (new_cy - prev.1).abs() < 0.01 {
                return false;
            }
            anchors[idx].center_x = new_cx;
            anchors[idx].center_y = new_cy;
            true
        } else {
            anchors.push(StaticAnchor {
                label: label_lc.to_string(),
                center_x: center.0,
                center_y: center.1,
            });
            true
        }
    }

    // ---------------------------------------------------------------------
    // Persistence
    // ---------------------------------------------------------------------

    fn load_registry(camera_id: CameraId, path: &std::path::Path) -> Vec<StaticAnchor> {
        if !path.exists() {
            return Vec::new();
        }
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                warn!(camera_id, path = %path.display(), "static-object registry read failed: {e}");
                return Vec::new();
            }
        };
        let doc: RegistryFile = match serde_json::from_slice(&bytes) {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    camera_id,
                    path = %path.display(),
                    "static-object registry parse failed (treating as empty): {e}"
                );
                return Vec::new();
            }
        };
        if doc.camera_id != camera_id {
            warn!(
                camera_id,
                file_camera_id = doc.camera_id,
                path = %path.display(),
                "static-object registry camera_id mismatch — ignoring",
            );
            return Vec::new();
        }
        doc.anchors
    }

    fn save_registry(&self) {
        let Some(path) = &self.persistence_path else {
            return;
        };
        if !self.cfg.persistence_enabled {
            return;
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = fs::create_dir_all(parent) {
                    warn!(camera_id = self.camera_id, path = %path.display(),
                        "static-object registry mkdir failed: {e}");
                    return;
                }
            }
        }
        let doc = RegistryFile {
            version: 1,
            camera_id: self.camera_id,
            anchors: self.anchors.clone(),
        };
        match serde_json::to_vec_pretty(&doc) {
            Ok(bytes) => {
                if let Err(e) = fs::write(path, bytes) {
                    warn!(camera_id = self.camera_id, path = %path.display(),
                        "static-object registry write failed: {e}");
                }
            }
            Err(e) => {
                warn!(
                    camera_id = self.camera_id,
                    "static-object registry serialize failed: {e}"
                );
            }
        }
    }
}

fn is_vehicle_label(label: &str) -> bool {
    let lc = label.to_lowercase();
    lc.starts_with("vehicle") || lc == "car" || lc == "truck" || lc == "bus" || lc == "motorcycle"
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use nexus_types::{BBox, Frame, PixelFormat, TrackedObject};
    use std::sync::Arc;

    fn frame(camera_id: CameraId, frame_id: u64, ms: i64) -> Frame {
        Frame {
            camera_id,
            frame_id,
            captured_at: Utc.timestamp_millis_opt(ms).unwrap(),
            width: 1920,
            height: 1080,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![]),
            trace_id: format!("trace-{frame_id}"),
        }
    }

    fn vehicle(track_id: TrackId, cx: f32, cy: f32) -> TrackedObject {
        TrackedObject {
            track_id,
            label: "vehicle.car".into(),
            confidence: 0.95,
            bbox: BBox {
                x1: cx - 25.0,
                y1: cy - 15.0,
                x2: cx + 25.0,
                y2: cy + 15.0,
            },
            age_frames: 1,
            age_ms: 33,
            attributes: serde_json::Map::new(),
        }
    }

    fn person(track_id: TrackId, cx: f32, cy: f32) -> TrackedObject {
        TrackedObject {
            track_id,
            label: "person".into(),
            confidence: 0.95,
            bbox: BBox {
                x1: cx - 10.0,
                y1: cy - 20.0,
                x2: cx + 10.0,
                y2: cy + 20.0,
            },
            age_frames: 1,
            age_ms: 33,
            attributes: serde_json::Map::new(),
        }
    }

    #[test]
    fn parked_vehicle_is_suppressed_after_dwell_frames() {
        // Tight thresholds so the test runs in 4 frames.
        let cfg = StaticObjectConfig {
            dwell_frames: 3,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 5,
            persistence_enabled: false,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);

        // Frames 0..3: stationary at (500, 500).
        for i in 0..3 {
            let mut objs = vec![vehicle(7, 500.0, 500.0)];
            f.filter(&frame(1, i, i as i64 * 33), &mut objs);
            // Promotion happens at frame index 2 (static_frames == 3).
            // Suppression is in the SAME frame the promotion happens.
            if i < 2 {
                assert_eq!(objs.len(), 1, "frame {i}: not yet promoted");
            } else {
                assert!(objs.is_empty(), "frame {i}: should be suppressed");
            }
        }
    }

    #[test]
    fn moving_vehicle_is_not_suppressed() {
        let cfg = StaticObjectConfig {
            dwell_frames: 3,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 5,
            persistence_enabled: false,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);

        // Slide 50px each frame — well above threshold of 10.
        for i in 0..6 {
            let mut objs = vec![vehicle(7, 500.0 + i as f32 * 50.0, 500.0)];
            f.filter(&frame(1, i, i as i64 * 33), &mut objs);
            assert_eq!(objs.len(), 1, "frame {i}: moving track must pass through");
        }
    }

    #[test]
    fn non_vehicle_labels_bypass_filter() {
        // Even a perfectly stationary person must NEVER be dropped.
        let cfg = StaticObjectConfig {
            dwell_frames: 1,
            significant_movement_pixels: 1,
            significant_movement_frames: 1,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 5,
            persistence_enabled: false,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);
        for i in 0..5 {
            let mut objs = vec![person(42, 100.0, 100.0)];
            f.filter(&frame(1, i, i as i64 * 33), &mut objs);
            assert_eq!(objs.len(), 1, "frame {i}: person must pass");
        }
    }

    #[test]
    fn promoted_track_writes_persistent_anchor() {
        let cfg = StaticObjectConfig {
            dwell_frames: 2,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 20,
            persistence_enabled: true,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);
        for i in 0..3 {
            let mut objs = vec![vehicle(9, 800.0, 400.0)];
            f.filter(&frame(1, i, i as i64 * 33), &mut objs);
        }
        assert_eq!(f.anchors().len(), 1);
        assert_eq!(f.anchors()[0].label, "vehicle.car");
        // Center should be very close to (800, 400).
        assert!((f.anchors()[0].center_x - 800.0).abs() < 1.0);
        assert!((f.anchors()[0].center_y - 400.0).abs() < 1.0);
    }

    #[test]
    fn fresh_track_matching_existing_anchor_is_suppressed() {
        // Pre-seed with an anchor on disk via load.
        let cfg = StaticObjectConfig {
            dwell_frames: 999,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 30,
            persistence_enabled: true,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);
        f.anchors.push(StaticAnchor {
            label: "vehicle.car".into(),
            center_x: 500.0,
            center_y: 500.0,
        });

        // A fresh track sitting near the anchor should be suppressed
        // immediately, even though `dwell_frames` is huge — that's the
        // whole point of persistence.
        let mut objs = vec![vehicle(11, 510.0, 505.0)];
        f.filter(&frame(1, 0, 0), &mut objs);
        assert!(objs.is_empty(), "should be suppressed via anchor match");
    }

    #[test]
    fn anchor_is_erased_when_vehicle_starts_moving_again() {
        let cfg = StaticObjectConfig {
            dwell_frames: 999,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 30,
            persistence_enabled: true,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);
        f.anchors.push(StaticAnchor {
            label: "vehicle.car".into(),
            center_x: 500.0,
            center_y: 500.0,
        });

        // Frame 0: at the anchor, suppressed.
        let mut objs = vec![vehicle(11, 500.0, 500.0)];
        f.filter(&frame(1, 0, 0), &mut objs);
        assert!(objs.is_empty());

        // Frames 1+: 12px/frame — above the 10px movement threshold but
        // small enough to stay inside the 30px anchor-match radius for
        // a couple of frames so the anchor can be erased.
        let mut emerged = false;
        for i in 1..6 {
            let mut objs = vec![vehicle(11, 500.0 + i as f32 * 12.0, 500.0)];
            f.filter(&frame(1, i, i as i64 * 33), &mut objs);
            if !objs.is_empty() {
                emerged = true;
                break;
            }
        }
        assert!(emerged, "moving track should eventually be emitted again");
        assert!(f.anchors().is_empty(), "matching anchor should be erased");
    }

    #[test]
    fn registry_round_trips_through_disk() {
        let tmp = std::env::temp_dir().join(format!(
            "nexus-static-{}.json",
            std::process::id().wrapping_add(rand_suffix())
        ));
        let _ = std::fs::remove_file(&tmp);

        let cfg = StaticObjectConfig {
            dwell_frames: 2,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 20,
            persistence_enabled: true,
        };
        // Writer phase.
        {
            let mut f = StaticObjectFilter::new(cfg.clone(), 1, Some(tmp.clone()));
            for i in 0..3 {
                let mut objs = vec![vehicle(13, 700.0, 300.0)];
                f.filter(&frame(1, i, i as i64 * 33), &mut objs);
            }
            assert_eq!(f.anchors().len(), 1);
        }
        // Reader phase: a fresh filter should pick up the anchor.
        {
            let f = StaticObjectFilter::new(cfg, 1, Some(tmp.clone()));
            assert_eq!(f.anchors().len(), 1);
            assert_eq!(f.anchors()[0].label, "vehicle.car");
        }

        let _ = std::fs::remove_file(&tmp);
    }

    fn rand_suffix() -> u32 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    }
}

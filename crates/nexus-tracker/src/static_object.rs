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
use serde_json::Value;
use tracing::warn;

/// Attribute key stamped on a `TrackedObject` by [`StaticObjectFilter::classify`]
/// when the track has been promoted to "static" (parked vehicle, etc.).
/// Lives in `TrackedObject.attributes` so the wire schema doesn't need a
/// dedicated field and so the existing `motion.*` / `dwell.*` namespace
/// pattern is preserved.
///
/// The supervisor partitions on this attribute BEFORE invoking the rule
/// evaluator + motion lifecycle, so a `true` value here means "the live
/// viewer should still draw this box (de-emphasised) but no rule / clip
/// / motion event will fire on it".
pub const STATIC_ATTRIBUTE_KEY: &str = "tracker.is_static";

/// Diagnostic attribute keys stamped on every vehicle-labelled
/// `TrackedObject` that the filter has seen this frame. Exposes the
/// FSM's internal state so the live viewer can render an overlay
/// explaining why a given box was (or wasn't) promoted to static.
/// Cheap: three JSON numbers per vehicle track per frame.
pub const EMA_ATTRIBUTE_KEY: &str = "tracker.movement_ema";
pub const STATIC_FRAMES_ATTRIBUTE_KEY: &str = "tracker.static_frames";
pub const MOVING_FRAMES_ATTRIBUTE_KEY: &str = "tracker.moving_consecutive_frames";

/// Helper that mirrors the convention used by the supervisor: returns
/// `true` iff `o.attributes[STATIC_ATTRIBUTE_KEY] == true`. Centralised
/// here so callers don't reimplement the JSON unwrap pattern.
pub fn is_object_static(o: &TrackedObject) -> bool {
    matches!(
        o.attributes.get(STATIC_ATTRIBUTE_KEY),
        Some(Value::Bool(true))
    )
}

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
/// pixel coordinates. `last_seen_unix_ms` is the wall-clock time of
/// the most recent matching observation — stamped from the frame's
/// `captured_at` so a long offline gap correctly ages the anchor.
/// Optional so existing registry files (from before the TTL sweep
/// landed) deserialise unchanged; first matching observation after
/// load promotes it from `None` to `Some(frame_ms)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticAnchor {
    pub label: String,
    pub center_x: f32,
    pub center_y: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_unix_ms: Option<i64>,
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
    /// Phase 8.1 — extra (non-vehicle) lowercased label classes that are
    /// also eligible for static-anchor promotion (equipment: `ladder`,
    /// `wheelbarrow`, etc.). Empty preserves vehicle-only behaviour.
    extra_anchor_classes: Vec<String>,
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
        Self::with_anchor_classes(cfg, camera_id, persistence_path, Vec::new())
    }

    /// Like [`Self::new`] but also promotes the supplied lowercased
    /// label classes (Phase 8.1 `static_anchor_classes`) to anchors.
    pub fn with_anchor_classes(
        cfg: StaticObjectConfig,
        camera_id: CameraId,
        persistence_path: Option<PathBuf>,
        extra_anchor_classes: Vec<String>,
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
            extra_anchor_classes: extra_anchor_classes
                .into_iter()
                .map(|s| s.to_lowercase())
                .collect(),
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

    /// Wipe every persisted anchor and per-track state for this
    /// camera, then remove the on-disk registry file (so a restart
    /// doesn't resurrect what the operator just cleared). Used by
    /// `DELETE /api/v1/cameras/{id}/static-anchors` — the supervisor
    /// polls a shared clear-registry per frame and invokes this
    /// when an operator requests a reset.
    ///
    /// Existing parked vehicles still in view will re-promote
    /// naturally after `dwell_frames` static frames; this is the
    /// intended behaviour (operators clear the map because a
    /// vehicle drove off undetected, not because they want
    /// permanent suppression of an in-view car).
    pub fn clear(&mut self) {
        self.anchors.clear();
        self.state_by_track.clear();
        if let Some(path) = &self.persistence_path {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(
                        camera_id = self.camera_id,
                        path = %path.display(),
                        "static-object registry clear failed: {e}",
                    );
                }
            }
        }
    }

    /// Classify each track in `objects` as static-or-not, and stamp
    /// [`STATIC_ATTRIBUTE_KEY`] = `true` on the attributes map of any
    /// object that the suppression FSM has promoted (or that matches
    /// a persisted anchor and isn't moving again yet). Does NOT
    /// remove anything from `objects` — production callers want the
    /// static tracks to flow through to the live viewer with a
    /// "static" visual indicator, even though the supervisor will
    /// hide them from rules + the motion lifecycle.
    ///
    /// Mutates `objects` in place (only the attributes maps of
    /// suppressed tracks); mutates internal per-track state and the
    /// persistent anchor registry.
    pub fn classify(&mut self, _frame: &Frame, objects: &mut [TrackedObject]) {
        let mut dirty = false;

        // Walk the object list, classifying each. Borrow-checker: pull
        // values out before the per-track state borrow.
        let cfg_dwell = self.cfg.dwell_frames.max(1);
        let cfg_sig_px = self.cfg.significant_movement_pixels.max(1) as f64;
        let cfg_sig_frames = self.cfg.significant_movement_frames.max(1);
        let cfg_alpha = self.cfg.movement_ema_alpha.clamp(0.01, 1.0) as f64;
        let cfg_match_dist = self.cfg.match_distance_pixels.max(1) as f32;
        let cfg_reset_px = self.cfg.track_id_reuse_reset_pixels as f64;
        let cfg_persistence = self.cfg.persistence_enabled;
        // Wall-clock used to refresh / age anchors. Pulled from the
        // frame so tests and replay scenarios behave deterministically.
        let frame_ms = _frame.captured_at.timestamp_millis();

        // Build a "static?" verdict per-index without touching the
        // objects yet, because we mutate `self.anchors` inside the
        // loop.
        let mut suppress_verdict: Vec<bool> = Vec::with_capacity(objects.len());

        for o in objects.iter() {
            if !is_vehicle_label(&o.label)
                && !self
                    .extra_anchor_classes
                    .iter()
                    .any(|c| c == &o.label.to_lowercase())
            {
                suppress_verdict.push(false);
                continue;
            }

            let center = o.bbox.center();
            let state = self.state_by_track.entry(o.track_id).or_default();

            // ---- ID-reuse guard ----
            // If the apparent center jumped further than the
            // configured threshold in one frame, the upstream tracker
            // has almost certainly recycled this `track_id` onto a
            // different physical object (occlusion -> mis-association,
            // or a stale ID re-fired after a gap). Wipe the per-track
            // state so the new vehicle doesn't inherit a stale
            // `static_promoted = true` from the previous occupant.
            // `cfg_reset_px == 0` disables the guard entirely.
            if cfg_reset_px > 0.0 {
                if let Some((px, py)) = state.last_center {
                    let dx = (center.0 - px) as f64;
                    let dy = (center.1 - py) as f64;
                    if (dx * dx + dy * dy).sqrt() > cfg_reset_px {
                        *state = PerTrackState::default();
                    }
                }
            }

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
                } else {
                    // Still parked — refresh `last_seen` so the TTL
                    // sweep doesn't prune an anchor whose vehicle is
                    // visibly still there. In-memory update only; the
                    // disk write is debounced to structural changes
                    // (promote / demote / sweep / centroid drift).
                    self.anchors[idx].last_seen_unix_ms = Some(frame_ms);
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
                && Self::upsert_anchor(
                    &mut self.anchors,
                    &label_lc,
                    center,
                    cfg_match_dist,
                    frame_ms,
                )
            {
                dirty = true;
            }

            suppress_verdict.push(suppress);
        }

        // Stamp the attribute on suppressed tracks, plus the
        // diagnostic EMA / counter triple on every vehicle-labelled
        // track. The UI's static-debug toggle reads the diagnostic
        // attrs to show *why* the FSM made the call it did (so an
        // operator can tell `ema = 5.0` (truly slow) apart from
        // `ema = 35.99` (just under the cliff)).
        for (o, suppress) in objects.iter_mut().zip(suppress_verdict.iter().copied()) {
            if suppress {
                o.attributes
                    .insert(STATIC_ATTRIBUTE_KEY.to_string(), Value::Bool(true));
            }
            if is_vehicle_label(&o.label) {
                if let Some(state) = self.state_by_track.get(&o.track_id) {
                    // `f64 -> serde_json::Number` may fail for NaN /
                    // Inf; `from_f64` returns `Option`. EMA should
                    // never be either but be defensive.
                    if let Some(n) = serde_json::Number::from_f64(state.movement_ema) {
                        o.attributes
                            .insert(EMA_ATTRIBUTE_KEY.to_string(), Value::Number(n));
                    }
                    o.attributes.insert(
                        STATIC_FRAMES_ATTRIBUTE_KEY.to_string(),
                        Value::Number(state.static_frames.into()),
                    );
                    o.attributes.insert(
                        MOVING_FRAMES_ATTRIBUTE_KEY.to_string(),
                        Value::Number(state.moving_consecutive_frames.into()),
                    );
                }
            }
        }

        if dirty {
            self.save_registry();
        }

        // ---- TTL sweep ----
        // After per-track classification, prune any anchor whose last
        // matching observation is older than `anchor_ttl_secs`. Anchors
        // loaded from disk in the legacy (pre-TTL) shape carry
        // `last_seen_unix_ms = None`; bootstrap those to the current
        // frame so they get one full TTL window after restart before
        // becoming eligible for sweeping. Only persists when an actual
        // removal happens to avoid disk thrash from the per-frame
        // refresh stamps above.
        if cfg_persistence && self.cfg.anchor_ttl_secs > 0 {
            let ttl_ms = (self.cfg.anchor_ttl_secs as i64).saturating_mul(1_000);
            let mut swept = false;
            self.anchors.retain_mut(|a| {
                let last = a.last_seen_unix_ms.get_or_insert(frame_ms);
                if frame_ms.saturating_sub(*last) > ttl_ms {
                    swept = true;
                    false
                } else {
                    true
                }
            });
            if swept {
                self.save_registry();
            }
        }
    }

    /// Back-compat wrapper around [`classify`](Self::classify) that
    /// physically removes suppressed tracks from `objects`. Used by
    /// the in-crate unit tests and any caller that wants the v1
    /// "drop entirely" semantics. Production code in
    /// `nexus-pipeline::supervisor` calls `classify` instead so the
    /// live viewer can still render static objects with a distinct
    /// style.
    pub fn filter(&mut self, frame: &Frame, objects: &mut Vec<TrackedObject>) {
        self.classify(frame, objects);
        objects.retain(|o| !is_object_static(o));
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
        now_ms: i64,
    ) -> bool {
        if let Some(idx) = Self::match_anchor(anchors, label_lc, center, max_dist_px) {
            // Average toward the new observation — same shape as v1
            // (`(old + new) * 0.5`). Tiny drift; only triggers a save
            // when the average actually moves the centroid. Refresh
            // `last_seen_unix_ms` unconditionally so a parked vehicle
            // whose centroid never drifts beyond the 0.01 threshold
            // still keeps its TTL fresh.
            let prev = (anchors[idx].center_x, anchors[idx].center_y);
            anchors[idx].last_seen_unix_ms = Some(now_ms);
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
                last_seen_unix_ms: Some(now_ms),
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
            detection_bbox: None,
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
            detection_bbox: None,
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
            track_id_reuse_reset_pixels: 0,
            anchor_ttl_secs: 0,
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
            track_id_reuse_reset_pixels: 0,
            anchor_ttl_secs: 0,
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
            track_id_reuse_reset_pixels: 0,
            anchor_ttl_secs: 0,
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
            track_id_reuse_reset_pixels: 0,
            anchor_ttl_secs: 0,
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
            track_id_reuse_reset_pixels: 0,
            anchor_ttl_secs: 0,
            persistence_enabled: true,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);
        f.anchors.push(StaticAnchor {
            label: "vehicle.car".into(),
            center_x: 500.0,
            center_y: 500.0,
            last_seen_unix_ms: None,
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
            track_id_reuse_reset_pixels: 0,
            anchor_ttl_secs: 0,
            persistence_enabled: true,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);
        f.anchors.push(StaticAnchor {
            label: "vehicle.car".into(),
            center_x: 500.0,
            center_y: 500.0,
            last_seen_unix_ms: None,
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
            track_id_reuse_reset_pixels: 0,
            anchor_ttl_secs: 0,
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

    #[test]
    fn track_id_reuse_resets_state() {
        // Promote a track at (500, 500), then on the next frame the
        // same `track_id` shows up 400 px away. With the reset guard
        // enabled the new occupant must NOT inherit `static_promoted`.
        let cfg = StaticObjectConfig {
            dwell_frames: 2,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 5,
            // Disable persistence so an anchor at the old location
            // can't independently suppress the new one.
            persistence_enabled: false,
            // Guard at 100 px — well below the 400 px jump below.
            track_id_reuse_reset_pixels: 100,
            anchor_ttl_secs: 0,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);

        // Park track 99 at (500, 500) for 3 frames -> promoted.
        for i in 0..3 {
            let mut objs = vec![vehicle(99, 500.0, 500.0)];
            f.filter(&frame(1, i, i as i64 * 33), &mut objs);
            if i >= 1 {
                assert!(objs.is_empty(), "frame {i}: should be suppressed");
            }
        }

        // Frame 3: the same `track_id` 99 reappears 400 px away. The
        // reset guard should wipe the FSM state -> the new occupant
        // is NOT static and the box must pass through.
        let mut objs = vec![vehicle(99, 900.0, 500.0)];
        f.filter(&frame(1, 3, 99), &mut objs);
        assert_eq!(
            objs.len(),
            1,
            "track-id-reuse guard should let the new physical object through"
        );

        // And it should stay through on the very next frame (state
        // truly cleared, not just one-shot).
        let mut objs = vec![vehicle(99, 910.0, 500.0)];
        f.filter(&frame(1, 4, 132), &mut objs);
        assert_eq!(objs.len(), 1, "second frame must also pass");
    }

    #[test]
    fn track_id_reuse_guard_disabled_when_zero() {
        // Same scenario as above but with `track_id_reuse_reset_pixels: 0`
        // — the guard is off, so the new occupant DOES inherit the
        // promoted state. Verifies the kill switch.
        let cfg = StaticObjectConfig {
            dwell_frames: 2,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 5,
            persistence_enabled: false,
            track_id_reuse_reset_pixels: 0,
            anchor_ttl_secs: 0,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);
        for i in 0..3 {
            let mut objs = vec![vehicle(99, 500.0, 500.0)];
            f.filter(&frame(1, i, i as i64 * 33), &mut objs);
        }
        // Same track_id, huge jump — but moving_consecutive_frames
        // climbs to 1 from this single fast move (instant_movement =
        // 400 px >= 10 px threshold). With sig_frames = 2 the
        // suppression should still hold for this one frame.
        let mut objs = vec![vehicle(99, 900.0, 500.0)];
        f.filter(&frame(1, 3, 99), &mut objs);
        assert!(
            objs.is_empty(),
            "with guard disabled, the new occupant inherits suppression"
        );
    }

    #[test]
    fn stale_anchor_is_swept_after_ttl_with_no_matching_observation() {
        // Seed a registry with one anchor whose last_seen is well in
        // the past. Run classify against a frame whose timestamp is
        // beyond the TTL — the anchor MUST be pruned so the UI overlay
        // and suppression FSM don't keep showing it after the vehicle
        // has driven out of view.
        let cfg = StaticObjectConfig {
            dwell_frames: 999,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 30,
            track_id_reuse_reset_pixels: 0,
            persistence_enabled: true,
            // 60-second TTL — frames 0 ms vs 120_000 ms straddles it.
            anchor_ttl_secs: 60,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);
        f.anchors.push(StaticAnchor {
            label: "vehicle.car".into(),
            center_x: 100.0,
            center_y: 100.0,
            // Stale relative to the frame timestamp below.
            last_seen_unix_ms: Some(0),
        });

        // Classify a frame with NO objects at the anchor location.
        // Frame timestamp is 120_000 ms — 120 s after last_seen,
        // beyond the 60 s TTL.
        let mut objs: Vec<TrackedObject> = vec![];
        f.classify(&frame(1, 1, 120_000), &mut objs);

        assert!(
            f.anchors().is_empty(),
            "stale anchor must be swept once TTL elapses without an observation"
        );
    }

    #[test]
    fn fresh_observation_refreshes_anchor_ttl() {
        // Inverse of the above: a parked vehicle keeps reappearing in
        // every frame. Even with a tight TTL the anchor must stay put.
        let cfg = StaticObjectConfig {
            dwell_frames: 999,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 30,
            track_id_reuse_reset_pixels: 0,
            persistence_enabled: true,
            anchor_ttl_secs: 1,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);
        f.anchors.push(StaticAnchor {
            label: "vehicle.car".into(),
            center_x: 200.0,
            center_y: 200.0,
            last_seen_unix_ms: Some(0),
        });

        // Frame at t = 5 s, vehicle still parked at the anchor.
        let mut objs = vec![vehicle(42, 200.0, 200.0)];
        f.classify(&frame(1, 1, 5_000), &mut objs);

        assert_eq!(
            f.anchors().len(),
            1,
            "anchor with a matching observation must NOT be swept"
        );
    }

    #[test]
    fn legacy_anchor_without_last_seen_gets_one_full_ttl_grace_period() {
        // Anchors persisted by an earlier engine version carry
        // last_seen_unix_ms = None. The first classify call must
        // bootstrap them with the current frame timestamp, NOT
        // immediately sweep them. This avoids a one-time mass-prune
        // event after upgrading the binary.
        let cfg = StaticObjectConfig {
            dwell_frames: 999,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 30,
            track_id_reuse_reset_pixels: 0,
            persistence_enabled: true,
            anchor_ttl_secs: 60,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);
        f.anchors.push(StaticAnchor {
            label: "vehicle.car".into(),
            center_x: 300.0,
            center_y: 300.0,
            last_seen_unix_ms: None,
        });

        // Classify with no objects at the anchor — but since the
        // legacy anchor's last_seen is None, the bootstrap path
        // stamps the current frame_ms and keeps it.
        let mut objs: Vec<TrackedObject> = vec![];
        f.classify(&frame(1, 1, 1_000_000), &mut objs);

        assert_eq!(
            f.anchors().len(),
            1,
            "legacy anchor (last_seen = None) must survive the first classify"
        );
        assert_eq!(
            f.anchors()[0].last_seen_unix_ms,
            Some(1_000_000),
            "bootstrap must stamp the current frame_ms onto the legacy anchor"
        );
    }
}

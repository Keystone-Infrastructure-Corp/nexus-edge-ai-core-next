//! Per-camera static-object filter. Mirrors v1's
//! `EventFilter::staticVehicle*` block in
//! `src/tracking/event_filter.cpp`.
//!
//! Vehicles (and, with opt-in `anchor_persons`, people) whose smoothed per-frame
//! movement stays below `significant_movement_pixels` for `dwell_frames`
//! consecutive frames (default: `dwell_secs` at the measured inference
//! rate) are *promoted* to "static" and dropped from the rule-eval slice
//! (i.e. parked cars stop firing alerts). Promoted tracks are written
//! to a per-camera anchor registry on disk so the suppression survives
//! a restart.
//!
//! A previously-suppressed track that starts moving again
//! (significant-movement EMA crossed for
//! `significant_movement_frames` consecutive frames, or its centre more
//! than `match_distance_pixels` from where it parked) gets demoted —
//! the matching anchor is erased from the registry and subsequent
//! frames flow through to the rule layer.
//!
//! State surface: one `StaticObjectFilter` per camera, owned by the
//! supervisor task. Per-track state is keyed by `track_id` only (the
//! filter is already scoped to its camera). The on-disk registry is
//! the only cross-restart state.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use nexus_config::{ParkingPolicyV2, StaticObjectConfig, ZoneConfig, ZoneKind};
use nexus_types::{CameraId, Frame, TrackId, TrackedObject};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::annotator::point_in_normalized_polygon;
use crate::parking_stillness::{InvalidStillnessParam, StillnessParams, StillnessWindow};
use crate::rate::InferenceRate;

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
/// Monotonic per-track generation for parking-lot alert deduplication. It
/// starts at zero and advances when the track breaks a closed static-object
/// gate (sustained movement, or leaving where it parked) and when the
/// ID-reuse guard resets the track.
pub const ALERT_EPOCH_ATTRIBUTE_KEY: &str = "tracker.static_alert_epoch";

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
    /// Alert generation for this physical parking episode. The rule layer
    /// emits at most once per generation while still honoring its normal
    /// debounce before the first emission.
    alert_epoch: u64,
    /// Pixel center of the anchor this track owns, recorded when it was
    /// promoted. Demotion erases the anchor by *this* position, not by
    /// the track's current one: a vehicle that has driven far enough to
    /// break the static gate has necessarily left its own
    /// `match_distance_pixels` radius, so matching on the live center
    /// would leave the anchor orphaned and blind the parking space to
    /// every future arrival.
    anchor_center: Option<(f32, f32)>,
    /// The track's own centre on its first still frame after promotion.
    /// Unlike `anchor_center`, never a neighbour's anchor the track was
    /// matched to, so a parked car's jitter is measured from where *it*
    /// stands (#367).
    parked_at: Option<(f32, f32)>,
    /// Set when the track breaks its static gate. A departed track is a
    /// moving vehicle, so another vehicle's anchor must not hide it before
    /// it parks again (#368 review). Only read while the track is not
    /// promoted, so promotion need not clear it.
    departed: bool,
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
    /// Measured inference rate; converts `dwell_secs` to frames.
    rate: InferenceRate,
    /// SPEC-077 `parking_policy_v2`; `None` is the legacy per-frame filter.
    parking_v2: Option<ParkingV2>,
}

/// SPEC-077 v2 state: only an observed dwell, timed on capture stamps,
/// inside a parking zone suppresses a track.
struct ParkingV2 {
    params: StillnessParams,
    max_gap_ms: f64,
    /// Normalized polygons of the camera's parking zones.
    zones: Vec<Vec<(f32, f32)>>,
    /// Capture milliseconds count from the first classified frame.
    origin: Option<Instant>,
    /// Analysis-frame size; a change resets every candidate and rescales
    /// every anchor.
    geometry: Option<(u32, u32)>,
    /// Classified frames so far.
    seq: u64,
    tracks: HashMap<TrackId, V2Track>,
}

struct V2Track {
    window: StillnessWindow,
    alert_epoch: u64,
    /// The `seq` of the last frame that carried this track, and its capture ms.
    seen_seq: u64,
    seen_ms: u64,
    /// The last observed center and its capture ms.
    last: Option<((f32, f32), u64)>,
    /// The anchor of the track's current parked episode (with persistence
    /// off, the center of its first qualified dwell).
    parked_at: Option<(f32, f32)>,
    /// Owns `parked_at`: observed there after a qualified dwell and seen on
    /// every frame since, within the observation gap and the ID-reuse step.
    /// Only an owner seen leaving erases the anchor.
    owner: bool,
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
            rate: InferenceRate::default(),
            parking_v2: None,
        }
    }

    /// SPEC-077: replace the legacy per-frame FSM with `parking_policy_v2`.
    /// A track is static only while its own observed dwell inside one of
    /// `zones`' parking polygons qualifies; the other zone kinds are ignored.
    pub fn with_parking_policy(
        mut self,
        policy: &ParkingPolicyV2,
        zones: &[ZoneConfig],
    ) -> Result<Self, InvalidStillnessParam> {
        let params = StillnessParams::new(
            policy.dwell_secs,
            policy.max_speed_box_diagonals_per_sec,
            policy.max_displacement_box_diagonals,
            policy.max_observation_gap_secs,
        )?;
        self.parking_v2 = Some(ParkingV2 {
            params,
            max_gap_ms: policy.max_observation_gap_secs * 1000.0,
            zones: zones
                .iter()
                .filter(|z| z.kind == ZoneKind::Parking)
                .map(|z| z.polygon.clone())
                .collect(),
            origin: None,
            geometry: None,
            seq: 0,
            tracks: HashMap::new(),
        });
        Ok(self)
    }

    pub fn name(&self) -> &'static str {
        "static_object"
    }

    /// Read-only view of the current persistent anchor set. Useful for
    /// tests; no production code should mutate the registry directly.
    pub fn anchors(&self) -> &[StaticAnchor] {
        &self.anchors
    }

    /// The anchors that take part in suppression on `frame`: all of them
    /// for a legacy camera; for v2 only those whose center is inside a
    /// current Parking polygon (outside anchors wait out their TTL).
    pub fn scoped_anchors(&self, frame: &Frame) -> Cow<'_, [StaticAnchor]> {
        let Some(v2) = &self.parking_v2 else {
            return Cow::Borrowed(&self.anchors);
        };
        let size = (frame.width, frame.height);
        let (fw, fh) = (size.0.max(1) as f32, size.1.max(1) as f32);
        Cow::Owned(
            self.anchors
                .iter()
                .map(|a| {
                    // Anchors stay in the last classified frame's pixels
                    // until `classify` sees a resize and rescales them.
                    let (center_x, center_y) = v2.geometry.map_or((a.center_x, a.center_y), |g| {
                        rescale((a.center_x, a.center_y), g, size)
                    });
                    StaticAnchor {
                        center_x,
                        center_y,
                        ..a.clone()
                    }
                })
                .filter(|a| {
                    v2.zones
                        .iter()
                        .any(|z| point_in_normalized_polygon(a.center_x / fw, a.center_y / fh, z))
                })
                .collect(),
        )
    }

    /// Is `label` eligible for static-anchor promotion on this camera?
    /// Vehicles always are; `person` is when `anchor_persons` is set;
    /// Phase 8.1 `static_anchor_classes` adds equipment labels. Single
    /// source of truth for both the classification pass and the
    /// attribute-stamping pass.
    fn is_anchor_eligible(&self, label: &str) -> bool {
        is_vehicle_label(label)
            || (self.cfg.anchor_persons && label.eq_ignore_ascii_case("person"))
            || self
                .extra_anchor_classes
                .iter()
                .any(|c| c == &label.to_lowercase())
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
        if let Some(v2) = &mut self.parking_v2 {
            v2.tracks.clear();
        }
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
        // Wall-clock used to refresh / age anchors. Pulled from the
        // frame so tests and replay scenarios behave deterministically.
        let frame_ms = _frame.captured_at.timestamp_millis();
        if self.parking_v2.is_some() {
            self.classify_v2(_frame, objects);
            self.sweep_anchors(frame_ms);
            return;
        }
        self.rate.observe_at(_frame.captured_mono);

        // Walk the object list, classifying each. Borrow-checker: pull
        // values out before the per-track state borrow.
        let cfg_dwell = self
            .cfg
            .dwell_frames
            .unwrap_or_else(|| self.rate.frames(self.cfg.dwell_secs))
            .max(1);
        let cfg_sig_px = self.cfg.significant_movement_pixels.max(1) as f64;
        let cfg_sig_frames = self.cfg.significant_movement_frames.max(1);
        let cfg_alpha = self.cfg.movement_ema_alpha.clamp(0.01, 1.0) as f64;
        let cfg_match_dist = self.cfg.match_distance_pixels.max(1) as f32;
        // Largest single-frame step that still counts as standing still:
        // above box jitter on a parked car, below a vehicle driving past
        // one (#367).
        let cfg_still_px = (cfg_match_dist / 4.0) as f64;
        let cfg_reset_px = self.cfg.track_id_reuse_reset_pixels as f64;
        let cfg_persistence = self.cfg.persistence_enabled;

        // Build a "static?" verdict per-index without touching the
        // objects yet, because we mutate `self.anchors` inside the
        // loop.
        let mut suppress_verdict: Vec<bool> = Vec::with_capacity(objects.len());

        for o in objects.iter() {
            if !self.is_anchor_eligible(&o.label) {
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
            //
            // Only a parked track starts a new alert episode here. At low
            // analysis rates a moving vehicle's own steps exceed this
            // threshold (#362), and this filter cannot tell them from a
            // swap; a vehicle that never parked has one episode for its
            // whole visit, so a new one per step would re-alert it.
            if cfg_reset_px > 0.0 {
                if let Some((px, py)) = state.last_center {
                    let dx = (center.0 - px) as f64;
                    let dy = (center.1 - py) as f64;
                    if (dx * dx + dy * dy).sqrt() > cfg_reset_px {
                        let next_alert_epoch = state
                            .alert_epoch
                            .saturating_add(u64::from(state.static_promoted));
                        *state = PerTrackState::default();
                        state.alert_epoch = next_alert_epoch;
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
            // Only a frame that stood still may place or refresh an
            // anchor. A track's first frame has no step to judge, and at
            // low frame rates a passing car is mostly first frames. A
            // coasting frame (no detection, the tracker's last box) shows
            // no movement because nothing was seen (#368 review).
            let still = o.detection_bbox.is_some()
                && state.last_center.is_some()
                && instant_movement <= cfg_still_px;
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

            // Demote only after sustained movement. Clearing
            // `static_promoted` before the registry upsert below is
            // essential: otherwise the erased anchor is recreated in the
            // same frame and the vehicle never starts a fresh dwell cycle.
            //
            // A promoted track whose centre is more than the match radius
            // from where it parked is no longer that parked vehicle: at
            // low frame rates ByteTrack links a passing car into a parked
            // car's track, and the car must not inherit the parked verdict
            // while its EMA catches up (#367).
            let left_its_spot = state.static_promoted
                && state
                    .parked_at
                    .is_some_and(|(px, py)| (px - center.0).hypot(py - center.1) > cfg_match_dist);
            let broke_static_gate = left_its_spot
                || (state.moving_consecutive_frames >= cfg_sig_frames
                    && (state.static_promoted || matched_anchor_index.is_some()));
            if broke_static_gate {
                // Erase this track's own anchor. Prefer the position it
                // was parked at (`anchor_center`) over the live center —
                // by the time the gate breaks the vehicle is typically
                // well outside `match_distance_pixels` of its anchor, so
                // `matched_anchor_index` is `None` and matching on the
                // live center would strand the anchor in the registry.
                let probe = state.anchor_center.take();
                let stale_index = match probe {
                    Some(parked_at) => {
                        Self::match_anchor(&self.anchors, &label_lc, parked_at, cfg_match_dist)
                    }
                    None => matched_anchor_index,
                };
                if let Some(idx) = stale_index {
                    self.anchors.remove(idx);
                    dirty = true;
                }
                state.static_promoted = false;
                state.parked_at = None;
                state.departed = true;
                state.static_frames = 0;
                state.alert_epoch = state.alert_epoch.saturating_add(1);
            } else if let Some(idx) = matched_anchor_index.filter(|_| still) {
                // Still parked — refresh `last_seen` so the TTL sweep
                // doesn't prune an anchor whose vehicle is visibly still
                // there. Traffic passing through does not count, or a lane
                // anchor would never age out. In-memory update only; the
                // disk write is debounced to structural changes (promote /
                // demote / sweep).
                self.anchors[idx].last_seen_unix_ms = Some(frame_ms);
            }

            // The post-demotion match (we may have just removed it):
            let still_matches_anchor = if cfg_persistence {
                Self::match_anchor(&self.anchors, &label_lc, center, cfg_match_dist).is_some()
            } else {
                false
            };

            let suppress = ((still_matches_anchor && !state.departed) || state.static_promoted)
                && state.moving_consecutive_frames < cfg_sig_frames;

            // Promote into the registry while we hold the suppression
            // verdict — doing this lazily on the next frame would lose
            // anchors across a fast restart.
            //
            // `moving_consecutive_frames == 0` keeps a departing vehicle
            // from minting a fresh anchor along its exit path: it stays
            // `static_promoted` until the gate actually breaks, and
            // without this guard each frame of the drive-off would push
            // a new anchor once it cleared `match_distance_pixels`,
            // littering the lot with permanent blind spots. `still` does
            // the same for a car crawling under the EMA threshold.
            if state.static_promoted
                && cfg_persistence
                && state.moving_consecutive_frames == 0
                && still
            {
                let (anchor_at, mutated) = Self::upsert_anchor(
                    &mut self.anchors,
                    &label_lc,
                    center,
                    cfg_match_dist,
                    frame_ms,
                );
                // Remember which anchor this track owns so demotion can
                // erase it after the vehicle has moved away from it.
                state.anchor_center = Some(anchor_at);
                state.parked_at.get_or_insert(center);
                if mutated {
                    dirty = true;
                }
            }

            suppress_verdict.push(suppress);
        }

        // Stamp the attribute on suppressed tracks, plus the
        // diagnostic EMA / counter triple on every anchor-eligible
        // track. The UI's static-debug toggle reads the diagnostic
        // attrs to show *why* the FSM made the call it did (so an
        // operator can tell `ema = 5.0` (truly slow) apart from
        // `ema = 35.99` (just under the cliff)).
        //
        // Eligibility here MUST match the classification loop above:
        // every track the FSM keeps state for also carries the alert
        // epoch, otherwise nexus-rules silently falls back to the
        // legacy camera-scoped cooldown for that class (BUG-020).
        for (o, suppress) in objects.iter_mut().zip(suppress_verdict.iter().copied()) {
            if suppress {
                o.attributes
                    .insert(STATIC_ATTRIBUTE_KEY.into(), Value::Bool(true));
            }
            if self.is_anchor_eligible(&o.label) {
                if let Some(state) = self.state_by_track.get(&o.track_id) {
                    // `f64 -> serde_json::Number` may fail for NaN /
                    // Inf; `from_f64` returns `Option`. EMA should
                    // never be either but be defensive.
                    if let Some(n) = serde_json::Number::from_f64(state.movement_ema) {
                        o.attributes
                            .insert(EMA_ATTRIBUTE_KEY.into(), Value::Number(n));
                    }
                    o.attributes.insert(
                        STATIC_FRAMES_ATTRIBUTE_KEY.into(),
                        Value::Number(state.static_frames.into()),
                    );
                    o.attributes.insert(
                        MOVING_FRAMES_ATTRIBUTE_KEY.into(),
                        Value::Number(state.moving_consecutive_frames.into()),
                    );
                    o.attributes.insert(
                        ALERT_EPOCH_ATTRIBUTE_KEY.into(),
                        Value::Number(state.alert_epoch.into()),
                    );
                }
            }
        }

        if dirty {
            self.save_registry();
        }
        self.sweep_anchors(frame_ms);
    }

    /// SPEC-077 v2 classification. Each track's observed (detector-matched)
    /// box feeds its [`StillnessWindow`] in analysis-frame pixels, the
    /// supervisor frame the detector already mapped its boxes back into,
    /// timed by monotonic capture milliseconds. Coasting frames and
    /// frames a track is missing from only end its window. Track age,
    /// update counts and movement EMAs never credit stillness.
    ///
    /// Anchors are bookkeeping, never a verdict: a track is static only on
    /// its own qualified dwell. Only then does it place, match or refresh
    /// an anchor, and only one whose center is also inside a parking zone,
    /// so anchors outside the zones age out on their TTL.
    fn classify_v2(&mut self, frame: &Frame, objects: &mut [TrackedObject]) {
        let eligible: Vec<bool> = objects
            .iter()
            .map(|o| self.is_anchor_eligible(&o.label))
            .collect();
        // Each observed eligible object, to tell a departure from an
        // identity swap: the parked car still at its anchor.
        let observed: Vec<(TrackId, String, (f32, f32))> = objects
            .iter()
            .zip(&eligible)
            .filter(|(_, e)| **e)
            .filter_map(|(o, _)| {
                Some((
                    o.track_id,
                    o.label.to_lowercase(),
                    o.detection_bbox?.center(),
                ))
            })
            .collect();
        let frame_ms = frame.captured_at.timestamp_millis();
        let match_dist = self.cfg.match_distance_pixels.max(1) as f32;
        let reset_px = self.cfg.track_id_reuse_reset_pixels as f32;
        let persistence = self.cfg.persistence_enabled;
        let mut dirty = false;
        let Self {
            parking_v2,
            anchors,
            ..
        } = self;
        let Some(v2) = parking_v2.as_mut() else {
            return;
        };
        let origin = *v2.origin.get_or_insert(frame.captured_mono);
        let at_ms = frame
            .captured_mono
            .saturating_duration_since(origin)
            .as_millis() as u64;
        let geometry = (frame.width, frame.height);
        if let Some(old) = v2.geometry.replace(geometry).filter(|g| *g != geometry) {
            // Each anchor keeps its place in the view (BUG-258), now in the
            // new geometry's pixels.
            for a in anchors.iter_mut() {
                (a.center_x, a.center_y) = rescale((a.center_x, a.center_y), old, geometry);
                dirty = true;
            }
            // Pixels of the old geometry say nothing about the new one.
            for t in v2.tracks.values_mut() {
                t.window.miss();
                t.last = None;
                t.owner = false;
                if t.parked_at.take().is_some() {
                    t.alert_epoch += 1;
                }
            }
        }
        v2.seq += 1;
        let (fw, fh) = (frame.width.max(1) as f32, frame.height.max(1) as f32);
        let zones = &v2.zones;
        let in_parking = |(x, y): (f32, f32)| {
            zones
                .iter()
                .any(|z| point_in_normalized_polygon(x / fw, y / fh, z))
        };
        let (params, max_gap_ms) = (v2.params, v2.max_gap_ms);
        for (o, eligible) in objects.iter_mut().zip(eligible) {
            if !eligible {
                continue;
            }
            let t = v2.tracks.entry(o.track_id).or_insert_with(|| V2Track {
                window: StillnessWindow::new(params),
                alert_epoch: 0,
                seen_seq: 0,
                seen_ms: 0,
                last: None,
                parked_at: None,
                owner: false,
            });
            // Missing from the last frame: nothing observed it there, and a
            // tracker may have given its id to another object, which
            // inherits no parked episode; its anchor is left to expire.
            if t.seen_seq + 1 != v2.seq {
                t.window.miss();
                t.owner = false;
                if t.parked_at.take().is_some() {
                    t.alert_epoch += 1;
                }
            }
            (t.seen_seq, t.seen_ms) = (v2.seq, at_ms);
            let mut still = false;
            match o.detection_bbox {
                Some(b) => {
                    let center = b.center();
                    let inside = in_parking(center);
                    let label = o.label.to_lowercase();
                    // A jump past the ID-reuse step is another object under
                    // this id: it inherits no dwell, even one within the
                    // motion bounds of a large box, and no parked episode.
                    if reset_px > 0.0 && t.last.is_some_and(|(p, _)| distance(p, center) > reset_px)
                    {
                        t.window.miss();
                        t.owner = false;
                        if t.parked_at.take().is_some() {
                            t.alert_epoch += 1;
                        }
                    }
                    t.owner &= t
                        .last
                        .is_some_and(|(_, ms)| at_ms > ms && (at_ms - ms) as f64 <= max_gap_ms);
                    t.last = Some((center, at_ms));
                    // Seen away from where it parked, or with another object
                    // nearer where its stillness began (its anchor once that
                    // window ended) than it is, which may be the one that
                    // parked: its next episode. Not nearer the anchor: an id
                    // that dwelled afresh beside it matches the same anchor.
                    // Only an owner seen leaving an anchor no other object
                    // still stands at erases it; otherwise it is left to expire.
                    if let Some(p) = t.parked_at {
                        let left = !inside || distance(p, center) > match_dist;
                        let began = t
                            .window
                            .first_center()
                            .map_or(p, |(x, y)| (x as f32, y as f32));
                        let swapped = observed.iter().any(|(id, l, c)| {
                            *id != o.track_id
                                && *l == label
                                && if left {
                                    distance(*c, p) <= match_dist
                                } else {
                                    distance(*c, began) < distance(center, began)
                                }
                        });
                        if left || swapped {
                            if t.owner && !swapped {
                                if let Some(i) = anchors
                                    .iter()
                                    .position(|a| a.label == label && (a.center_x, a.center_y) == p)
                                {
                                    anchors.remove(i);
                                    dirty = true;
                                }
                            }
                            // An identity another object may hold borrows no dwell.
                            if swapped {
                                t.window.miss();
                            }
                            t.parked_at = None;
                            t.owner = false;
                            t.alert_epoch += 1;
                        }
                    }
                    still =
                        t.window
                            .observe(at_ms, center, (b.x2 - b.x1).hypot(b.y2 - b.y1), inside);
                    if still {
                        let own = t.parked_at.filter(|_| t.owner);
                        let parked_at = if persistence {
                            // Its own anchor, else one in parking within
                            // the match radius, else a new one here. An
                            // anchor never moves (BUG-258).
                            let i = anchors
                                .iter()
                                .position(|a| {
                                    a.label == label && Some((a.center_x, a.center_y)) == own
                                })
                                .or_else(|| {
                                    anchors.iter().position(|a| {
                                        let at = (a.center_x, a.center_y);
                                        a.label == label
                                            && distance(at, center) <= match_dist
                                            && in_parking(at)
                                    })
                                });
                            match i {
                                Some(i) => {
                                    anchors[i].last_seen_unix_ms = Some(frame_ms);
                                    (anchors[i].center_x, anchors[i].center_y)
                                }
                                None => {
                                    anchors.push(StaticAnchor {
                                        label,
                                        center_x: center.0,
                                        center_y: center.1,
                                        last_seen_unix_ms: Some(frame_ms),
                                    });
                                    dirty = true;
                                    center
                                }
                            }
                        } else {
                            t.parked_at.unwrap_or(center)
                        };
                        t.parked_at = Some(parked_at);
                        t.owner = true;
                    }
                }
                None => {
                    t.window.miss();
                    t.owner = false;
                }
            }
            if still {
                o.attributes
                    .insert(STATIC_ATTRIBUTE_KEY.into(), Value::Bool(true));
            }
            o.attributes.insert(
                ALERT_EPOCH_ATTRIBUTE_KEY.into(),
                Value::Number(t.alert_epoch.into()),
            );
        }
        // Any absence already ends a track's window, ownership and parked
        // episode, and the rules drop an absent track's alert state; within
        // the gap a return still moves on to a new alert epoch. Past it,
        // forget the id too, so ids cannot pile up for the camera's lifetime.
        v2.tracks
            .retain(|_, t| at_ms.saturating_sub(t.seen_ms) as f64 <= max_gap_ms);
        if dirty {
            self.save_registry();
        }
    }

    /// After per-track classification, prune any anchor whose last
    /// matching observation is older than `anchor_ttl_secs`. Anchors
    /// loaded from disk in the legacy (pre-TTL) shape carry
    /// `last_seen_unix_ms = None`; bootstrap those to the current
    /// frame so they get one full TTL window after restart before
    /// becoming eligible for sweeping. Only persists when an actual
    /// removal happens to avoid disk thrash from the per-frame
    /// refresh stamps above.
    fn sweep_anchors(&mut self, frame_ms: i64) {
        if self.cfg.persistence_enabled && self.cfg.anchor_ttl_secs > 0 {
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

    /// Inserts or refreshes. Returns the resolved anchor center (so the
    /// caller can remember which anchor a track owns) and whether the
    /// registry mutated. An existing anchor never moves: averaging it
    /// toward each observation let a vehicle linked into a parked
    /// car's track drag the anchor into the lane (#367).
    fn upsert_anchor(
        anchors: &mut Vec<StaticAnchor>,
        label_lc: &str,
        center: (f32, f32),
        max_dist_px: f32,
        now_ms: i64,
    ) -> ((f32, f32), bool) {
        if let Some(idx) = Self::match_anchor(anchors, label_lc, center, max_dist_px) {
            anchors[idx].last_seen_unix_ms = Some(now_ms);
            ((anchors[idx].center_x, anchors[idx].center_y), false)
        } else {
            anchors.push(StaticAnchor {
                label: label_lc.to_string(),
                center_x: center.0,
                center_y: center.1,
                last_seen_unix_ms: Some(now_ms),
            });
            (center, true)
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

fn distance(a: (f32, f32), b: (f32, f32)) -> f32 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

/// A pixel point of a `from`-sized frame in a `to`-sized frame of the same view.
fn rescale((x, y): (f32, f32), from: (u32, u32), to: (u32, u32)) -> (f32, f32) {
    (
        x * to.0.max(1) as f32 / from.0.max(1) as f32,
        y * to.1.max(1) as f32 / from.1.max(1) as f32,
    )
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

    /// The monotonic stamp read beside wall time `ms`, for a clock that was
    /// never stepped.
    fn mono_ms(ms: i64) -> std::time::Instant {
        static T0: std::sync::LazyLock<std::time::Instant> =
            std::sync::LazyLock::new(std::time::Instant::now);
        *T0 + std::time::Duration::from_millis(ms as u64)
    }

    fn frame(camera_id: CameraId, frame_id: u64, ms: i64) -> Frame {
        Frame {
            camera_id,
            frame_id,
            captured_at: Utc.timestamp_millis_opt(ms).unwrap(),
            captured_mono: mono_ms(ms),
            width: 1920,
            height: 1080,
            format: PixelFormat::Rgb24,
            data: Arc::new(vec![]),
            trace_id: format!("trace-{frame_id}"),
        }
    }

    fn vehicle(track_id: TrackId, cx: f32, cy: f32) -> TrackedObject {
        let bbox = BBox {
            x1: cx - 25.0,
            y1: cy - 15.0,
            x2: cx + 25.0,
            y2: cy + 15.0,
        };
        TrackedObject {
            track_id,
            label: "vehicle.car".into(),
            confidence: 0.95,
            bbox,
            // A detected frame: only those may place or refresh an anchor.
            detection_bbox: Some(bbox),
            age_frames: 1,
            age_ms: 33,
            attributes: Default::default(),
        }
    }

    fn person(track_id: TrackId, cx: f32, cy: f32) -> TrackedObject {
        labeled(track_id, "person", cx, cy)
    }

    fn labeled(track_id: TrackId, label: &str, cx: f32, cy: f32) -> TrackedObject {
        let bbox = BBox {
            x1: cx - 10.0,
            y1: cy - 20.0,
            x2: cx + 10.0,
            y2: cy + 20.0,
        };
        TrackedObject {
            track_id,
            label: label.into(),
            confidence: 0.95,
            bbox,
            detection_bbox: Some(bbox),
            age_frames: 1,
            age_ms: 33,
            attributes: Default::default(),
        }
    }

    #[test]
    fn parked_vehicle_is_suppressed_after_dwell_frames() {
        // Tight thresholds so the test runs in 4 frames.
        let cfg = StaticObjectConfig {
            dwell_frames: Some(3),
            dwell_secs: 0.0,
            anchor_persons: false,
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
            dwell_frames: Some(3),
            dwell_secs: 0.0,
            anchor_persons: false,
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
    fn extra_anchor_class_receives_alert_epoch_attribute() {
        // Phase 8.1 equipment classes participate in the suppression FSM,
        // so they MUST also carry the alert-epoch attribute — otherwise
        // nexus-rules falls back to the legacy camera-scoped cooldown and
        // re-alerts a stationary ladder every cooldown window (BUG-020,
        // equipment variant).
        let cfg = StaticObjectConfig {
            dwell_frames: Some(3),
            dwell_secs: 0.0,
            anchor_persons: false,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 5,
            track_id_reuse_reset_pixels: 0,
            anchor_ttl_secs: 0,
            persistence_enabled: false,
        };
        let mut f =
            StaticObjectFilter::with_anchor_classes(cfg, 1, None, vec!["ladder".to_string()]);

        let mut objs = vec![labeled(9, "ladder", 400.0, 400.0)];
        f.classify(&frame(1, 0, 0), &mut objs);

        assert_eq!(
            objs[0].attributes.get(ALERT_EPOCH_ATTRIBUTE_KEY),
            Some(&serde_json::json!(0)),
            "equipment anchor classes must be stamped with the alert epoch"
        );
    }

    #[test]
    fn departing_vehicle_erases_the_anchor_it_parked_on() {
        // A vehicle that breaks the static gate has, by definition,
        // moved further than `significant_movement_pixels` for several
        // frames — which is normally well outside the anchor's
        // `match_distance_pixels` radius. Demotion must still find and
        // erase the anchor it parked on, otherwise the spot keeps a
        // permanent anchor that silently suppresses every future
        // arrival (and the newcomer's own match keeps refreshing the
        // TTL, so the sweep never reclaims it either).
        let cfg = StaticObjectConfig {
            dwell_frames: Some(3),
            dwell_secs: 0.0,
            anchor_persons: false,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 5,
            track_id_reuse_reset_pixels: 0,
            anchor_ttl_secs: 0,
            persistence_enabled: true,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);

        for i in 0..6 {
            let mut objs = vec![vehicle(7, 500.0, 500.0)];
            f.classify(&frame(1, i, i as i64 * 33), &mut objs);
        }
        assert_eq!(f.anchors().len(), 1, "parked vehicle registers one anchor");

        // Drive off at 50px/frame — ten times the match radius.
        for i in 1..=3u64 {
            let mut objs = vec![vehicle(7, 500.0 + i as f32 * 50.0, 500.0)];
            f.classify(&frame(1, 5 + i, (5 + i) as i64 * 33), &mut objs);
            assert!(
                f.anchors().len() <= 1,
                "a departing vehicle must not mint new anchors along its exit \
                 path (got {} after moving frame {i})",
                f.anchors().len()
            );
        }

        assert!(
            f.anchors().is_empty(),
            "breaking the static gate must erase the vehicle's own anchor, \
             leaving the parking space free to alert on the next arrival"
        );
    }

    /// #335 invariant. At Hailo-8's ~1.9 fps a parked vehicle the detector
    /// sees once and then loses must promote to static before ByteTrack
    /// retires its track (default dwell 5 s ≈ 10 frames < max_lost 30).
    #[test]
    fn default_dwell_promotes_before_retirement_at_hailo_rate() {
        use crate::bytetrack::ByteTrackTracker;
        use crate::Tracker;
        use nexus_config::TrackerConfig;
        use nexus_types::Detection;

        let cfg = TrackerConfig::default();
        let tracker = ByteTrackTracker::new(cfg.bytetrack);
        let mut f = StaticObjectFilter::new(cfg.static_object, 1, None);
        let car = vehicle(0, 500.0, 300.0);
        let mut promoted_at = None;
        let mut retired_at = None;
        for i in 0..100u64 {
            let ms = (i as f64 / 1.9 * 1_000.0) as i64;
            let dets = if i == 0 {
                vec![Detection {
                    label: car.label.clone(),
                    confidence: 0.9,
                    bbox: car.bbox,
                    attributes: Default::default(),
                }]
            } else {
                vec![]
            };
            // Capture time at 1.9 fps, the stamp `frame` gives the classifier.
            let mut tracked = tracker.update(dets, mono_ms(ms));
            if tracked.is_empty() {
                retired_at = Some(i);
                break;
            }
            f.classify(&frame(1, i, ms), &mut tracked);
            if promoted_at.is_none() && tracked.iter().any(is_object_static) {
                promoted_at = Some(i);
            }
        }
        let (p, r) = (promoted_at, retired_at);
        assert!(
            matches!((p, r), (Some(p), Some(r)) if p < r),
            "promoted at {p:?}, retired at {r:?}"
        );
    }

    /// The measured inference rate is a duration between frames, so it is
    /// read on the monotonic capture stamp: an hour's step of the wall clock
    /// either way leaves the default dwell at 10 frames at 2 fps.
    #[test]
    fn a_wall_clock_step_does_not_move_the_default_dwell() {
        for step_ms in [3_600_000i64, -3_600_000] {
            let mut f = StaticObjectFilter::new(
                StaticObjectConfig {
                    persistence_enabled: false,
                    ..Default::default()
                },
                1,
                None,
            );
            let promoted_at = (0..40u64).find(|&i| {
                let mut fr = frame(1, i, i as i64 * 500);
                if i >= 3 {
                    fr.captured_at += chrono::Duration::milliseconds(step_ms);
                }
                let mut objs = vec![vehicle(1, 500.0, 300.0)];
                f.classify(&fr, &mut objs);
                objs.iter().any(is_object_static)
            });
            assert_eq!(promoted_at, Some(9), "{step_ms} ms step");
        }
    }

    /// #335: at 30 fps the default dwell resolves to 150 frames — the
    /// pre-#335 `dwell_frames = 150` behaviour, unchanged.
    #[test]
    fn default_dwell_is_150_frames_at_30_fps() {
        let mut f = StaticObjectFilter::new(
            StaticObjectConfig {
                persistence_enabled: false,
                ..Default::default()
            },
            1,
            None,
        );
        let promoted_at = (0..200u64).find(|&i| {
            let mut objs = vec![vehicle(1, 500.0, 300.0)];
            f.classify(&frame(1, i, (i as f64 / 30.0 * 1_000.0) as i64), &mut objs);
            objs.iter().any(is_object_static)
        });
        assert_eq!(promoted_at, Some(149), "150th static frame promotes");
    }

    #[test]
    fn stationary_person_is_suppressed_when_anchor_persons_is_on() {
        // #335: a phantom stationary `person` (misdetected furniture) must
        // stop looping alerts on a parking_lot_mode camera.
        let cfg = StaticObjectConfig {
            dwell_frames: Some(3),
            anchor_persons: true,
            persistence_enabled: false,
            ..Default::default()
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);
        let mut kept = Vec::new();
        for i in 0..4 {
            let mut objs = vec![person(7, 100.0, 100.0)];
            f.filter(&frame(1, i, i as i64 * 500), &mut objs);
            kept.push(objs.len());
        }
        assert_eq!(kept, vec![1, 1, 0, 0], "promotes on the third static frame");
    }

    #[test]
    fn non_vehicle_labels_bypass_filter() {
        // With `anchor_persons` off, even a perfectly stationary person
        // must NEVER be dropped.
        let cfg = StaticObjectConfig {
            dwell_frames: Some(1),
            dwell_secs: 0.0,
            anchor_persons: false,
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
            dwell_frames: Some(2),
            dwell_secs: 0.0,
            anchor_persons: false,
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
            dwell_frames: Some(999),
            dwell_secs: 0.0,
            anchor_persons: false,
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
            dwell_frames: Some(999),
            dwell_secs: 0.0,
            anchor_persons: false,
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
    fn breaking_static_gate_advances_epoch_and_requires_fresh_dwell() {
        let cfg = StaticObjectConfig {
            dwell_frames: Some(3),
            dwell_secs: 0.0,
            anchor_persons: false,
            significant_movement_pixels: 10,
            significant_movement_frames: 2,
            movement_ema_alpha: 1.0,
            match_distance_pixels: 30,
            track_id_reuse_reset_pixels: 0,
            anchor_ttl_secs: 0,
            persistence_enabled: true,
        };
        let mut f = StaticObjectFilter::new(cfg, 1, None);

        for i in 0..3 {
            let mut objs = vec![vehicle(11, 500.0, 500.0)];
            f.classify(&frame(1, i, i as i64 * 33), &mut objs);
            if i == 2 {
                assert!(is_object_static(&objs[0]));
            }
        }
        assert_eq!(f.anchors().len(), 1);

        let mut moving = vec![vehicle(11, 512.0, 500.0)];
        f.classify(&frame(1, 3, 99), &mut moving);
        assert!(is_object_static(&moving[0]));

        let mut broke_gate = vec![vehicle(11, 524.0, 500.0)];
        f.classify(&frame(1, 4, 132), &mut broke_gate);
        assert!(!is_object_static(&broke_gate[0]));
        assert_eq!(
            broke_gate[0]
                .attributes
                .get(ALERT_EPOCH_ATTRIBUTE_KEY)
                .and_then(Value::as_u64),
            Some(1)
        );
        assert!(f.anchors().is_empty(), "demotion must not reinsert anchor");

        let mut stopped = vec![vehicle(11, 524.0, 500.0)];
        f.classify(&frame(1, 5, 165), &mut stopped);
        assert!(
            !is_object_static(&stopped[0]),
            "demoted vehicle must complete a fresh dwell cycle"
        );
    }

    #[test]
    fn registry_round_trips_through_disk() {
        let tmp = std::env::temp_dir().join(format!(
            "nexus-static-{}.json",
            std::process::id().wrapping_add(rand_suffix())
        ));
        let _ = std::fs::remove_file(&tmp);

        let cfg = StaticObjectConfig {
            dwell_frames: Some(2),
            dwell_secs: 0.0,
            anchor_persons: false,
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
            dwell_frames: Some(2),
            dwell_secs: 0.0,
            anchor_persons: false,
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
        assert_eq!(
            objs[0]
                .attributes
                .get(ALERT_EPOCH_ATTRIBUTE_KEY)
                .and_then(Value::as_u64),
            Some(1),
            "reused track_id must begin a fresh alert epoch"
        );

        // And it should stay through on the very next frame (state
        // truly cleared, not just one-shot).
        let mut objs = vec![vehicle(99, 910.0, 500.0)];
        f.filter(&frame(1, 4, 132), &mut objs);
        assert_eq!(objs.len(), 1, "second frame must also pass");
    }

    /// #362: at low analysis rates ByteTrack links a moving vehicle across
    /// steps longer than `track_id_reuse_reset_pixels`. Such a step must not
    /// restart its alert epoch: that re-armed the alert on every frame, or
    /// reset a `consecutive_frames` streak on every frame so it never fired.
    /// The second path pauses first, then drives off in sub-threshold steps
    /// before a long one: by then it is moving, not standing.
    #[test]
    fn a_moving_vehicle_keeps_its_alert_epoch_across_long_steps() {
        let paths: [&[f32]; 2] = [
            &[60.0, 135.0, 210.0, 285.0, 360.0],
            &[60.0, 65.0, 115.0, 165.0, 215.0, 265.0, 340.0],
        ];
        for path in paths {
            let mut f = StaticObjectFilter::new(morgan_cfg(), 1, None);
            let epochs: Vec<_> = path
                .iter()
                .enumerate()
                .map(|(i, &x)| {
                    let mut objs = vec![vehicle(42, x, 175.0)];
                    f.classify(&frame(1, i as u64, i as i64 * 690), &mut objs);
                    objs[0]
                        .attributes
                        .get(ALERT_EPOCH_ATTRIBUTE_KEY)
                        .and_then(Value::as_u64)
                })
                .collect();
            assert_eq!(epochs, vec![Some(0); path.len()], "{path:?}");
        }
    }

    /// The guard's original case, for a track not yet promoted: a car three
    /// frames into its five-frame dwell, then a passer linked into its track
    /// about 70 px away. The car is wholly hidden on the frame before (a
    /// coasting frame), partly hidden (its visible part boxed 18 px off), or
    /// a box that jitters 12 px and so is never still. The passer must not
    /// inherit the dwell and be hidden. A track that never parked keeps its
    /// one alert episode, so the passer shares the car's (#362).
    #[test]
    fn a_passer_linked_into_a_car_still_learning_its_dwell_is_not_hidden() {
        type Seen = (f32, f32, bool);
        let cases: [(&str, [Seen; 4], f32); 3] = [
            (
                "wholly hidden",
                [
                    (100.0, 175.0, true),
                    (100.0, 172.0, true),
                    (100.0, 175.0, true),
                    (100.0, 175.0, false),
                ],
                188.0,
            ),
            (
                "partly hidden",
                [
                    (100.0, 175.0, true),
                    (100.0, 172.0, true),
                    (100.0, 175.0, true),
                    (118.0, 175.0, true),
                ],
                188.0,
            ),
            (
                "jittering",
                [
                    (100.0, 175.0, true),
                    (112.0, 175.0, true),
                    (100.0, 175.0, true),
                    (112.0, 175.0, true),
                ],
                182.0,
            ),
        ];
        for (case, seen, passer_x) in cases {
            let mut f = StaticObjectFilter::new(morgan_cfg(), 1, None);
            for (i, (x, y, detected)) in seen.into_iter().enumerate() {
                let mut car = vehicle(7, x, y);
                if !detected {
                    car.detection_bbox = None;
                }
                let mut objs = vec![car];
                f.classify(&frame(1, i as u64, i as i64 * 690), &mut objs);
            }
            for k in 0..4u64 {
                let x = passer_x + k as f32 * 50.0;
                let i = 4 + k;
                let mut objs = vec![vehicle(7, x, 175.0)];
                f.classify(&frame(1, i, i as i64 * 690), &mut objs);
                assert!(
                    !is_object_static(&objs[0]),
                    "{case}: passer hidden at x = {x}"
                );
                assert_eq!(
                    objs[0]
                        .attributes
                        .get(ALERT_EPOCH_ATTRIBUTE_KEY)
                        .and_then(Value::as_u64),
                    Some(0),
                    "{case}: a track that never parked keeps its alert episode"
                );
            }
        }
    }

    /// A car that left its spot is `departed`, so other cars' anchors do not
    /// hide it while it drives. If its track is then linked onto another
    /// parked car (a step over the guard's threshold), that car is parked,
    /// not departing: its anchor must hide it again once it is still.
    #[test]
    fn a_departed_track_linked_onto_a_parked_car_is_hidden_again() {
        let mut f = StaticObjectFilter::new(morgan_cfg(), 1, None);
        let mut i = 0u64;
        let mut step = |f: &mut StaticObjectFilter, objs: Vec<TrackedObject>| {
            let mut objs = objs;
            f.classify(&frame(1, i, i as i64 * 690), &mut objs);
            i += 1;
            objs
        };
        for k in 0..8 {
            let jitter = (k % 2) as f32 * 3.0;
            step(
                &mut f,
                vec![
                    vehicle(1, 100.0, 175.0 + jitter),
                    vehicle(2, 300.0, 175.0 + jitter),
                ],
            );
        }
        assert_eq!(f.anchors().len(), 2, "both cars are anchored");
        for x in [145.0, 190.0, 235.0] {
            let objs = step(&mut f, vec![vehicle(1, x, 175.0), vehicle(2, 300.0, 175.0)]);
            assert!(
                !is_object_static(&objs[0]),
                "the departing car is visible at x = {x}"
            );
        }
        // Track 1 now carries car 2, 65 px on; track 2 is gone.
        let hidden: Vec<bool> = (0..4)
            .map(|k| {
                let objs = step(
                    &mut f,
                    vec![vehicle(1, 300.0, 175.0 + (k % 2) as f32 * 3.0)],
                );
                is_object_static(&objs[0])
            })
            .collect();
        assert!(
            hidden[2..].iter().all(|&h| h),
            "car 2 hidden again: {hidden:?}"
        );
    }

    #[test]
    fn track_id_reuse_guard_disabled_when_zero() {
        // Same scenario as above but with `track_id_reuse_reset_pixels: 0`
        // — the guard is off, so the new occupant DOES inherit the
        // promoted state. Verifies the kill switch.
        let cfg = StaticObjectConfig {
            dwell_frames: Some(2),
            dwell_secs: 0.0,
            anchor_persons: false,
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
            dwell_frames: Some(999),
            dwell_secs: 0.0,
            anchor_persons: false,
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
            dwell_frames: Some(999),
            dwell_secs: 0.0,
            anchor_persons: false,
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

        // Frames 0.9 s apart over 5 s, vehicle still parked at the anchor.
        // Each one after the first stood still, so each refreshes (#367:
        // a track's first frame alone does not).
        for i in 1..=6u64 {
            let mut objs = vec![vehicle(42, 200.0, 200.0)];
            f.classify(&frame(1, i, i as i64 * 900), &mut objs);
        }

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
            dwell_frames: Some(999),
            dwell_secs: 0.0,
            anchor_persons: false,
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

    /// MORGAN's deployed settings (#367), in 512×288 analysis pixels, with
    /// a short pinned dwell so the replays stay small.
    fn morgan_cfg() -> StaticObjectConfig {
        StaticObjectConfig {
            dwell_frames: Some(5),
            dwell_secs: 0.0,
            anchor_persons: false,
            significant_movement_pixels: 36,
            significant_movement_frames: 3,
            movement_ema_alpha: 0.35,
            match_distance_pixels: 40,
            track_id_reuse_reset_pixels: 60,
            anchor_ttl_secs: 3600,
            persistence_enabled: true,
        }
    }

    /// Park track 8587 at (24, 270) with a few px of box jitter, then
    /// replay the grey sedan ByteTrack linked into it (#367, Cam 01,
    /// 11:54:44 UTC). Returns, per drive-off frame, the registry and
    /// whether the sedan was hidden.
    fn replay_linked_sedan() -> Vec<(Vec<StaticAnchor>, bool)> {
        let mut f = StaticObjectFilter::new(morgan_cfg(), 1, None);
        let mut ms = 0;
        for i in 0..10u64 {
            let mut objs = vec![vehicle(8587, 24.0, 270.0 - (i % 2) as f32 * 5.0)];
            f.classify(&frame(1, i, ms), &mut objs);
            ms += 690;
        }
        assert_eq!(f.anchors().len(), 1, "the parked car is anchored");
        let path = [(36.0, 263.0), (47.0, 250.0), (68.0, 229.0), (88.0, 200.0)];
        path.iter()
            .enumerate()
            .map(|(i, &(x, y))| {
                let mut objs = vec![vehicle(8587, x, y)];
                f.classify(&frame(1, 10 + i as u64, ms), &mut objs);
                ms += 690;
                (f.anchors().to_vec(), is_object_static(&objs[0]))
            })
            .collect()
    }

    #[test]
    fn a_vehicle_linked_into_a_parked_track_does_not_drag_its_anchor() {
        for (i, (anchors, _)) in replay_linked_sedan().iter().enumerate() {
            for a in anchors {
                let off = ((a.center_x - 24.0).powi(2) + (a.center_y - 268.0).powi(2)).sqrt();
                assert!(
                    off < 5.0,
                    "drive-off frame {i}: anchor at ({}, {}) left the parked \
                     spot and followed the linked vehicle into the lane",
                    a.center_x,
                    a.center_y
                );
            }
        }
    }

    #[test]
    fn a_linked_vehicle_is_not_hidden_once_it_leaves_its_anchor() {
        let hidden: Vec<bool> = replay_linked_sedan().into_iter().map(|(_, h)| h).collect();
        // (68, 229) is the first centre beyond the 40 px match radius.
        assert_eq!(
            &hidden[2..],
            &[false, false],
            "a vehicle more than match_distance_pixels from the anchor its \
             track owns is not that parked car, and must reach the rules \
             (hidden per frame: {hidden:?})"
        );
    }

    #[test]
    fn a_slow_drag_inside_the_match_radius_does_not_move_the_anchor() {
        let mut f = StaticObjectFilter::new(morgan_cfg(), 1, None);
        let mut ms = 0;
        for i in 0..8u64 {
            let mut objs = vec![vehicle(1, 200.0, 100.0)];
            f.classify(&frame(1, i, ms), &mut objs);
            ms += 690;
        }
        // 5–12 px steps: inside the match radius, under the EMA threshold.
        for (i, x) in [205.0, 212.0, 224.0, 236.0].into_iter().enumerate() {
            let mut objs = vec![vehicle(1, x, 100.0)];
            f.classify(&frame(1, 8 + i as u64, ms), &mut objs);
            ms += 690;
            let a = &f.anchors()[0];
            assert_eq!(
                (a.center_x, a.center_y),
                (200.0, 100.0),
                "a moving observation must not move the anchor (step {i})"
            );
        }
    }

    #[test]
    fn traffic_through_an_anchor_does_not_keep_it_alive() {
        // An anchor left in a lane (the #367 residue, or one loaded from a
        // pre-fix registry) sees only passing vehicles: each a fresh
        // two-frame track fragment moving 20 px/frame through it. Only a
        // vehicle standing still may refresh it, so it must age out.
        for last_seen in [Some(0), None] {
            let mut f = StaticObjectFilter::new(
                StaticObjectConfig {
                    anchor_ttl_secs: 60,
                    ..morgan_cfg()
                },
                1,
                None,
            );
            f.anchors.push(StaticAnchor {
                label: "vehicle.car".into(),
                center_x: 421.0,
                center_y: 115.0,
                last_seen_unix_ms: last_seen,
            });
            let mut frame_id = 0;
            for pass in 0..12u64 {
                let ms = pass as i64 * 10_000;
                for (step, x) in [411.0, 431.0].into_iter().enumerate() {
                    let mut objs = vec![vehicle(100 + pass, x, 115.0)];
                    f.classify(&frame(1, frame_id, ms + step as i64 * 690), &mut objs);
                    frame_id += 1;
                }
            }
            assert!(
                f.anchors().is_empty(),
                "traffic kept a lane anchor alive past its TTL \
                 (last_seen at start: {last_seen:?})"
            );
        }
    }

    /// ByteTrack keeps emitting a lost track at its last box, with no
    /// detection, for up to `max_lost_frames`. Those coasting frames show
    /// no movement but saw nothing, so they must not refresh an anchor, or
    /// every passing fragment keeps a lane anchor alive for 30 more frames
    /// (#368 review).
    #[test]
    fn a_coasting_track_does_not_refresh_an_anchor() {
        use crate::bytetrack::ByteTrackTracker;
        use crate::Tracker;
        use nexus_config::TrackerConfig;
        use nexus_types::Detection;

        let tracker = ByteTrackTracker::new(TrackerConfig::default().bytetrack);
        let mut f = StaticObjectFilter::new(morgan_cfg(), 1, None);
        f.anchors.push(StaticAnchor {
            label: "vehicle.car".into(),
            center_x: 500.0,
            center_y: 300.0,
            last_seen_unix_ms: Some(0),
        });
        let car = vehicle(0, 500.0, 300.0);
        for i in 0..10u64 {
            let ms = i as i64 * 690;
            // Detected on frames 0 and 1, then only coasting.
            let dets = if i < 2 {
                vec![Detection {
                    label: car.label.clone(),
                    confidence: 0.9,
                    bbox: car.bbox,
                    attributes: Default::default(),
                }]
            } else {
                vec![]
            };
            let mut tracked = tracker.update(dets, mono_ms(ms));
            assert_eq!(tracked.len(), 1, "frame {i}: the track is still emitted");
            f.classify(&frame(1, i, ms), &mut tracked);
        }
        assert_eq!(
            f.anchors()[0].last_seen_unix_ms,
            Some(690),
            "only the last detected still frame may refresh the anchor"
        );
    }

    // ---- SPEC-077 parking_policy_v2 ----

    use nexus_config::{ParkingPolicyV2, ZoneConfig, ZoneKind};

    const HD: (u32, u32) = (1920, 1080);
    const V2_FPS: [f64; 5] = [0.5, 1.45, 2.0, 8.0, 30.0];

    /// The plan's test values, not customer defaults: dwell 5 s, speed
    /// <= 0.05 diagonals/s, drift <= 0.10 diagonals, gap <= 3 s.
    fn v2_policy() -> ParkingPolicyV2 {
        ParkingPolicyV2 {
            dwell_secs: 5.0,
            max_speed_box_diagonals_per_sec: 0.05,
            max_displacement_box_diagonals: 0.10,
            max_observation_gap_secs: 3.0,
        }
    }

    /// Stalls over the left half of the frame; the right half is the lane.
    fn stalls() -> Vec<ZoneConfig> {
        vec![ZoneConfig {
            id: "stalls".into(),
            name: "Stalls".into(),
            polygon: vec![(0.0, 0.0), (0.5, 0.0), (0.5, 1.0), (0.0, 1.0)],
            kind: ZoneKind::Parking,
            min_bbox_area_px_override: None,
        }]
    }

    fn v2_filter() -> StaticObjectFilter {
        StaticObjectFilter::new(StaticObjectConfig::default(), 1, None)
            .with_parking_policy(&v2_policy(), &stalls())
            .expect("valid policy")
    }

    /// A tracked box the detector did not see this frame.
    fn coasting(mut o: TrackedObject) -> TrackedObject {
        o.detection_bbox = None;
        o
    }

    /// Classify `objs` on a `size` frame captured at `ms`.
    fn v2_step(
        f: &mut StaticObjectFilter,
        ms: i64,
        size: (u32, u32),
        mut objs: Vec<TrackedObject>,
    ) -> Vec<TrackedObject> {
        let mut fr = frame(1, 0, ms);
        (fr.width, fr.height) = size;
        f.classify(&fr, &mut objs);
        objs
    }

    /// Capture ms of `fps` frames from `from_ms` until `to_ms`.
    fn capture_ms(fps: f64, from_ms: i64, to_ms: i64) -> Vec<i64> {
        (0..)
            .map(|k| from_ms + (k as f64 * 1000.0 / fps).round() as i64)
            .take_while(|&ms| ms <= to_ms)
            .collect()
    }

    /// The capture ms of the first frame on which the one object is static.
    fn first_static(
        f: &mut StaticObjectFilter,
        frames: &[i64],
        at: impl Fn(usize) -> TrackedObject,
    ) -> Option<i64> {
        let mut first = None;
        for (k, &ms) in frames.iter().enumerate() {
            let out = v2_step(f, ms, HD, vec![at(k)]);
            if first.is_none() && is_object_static(&out[0]) {
                first = Some(ms);
            }
        }
        first
    }

    /// Dwell is observed capture time, not frames: a parked car promotes on
    /// the first frame at or after 5 s at every analysis rate.
    #[test]
    fn v2_promotes_after_observed_capture_seconds_at_any_rate() {
        for fps in V2_FPS {
            let frames = capture_ms(fps, 0, 8_000);
            let want = frames.iter().copied().find(|&ms| ms >= 5_000);
            let got = first_static(&mut v2_filter(), &frames, |_| vehicle(1, 400.0, 500.0));
            assert_eq!(got, want, "{fps} fps");
        }
    }

    /// SPEC-077: at 1.45 fps a vehicle moving 20 px a frame never promotes.
    #[test]
    fn v2_a_vehicle_moving_20_px_a_frame_at_1_45_fps_never_promotes() {
        let frames = capture_ms(1.45, 0, 30_000);
        let got = first_static(&mut v2_filter(), &frames, |k| {
            vehicle(1, 100.0 + 20.0 * k as f32, 500.0)
        });
        assert_eq!(got, None);
    }

    /// A car queueing in the lane, outside every parking zone, stands still
    /// for far longer than the dwell and is never hidden.
    #[test]
    fn v2_a_vehicle_standing_in_the_lane_is_never_hidden() {
        for fps in V2_FPS {
            let frames = capture_ms(fps, 0, 30_000);
            let got = first_static(&mut v2_filter(), &frames, |_| vehicle(1, 1400.0, 500.0));
            assert_eq!(got, None, "{fps} fps");
        }
    }

    /// Coasting frames are not observations: they are never static, and a
    /// reacquired car must hold still, observed, for a fresh dwell.
    #[test]
    fn v2_coasting_frames_credit_no_stillness() {
        let mut f = v2_filter();
        let car = || vehicle(1, 400.0, 500.0);
        for ms in capture_ms(1.45, 0, 6_000) {
            v2_step(&mut f, ms, HD, vec![car()]);
        }
        assert!(is_object_static(
            &v2_step(&mut f, 6_900, HD, vec![car()])[0]
        ));
        for ms in [7_600, 8_300, 9_000] {
            let out = v2_step(&mut f, ms, HD, vec![coasting(car())]);
            assert!(!is_object_static(&out[0]), "coasting at {ms} ms");
        }
        let back = capture_ms(1.45, 9_700, 20_000);
        assert_eq!(
            first_static(&mut f, &back, |_| car()),
            back.iter().copied().find(|&ms| ms >= 9_700 + 5_000),
            "a fresh observed dwell from reacquisition"
        );
    }

    /// The same pixel center on a new frame geometry is a new candidate.
    #[test]
    fn v2_a_frame_geometry_change_starts_a_fresh_candidate() {
        let mut f = v2_filter();
        let car = || vehicle(1, 400.0, 500.0);
        for ms in capture_ms(1.0, 0, 4_000) {
            assert!(!is_object_static(&v2_step(&mut f, ms, HD, vec![car()])[0]));
        }
        let first = capture_ms(1.0, 5_000, 12_000)
            .into_iter()
            .find(|&ms| is_object_static(&v2_step(&mut f, ms, (1920, 1200), vec![car()])[0]));
        assert_eq!(first, Some(10_000), "no elapsed time is inherited");
    }

    /// A resize keeps each anchor at its place in the view: the annotator
    /// sees it there from the first resized frame, and the car's fresh dwell
    /// in the new geometry matches it instead of placing a second one.
    #[test]
    fn v2_a_resize_keeps_each_anchor_at_its_place_in_the_view() {
        let mut f = v2_filter();
        park_all(&mut f, &[(1, 400.0, 500.0)]);
        assert_eq!(anchor_centers(&f), vec![(400.0, 500.0)]);
        let half = (960, 540);
        let mut fr = frame(1, 0, 7_600);
        (fr.width, fr.height) = half;
        let scoped: Vec<_> = f
            .scoped_anchors(&fr)
            .iter()
            .map(|a| (a.center_x, a.center_y))
            .collect();
        assert_eq!(
            scoped,
            vec![(200.0, 250.0)],
            "before the frame is classified"
        );
        let mut out = Vec::new();
        for ms in capture_ms(1.45, 7_600, 14_000) {
            out = v2_step(&mut f, ms, half, vec![vehicle(1, 200.0, 250.0)]);
        }
        assert!(is_object_static(&out[0]), "dwelled again");
        assert_eq!(anchor_centers(&f), vec![(200.0, 250.0)], "not duplicated");
    }

    /// Leaving where it parked starts the car's next alert episode.
    #[test]
    fn v2_leaving_its_spot_starts_a_new_alert_episode() {
        let mut f = v2_filter();
        let epoch = |o: &TrackedObject| o.attributes[ALERT_EPOCH_ATTRIBUTE_KEY].clone();
        for ms in capture_ms(1.45, 0, 6_000) {
            v2_step(&mut f, ms, HD, vec![vehicle(1, 400.0, 500.0)]);
        }
        let parked = v2_step(&mut f, 6_900, HD, vec![vehicle(1, 400.0, 500.0)]);
        assert!(is_object_static(&parked[0]));
        assert_eq!(epoch(&parked[0]), 0);
        let mut x = 400.0;
        for ms in capture_ms(1.45, 7_590, 12_000) {
            x += 15.0;
            let out = v2_step(&mut f, ms, HD, vec![vehicle(1, x, 500.0)]);
            assert!(!is_object_static(&out[0]), "moving at {ms} ms");
            let want = if x - 400.0 > 40.0 { 1 } else { 0 };
            assert_eq!(epoch(&out[0]), want, "x = {x}");
        }
    }

    fn anchor_centers(f: &StaticObjectFilter) -> Vec<(f32, f32)> {
        f.anchors()
            .iter()
            .map(|a| (a.center_x, a.center_y))
            .collect()
    }

    fn epoch_of(o: &TrackedObject) -> u64 {
        o.attributes[ALERT_EPOCH_ATTRIBUTE_KEY]
            .as_u64()
            .expect("epoch")
    }

    /// Hold `cars` (track, x, y) still at 1.45 fps until 6.9 s; the last
    /// frame's objects.
    fn park_all(f: &mut StaticObjectFilter, cars: &[(TrackId, f32, f32)]) -> Vec<TrackedObject> {
        let mut out = Vec::new();
        for ms in capture_ms(1.45, 0, 6_900) {
            out = v2_step(
                f,
                ms,
                HD,
                cars.iter().map(|&(id, x, y)| vehicle(id, x, y)).collect(),
            );
        }
        out
    }

    fn v2_filter_with_ttl(anchor_ttl_secs: u32) -> StaticObjectFilter {
        let cfg = StaticObjectConfig {
            anchor_ttl_secs,
            ..Default::default()
        };
        StaticObjectFilter::new(cfg, 1, None)
            .with_parking_policy(&v2_policy(), &stalls())
            .expect("valid policy")
    }

    /// Both the car and the anchor must be inside a parking zone: an anchor
    /// from before the zones, 30 px across the lane line from a stall, is
    /// neither matched nor refreshed nor moved, and ages out; a car parked
    /// in the lane places none.
    #[test]
    fn v2_anchors_are_placed_matched_and_refreshed_only_inside_parking() {
        let mut f = v2_filter_with_ttl(10);
        f.anchors.push(StaticAnchor {
            label: "vehicle.car".into(),
            center_x: 980.0,
            center_y: 500.0,
            last_seen_unix_ms: Some(0),
        });
        let cars = [(1, 950.0, 500.0), (2, 1400.0, 700.0)];
        let out = park_all(&mut f, &cars);
        assert!(is_object_static(&out[0]) && !is_object_static(&out[1]));
        assert_eq!(anchor_centers(&f), vec![(980.0, 500.0), (950.0, 500.0)]);
        assert_eq!(f.anchors()[0].last_seen_unix_ms, Some(0));
        for ms in capture_ms(1.45, 7_600, 11_000) {
            v2_step(
                &mut f,
                ms,
                HD,
                cars.iter().map(|&(id, x, y)| vehicle(id, x, y)).collect(),
            );
        }
        assert_eq!(
            anchor_centers(&f),
            vec![(950.0, 500.0)],
            "the outside one expired"
        );
    }

    /// A car crawling past two parked cars, through each one's anchor radius:
    /// it is never hidden, keeps one alert episode, and both anchors and
    /// both parked verdicts stay.
    #[test]
    fn v2_a_passing_car_cannot_claim_erase_or_reset_an_anchor() {
        let mut f = v2_filter();
        let parked = [(1, 300.0, 500.0), (3, 300.0, 700.0)];
        park_all(&mut f, &parked);
        let anchors = anchor_centers(&f);
        assert_eq!(anchors.len(), 2);
        for (k, ms) in capture_ms(1.45, 7_600, 20_000).into_iter().enumerate() {
            let mut objs: Vec<_> = parked.iter().map(|&(id, x, y)| vehicle(id, x, y)).collect();
            objs.push(vehicle(2, 320.0, 300.0 + 30.0 * k as f32));
            let out = v2_step(&mut f, ms, HD, objs);
            assert!(
                is_object_static(&out[0]) && is_object_static(&out[1]),
                "{ms} ms"
            );
            assert!(!is_object_static(&out[2]), "passer hidden at {ms} ms");
            assert_eq!(epoch_of(&out[2]), 0, "passer's episode at {ms} ms");
        }
        assert_eq!(anchor_centers(&f), anchors);
    }

    /// An owner seen driving out of its anchor's radius erases that anchor.
    #[test]
    fn v2_an_owner_seen_leaving_erases_its_anchor() {
        let mut f = v2_filter();
        park_all(&mut f, &[(1, 400.0, 500.0)]);
        assert_eq!(anchor_centers(&f), vec![(400.0, 500.0)]);
        let mut x = 400.0;
        for ms in capture_ms(1.45, 7_600, 11_000) {
            x += 15.0;
            v2_step(&mut f, ms, HD, vec![vehicle(1, x, 500.0)]);
            assert_eq!(f.anchors().is_empty(), x - 400.0 > 40.0, "x = {x}");
        }
    }

    /// Each way an owner's track can come back as possibly another object:
    /// the anchor stays, to expire on its TTL, and the moving object is
    /// visible in a new alert episode.
    #[test]
    fn v2_an_ambiguous_identity_leaves_the_anchor_to_expire() {
        let away = || vehicle(1, 450.0, 500.0);
        // (name, frames as (capture ms, objects))
        type Case = (&'static str, Vec<(i64, Vec<TrackedObject>)>);
        let cases: Vec<Case> = vec![
            (
                "a jump past the ID-reuse step",
                vec![(7_600, vec![vehicle(1, 465.0, 500.0)])],
            ),
            (
                "another track still at the anchor",
                vec![(7_600, vec![away(), vehicle(9, 400.0, 500.0)])],
            ),
            (
                "after a coasting frame",
                vec![
                    (7_600, vec![coasting(vehicle(1, 400.0, 500.0))]),
                    (8_300, vec![away()]),
                ],
            ),
            (
                "after a frame without the track (a reused id)",
                vec![(7_600, vec![]), (8_300, vec![away()])],
            ),
            ("after an observation gap", vec![(11_000, vec![away()])]),
        ];
        for (name, frames) in cases {
            let mut f = v2_filter_with_ttl(10);
            park_all(&mut f, &[(1, 400.0, 500.0)]);
            let mut last = Vec::new();
            for (ms, objs) in frames {
                last = v2_step(&mut f, ms, HD, objs);
            }
            assert_eq!(anchor_centers(&f), vec![(400.0, 500.0)], "{name}");
            assert!(!is_object_static(&last[0]), "{name}");
            assert_eq!(epoch_of(&last[0]), 1, "{name}");
            v2_step(&mut f, 17_000, HD, vec![vehicle(1, 600.0, 500.0)]);
            assert!(f.anchors().is_empty(), "{name}: expired on its TTL");
        }
    }

    /// An 800x600 box at (x, 500): a 1000 px diagonal, whose drift bound
    /// (0.10 diagonals) is 100 px.
    fn big_car(track_id: TrackId, x: f32) -> TrackedObject {
        let mut o = vehicle(track_id, x, 500.0);
        let b = BBox {
            x1: x - 400.0,
            y1: 200.0,
            x2: x + 400.0,
            y2: 800.0,
        };
        (o.bbox, o.detection_bbox) = (b, Some(b));
        o
    }

    /// A parked track whose id jumps past the ID-reuse step yet within both
    /// motion bounds (a 1000 px diagonal, 2 s later) is another object: it
    /// is visible in a new alert episode until its own fresh dwell, also
    /// when it lands inside the anchor radius the parked car matched.
    #[test]
    fn v2_a_jump_past_the_id_reuse_step_starts_a_fresh_dwell() {
        let big = |x| big_car(1, x);
        // (name, seeded anchor, parked x, x after the jump, anchors after)
        let cases = [
            (
                "75 px from its own anchor",
                None,
                400.0,
                475.0,
                vec![(400.0, 500.0), (475.0, 500.0)],
            ),
            (
                "61 px, within the anchor it matched",
                Some(anchor_at(400.0, 500.0)),
                430.0,
                369.0,
                vec![(400.0, 500.0)],
            ),
        ];
        for (name, seeded, from, to, anchors) in cases {
            let mut f = v2_filter();
            f.anchors.extend(seeded);
            let mut out = Vec::new();
            for ms in capture_ms(1.45, 0, 6_900) {
                out = v2_step(&mut f, ms, HD, vec![big(from)]);
            }
            assert!(is_object_static(&out[0]), "{name}: parked");
            assert_eq!(epoch_of(&out[0]), 0, "{name}");
            for ms in capture_ms(1.45, 8_900, 16_000) {
                let out = v2_step(&mut f, ms, HD, vec![big(to)]);
                assert_eq!(is_object_static(&out[0]), ms >= 13_900, "{name}: {ms} ms");
                assert_eq!(epoch_of(&out[0]), 1, "{name}: {ms} ms");
            }
            assert_eq!(anchor_centers(&f), anchors, "{name}");
        }
    }

    /// A large parked car's id seen 50 px away, out of the anchor radius, or
    /// 30 px away, inside it, both inside the ID-reuse step and the drift
    /// bound, while another track stands at its anchor: that ambiguous
    /// identity borrows no dwell and erases no anchor. Once it has its own
    /// dwell it stays parked beside the other. With nothing else at its
    /// anchor, the same 30 px creep keeps the owner parked in its episode.
    #[test]
    fn v2_an_ambiguous_identity_cannot_borrow_a_large_boxs_dwell() {
        // (name, the id's new x, another track at the anchor, anchors after)
        let cases = [
            ("50 px", 450.0, true, vec![(400.0, 500.0), (450.0, 500.0)]),
            ("30 px", 430.0, true, vec![(400.0, 500.0)]),
            ("30 px, alone", 430.0, false, vec![(400.0, 500.0)]),
        ];
        for (name, x, other, anchors) in cases {
            let mut f = v2_filter();
            for ms in capture_ms(1.45, 0, 6_900) {
                v2_step(&mut f, ms, HD, vec![big_car(1, 400.0)]);
            }
            for ms in capture_ms(1.45, 7_590, 20_000) {
                let mut objs = vec![big_car(1, x)];
                objs.extend(other.then(|| big_car(9, 400.0)));
                let out = v2_step(&mut f, ms, HD, objs);
                let (parked, episode) = if other { (ms >= 12_590, 1) } else { (true, 0) };
                assert_eq!(is_object_static(&out[0]), parked, "{name}: {ms} ms");
                assert_eq!(epoch_of(&out[0]), episode, "{name}: {ms} ms");
            }
            assert_eq!(anchor_centers(&f), anchors, "{name}");
        }
    }

    /// A parked car whose detection drops out keeps its episode but not its
    /// anchor: coasting refreshes nothing, and it reclaims the same anchor
    /// only after a fresh observed dwell. A lone arrival that then only
    /// coasts places no anchor.
    #[test]
    fn v2_a_reacquired_car_reclaims_its_anchor_only_after_a_fresh_dwell() {
        let mut f = v2_filter();
        park_all(&mut f, &[(1, 400.0, 500.0)]);
        let refreshed = |f: &StaticObjectFilter| f.anchors()[0].last_seen_unix_ms;
        let before = refreshed(&f);
        for ms in [7_600, 8_300] {
            v2_step(&mut f, ms, HD, vec![coasting(vehicle(1, 400.0, 500.0))]);
        }
        for ms in capture_ms(1.45, 9_000, 16_000) {
            let out = v2_step(&mut f, ms, HD, vec![vehicle(1, 400.0, 500.0)]);
            let qualified = ms >= 14_000;
            assert_eq!(is_object_static(&out[0]), qualified, "{ms} ms");
            assert_eq!(epoch_of(&out[0]), 0, "the same parked episode");
            if !qualified {
                assert_eq!(refreshed(&f), before, "{ms} ms");
            }
        }
        assert_eq!(anchor_centers(&f), vec![(400.0, 500.0)]);
        assert!(refreshed(&f) > before);

        let mut lone = v2_filter();
        v2_step(&mut lone, 0, HD, vec![vehicle(1, 400.0, 500.0)]);
        for ms in capture_ms(1.45, 690, 20_000) {
            v2_step(&mut lone, ms, HD, vec![coasting(vehicle(1, 400.0, 500.0))]);
        }
        assert!(lone.anchors().is_empty());
    }

    /// A parked id missing from a frame and back at its spot may be another
    /// car: it is visible in a new alert episode until its own fresh dwell,
    /// which matches the same anchor.
    #[test]
    fn v2_a_parked_id_missing_from_a_frame_returns_in_a_new_episode() {
        let mut f = v2_filter();
        park_all(&mut f, &[(1, 400.0, 500.0)]);
        v2_step(&mut f, 7_600, HD, vec![]);
        for ms in capture_ms(1.45, 8_300, 16_000) {
            let out = v2_step(&mut f, ms, HD, vec![vehicle(1, 400.0, 500.0)]);
            assert_eq!(is_object_static(&out[0]), ms >= 13_300, "{ms} ms");
            assert_eq!(epoch_of(&out[0]), 1, "{ms} ms");
        }
        assert_eq!(anchor_centers(&f), vec![(400.0, 500.0)]);
    }

    /// Track ids come and go for a camera's lifetime: the filter keeps only
    /// those seen within the observation gap.
    #[test]
    fn v2_forgets_tracks_absent_past_the_observation_gap() {
        let mut f = v2_filter();
        for (k, ms) in capture_ms(2.0, 0, 60_000).into_iter().enumerate() {
            v2_step(&mut f, ms, HD, vec![vehicle(k as TrackId, 400.0, 500.0)]);
        }
        let kept = f.parking_v2.as_ref().map(|v2| v2.tracks.len());
        assert_eq!(kept, Some(7), "this frame's id and those of the last 3 s");
    }

    fn anchor_at(x: f32, y: f32) -> StaticAnchor {
        StaticAnchor {
            label: "vehicle.car".into(),
            center_x: x,
            center_y: y,
            last_seen_unix_ms: Some(0),
        }
    }

    /// Whether a person standing on (x, y) is stamped near a static vehicle
    /// when the annotator is handed the filter's scoped anchors.
    fn stamped_near_anchor(f: &StaticObjectFilter, x: f32, y: f32) -> bool {
        use crate::annotator::TrackAnnotator;
        let mut fr = frame(1, 0, 0);
        (fr.width, fr.height) = HD;
        let mut objs = vec![person(9, x, y)];
        TrackAnnotator::new(nexus_config::AnnotatorConfig::default()).annotate(
            &fr,
            &[],
            &f.scoped_anchors(&fr),
            &mut objs,
        );
        objs[0]
            .attributes
            .contains_key("motion.near_static_vehicle_id")
    }

    /// SPEC-077: a v2 camera ignores anchors outside Parking, so the
    /// annotator must not see them either; legacy still sees every anchor.
    #[test]
    fn v2_annotator_sees_only_in_parking_anchors() {
        let mut v2 = v2_filter();
        let mut legacy = StaticObjectFilter::new(StaticObjectConfig::default(), 1, None);
        for f in [&mut v2, &mut legacy] {
            f.anchors.push(anchor_at(400.0, 500.0)); // stall
            f.anchors.push(anchor_at(1400.0, 700.0)); // lane, a legacy-era anchor
        }
        assert!(stamped_near_anchor(&v2, 400.0, 500.0));
        assert!(!stamped_near_anchor(&v2, 1400.0, 700.0));
        assert!(stamped_near_anchor(&legacy, 400.0, 500.0));
        assert!(stamped_near_anchor(&legacy, 1400.0, 700.0));
    }
}

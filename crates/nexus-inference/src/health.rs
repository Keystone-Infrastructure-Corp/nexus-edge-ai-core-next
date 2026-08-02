//! Detector health registry — process-wide record of why object
//! detection is unavailable or degraded.
//!
//! ## Why this exists
//!
//! Before this module, a detector whose ONNX model failed to load was
//! silently swapped for [`crate::detectors::MockDetector`], which emits
//! one synthetic `person` box **per frame, per camera, forever**. The
//! rules engine cannot distinguish a synthetic detection from a real
//! one, so a single missing model file turned into a fleet-wide alert
//! flood of fabricated "person" events — with the only evidence being a
//! `WARN` line at boot that nobody was watching.
//!
//! A detector that cannot load its model must **fail loudly and produce
//! nothing**, never fabricate. This registry is the "loudly" half: the
//! build path records a structured reason here, the engine's health
//! endpoint reports `status: "degraded"`, and the cloud heartbeat can
//! surface the same signal in the console.
//!
//! ## Why a process-global
//!
//! `build_detector_for_yolo(&InferenceConfig)` and its siblings are free
//! functions with no engine context — they're called from the engine,
//! from `DetectorPool` worker threads, and from the standalone
//! `nexus-inference-worker` binary. Threading a handle through every one
//! of those call sites would touch far more code than the signal is
//! worth. A module-level registry keeps the reporting edge-triggered and
//! local to the failure site.

use std::sync::{Arc, OnceLock, RwLock};

use crate::detectors::{Detector, MockDetector, UnavailableDetector};

/// One reason detection is degraded on this engine.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DetectorDegradation {
    /// Model kind that failed to build (`yolo`, `yoloe`, `yolo_world`, …).
    pub kind: String,
    /// Operator-facing cause, verbatim from the underlying
    /// [`crate::InferenceError`] — includes the resolver's
    /// "missing the shape-matched ONNX … Available in pack: […]"
    /// diagnostic, which names the exact fix.
    pub reason: String,
}

fn registry() -> &'static RwLock<Vec<DetectorDegradation>> {
    static REGISTRY: OnceLock<RwLock<Vec<DetectorDegradation>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Record that a detector could not be built and that this engine is
/// therefore not performing real detection for `kind`.
///
/// Deduplicated: the pool builds one detector per worker slot (plus a
/// `fail_soft` fallback), so the same failure arrives 3–4 times at boot
/// and would otherwise be reported as several distinct problems.
pub fn record_degraded(kind: &str, reason: impl Into<String>) {
    let entry = DetectorDegradation {
        kind: kind.to_string(),
        reason: reason.into(),
    };
    if let Ok(mut guard) = registry().write() {
        if !guard.contains(&entry) {
            guard.push(entry);
        }
    }
}

/// Clear all recorded degradations. Called when a detector for `kind`
/// builds successfully, so a healthy rebuild (e.g. after the operator
/// fixes the preset and restarts) doesn't keep reporting a stale
/// failure.
pub fn clear_degraded(kind: &str) {
    if let Ok(mut guard) = registry().write() {
        guard.retain(|d| d.kind != kind);
    }
}

/// Snapshot of every currently-recorded degradation. Empty == healthy.
pub fn degradations() -> Vec<DetectorDegradation> {
    registry()
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| Vec::new())
}

/// `true` when object detection is not fully operational on this engine.
pub fn is_degraded() -> bool {
    registry().read().map(|g| !g.is_empty()).unwrap_or(false)
}

/// `true` iff the operator has explicitly opted in to the synthetic
/// [`crate::detectors::MockDetector`] as a *fallback* for a detector
/// that failed to load.
///
/// This is a **development / CI escape hatch only**. The mock fabricates
/// a moving `person` box on every frame; in production that manufactures
/// alerts for objects that do not exist. Unset (the default) means a
/// failed model load yields zero detections plus a degraded health
/// state, never fake ones.
///
/// Note this does NOT gate an explicit `[inference.model] kind = "mock"`
/// — asking for the mock by name is a legitimate test configuration and
/// stays available.
pub fn mock_fallback_allowed() -> bool {
    matches!(
        std::env::var("NEXUS_ALLOW_MOCK_DETECTOR").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes")
    )
}

/// The single decision point for "a detector failed to build".
///
/// Logs at `ERROR` (not `WARN` — this is a total loss of detection for
/// `kind`, not a nuisance), records the reason so the health endpoint
/// and cloud heartbeat can report `degraded`, and returns a detector
/// that yields **no** detections.
///
/// The synthetic [`MockDetector`] is only substituted when an operator
/// has explicitly set `NEXUS_ALLOW_MOCK_DETECTOR=1`; see
/// [`mock_fallback_allowed`] for why that is not the default.
pub fn degraded_detector(kind: &str, reason: impl std::fmt::Display) -> Arc<dyn Detector> {
    let reason = reason.to_string();

    if mock_fallback_allowed() {
        tracing::warn!(
            kind,
            reason = %reason,
            "detector unavailable; NEXUS_ALLOW_MOCK_DETECTOR is set, so substituting the \
             SYNTHETIC mock detector — it fabricates a 'person' box on every frame and \
             MUST NOT be used in production"
        );
        record_degraded(
            kind,
            format!("{reason} (synthetic mock detector substituted)"),
        );
        return Arc::new(MockDetector::new());
    }

    tracing::error!(
        kind,
        reason = %reason,
        "DETECTION DISABLED: detector failed to build; this engine will report ZERO \
         detections for this model kind until the configuration is corrected and the \
         engine restarted. Check inference.model preset/pack_path against the shapes \
         present in the model pack"
    );
    record_degraded(kind, reason);
    Arc::new(UnavailableDetector::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: these share one process-global registry, so they assert on
    // their own `kind` namespace rather than on total emptiness.

    #[test]
    fn record_then_clear_roundtrips() {
        record_degraded("test-roundtrip", "boom");
        assert!(degradations()
            .iter()
            .any(|d| d.kind == "test-roundtrip" && d.reason == "boom"));
        assert!(is_degraded());
        clear_degraded("test-roundtrip");
        assert!(!degradations().iter().any(|d| d.kind == "test-roundtrip"));
    }

    #[test]
    fn duplicate_records_are_deduplicated() {
        clear_degraded("test-dedup");
        record_degraded("test-dedup", "same");
        record_degraded("test-dedup", "same");
        record_degraded("test-dedup", "same");
        assert_eq!(
            degradations()
                .iter()
                .filter(|d| d.kind == "test-dedup")
                .count(),
            1,
            "pool builds N workers; one failure must not report N times"
        );
        clear_degraded("test-dedup");
    }
}

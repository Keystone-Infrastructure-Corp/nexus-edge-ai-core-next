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

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use nexus_config::InferenceConfig;

use crate::detectors::{Detector, MockDetector, UnavailableDetector};

/// One reason detection is degraded on this engine.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DetectorDegradation {
    /// Model kind that failed to build (`yolo`, `yoloe`, `yolo_world`, …).
    pub kind: String,
    /// Resolved model/EP identity; absent in older health payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    /// Operator-facing cause, verbatim from the underlying
    /// [`crate::InferenceError`] — includes the resolver's
    /// "missing the shape-matched ONNX … Available in pack: […]"
    /// diagnostic, which names the exact fix.
    pub reason: String,
}

fn registry() -> &'static RwLock<Vec<(u64, DetectorDegradation)>> {
    static REGISTRY: OnceLock<RwLock<Vec<(u64, DetectorDegradation)>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

pub(crate) struct DegradationRegistration(u64);

impl Drop for DegradationRegistration {
    fn drop(&mut self) {
        if let Ok(mut guard) = registry().write() {
            guard.retain(|(id, _)| *id != self.0);
        }
    }
}

pub(crate) fn register(entry: DetectorDegradation) -> DegradationRegistration {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut guard) = registry().write() {
        guard.push((id, entry));
    }
    DegradationRegistration(id)
}

/// Snapshot deduplicates equal failures without merging their ownership.
pub fn degradations() -> Vec<DetectorDegradation> {
    let mut entries = Vec::new();
    if let Ok(guard) = registry().read() {
        for (_, entry) in guard.iter() {
            if !entries.contains(entry) {
                entries.push(entry.clone());
            }
        }
    }
    entries
}

/// `true` when object detection is not fully operational on this engine.
pub fn is_degraded() -> bool {
    registry().read().map(|g| !g.is_empty()).unwrap_or(false)
}

pub(crate) fn layer_identity(cfg: &InferenceConfig) -> String {
    // Fingerprint the resolved model and providers without publishing local paths.
    let mut identity = std::collections::hash_map::DefaultHasher::new();
    format!("{:?}", cfg.model).hash(&mut identity);
    cfg.ep_priority.hash(&mut identity);
    format!(
        "{}@{}x{}/{:016x} ep=[{}]",
        cfg.model.kind,
        cfg.model.input_width,
        cfg.model.input_height,
        identity.finish(),
        cfg.ep_priority.join(",")
    )
}

struct DegradedDetector {
    inner: Arc<dyn Detector>,
    _registration: DegradationRegistration,
}

#[async_trait::async_trait]
impl Detector for DegradedDetector {
    async fn detect(
        &self,
        frame: &nexus_types::Frame,
        prompts: &[String],
    ) -> Result<Vec<nexus_types::Detection>, crate::InferenceError> {
        self.inner.detect(frame, prompts).await
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
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
    degraded_detector_in_layer(kind, None, reason)
}

/// Build a fail-soft detector whose health belongs to this resolved layer.
pub fn degraded_detector_for_config(
    cfg: &InferenceConfig,
    reason: impl std::fmt::Display,
) -> Arc<dyn Detector> {
    degraded_detector_in_layer(&cfg.model.kind, Some(layer_identity(cfg)), reason)
}

fn degraded_detector_in_layer(
    kind: &str,
    layer: Option<String>,
    reason: impl std::fmt::Display,
) -> Arc<dyn Detector> {
    let mut reason = reason.to_string();
    let inner: Arc<dyn Detector> = if mock_fallback_allowed() {
        tracing::warn!(kind, %reason, "detector unavailable; explicit synthetic mock fallback enabled");
        reason.push_str(" (synthetic mock detector substituted)");
        Arc::new(MockDetector::new())
    } else {
        tracing::error!(kind, %reason, "DETECTION DISABLED: correct the model configuration and rebuild the layer");
        Arc::new(UnavailableDetector::new())
    };
    Arc::new(DegradedDetector {
        inner,
        _registration: register(DetectorDegradation {
            kind: kind.to_owned(),
            layer,
            reason,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: these share one process-global registry, so they assert on
    // their own `kind` namespace rather than on total emptiness.

    #[test]
    fn layer_identity_labels_providers_without_disclosing_model_paths() {
        let mut cfg = InferenceConfig {
            ep_priority: vec!["hailo".into(), "cpu".into()],
            model: nexus_config::ModelConfig {
                pack_path: Some("/private/customer/models".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let first = layer_identity(&cfg);
        assert!(first.contains("ep=[hailo,cpu]"), "{first}");
        assert!(!first.contains("/private/customer"));
        cfg.model.pack_path = Some("/private/other/models".into());
        assert_ne!(first, layer_identity(&cfg));
    }

    #[test]
    fn registration_drop_clears_only_its_owned_failure() {
        let entry = DetectorDegradation {
            kind: "test-owned-registration".into(),
            layer: None,
            reason: "boom".into(),
        };
        let first = register(entry.clone());
        let second = register(entry.clone());
        assert_eq!(degradations().iter().filter(|d| **d == entry).count(), 1);
        drop(first);
        assert!(degradations().contains(&entry));
        drop(second);
        assert!(!degradations().contains(&entry));
    }
}

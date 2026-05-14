//! `InferenceRouter` — picks the right [`Detector`] per camera.
//!
//! M3 closeout. The rest of the stack already knows how to:
//!   * build one detector at a time ([`crate::build`]),
//!   * route per-frame work through it (the supervisor),
//!   * fan-push per-camera config to every backend in a pool.
//!
//! What was missing: the ability to run *different* model kinds for
//! different cameras in the same engine — e.g. one operator running
//! `yolo` on cameras with hard accuracy requirements and `yolo_world`
//! on the rest. That's what `CameraConfig.model_override` is for, and
//! this router is the layer that honors it.
//!
//! Shape:
//!   * One [`InferenceLayer`] per *unique* model kind referenced by any
//!     enabled camera — the default kind plus every distinct override.
//!     Layers are built with the same backend / pool_worker_kind /
//!     workers / ep_priority as the default, only the model substruct
//!     is swapped. Operators paying for two pools of N workers each is
//!     a deliberate cost — if you don't want it, don't override.
//!   * [`detector_for_camera`] picks the layer keyed by the camera's
//!     override, falling back to the default kind.
//!   * [`default_pool`] gives the OPS API back its single-pool view
//!     (shows the default kind's pool — every other kind's pool is
//!     observable on `pools()` once the API surfaces it).

use std::collections::BTreeMap;
use std::sync::Arc;

use nexus_config::{CameraConfig, InferenceConfig, ModelConfig};
use tracing::{info, warn};

use crate::detectors::{Detector, InferenceError};
use crate::pool::DetectorPool;
use crate::{build, InferenceLayer};

pub struct InferenceRouter {
    /// Default layer — used by every camera that doesn't override.
    default_kind: String,
    layers: BTreeMap<String, InferenceLayer>,
}

impl InferenceRouter {
    /// Build a router from one default config + the list of cameras that
    /// will run on it. Walks `cameras` for every distinct
    /// `model_override.kind` and builds an additional [`InferenceLayer`]
    /// for each, sharing the default's backend / workers / ep_priority.
    ///
    /// Disabled cameras are still considered — we want the router to
    /// own a layer the moment a camera is re-enabled at runtime, not
    /// only after a process restart. Building unused layers is cheap on
    /// the in_process backend (one Arc) and the operator opted in by
    /// declaring the override.
    pub fn build(
        default_cfg: &InferenceConfig,
        cameras: &[CameraConfig],
    ) -> Result<Self, InferenceError> {
        let default_kind = default_cfg.model.kind.clone();
        let mut layers: BTreeMap<String, InferenceLayer> = BTreeMap::new();

        let default_layer = build(default_cfg)?;
        info!(kind = %default_kind, "router: built default inference layer");
        layers.insert(default_kind.clone(), default_layer);

        // Walk overrides, dedup by (kind, …model fields). For now we key
        // the layer table by the kind string only — two cameras that pick
        // the same `kind` but different thresholds share one layer (and
        // the per-camera score_threshold is honored at the rule layer).
        // If we ever need per-camera-thresholds-in-the-detector, we'll
        // rev the key shape here without changing callers.
        for cam in cameras {
            let Some(override_cfg) = cam.model_override.as_ref() else {
                continue;
            };
            let kind = override_cfg.kind.clone();
            if kind == default_kind || layers.contains_key(&kind) {
                continue;
            }
            let derived = derive_inference_cfg(default_cfg, override_cfg);
            match build(&derived) {
                Ok(layer) => {
                    info!(
                        kind = %kind,
                        camera_id = cam.id,
                        "router: built override inference layer"
                    );
                    layers.insert(kind, layer);
                }
                Err(e) => {
                    warn!(
                        kind = %kind,
                        camera_id = cam.id,
                        "router: failed to build override layer ({e}); \
                         camera will fall back to the default kind"
                    );
                }
            }
        }

        Ok(Self {
            default_kind,
            layers,
        })
    }

    /// Detector for a given camera. Picks the override if its kind has a
    /// layer; falls back to the default kind otherwise. Always returns
    /// some `Arc<dyn Detector>` — the default layer is built before this
    /// can be called, so the fallback is total.
    pub fn detector_for_camera(&self, cam: &CameraConfig) -> Arc<dyn Detector> {
        let kind = cam
            .model_override
            .as_ref()
            .map(|m| m.kind.as_str())
            .unwrap_or(self.default_kind.as_str());
        match self.layers.get(kind) {
            Some(layer) => layer.detector.clone(),
            None => {
                // Build-time warning already explained why we don't have
                // this layer; on the hot path just use the default.
                self.layers
                    .get(self.default_kind.as_str())
                    .expect("router invariant: default layer present")
                    .detector
                    .clone()
            }
        }
    }

    /// Default kind's pool, for back-compat with the existing
    /// `/api/backends` endpoint.
    pub fn default_pool(&self) -> Option<Arc<DetectorPool>> {
        self.layers
            .get(self.default_kind.as_str())
            .and_then(|l| l.pool.clone())
    }

    /// Every (kind, pool) the router owns. Future expansion of the
    /// OPS API — today only `default_pool()` is surfaced.
    pub fn pools(&self) -> Vec<(String, Arc<DetectorPool>)> {
        self.layers
            .iter()
            .filter_map(|(k, l)| l.pool.clone().map(|p| (k.clone(), p)))
            .collect()
    }

    /// Every (kind, detector) — useful for fan-pushing per-camera config
    /// updates to the right detector at startup or hot reload.
    pub fn detectors(&self) -> Vec<(String, Arc<dyn Detector>)> {
        self.layers
            .iter()
            .map(|(k, l)| (k.clone(), l.detector.clone()))
            .collect()
    }

    pub fn default_kind(&self) -> &str {
        &self.default_kind
    }
}

/// Build a per-kind [`InferenceConfig`] from the default by swapping the
/// `model` substruct for the camera's override. Backend strategy /
/// worker count / EP priority / fail-soft are inherited because they're
/// host-level decisions, not per-camera ones.
fn derive_inference_cfg(
    default: &InferenceConfig,
    override_model: &ModelConfig,
) -> InferenceConfig {
    let mut derived = default.clone();
    derived.model = override_model.clone();
    derived
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::{
        CameraConfig, InferenceBackendKind, InferenceConfig, ModelConfig, PoolWorkerKind,
    };
    use url::Url;

    fn cfg_with_kind(kind: &str) -> InferenceConfig {
        InferenceConfig {
            backend: InferenceBackendKind::InProcess,
            pool_worker_kind: PoolWorkerKind::Thread,
            workers: 1,
            restart_backoff_ms: 0,
            fail_soft: false,
            ep_priority: vec!["cpu".into()],
            model: ModelConfig {
                kind: kind.into(),
                ..Default::default()
            },
        }
    }

    fn cam(id: i64, override_kind: Option<&str>) -> CameraConfig {
        CameraConfig {
            id,
            name: format!("cam-{id}"),
            url: Url::parse("virtual://test").unwrap(),
            enabled: true,
            prompts: vec![],
            model_override: override_kind.map(|k| ModelConfig {
                kind: k.into(),
                ..Default::default()
            }),
            zones: vec![],
            max_fps: 0,
            parking_lot_mode: false,
        }
    }

    #[test]
    fn router_builds_only_default_when_no_overrides() {
        let cfg = cfg_with_kind("mock");
        let cams = vec![cam(1, None), cam(2, None)];
        let router = InferenceRouter::build(&cfg, &cams).unwrap();
        assert_eq!(router.default_kind(), "mock");
        assert_eq!(router.detectors().len(), 1);
    }

    #[test]
    fn router_builds_one_layer_per_unique_override_kind() {
        let cfg = cfg_with_kind("mock");
        let cams = vec![
            cam(1, None), // default
            cam(2, Some("classifier_ensemble")),
            cam(3, Some("classifier_ensemble")), // dedup
            cam(4, Some("open_vocab")),
        ];
        let router = InferenceRouter::build(&cfg, &cams).unwrap();
        let detectors = router.detectors();
        let kinds: Vec<&str> = detectors.iter().map(|(k, _)| k.as_str()).collect();
        assert!(kinds.contains(&"mock"));
        assert!(kinds.contains(&"classifier_ensemble"));
        assert!(kinds.contains(&"open_vocab"));
        assert_eq!(kinds.len(), 3);
    }

    #[test]
    fn router_picks_override_detector_for_camera() {
        let cfg = cfg_with_kind("mock");
        let cams = vec![cam(1, None), cam(2, Some("classifier_ensemble"))];
        let router = InferenceRouter::build(&cfg, &cams).unwrap();
        let d1 = router.detector_for_camera(&cams[0]);
        let d2 = router.detector_for_camera(&cams[1]);
        assert_eq!(d1.name(), "mock");
        assert_eq!(d2.name(), "classifier_ensemble");
    }

    #[test]
    fn router_falls_back_to_default_for_unknown_override_kind() {
        let cfg = cfg_with_kind("mock");
        // "no_such_kind" still gets a layer (build_detector falls back to
        // MockDetector with a warn), but the contract here is that even
        // an explicitly-unknown override resolves to *some* detector and
        // the engine never panics on the hot path. Verify that with an
        // override the router didn't see at build time, the default kind
        // is what wins.
        let cams = vec![cam(1, Some("phantom_kind"))];
        let router = InferenceRouter::build(&cfg, &cams).unwrap();
        // Now spawn a camera that wasn't in the build set with an override
        // we never knew about → must still resolve to the default detector.
        let stray = cam(99, Some("never_seen"));
        let d = router.detector_for_camera(&stray);
        assert_eq!(d.name(), "mock");
    }

    #[test]
    fn router_default_pool_is_none_for_in_process_default() {
        let cfg = cfg_with_kind("mock");
        let router = InferenceRouter::build(&cfg, &[]).unwrap();
        assert!(router.default_pool().is_none());
    }
}

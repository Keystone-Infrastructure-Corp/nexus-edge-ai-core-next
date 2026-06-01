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
//!   * One [`InferenceLayer`] per *unique* model identity referenced by
//!     any enabled camera. As of v0.1.19 identity = `(kind, input_w,
//!     input_h)` — two cameras that both override `kind = "yolo"` but
//!     ask for different input sizes (640 vs. 1280) get *separate*
//!     layers, each backed by its own per-size ONNX. Without this
//!     per-size key, the router would dedupe by kind alone and the
//!     first camera to introduce a kind would lock in its size for
//!     every sibling — silently. Layers share backend / workers /
//!     ep_priority with the default; only the `model` substruct
//!     changes.
//!   * [`detector_for_camera`] picks the layer keyed by the camera's
//!     override (or the default identity), falling back to the
//!     default if the override couldn't be built.
//!   * [`default_pool`] gives the OPS API back its single-pool view
//!     (shows the default identity's pool — every other identity's
//!     pool is observable on `pools()` once the API surfaces it).
//!   * [`detectors`] returns `(kind, detector)` pairs (kind only —
//!     callers like `models_catalog` collect into a kind-keyed set
//!     and don't care which size variant). If two layers share a
//!     kind, both surface as `("yolo", _)` — consumers that need to
//!     disambiguate should use [`layer_keys`].

use std::collections::BTreeMap;
use std::sync::Arc;

use nexus_config::{CameraConfig, InferenceConfig, ModelConfig};
use tracing::{info, warn};

use crate::detectors::{Detector, InferenceError};
use crate::pool::DetectorPool;
use crate::visual_prompts::VisualPromptStore;
use crate::{build_with_context, BuildContext, InferenceLayer};

/// Composite identity key for an inference layer. Two cameras share a
/// layer iff they would produce equal `LayerKey`s. Built from the
/// `kind` + `(input_w, input_h)` of the camera's resolved model config
/// (override if set, default otherwise).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct LayerKey {
    kind: String,
    input_w: u32,
    input_h: u32,
}

impl LayerKey {
    fn from_model(model: &ModelConfig) -> Self {
        Self {
            kind: model.kind.clone(),
            input_w: model.input_width,
            input_h: model.input_height,
        }
    }

    fn display(&self) -> String {
        format!("{}@{}x{}", self.kind, self.input_w, self.input_h)
    }
}

pub struct InferenceRouter {
    /// Default identity — used by every camera that doesn't override.
    default_key: LayerKey,
    layers: BTreeMap<LayerKey, InferenceLayer>,
}

impl InferenceRouter {
    /// Build a router from one default config + the list of cameras that
    /// will run on it. Walks `cameras` for every distinct override
    /// identity and builds an additional [`InferenceLayer`] for each,
    /// sharing the default's backend / workers / ep_priority.
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
        Self::build_with_visual_store(default_cfg, cameras, None, None)
    }

    /// Same as [`Self::build`] but also wires a [`VisualPromptStore`]
    /// (required when any camera uses `model_override.kind =
    /// "yoloe_visual"`) and an optional embedding-dim override.
    pub fn build_with_visual_store(
        default_cfg: &InferenceConfig,
        cameras: &[CameraConfig],
        visual_prompt_store: Option<Arc<dyn VisualPromptStore>>,
        visual_embedding_dim: Option<usize>,
    ) -> Result<Self, InferenceError> {
        let ctx = BuildContext {
            visual_prompt_store,
            visual_embedding_dim,
        };
        let default_key = LayerKey::from_model(&default_cfg.model);
        let mut layers: BTreeMap<LayerKey, InferenceLayer> = BTreeMap::new();

        let default_layer = build_with_context(default_cfg, &ctx)?;
        info!(
            key = %default_key.display(),
            "router: built default inference layer"
        );
        layers.insert(default_key.clone(), default_layer);

        // Walk overrides, dedup by (kind, input_w, input_h). Two
        // cameras that pick the same kind+size share one layer (and
        // the per-camera score_threshold is honored at the rule layer).
        // The (kind, size) key matters: as of v0.1.19 we ship three
        // per-size yolo26n ONNXs and the engine's `from_config` picks
        // the file by input_width — two cameras with kind=yolo but
        // different sizes MUST get different layers, otherwise the
        // first one to be processed silently locks the size.
        for cam in cameras {
            let Some(override_cfg) = cam.detector.model_override.as_ref() else {
                continue;
            };
            let key = LayerKey::from_model(override_cfg);
            if key == default_key || layers.contains_key(&key) {
                continue;
            }
            let derived = derive_inference_cfg(default_cfg, override_cfg);
            match build_with_context(&derived, &ctx) {
                Ok(layer) => {
                    info!(
                        key = %key.display(),
                        camera_id = cam.id,
                        "router: built override inference layer"
                    );
                    layers.insert(key, layer);
                }
                Err(e) => {
                    warn!(
                        key = %key.display(),
                        camera_id = cam.id,
                        "router: failed to build override layer ({e}); \
                         camera will fall back to the default identity"
                    );
                }
            }
        }

        Ok(Self {
            default_key,
            layers,
        })
    }

    /// Detector for a given camera. Picks the override-identity layer
    /// if one exists for that camera's override; falls back to the
    /// default identity otherwise. Always returns some `Arc<dyn
    /// Detector>` — the default layer is built before this can be
    /// called, so the fallback is total.
    pub fn detector_for_camera(&self, cam: &CameraConfig) -> Arc<dyn Detector> {
        let key = cam
            .detector
            .model_override
            .as_ref()
            .map(LayerKey::from_model)
            .unwrap_or_else(|| self.default_key.clone());
        match self.layers.get(&key) {
            Some(layer) => layer.detector.clone(),
            None => {
                // Build-time warning already explained why we don't have
                // this layer; on the hot path just use the default.
                self.layers
                    .get(&self.default_key)
                    .expect("router invariant: default layer present")
                    .detector
                    .clone()
            }
        }
    }

    /// Default identity's pool, for back-compat with the existing
    /// `/api/v1/backends` endpoint.
    pub fn default_pool(&self) -> Option<Arc<DetectorPool>> {
        self.layers
            .get(&self.default_key)
            .and_then(|l| l.pool.clone())
    }

    /// Every (kind, pool) the router owns. Future expansion of the
    /// OPS API — today only `default_pool()` is surfaced. Returns the
    /// kind alone (not the composite identity) so the wire shape
    /// stays back-compat; two layers that share a kind both surface
    /// as `("yolo", _)`.
    pub fn pools(&self) -> Vec<(String, Arc<DetectorPool>)> {
        self.layers
            .iter()
            .filter_map(|(k, l)| l.pool.clone().map(|p| (k.kind.clone(), p)))
            .collect()
    }

    /// Every (kind, detector) — useful for fan-pushing per-camera config
    /// updates to the right detector at startup or hot reload. Returns
    /// the kind alone (not the composite identity); see [`layer_keys`]
    /// for the disambiguated form.
    pub fn detectors(&self) -> Vec<(String, Arc<dyn Detector>)> {
        self.layers
            .iter()
            .map(|(k, l)| (k.kind.clone(), l.detector.clone()))
            .collect()
    }

    /// Every layer's composite identity (`kind@WxH`) as a display
    /// string. Useful for diagnostics / `/api/v1/backends` extensions —
    /// callers that need to enumerate every distinct (kind, size)
    /// layer should consume this instead of [`detectors`].
    pub fn layer_keys(&self) -> Vec<String> {
        self.layers.keys().map(LayerKey::display).collect()
    }

    pub fn default_kind(&self) -> &str {
        &self.default_key.kind
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
            ingest: nexus_config::CameraIngest {
                url: Url::parse("virtual://test").unwrap(),
                enabled: true,
                max_fps: 0,
                codec: None,
            },
            detector: nexus_config::CameraDetector {
                prompts: vec![],
                visual_prompts: vec![],
                model_override: override_kind.map(|k| ModelConfig {
                    kind: k.into(),
                    ..Default::default()
                }),
            },
            behavior: nexus_config::CameraBehavior {
                parking_lot_mode: false,
                anchor_ttl_secs: None,
                ..Default::default()
            },
            zones: vec![],
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

    #[test]
    fn router_builds_yoloe_visual_layer_with_visual_store() {
        // With ort off (CI default for `cargo test`), the yoloe_visual
        // arm falls back to MockDetector with a warn — but the router
        // MUST still register a layer keyed by the override kind so
        // `detector_for_camera` resolves it instead of bouncing to the
        // default. With ort on, the same arm builds the real detector
        // when pack_path is set; we don't exercise that here (needs
        // real ONNX) — the unit covers the routing shape only.
        use crate::visual_prompts::InMemoryVisualPromptStore;
        use std::sync::Arc;

        let cfg = cfg_with_kind("mock");
        let cams = vec![cam(1, None), cam(2, Some("yoloe_visual"))];
        let store: Arc<dyn crate::visual_prompts::VisualPromptStore> =
            Arc::new(InMemoryVisualPromptStore::new());
        let router =
            InferenceRouter::build_with_visual_store(&cfg, &cams, Some(store), Some(8)).unwrap();
        let kinds: Vec<String> = router.detectors().into_iter().map(|(k, _)| k).collect();
        assert!(kinds.contains(&"mock".to_string()));
        assert!(kinds.contains(&"yoloe_visual".to_string()));
    }

    /// M3.2 — a camera with `model_override.kind = "ensemble"` resolves
    /// to a real `EnsembleDetector` (`name() == "ensemble"`) that fans
    /// frames to its declared members. We don't run inference here —
    /// that's covered by ensemble unit tests — only the routing shape.
    #[test]
    fn router_builds_ensemble_layer_for_camera() {
        use url::Url;
        let cfg = cfg_with_kind("mock");
        let mut cam2 = cam(2, None);
        cam2.detector.model_override = Some(ModelConfig {
            kind: "ensemble".into(),
            members: vec![
                ModelConfig {
                    kind: "mock".into(),
                    ..Default::default()
                },
                ModelConfig {
                    kind: "mock".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let cams = vec![
            CameraConfig {
                id: 1,
                name: "default-cam".into(),
                ingest: nexus_config::CameraIngest {
                    url: Url::parse("virtual://test").unwrap(),
                    enabled: true,
                    max_fps: 0,
                    codec: None,
                },
                detector: nexus_config::CameraDetector {
                    prompts: vec![],
                    visual_prompts: vec![],
                    model_override: None,
                },
                behavior: nexus_config::CameraBehavior {
                    parking_lot_mode: false,
                    anchor_ttl_secs: None,
                    ..Default::default()
                },
                zones: vec![],
            },
            cam2.clone(),
        ];
        let router = InferenceRouter::build(&cfg, &cams).unwrap();
        let kinds: Vec<String> = router.detectors().into_iter().map(|(k, _)| k).collect();
        assert!(kinds.contains(&"ensemble".to_string()), "got {kinds:?}");
        let d = router.detector_for_camera(&cam2);
        assert_eq!(d.name(), "ensemble");
    }

    /// v0.1.19 — two cameras that share a kind but ask for different
    /// input sizes MUST get separate layers. Without per-size keying,
    /// the second camera would silently inherit the first one's size
    /// (and the first one to be processed would lock the size for
    /// every sibling). Verified via `layer_keys()`, which exposes
    /// the composite identity.
    #[test]
    fn router_dedups_by_kind_plus_input_size() {
        let cfg = cfg_with_kind("mock");
        let mut cam_640 = cam(1, None);
        cam_640.detector.model_override = Some(ModelConfig {
            kind: "mock".into(),
            input_width: 640,
            input_height: 640,
            ..Default::default()
        });
        let mut cam_960 = cam(2, None);
        cam_960.detector.model_override = Some(ModelConfig {
            kind: "mock".into(),
            input_width: 960,
            input_height: 960,
            ..Default::default()
        });
        let mut cam_1280 = cam(3, None);
        cam_1280.detector.model_override = Some(ModelConfig {
            kind: "mock".into(),
            input_width: 1280,
            input_height: 1280,
            ..Default::default()
        });
        // Duplicate of cam_960 to verify dedup still works at the new
        // composite-key level.
        let cam_960_dup = {
            let mut c = cam(4, None);
            c.detector.model_override = Some(ModelConfig {
                kind: "mock".into(),
                input_width: 960,
                input_height: 960,
                ..Default::default()
            });
            c
        };
        let cams = vec![cam_640.clone(), cam_960.clone(), cam_1280, cam_960_dup];
        let router = InferenceRouter::build(&cfg, &cams).unwrap();

        // Default cfg is 640×640 mock — so cam_640's identity collides
        // with the default and we should see exactly three distinct
        // layers: mock@640x640 (default + cam_640 + cam_960_dup share
        // none of these, only the default's size), mock@960x960,
        // mock@1280x1280. Actually only the default layer is at 640,
        // cam_640's override is its own identity but happens to equal
        // the default → also collides into one layer.
        let keys = router.layer_keys();
        assert!(keys.iter().any(|k| k == "mock@640x640"), "got {keys:?}");
        assert!(keys.iter().any(|k| k == "mock@960x960"), "got {keys:?}");
        assert!(keys.iter().any(|k| k == "mock@1280x1280"), "got {keys:?}");
        // Exactly three distinct identities — no duplicate 960 layer.
        assert_eq!(keys.len(), 3, "got {keys:?}");
    }
}

//! Detector prompt catalog — `GET /api/v1/models/prompts`.
//!
//! Surfaces the prompt vocabulary the engine's currently-loaded
//! detector kinds will actually emit, so the camera + rules forms
//! in the UI stop showing closed-vocab COCO labels when an
//! open-vocab (yolo_world) detector is active and vice versa.
//!
//! Built once at engine boot from the [`InferenceRouter`]'s known
//! kinds + a tiny manifest peek for open-vocab vocabularies. The
//! API state stores it in an `Arc<_>` and clones the Arc into the
//! response so the handler is a no-op JSON serialize.
//!
//! The catalog is deliberately a STATIC snapshot of what the
//! engine booted with. If an operator drops a new `model_override`
//! on a camera the catalog won't reflect it until the next engine
//! restart — same lifecycle as the router itself.

use std::collections::BTreeMap;

use serde::Serialize;
use tracing::warn;

use nexus_inference::InferenceRouter;

/// Every detector kind the engine knows how to build
/// (`crates/nexus-inference/src/lib.rs :: build_detector` match
/// arms). Sorted alphabetically so the UI dropdown is stable
/// regardless of `router.detectors()` iteration order. Aliases
/// (`yolo26n`, `closed_vocab`, `open_vocab`, `ppe`) are NOT listed
/// here — the canonical kind names are what the camera form
/// persists.
pub(crate) const KNOWN_KINDS: &[&str] = &[
    "classifier_ensemble",
    "mock",
    "yolo",
    "yolo_world",
    "yoloe",
    "yoloe_promptfree",
    "yoloe_visual",
];

/// Top-level response shape returned by `GET /api/v1/models/prompts`.
#[derive(Clone, Serialize)]
pub struct ModelPromptsCatalog {
    /// `inference.model.kind` from the loaded config — the kind
    /// every camera that does NOT set `model_override` will run
    /// against. The UI uses this to pick a prompt source when a
    /// camera has no override.
    pub default_kind: String,
    /// One entry per detector kind the engine knows how to build,
    /// regardless of whether the router currently has a layer for
    /// it. The UI camera form lets operators select any of these
    /// as an override; `loaded` indicates whether picking it would
    /// require an engine restart to materialise the layer.
    pub kinds: Vec<DetectorPromptInfo>,
    /// Same entries as `kinds`, indexed by `kind` for cheap O(1)
    /// lookup. The UI's chip-suggestion path consumes this shape;
    /// the array form is kept for stable ordering in the dropdown.
    pub by_kind: BTreeMap<String, DetectorPromptInfo>,
}

#[derive(Clone, Serialize)]
pub struct DetectorPromptInfo {
    pub kind: String,
    /// Open-vocab detectors (yolo_world / yoloe) accept any
    /// user-supplied prompt string and emit only labels from the
    /// baked vocab. Closed-vocab detectors (yolo / classifier_ensemble)
    /// emit a fixed label set so the UI should render a chip strip
    /// rather than a free-text suggestion box.
    pub open_vocab: bool,
    /// Every label this detector kind is known to emit. Empty for
    /// detectors whose vocabulary is unknown (e.g. `mock`) — the
    /// UI should then fall back to a plain text input with no
    /// chip chooser.
    pub prompts: Vec<String>,
    /// Optional groupings the UI can render as titled chip rows.
    /// Empty Vec means "ungrouped" — render the prompts flat.
    pub groups: Vec<DetectorPromptGroup>,
    /// Human-readable note describing the detector. Shown beneath
    /// the chip strip in the camera form.
    pub note: Option<String>,
    /// `true` IFF the router has already built a layer for this
    /// kind at engine boot. Operators who pick an unloaded kind
    /// as a camera `model_override` won't see it take effect
    /// until the engine restarts (the router only walks the
    /// camera table once, at startup). The UI uses this flag to
    /// surface a "restart engine to activate" hint.
    pub loaded: bool,
}

#[derive(Clone, Serialize)]
pub struct DetectorPromptGroup {
    pub name: String,
    pub labels: Vec<String>,
}

/// Build the catalog by enumerating every detector kind the
/// engine knows how to build and resolving each kind's prompt
/// vocabulary. For `yolo_world` / `yoloe` this reads the
/// model-pack `models-manifest.json`; for `yolo` we hard-code
/// the COCO→domain mapping that mirrors
/// `crates/nexus-inference/src/yolo.rs::map_coco_to_domain_label`.
pub fn build_catalog(
    inference_cfg: &nexus_config::InferenceConfig,
    router: &InferenceRouter,
) -> ModelPromptsCatalog {
    let default_kind = inference_cfg.model.kind.clone();
    let loaded: std::collections::HashSet<String> =
        router.detectors().into_iter().map(|(k, _)| k).collect();
    let kinds: Vec<DetectorPromptInfo> = KNOWN_KINDS
        .iter()
        .map(|k| {
            let mut info = info_for_kind(k, inference_cfg);
            info.loaded = loaded.contains(*k);
            info
        })
        .collect();
    let by_kind: BTreeMap<String, DetectorPromptInfo> = kinds
        .iter()
        .map(|info| (info.kind.clone(), info.clone()))
        .collect();
    ModelPromptsCatalog {
        default_kind,
        kinds,
        by_kind,
    }
}

fn info_for_kind(kind: &str, cfg: &nexus_config::InferenceConfig) -> DetectorPromptInfo {
    match kind {
        "yolo" | "yolo26n" | "closed_vocab" => coco_info(kind),
        "open_vocab" | "yolo_world" => yolo_world_info(kind, cfg),
        "yoloe" => yoloe_text_info(kind, cfg),
        "yoloe_promptfree" => yoloe_promptfree_info(kind, cfg),
        "yoloe_visual" => yoloe_visual_info(kind),
        "classifier_ensemble" | "ppe" => DetectorPromptInfo {
            kind: kind.into(),
            open_vocab: false,
            prompts: vec![],
            groups: vec![],
            note: Some(
                "NOT IMPLEMENTED — no model ships for this kind. Selecting it \
                 reports zero detections and marks engine health degraded. \
                 Kept listed so cameras that already persist it still render."
                    .into(),
            ),
            loaded: false,
        },
        "mock" => DetectorPromptInfo {
            kind: kind.into(),
            open_vocab: false,
            prompts: vec![],
            groups: vec![],
            note: Some(
                "Mock detector — emits deterministic placeholder boxes (no real labels).".into(),
            ),
            loaded: false,
        },
        other => DetectorPromptInfo {
            kind: other.into(),
            open_vocab: false,
            prompts: vec![],
            groups: vec![],
            note: Some(format!("Unknown detector kind {other:?}.")),
            loaded: false,
        },
    }
}

/// Mirror of `yolo.rs::map_coco_to_domain_label`. Single source of
/// truth lives in that match arm; this list is the API-surface copy
/// the UI consumes so operators don't see a stale, hard-coded list
/// of labels that the running detector isn't emitting.
const COCO_DOMAIN_LABELS: &[(u8, &str, &str)] = &[
    (0, "person", "People"),
    (2, "vehicle.car", "Vehicles"),
    (7, "vehicle.truck", "Vehicles"),
    (5, "vehicle.bus", "Vehicles"),
    (3, "vehicle.motorcycle", "Vehicles"),
    (1, "vehicle.bicycle", "Vehicles"),
    (16, "animal.dog", "Animals"),
    (15, "animal.cat", "Animals"),
    (14, "animal.bird", "Animals"),
    (24, "carried.backpack", "Carried"),
    (26, "carried.handbag", "Carried"),
    (28, "carried.suitcase", "Carried"),
];

fn coco_info(kind: &str) -> DetectorPromptInfo {
    let prompts: Vec<String> = COCO_DOMAIN_LABELS
        .iter()
        .map(|(_, l, _)| (*l).into())
        .collect();
    // Preserve insertion order by group (People → Vehicles → Animals → Carried)
    // so the UI lays the chip strip out the same way every render.
    let mut groups: Vec<DetectorPromptGroup> = Vec::new();
    for (_, label, group) in COCO_DOMAIN_LABELS {
        if let Some(g) = groups.iter_mut().find(|g| g.name == *group) {
            g.labels.push((*label).into());
        } else {
            groups.push(DetectorPromptGroup {
                name: (*group).into(),
                labels: vec![(*label).into()],
            });
        }
    }
    DetectorPromptInfo {
        kind: kind.into(),
        open_vocab: false,
        prompts,
        groups,
        note: Some(
            "Closed-vocab YOLO (COCO). Detector emits exactly these 12 \
             domain labels; rules should match them verbatim \
             (e.g. `object.label == 'vehicle.car'`)."
                .into(),
        ),
        loaded: false,
    }
}

fn yolo_world_info(kind: &str, cfg: &nexus_config::InferenceConfig) -> DetectorPromptInfo {
    let prompts = match read_manifest_prompts(cfg, "yolo_world_v2_s") {
        Ok(p) => p,
        Err(e) => {
            warn!(
                kind, error = %e,
                "model catalog: yolo-world vocab unavailable; falling back to empty prompt list"
            );
            Vec::new()
        }
    };
    DetectorPromptInfo {
        kind: kind.into(),
        open_vocab: true,
        prompts,
        groups: vec![],
        note: Some(
            "Open-vocab YOLO-World. Operators may use any of these baked \
             prompts (typing them as-is). The detector will only emit \
             labels from the baked vocabulary, so rules that match labels \
             not in this list will never fire."
                .into(),
        ),
        loaded: false,
    }
}

fn yoloe_text_info(kind: &str, cfg: &nexus_config::InferenceConfig) -> DetectorPromptInfo {
    let prompts = match read_manifest_prompts(cfg, "yoloe26_s") {
        Ok(p) => p,
        Err(e) => {
            warn!(
                kind, error = %e,
                "model catalog: yoloe vocab unavailable; falling back to empty prompt list \
                 (run tools/models/gen_yoloe.py to materialise the artifact + manifest entry)"
            );
            Vec::new()
        }
    };
    DetectorPromptInfo {
        kind: kind.into(),
        open_vocab: true,
        prompts,
        groups: vec![],
        note: Some(
            "Open-vocab YOLOE (text-prompt mode). Vocabulary is baked at \
             export time via tools/models/gen_yoloe.py; per-camera \
             `prompts` selects a subset to emit."
                .into(),
        ),
        loaded: false,
    }
}

fn yoloe_promptfree_info(kind: &str, cfg: &nexus_config::InferenceConfig) -> DetectorPromptInfo {
    // Prompt-free wraps the same yoloe26 graph but ignores per-camera
    // prompt subsets and caps results at model.top_k. Surface the
    // identical baked vocabulary so the UI can show what *could*
    // come out before top_k truncation.
    let prompts = read_manifest_prompts(cfg, "yoloe26_s").unwrap_or_default();
    DetectorPromptInfo {
        kind: kind.into(),
        open_vocab: true,
        prompts,
        groups: vec![],
        note: Some(
            "Prompt-free YOLOE — ignores per-camera `prompts` and \
             emits every label above the score threshold, capped at \
             `inference.model.top_k`. Useful for triage / discovery."
                .into(),
        ),
        loaded: false,
    }
}

fn yoloe_visual_info(kind: &str) -> DetectorPromptInfo {
    DetectorPromptInfo {
        kind: kind.into(),
        open_vocab: false,
        prompts: vec![],
        groups: vec![],
        note: Some(
            "Visual-prompt YOLOE — classes are defined by image \
             embeddings stored in the visual-prompt library. Attach \
             a visual prompt to the camera in the Visual Prompts tab; \
             this detector does not consume the per-camera `prompts` \
             text list."
                .into(),
        ),
        loaded: false,
    }
}

fn read_manifest_prompts(
    cfg: &nexus_config::InferenceConfig,
    model_id: &str,
) -> Result<Vec<String>, String> {
    let pack =
        cfg.model.pack_path.as_ref().ok_or_else(|| {
            "inference.model.pack_path not set; cannot resolve manifest".to_string()
        })?;
    let manifest_path = pack.join("models-manifest.json");
    let bytes = std::fs::read(&manifest_path)
        .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse manifest: {e}"))?;
    let models = json
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "manifest missing `models` array".to_string())?;
    let entry = models
        .iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model_id))
        .ok_or_else(|| format!("no manifest entry for `{model_id}`"))?;
    let prompts = entry
        .get("prompts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "manifest entry missing `prompts` array".to_string())?;
    Ok(prompts
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coco_info_has_12_grouped_labels() {
        let info = coco_info("yolo");
        assert_eq!(info.prompts.len(), 12);
        assert!(!info.open_vocab);
        let total: usize = info.groups.iter().map(|g| g.labels.len()).sum();
        assert_eq!(total, 12);
        let names: Vec<&str> = info.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["People", "Vehicles", "Animals", "Carried"]);
    }

    #[test]
    fn unknown_kind_returns_empty_with_note() {
        let cfg = nexus_config::InferenceConfig::default();
        let info = info_for_kind("never_heard_of_it", &cfg);
        assert!(info.prompts.is_empty());
        assert!(info.note.is_some());
    }

    #[test]
    fn yolo_world_without_pack_path_returns_empty() {
        let mut cfg = nexus_config::InferenceConfig::default();
        cfg.model.pack_path = None;
        let info = yolo_world_info("yolo_world", &cfg);
        assert!(info.open_vocab);
        assert!(info.prompts.is_empty());
    }
}

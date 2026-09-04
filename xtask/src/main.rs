//! # xtask — repo-maintenance tools
//!
//! Subcommands invoked via `cargo xtask <cmd>`.
//!
//! ## `check-models`
//!
//! Validates [`models/models-manifest.json`](../models/models-manifest.json)
//! against the engine's model-license + product-invariant rules
//! recorded in [`AGENTS.md`](../AGENTS.md) rule 2:
//!
//! 1. **No face-recognition extractors.** Model `id` and artifact
//!    `path` fields are case-insensitively scanned for substrings
//!    that identify known face-recognition model families
//!    (`AdaFace`, `ArcFace`, `InsightFace`, `Buffalo`, `FaceNet`,
//!    `SphereFace`, `CosFace`, `MagFace`). A match is an immediate
//!    failure — these never ship at the edge in v1 regardless of
//!    license, because face recognition undermines the cloud's
//!    pseudonymous-by-default identity vault. See
//!    [`nexus-cloud-console`'s `docs/product/WEDGE_PLAN.md`](../../nexus-cloud-console/docs/product/WEDGE_PLAN.md).
//!
//! 2. **License + dataset-license deny list.** Explicit values that
//!    are incompatible with the engine's AGPL-3.0-or-later license
//!    or with commercial redistribution are rejected:
//!    `non-commercial`, `nc`, `cc-by-nc-*`, `research`,
//!    `research-only`, `unknown`, `proprietary`. The check is
//!    case-insensitive on substring; `weights_dataset_license`
//!    values like `MS1M:research` and `Objects365:CC-BY-NC-4.0`
//!    trip this rule.
//!
//! 3. **Missing license / weights_dataset_license fields.** Warned
//!    by default and elevated to an error under `--strict`. The
//!    distinction lets the gate land before every existing
//!    manifest entry is fully back-filled; CI runs without
//!    `--strict` today and with `--strict` once back-fill is done.
//!
//! 4. **No Hailo artifact for the fire/smoke head.** Per
//!    [`ADR-065`](../../nexus-cloud-console/.obsidian-vault/decisions/ADR-065%20New%20Detection%20Capabilities%20Target%20ONNX%20Profiles%20And%20Defer%20Hailo.md),
//!    new detection capabilities (fire/smoke, pose) target ONNX
//!    profiles only and explicitly defer Hailo — no HEF, no
//!    calibration corpus, no DFC run. Any manifest entry identified
//!    as the fire/smoke head (by the `fire`/`smoke` naming already
//!    used in-repo — see `fire_smoke.rs`, `FireSmokeHead`) that
//!    declares a `hef`/Hailo-backend artifact is a hard error. This
//!    is deliberately scoped to that one head: other models (e.g.
//!    the primary detector) legitimately ship HEF artifacts under
//!    Hailo's continued support for existing capabilities.
//!
//! The check exits 0 on success, 1 on any rule violation, and
//! prints a one-line summary at the end either way.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;

/// CLI surface for `cargo xtask <subcommand>`.
#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Nexus engine repo maintenance tools")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Validate models/models-manifest.json against the rules in
    /// AGENTS.md §2. Exits non-zero on any deny-list violation.
    CheckModels(CheckModelsArgs),
}

#[derive(Debug, clap::Args)]
struct CheckModelsArgs {
    /// Path to the manifest. Defaults to `models/models-manifest.json`
    /// resolved relative to the workspace root.
    #[arg(long, default_value = "models/models-manifest.json")]
    manifest: PathBuf,

    /// Treat "missing license" / "missing weights_dataset_license"
    /// warnings as hard errors. Off by default during the back-fill
    /// transition; flip on in CI once every model entry declares
    /// both fields.
    #[arg(long)]
    strict: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::CheckModels(args) => check_models(args),
    }
}

// ---------------------------------------------------------------------------
// `check-models`
// ---------------------------------------------------------------------------

/// Substrings identifying face-recognition model families that MUST
/// NOT ship in `models/` at the edge. See AGENTS.md rule 2.
const FACE_REC_DENYLIST: &[&str] = &[
    "adaface",
    "arcface",
    "insightface",
    "buffalo",
    "facenet",
    "sphereface",
    "cosface",
    "magface",
];

/// Substrings that disqualify a license / dataset-license value.
/// Match is case-insensitive on the field's full string (so an entry
/// like `"weights_dataset_license": "MS1M:research"` trips on
/// `"research"`).
const LICENSE_DENYLIST: &[&str] = &[
    "non-commercial",
    "noncommercial",
    "nc-4.0",
    "cc-by-nc",
    "research-only",
    "research",
    "unknown",
    "proprietary",
];

/// SPEC-035 / ADR-042 — person-category and generic-face-covering terms
/// that must never appear as an open-vocab prompt. Mirrors
/// `HARM_TAXONOMY.md` §9 (and the cloud `check-taxonomy` PROHIBITED_TERMS)
/// so the edge manifest is held to the same rule the cloud catalogue is:
/// a prompt names an act, condition, or object, never a category of
/// person. `balaclava` is behaviourally specific and is *not* on the list,
/// so it passes while a generic `mask` / `face covering` fails.
const PROHIBITED_PROMPT_TERMS: &[&str] = &[
    "addict",
    "homeless",
    "vagrant",
    "gang member",
    "suspicious person",
    "loiterer",
    "mask",
    "face covering",
];

/// Substrings identifying the fire/smoke detection head by model `id`.
/// Mirrors the naming already established in-repo for this capability
/// (`crates/nexus-inference/src/fire_smoke.rs`, `FireSmokeHead`,
/// `InferenceConfig::fire_smoke_head_enabled`) rather than inventing a
/// new convention. ADR-065 defers Hailo for this head — it ships
/// ONNX-only, one artifact per ladder rung — so any manifest entry
/// matched by this marker that declares a HEF/Hailo-backend artifact
/// trips a hard error (see `fire_smoke_head_forbids_hailo_artifact`
/// below). This binds the moment such an entry is added; it does not
/// require the still-open model-cut work to land first.
const FIRE_SMOKE_HEAD_ID_MARKERS: &[&str] = &["fire", "smoke"];

/// Backend values that identify a Hailo-compiled artifact, as opposed
/// to the ONNX artifacts every profile but `hailo` consumes.
const HAILO_BACKEND_MARKERS: &[&str] = &["hef", "hailo"];

/// Subset of `models-manifest.json` that the check needs to inspect.
/// All other fields (artifacts, presets, prompts, thresholds, etc.)
/// are intentionally `serde(flatten)`-ignored.
#[derive(Debug, Deserialize)]
struct Manifest {
    models: Vec<ManifestModel>,
}

#[derive(Debug, Deserialize)]
struct ManifestModel {
    id: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    weights_dataset_license: Option<String>,
    #[serde(default)]
    artifacts: Vec<ManifestArtifact>,
    /// Open-vocabulary prompt terms baked into this model's graph at
    /// export. Empty for closed-vocab detectors. Scanned by the
    /// SPEC-035 term-list lint (ADR-042).
    #[serde(default)]
    prompts: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ManifestArtifact {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    backend: Option<String>,
}

/// Outcome of a single rule against the manifest.
#[derive(Debug, Default)]
struct CheckReport {
    errors: Vec<String>,
    warnings: Vec<String>,
}

impl CheckReport {
    fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }
    fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }
}

fn check_models(args: CheckModelsArgs) -> Result<()> {
    let manifest_path = resolve_manifest_path(&args.manifest);
    let bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse manifest {}", manifest_path.display()))?;

    let report = audit_manifest(&manifest);

    for w in &report.warnings {
        eprintln!("warn: {w}");
    }
    for e in &report.errors {
        eprintln!("error: {e}");
    }

    let fatal_warnings = args.strict && !report.warnings.is_empty();
    let failed = !report.errors.is_empty() || fatal_warnings;

    eprintln!(
        "check-models: {} model{}, {} warning{}, {} error{}{}",
        manifest.models.len(),
        if manifest.models.len() == 1 { "" } else { "s" },
        report.warnings.len(),
        if report.warnings.len() == 1 { "" } else { "s" },
        report.errors.len(),
        if report.errors.len() == 1 { "" } else { "s" },
        if args.strict { " (strict)" } else { "" },
    );

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Resolve the manifest path against the workspace root (parent of
/// `xtask/` when run via `cargo xtask`). Absolute paths are used as-is.
fn resolve_manifest_path(manifest: &Path) -> PathBuf {
    if manifest.is_absolute() {
        return manifest.to_path_buf();
    }
    // Walk up from CARGO_MANIFEST_DIR (set to `xtask/` by `cargo run`)
    // until we hit a dir that contains the requested file, or fall
    // back to cwd if the env var isn't set (running the binary directly).
    if let Some(crate_dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
        let crate_dir = PathBuf::from(crate_dir);
        if let Some(workspace_root) = crate_dir.parent() {
            let candidate = workspace_root.join(manifest);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    manifest.to_path_buf()
}

/// Inspect every model entry and return aggregated errors/warnings.
/// Pure; takes a parsed manifest and returns a report. Tests drive
/// this directly with synthetic manifests.
fn audit_manifest(manifest: &Manifest) -> CheckReport {
    let mut report = CheckReport::default();
    for model in &manifest.models {
        audit_model(model, &mut report);
    }
    report
}

fn audit_model(model: &ManifestModel, report: &mut CheckReport) {
    // Rule 1: face-recognition name pattern.
    let id_lc = model.id.to_lowercase();
    for needle in FACE_REC_DENYLIST {
        if id_lc.contains(needle) {
            report.error(format!(
                "model id '{}' matches face-rec denylist substring '{}' — see AGENTS.md rule 2",
                model.id, needle
            ));
        }
    }
    for art in &model.artifacts {
        if let Some(path) = &art.path {
            let path_lc = path.to_lowercase();
            for needle in FACE_REC_DENYLIST {
                if path_lc.contains(needle) {
                    report.error(format!(
                        "model '{}' artifact path '{}' matches face-rec denylist substring '{}' — see AGENTS.md rule 2",
                        model.id, path, needle
                    ));
                }
            }
        }
    }

    // Rule 2: license + dataset-license deny list.
    match &model.license {
        Some(lic) => {
            if let Some(bad) = license_denylist_hit(lic) {
                report.error(format!(
                    "model '{}' license '{}' contains denylisted token '{}'",
                    model.id, lic, bad
                ));
            }
        }
        None => {
            report.warn(format!(
                "model '{}' missing `license` field — backfill before --strict CI",
                model.id
            ));
        }
    }
    match &model.weights_dataset_license {
        Some(lic) => {
            if let Some(bad) = license_denylist_hit(lic) {
                report.error(format!(
                    "model '{}' weights_dataset_license '{}' contains denylisted token '{}'",
                    model.id, lic, bad
                ));
            }
        }
        None => {
            report.warn(format!(
                "model '{}' missing `weights_dataset_license` field — backfill before --strict CI",
                model.id
            ));
        }
    }

    // SPEC-035 / ADR-042: no open-vocab prompt names a category of
    // person or a generic face covering. `balaclava` is permitted.
    for prompt in &model.prompts {
        if let Some(term) = prohibited_prompt_hit(prompt) {
            report.error(format!(
                "model '{}' prompt '{}' contains prohibited §9 term '{}' — Sentinel classifies acts and conditions, never categories of person (ADR-042)",
                model.id, prompt, term
            ));
        }
    }

    // ADR-065 / SPEC-036: the fire/smoke head is deferred on Hailo — it
    // ships ONNX-only, one artifact per ladder rung, with no HEF, no
    // calibration corpus, and no DFC run. A Hailo/HEF artifact declared
    // for a model identified as the fire/smoke head is a structural
    // regression of that decision, not a style nit, so it is a hard
    // error regardless of `--strict`. Scoped to this one head: other
    // models (e.g. the primary detector) legitimately ship HEF
    // artifacts under Hailo's continued support for existing
    // capabilities.
    if FIRE_SMOKE_HEAD_ID_MARKERS
        .iter()
        .any(|marker| id_lc.contains(marker))
    {
        for art in &model.artifacts {
            let backend_lc = art.backend.as_deref().unwrap_or("").to_lowercase();
            let path_lc = art.path.as_deref().unwrap_or("").to_lowercase();
            let is_hailo_artifact = HAILO_BACKEND_MARKERS
                .iter()
                .any(|marker| backend_lc.contains(marker))
                || path_lc.ends_with(".hef");
            if is_hailo_artifact {
                report.error(format!(
                    "model '{}' (fire/smoke head) declares a Hailo artifact '{}' (backend '{}') — ADR-065 defers Hailo for new detection capabilities; the fire/smoke head ships ONNX-only, one artifact per ladder rung, with no HEF. Remove this artifact rather than adding Hailo backend work for it.",
                    model.id,
                    art.path.as_deref().unwrap_or("<no path>"),
                    art.backend.as_deref().unwrap_or("<no backend>"),
                ));
            }
        }
    }
}

/// Word-boundary, case-insensitive scan for a §9 prohibited prompt term.
/// Word-boundary matching is what lets `balaclava` pass while a bare
/// `mask` fails, and stops `mask` from tripping on an unrelated compound.
fn prohibited_prompt_hit(prompt: &str) -> Option<&'static str> {
    let lower = prompt.to_lowercase();
    let bytes = lower.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    for term in PROHIBITED_PROMPT_TERMS {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(term) {
            let start = from + rel;
            let end = start + term.len();
            let before_ok = start == 0 || !is_word(bytes[start - 1]);
            let after_ok = end == bytes.len() || !is_word(bytes[end]);
            if before_ok && after_ok {
                return Some(term);
            }
            from = start + 1;
        }
    }
    None
}

fn license_denylist_hit(value: &str) -> Option<&'static str> {
    let lc = value.to_lowercase();
    LICENSE_DENYLIST
        .iter()
        .copied()
        .find(|needle| lc.contains(needle))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Manifest {
        serde_json::from_str(json).expect("test manifest must parse")
    }

    #[test]
    fn empty_manifest_is_clean() {
        let r = audit_manifest(&parse(r#"{"models":[]}"#));
        assert!(r.errors.is_empty());
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn allowlisted_entry_passes() {
        let r = audit_manifest(&parse(
            r#"{"models":[{
                "id":"yolo26n",
                "license":"AGPL-3.0",
                "weights_dataset_license":"COCO:CC-BY-4.0",
                "artifacts":[]
            }]}"#,
        ));
        assert!(r.errors.is_empty(), "errors: {:?}", r.errors);
        assert!(r.warnings.is_empty(), "warnings: {:?}", r.warnings);
    }

    #[test]
    fn face_rec_id_is_rejected() {
        // The classic InsightFace ArcFace pattern — model id contains
        // a face-rec brand substring even when the artifact path is
        // generic.
        let r = audit_manifest(&parse(
            r#"{"models":[{
                "id":"arcface_r100",
                "license":"MIT",
                "weights_dataset_license":"VGGFace2:CC-BY-4.0",
                "artifacts":[{"path":"emb_r100.onnx"}]
            }]}"#,
        ));
        assert!(
            r.errors.iter().any(|e| e.contains("arcface")),
            "errors: {:?}",
            r.errors
        );
    }

    #[test]
    fn face_rec_artifact_path_is_rejected_even_with_clean_id() {
        let r = audit_manifest(&parse(
            r#"{"models":[{
                "id":"emb_v1",
                "license":"Apache-2.0",
                "weights_dataset_license":"DINOv2:Apache-2.0",
                "artifacts":[{"path":"adaface_r100.onnx"}]
            }]}"#,
        ));
        assert!(
            r.errors.iter().any(|e| e.contains("adaface")),
            "errors: {:?}",
            r.errors
        );
    }

    #[test]
    fn insightface_buffalo_bundle_is_rejected() {
        let r = audit_manifest(&parse(
            r#"{"models":[{
                "id":"buffalo_l",
                "license":"Apache-2.0",
                "weights_dataset_license":"Glint360K:CC-BY-NC-4.0",
                "artifacts":[]
            }]}"#,
        ));
        // Both `buffalo` (name) and `cc-by-nc` (dataset) hit.
        assert!(
            r.errors.iter().any(|e| e.contains("buffalo")),
            "errors: {:?}",
            r.errors
        );
        assert!(
            r.errors
                .iter()
                .any(|e| e.to_lowercase().contains("cc-by-nc")),
            "errors: {:?}",
            r.errors
        );
    }

    #[test]
    fn research_only_dataset_is_rejected() {
        let r = audit_manifest(&parse(
            r#"{"models":[{
                "id":"x",
                "license":"Apache-2.0",
                "weights_dataset_license":"MS1M:research",
                "artifacts":[]
            }]}"#,
        ));
        assert!(
            r.errors
                .iter()
                .any(|e| e.to_lowercase().contains("research")),
            "errors: {:?}",
            r.errors
        );
    }

    #[test]
    fn non_commercial_license_is_rejected() {
        let r = audit_manifest(&parse(
            r#"{"models":[{
                "id":"x",
                "license":"Non-Commercial",
                "weights_dataset_license":"COCO:CC-BY-4.0",
                "artifacts":[]
            }]}"#,
        ));
        assert!(r
            .errors
            .iter()
            .any(|e| e.to_lowercase().contains("non-commercial")));
    }

    #[test]
    fn missing_fields_warn_but_dont_error() {
        let r = audit_manifest(&parse(r#"{"models":[{"id":"x","artifacts":[]}]}"#));
        assert!(r.errors.is_empty(), "errors: {:?}", r.errors);
        assert_eq!(r.warnings.len(), 2);
    }

    #[test]
    fn generic_mask_prompt_is_rejected_but_balaclava_is_accepted() {
        // SPEC-035 / ADR-042: the term-list lint fed both a generic
        // `mask` and the behaviourally-specific `balaclava` must reject
        // the first and accept the second.
        let r = audit_manifest(&parse(
            r#"{"models":[{
                "id":"yolo_world_v2_s",
                "license":"AGPL-3.0",
                "weights_dataset_license":"COCO:CC-BY-4.0",
                "artifacts":[],
                "prompts":["mask","balaclava"]
            }]}"#,
        ));
        assert!(
            r.errors.iter().any(|e| e.contains("prompt 'mask'")),
            "generic mask must be rejected; errors: {:?}",
            r.errors
        );
        assert!(
            !r.errors.iter().any(|e| e.contains("balaclava")),
            "balaclava must be accepted; errors: {:?}",
            r.errors
        );
    }

    #[test]
    fn person_category_prompts_are_rejected() {
        for term in [
            "addict",
            "homeless person",
            "suspicious person",
            "gang member",
            "face covering",
        ] {
            let json = format!(
                r#"{{"models":[{{"id":"m","license":"AGPL-3.0","weights_dataset_license":"COCO:CC-BY-4.0","artifacts":[],"prompts":["{term}"]}}]}}"#
            );
            let r = audit_manifest(&parse(&json));
            assert!(
                r.errors.iter().any(|e| e.contains("prohibited §9 term")),
                "prompt '{term}' must be rejected; errors: {:?}",
                r.errors
            );
        }
    }

    #[test]
    fn benign_object_prompts_pass_the_term_lint() {
        // Every Pass A / Pass B term SPEC-035 adds names an act, object,
        // or fixture — none is a person category — so the lint is clean.
        let r = audit_manifest(&parse(
            r#"{"models":[{
                "id":"yolo_world_v2_s",
                "license":"AGPL-3.0",
                "weights_dataset_license":"COCO:CC-BY-4.0",
                "artifacts":[],
                "prompts":["bolt cutter","angle grinder","balaclava","gas can","tent","door","dumpster","ATM"]
            }]}"#,
        ));
        assert!(r.errors.is_empty(), "errors: {:?}", r.errors);
    }

    #[test]
    fn fire_smoke_head_onnx_only_passes() {
        // ADR-065: the fire/smoke head ships ONNX-only, one artifact per
        // ladder rung. An entry with only `onnx` artifacts must be clean.
        let r = audit_manifest(&parse(
            r#"{"models":[{
                "id":"fire_smoke_yolo26n",
                "license":"AGPL-3.0",
                "weights_dataset_license":"D-Fire:CC-BY-4.0",
                "artifacts":[
                    {"backend":"onnx","path":"fire_smoke_512x288.onnx"},
                    {"backend":"onnx","path":"fire_smoke_1024x576.onnx"},
                    {"backend":"onnx","path":"fire_smoke_1536x864.onnx"}
                ]
            }]}"#,
        ));
        assert!(r.errors.is_empty(), "errors: {:?}", r.errors);
    }

    #[test]
    fn fire_smoke_head_forbids_hailo_artifact() {
        // ADR-065 defers Hailo for the fire/smoke head. A `hef` backend
        // artifact declared for it must be a hard error naming the
        // head, the offending artifact, and ADR-065.
        let r = audit_manifest(&parse(
            r#"{"models":[{
                "id":"fire_smoke_yolo26n",
                "license":"AGPL-3.0",
                "weights_dataset_license":"D-Fire:CC-BY-4.0",
                "artifacts":[
                    {"backend":"onnx","path":"fire_smoke_512x288.onnx"},
                    {"backend":"hef","path":"fire_smoke_512x288.hef"}
                ]
            }]}"#,
        ));
        assert!(
            r.errors.iter().any(|e| e.contains("fire_smoke_yolo26n")
                && e.contains("fire_smoke_512x288.hef")
                && e.contains("ADR-065")),
            "expected a fire/smoke-head Hailo error naming the model, artifact, and ADR-065; errors: {:?}",
            r.errors
        );
    }

    #[test]
    fn hailo_artifacts_on_non_fire_smoke_models_are_unaffected() {
        // The rule must be scoped to the fire/smoke head only. Existing
        // models (e.g. the primary detector) legitimately ship HEF
        // artifacts under Hailo's continued support and must not trip
        // this check.
        let r = audit_manifest(&parse(
            r#"{"models":[{
                "id":"yolo26n",
                "license":"AGPL-3.0",
                "weights_dataset_license":"COCO:CC-BY-4.0",
                "artifacts":[
                    {"backend":"onnx","path":"yolo26n_512x288.onnx"},
                    {"backend":"hef","path":"yolo26n_512x288.hef"}
                ]
            }]}"#,
        ));
        assert!(r.errors.is_empty(), "errors: {:?}", r.errors);
    }

    #[test]
    fn shipping_manifest_is_clean_or_warn_only() {
        // The actual repo manifest must never error under the default
        // (non-strict) check — that's the gate this xtask provides.
        // Warnings about missing license fields on legacy entries are
        // acceptable until those entries are back-filled.
        let manifest_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask crate has a parent dir")
            .join("models/models-manifest.json");
        let bytes = std::fs::read(&manifest_path).expect("manifest readable");
        let manifest: Manifest = serde_json::from_slice(&bytes).expect("manifest parses");
        let r = audit_manifest(&manifest);
        assert!(
            r.errors.is_empty(),
            "models/models-manifest.json must pass non-strict check-models; errors: {:?}",
            r.errors
        );
    }
}

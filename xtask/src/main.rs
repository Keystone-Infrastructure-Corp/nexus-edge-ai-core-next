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
//! 2. **License allowlist.** `license` must exactly match one of
//!    `LICENSE_ALLOWLIST` (Apache-2.0, MIT, BSD-3-Clause,
//!    Apache-2.0 WITH LLVM-exception, AGPL-3.0, GPL-3.0). Any other
//!    present value is an error.
//!
//! 3. **Dataset-license deny list.** `weights_dataset_license` is
//!    rejected when it contains a `LICENSE_DENYLIST` substring
//!    (`non-commercial`, `cc-by-nc`, `research`, `academic`, ...;
//!    case-insensitive). A model may carry a non-empty
//!    `dataset_license_waiver` string: a denylist hit on that model's
//!    dataset license is then printed as a `waived:` line on every
//!    run instead of failing. A waiver on a model whose dataset
//!    license does not hit the denylist is itself an error, and a
//!    waiver never excuses the license allowlist, a missing field,
//!    the face-recognition denylist or the folder scan.
//!
//! 4. **Missing license / weights_dataset_license fields.** Warned
//!    by default and elevated to an error under `--strict`.
//!
//! 5. **Folder scan.** Every file under the models directory (by
//!    default the manifest's directory; `--models-dir` overrides)
//!    must be named by an artifact `path`, except
//!    `models-manifest.json`, `.gitkeep` and `<artifact>.sha256`
//!    sidecars of listed artifacts. An unlisted file is an error,
//!    and every file name is checked against the face-recognition
//!    denylist, so weights no manifest entry names cannot escape
//!    the rules above.
//!
//! The check exits 0 on success, 1 on any rule violation, and
//! prints a one-line summary at the end either way.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "xtask is a command-line tool; terminal output is its interface"
)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;

mod span_cardinality;

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
    /// Reject forbidden field keys (secrets, edge-resident URLs,
    /// per-request / per-frame ids) in `tracing` macros under crates/.
    /// Exits non-zero on any violation.
    CheckSpanCardinality,
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

    /// Directory scanned for weight files (recursive). Defaults to
    /// the directory containing the manifest; point it at downloaded
    /// release weights to audit those.
    #[arg(long)]
    models_dir: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::CheckModels(args) => check_models(args),
        Cmd::CheckSpanCardinality => {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .context("xtask manifest dir has no parent")?;
            std::process::exit(span_cardinality::run(root)?);
        }
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

/// Exact `license` values a model entry may declare. AGPL-3.0 and
/// GPL-3.0 are there because AGENTS.md rule 1 ships YOLO weights under
/// them and `deny.toml` allows them for the engine.
const LICENSE_ALLOWLIST: &[&str] = &[
    "Apache-2.0",
    "MIT",
    "BSD-3-Clause",
    "Apache-2.0 WITH LLVM-exception",
    "AGPL-3.0",
    "GPL-3.0",
];

/// Substrings that disqualify a `weights_dataset_license` value.
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
    "academic",
    "unknown",
    "proprietary",
];

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
    dataset_license_waiver: Option<String>,
    #[serde(default)]
    artifacts: Vec<ManifestArtifact>,
}

#[derive(Debug, Deserialize)]
struct ManifestArtifact {
    #[serde(default)]
    path: Option<String>,
}

/// Outcome of a single rule against the manifest.
#[derive(Debug, Default)]
struct CheckReport {
    errors: Vec<String>,
    warnings: Vec<String>,
    waived: Vec<String>,
}

impl CheckReport {
    fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }
    fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }
    fn waive(&mut self, msg: impl Into<String>) {
        self.waived.push(msg.into());
    }
}

fn check_models(args: CheckModelsArgs) -> Result<()> {
    let manifest_path = resolve_manifest_path(&args.manifest);
    let bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse manifest {}", manifest_path.display()))?;

    let mut report = audit_manifest(&manifest);

    let models_dir = match &args.models_dir {
        Some(dir) => dir.clone(),
        None => manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
    };
    let files = list_files(&models_dir)
        .with_context(|| format!("failed to scan models dir {}", models_dir.display()))?;
    audit_files(&files, &manifest, &mut report);

    for w in &report.waived {
        eprintln!("waived: {w}");
    }
    for w in &report.warnings {
        eprintln!("warn: {w}");
    }
    for e in &report.errors {
        eprintln!("error: {e}");
    }

    let fatal_warnings = args.strict && !report.warnings.is_empty();
    let failed = !report.errors.is_empty() || fatal_warnings;

    eprintln!(
        "check-models: {} model{}, {} file{} scanned, {} waived, {} warning{}, {} error{}{}",
        manifest.models.len(),
        if manifest.models.len() == 1 { "" } else { "s" },
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        report.waived.len(),
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

    // Rule 2: `license` must be on the allowlist.
    match &model.license {
        Some(lic) => {
            if !LICENSE_ALLOWLIST.contains(&lic.as_str()) {
                report.error(format!(
                    "model '{}' license '{}' is not on the allowlist ({}) — see AGENTS.md rule 2",
                    model.id,
                    lic,
                    LICENSE_ALLOWLIST.join(", ")
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

    // Rule 3: dataset-license deny list, with an explicit waiver.
    let waiver = model
        .dataset_license_waiver
        .as_deref()
        .filter(|w| !w.trim().is_empty());
    let mut dataset_hit = None;
    match &model.weights_dataset_license {
        Some(lic) => {
            dataset_hit = license_denylist_hit(lic).map(|bad| (lic, bad));
        }
        None => {
            report.warn(format!(
                "model '{}' missing `weights_dataset_license` field — backfill before --strict CI",
                model.id
            ));
        }
    }
    match (dataset_hit, waiver) {
        (Some((lic, bad)), Some(reason)) => report.waive(format!(
            "model '{}' weights_dataset_license '{}' contains denylisted token '{}' — {}",
            model.id, lic, bad, reason
        )),
        (Some((lic, bad)), None) => report.error(format!(
            "model '{}' weights_dataset_license '{}' contains denylisted token '{}'",
            model.id, lic, bad
        )),
        (None, _) if model.dataset_license_waiver.is_some() => report.error(format!(
            "model '{}' carries a `dataset_license_waiver` but its weights_dataset_license does not hit the denylist — remove the stale waiver",
            model.id
        )),
        (None, _) => {}
    }
}

/// Every regular file under `dir` (recursive, not following symlinked
/// directories), as `/`-separated paths relative to `dir`, sorted.
fn list_files(dir: &Path) -> std::io::Result<Vec<String>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                walk(root, &path, out)?;
            } else if let Ok(rel) = path.strip_prefix(root) {
                let parts: Vec<_> = rel.iter().map(|c| c.to_string_lossy()).collect();
                out.push(parts.join("/"));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// Rule 5: every file found must be named by an artifact `path`
/// (exempt: the manifest itself, `.gitkeep`, and `<artifact>.sha256`
/// sidecars of listed artifacts), and no file name may match the
/// face-recognition denylist. Pure over the file list.
fn audit_files(files: &[String], manifest: &Manifest, report: &mut CheckReport) {
    let listed: HashSet<&str> = manifest
        .models
        .iter()
        .flat_map(|m| &m.artifacts)
        .filter_map(|a| a.path.as_deref())
        .collect();
    for file in files {
        let lc = file.to_lowercase();
        for needle in FACE_REC_DENYLIST {
            if lc.contains(needle) {
                report.error(format!(
                    "file '{}' matches face-rec denylist substring '{}' — see AGENTS.md rule 2",
                    file, needle
                ));
            }
        }
        let basename = file.rsplit('/').next().unwrap_or(file);
        let is_sidecar = file
            .strip_suffix(".sha256")
            .is_some_and(|stem| listed.contains(stem));
        let exempt = file == "models-manifest.json" || basename == ".gitkeep" || is_sidecar;
        if !exempt && !listed.contains(file.as_str()) {
            report.error(format!(
                "file '{file}' is not named by any manifest artifact path — list it or remove it (AGENTS.md rule 2)"
            ));
        }
    }
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

    fn model_json(license: &str, dataset: &str, waiver: Option<&str>) -> String {
        let waiver = waiver
            .map(|w| format!(r#","dataset_license_waiver":"{w}""#))
            .unwrap_or_default();
        format!(
            r#"{{"models":[{{"id":"x","license":"{license}","weights_dataset_license":"{dataset}"{waiver},"artifacts":[]}}]}}"#
        )
    }

    #[test]
    fn license_allowlist_accepts_listed_values() {
        for lic in [
            "AGPL-3.0",
            "GPL-3.0",
            "Apache-2.0",
            "MIT",
            "BSD-3-Clause",
            "Apache-2.0 WITH LLVM-exception",
        ] {
            let r = audit_manifest(&parse(&model_json(lic, "COCO:CC-BY-4.0", None)));
            assert!(r.errors.is_empty(), "{lic}: {:?}", r.errors);
        }
    }

    #[test]
    fn license_allowlist_rejects_unlisted_values() {
        for lic in ["SSPL-1.0", "Llama Community License", "CC-BY-SA-4.0"] {
            let r = audit_manifest(&parse(&model_json(lic, "COCO:CC-BY-4.0", None)));
            assert!(
                r.errors.iter().any(|e| e.contains(lic)),
                "{lic}: {:?}",
                r.errors
            );
        }
    }

    #[test]
    fn academic_dataset_is_rejected() {
        let r = audit_manifest(&parse(&model_json(
            "MIT",
            "Objects365v1:academic-only",
            None,
        )));
        assert!(
            r.errors.iter().any(|e| e.contains("academic")),
            "errors: {:?}",
            r.errors
        );
    }

    #[test]
    fn waiver_turns_a_dataset_hit_into_a_waived_line() {
        let r = audit_manifest(&parse(&model_json(
            "AGPL-3.0",
            "Objects365v1:academic-only",
            Some("pending legal review"),
        )));
        assert!(r.errors.is_empty(), "errors: {:?}", r.errors);
        assert_eq!(r.waived.len(), 1);
        assert!(r.waived[0].contains("pending legal review"));
    }

    #[test]
    fn empty_waiver_does_not_waive_a_dataset_hit() {
        let r = audit_manifest(&parse(&model_json(
            "AGPL-3.0",
            "Objects365v1:academic-only",
            Some("  "),
        )));
        assert!(r.waived.is_empty());
        assert!(r.errors.iter().any(|e| e.contains("academic")));
    }

    #[test]
    fn waiver_does_not_excuse_a_license_miss() {
        let r = audit_manifest(&parse(&model_json(
            "SSPL-1.0",
            "Objects365v1:academic-only",
            Some("pending legal review"),
        )));
        assert_eq!(r.waived.len(), 1);
        assert!(
            r.errors.iter().any(|e| e.contains("SSPL-1.0")),
            "errors: {:?}",
            r.errors
        );
    }

    #[test]
    fn waiver_does_not_excuse_missing_fields() {
        let r = audit_manifest(&parse(
            r#"{"models":[{"id":"x","dataset_license_waiver":"why","artifacts":[]}]}"#,
        ));
        assert_eq!(r.warnings.len(), 2);
        assert!(
            r.errors.iter().any(|e| e.contains("stale")),
            "errors: {:?}",
            r.errors
        );
    }

    #[test]
    fn stale_waiver_is_an_error() {
        let r = audit_manifest(&parse(&model_json(
            "AGPL-3.0",
            "COCO:CC-BY-4.0",
            Some("no longer needed"),
        )));
        assert!(r.waived.is_empty());
        assert!(
            r.errors.iter().any(|e| e.contains("stale waiver")),
            "errors: {:?}",
            r.errors
        );
    }

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    const ONE_ARTIFACT: &str = r#"{"models":[{"id":"x","license":"MIT",
        "weights_dataset_license":"COCO:CC-BY-4.0","artifacts":[{"path":"x_512.onnx"}]}]}"#;

    #[test]
    fn unlisted_file_is_an_error() {
        let mut r = CheckReport::default();
        audit_files(
            &files(&["x_512.onnx", "stray.onnx"]),
            &parse(ONE_ARTIFACT),
            &mut r,
        );
        assert_eq!(r.errors.len(), 1, "errors: {:?}", r.errors);
        assert!(r.errors[0].contains("stray.onnx"));
    }

    #[test]
    fn face_rec_file_name_is_an_error() {
        let mut r = CheckReport::default();
        audit_files(
            &files(&["x_512.onnx", "ArcFace_r100.onnx"]),
            &parse(ONE_ARTIFACT),
            &mut r,
        );
        assert!(
            r.errors.iter().any(|e| e.contains("arcface")),
            "errors: {:?}",
            r.errors
        );
    }

    #[test]
    fn listed_files_sidecars_manifest_and_gitkeep_are_accepted() {
        let mut r = CheckReport::default();
        audit_files(
            &files(&[
                ".gitkeep",
                "models-manifest.json",
                "x_512.onnx",
                "x_512.onnx.sha256",
            ]),
            &parse(ONE_ARTIFACT),
            &mut r,
        );
        assert!(r.errors.is_empty(), "errors: {:?}", r.errors);
    }

    #[test]
    fn sidecar_of_an_unlisted_artifact_is_an_error() {
        let mut r = CheckReport::default();
        audit_files(&files(&["other.onnx.sha256"]), &parse(ONE_ARTIFACT), &mut r);
        assert_eq!(r.errors.len(), 1, "errors: {:?}", r.errors);
    }

    #[test]
    fn list_files_recurses_and_reports_relative_paths() {
        let dir = std::env::temp_dir().join(format!("xtask-list-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.onnx"), b"").unwrap();
        std::fs::write(dir.join("sub/b.onnx"), b"").unwrap();
        let found = list_files(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, files(&["a.onnx", "sub/b.onnx"]));
    }

    #[test]
    fn shipping_manifest_is_clean() {
        // The actual repo manifest and models/ folder must pass the
        // strict check: no errors, no warnings (every entry declares
        // both license fields), and no unlisted weight files.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask crate has a parent dir");
        let bytes = std::fs::read(root.join("models/models-manifest.json")).expect("readable");
        let manifest: Manifest = serde_json::from_slice(&bytes).expect("manifest parses");
        let mut r = audit_manifest(&manifest);
        audit_files(
            &list_files(&root.join("models")).unwrap(),
            &manifest,
            &mut r,
        );
        assert!(
            r.errors.is_empty() && r.warnings.is_empty(),
            "models/ must pass strict check-models; errors: {:?}, warnings: {:?}",
            r.errors,
            r.warnings
        );
    }
}

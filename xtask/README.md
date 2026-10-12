# xtask — repo maintenance tools

`cargo xtask <subcommand>` runs ad-hoc tools that don't belong in any
runtime crate. The `xtask = "run --package xtask --bin xtask --"`
alias in [`../.cargo/config.toml`](../.cargo/config.toml) keeps the
invocation short.

## `cargo xtask check-models`

Validates [`../models/models-manifest.json`](../models/models-manifest.json)
against the model-license + product-invariant rules recorded in
[`../AGENTS.md`](../AGENTS.md) rule 2:

| Rule | Behavior |
| --- | --- |
| Face-recognition denylist substrings in model `id` or artifact `path` (`AdaFace`, `ArcFace`, `InsightFace`, `Buffalo`, `FaceNet`, `SphereFace`, `CosFace`, `MagFace`) | **Hard error.** These never ship at the edge in v1. |
| `license` not exactly one of `Apache-2.0`, `MIT`, `BSD-3-Clause`, `Apache-2.0 WITH LLVM-exception`, `AGPL-3.0`, `GPL-3.0` | **Hard error.** Exact match. |
| `weights_dataset_license` deny values (`non-commercial`, `nc-4.0`, `cc-by-nc`, `research`, `research-only`, `academic`, `unknown`, `proprietary`) | **Hard error.** Case-insensitive substring match, unless the entry has a non-empty `dataset_license_waiver`: the hit is then printed as a `waived:` line on every run. A waiver never excuses the `license` allowlist, missing fields, the face-recognition denylist or the folder scan, and a waiver on an entry with no dataset-license hit is an error. |
| Missing `license` or `weights_dataset_license` on any entry | **Warning** by default; **error** under `--strict`. |
| A file under the models directory that no artifact `path` names (exempt: `models-manifest.json`, `.gitkeep`, `<artifact>.sha256` sidecars of listed artifacts); every file name is also checked against the face-recognition denylist | **Hard error.** The directory is the manifest's own by default; `--models-dir <path>` points the scan at downloaded weights. |

Exits non-zero on any error (or warning under `--strict`).

### CI integration

```yaml
- name: check models
  run: cargo xtask check-models
```

Every model entry now declares both `license` and
`weights_dataset_license`, so CI can run `--strict`; add
`--models-dir <path>` to audit downloaded weights.

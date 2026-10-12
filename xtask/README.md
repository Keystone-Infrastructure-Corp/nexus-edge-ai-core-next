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
| `weights_dataset_license` deny values (`non-commercial`, `nc-4.0`, `cc-by-nc`, `research`, `research-only`, `academic`, `unknown`, `proprietary`) | **Hard error.** Case-insensitive substring match; a hit is always an error. |
| Missing `license` or `weights_dataset_license` on any entry | **Warning** by default; **error** under `--strict`. |
| A file under the models directory that no artifact `path` names (exempt: `models-manifest.json`, `.gitkeep`, `<artifact>.sha256` sidecars of listed artifacts); every file name is also checked against the face-recognition denylist | **Hard error.** The directory is the manifest's own by default; `--models-dir <path>` points the scan at downloaded weights. |

There is no exemption for generator intermediates. `tools/models/gen_dinov2_hailo.py`
writes the build-only `models/dinov2_s_224_hailo_src.onnx` (gitignored, not
shipped), and a default run over a developer's `models/` will name it as unlisted:
delete it, or audit a release pack with `--models-dir`. The scan cannot tell a
developer's folder from a pack, and a pack that carries an unlisted ONNX is the
accident the rule catches. The `shipping_manifest_is_clean` unit test audits the
manifest and the git-tracked contents of `models/` only, so local files never
decide `cargo test`.

Exits non-zero on any error (or warning under `--strict`).

### CI integration

```yaml
- name: check models
  run: cargo xtask check-models
```

CI does not run `--strict` yet. The `yolo_world_v2_s` and `yoloe26_s` entries
carry no licence fields (artifact-level provenance is not verified), so they
produce missing-field warnings, which `--strict` would turn into failures.
Every other rule fails without `--strict`. Switch CI to `--strict` once those
entries carry licence fields backed by verified artifact provenance. The
`system-libs` job adds `--models-dir <path>` to audit the downloaded weights.

## `cargo xtask check-span-cardinality`

Scans `crates/**/*.rs` for forbidden field keys inside `tracing` event and
span macros (`trace!` .. `error!`, `event!`, `span!`, `*_span!`) and inside
`#[instrument(fields(...))]`. Spans reach the cloud through the `TraceLayer`
and the OTLP exporter, and [`../AGENTS.md`](../AGENTS.md) rule 6 keeps camera
credentials and RTSP / sink locations on the box, so these keys are blocked at
the source.

| Key | Why |
| --- | --- |
| `password`, `secret`, `token`, `api_key`, `credential`, `credentials` | Credential leak. Same names the engine's diagnostics redactor treats as secret. |
| `url`, `endpoint` | A camera's `ingest.url` is stored with `user:pass@` userinfo and was being logged under `url`; ONVIF and sink `endpoint`s are customer infrastructure. |
| `request_id`, `frame_id`, `trace_id` | One fresh value per request or frame: unbounded cardinality as an exported span attribute. |

Matched forms: `key = expr`, `key = %expr`, `key = ?expr`, `"key" = expr`,
and the shorthand `key,`. Dotted keys match on the last segment. In an event
macro everything after the first string literal is the message and is not
inspected; in a span macro that string is the span name and fields follow it.

A value rule backs the key list: the camera URL can come back under another key
(`rtsp_url = %cam.ingest.url`) or as a message argument
(`info!("connecting to {}", cam.ingest.url)`). Inside the same macro forms, any
argument whose expression contains `ingest.url` is flagged under the key
`ingest.url` (field values, message format arguments, and nested non-tracing
macros such as `format!`; text in string literals and comments is ignored).

Limits: the gate matches names and that one value shape, not secrets. Another
secret logged under a key that is not in the table above is not caught, nor is
a credential bound to a local and logged as `x = %local`. Suffix matching was
rejected on purpose, because it would flag `profile_token` (an ONVIF profile
name). Also not covered: `Span::record("key", ..)` and the implicit argument
capture of a bare `#[instrument]`.

Prints `file:line:key` for each violation and exits non-zero. To allow a
site, put a comment containing `allow: span-cardinality <key>` and a specific
reason on the violation's line or the line directly above it. The marker must
name the key it excuses (`ingest.url` for the value rule); a bare marker, or a
marker for a different key, suppresses nothing:

```rust
// allow: span-cardinality trace_id (root frame.lifecycle span only)
trace_id = %trace_id,
```

### CI integration

```yaml
- name: check span cardinality
  run: cargo xtask check-span-cardinality
```

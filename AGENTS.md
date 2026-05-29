# AGENTS.md — Guidance for Coding Agents

> Read this before editing any file in this repository.

## What this repo is

The **edge engine** for Nexus Edge AI: a Rust workspace that runs on-premises camera
appliances (T10 / T24 / T36 / T36-S tiers). It owns the GStreamer pipeline, ONNX-Runtime
inference, multi-object tracking, rules evaluation, motion-clip recording, the local
admin/UI server, and the WSS tunnel client to the cloud. The engine is **functional in
total isolation from any cloud** — the cloud control plane is an optional companion.

The companion cloud-side control plane lives in
[nexus-cloud-console](../nexus-cloud-console). The two repos communicate through exactly
one contract: the wire protocol vendored under
[crates/nexus-cloud-protocol](crates/nexus-cloud-protocol) and
[crates/nexus-cloud-client](crates/nexus-cloud-client).

This repo keeps only **install + dev** docs:

- [docs/INSTALL.md](docs/INSTALL.md) — bring-up on each hardware tier
- [docs/HARDWARE_TIERS.md](docs/HARDWARE_TIERS.md) — tier selection matrix
- [docs/DEV_NOTES.md](docs/DEV_NOTES.md) — developer workflow, ORT setup, model gen

All architecture, pipeline design, milestone plans (M2/M3/M6/M7/M_ADMIN/M_OTA), the
business plan, comparison study, and roadmap live in
[../nexus-cloud-console/docs/edge-core/](../nexus-cloud-console/docs/edge-core/) and
[../nexus-cloud-console/docs/product/](../nexus-cloud-console/docs/product/), with the
top-level index at [../nexus-cloud-console/docs/README.md](../nexus-cloud-console/docs/README.md).
The wedge plan that drives the next three phases of work is
[../nexus-cloud-console/docs/product/WEDGE_PLAN.md](../nexus-cloud-console/docs/product/WEDGE_PLAN.md).

## Hard rules

1. **License discipline (engine is AGPL-3.0-or-later).** This repo's [LICENSE](LICENSE) is
   **AGPL-3.0-or-later**, declared in workspace `Cargo.toml`. Implications:
   - Any new top-level Cargo dep MUST be license-compatible with AGPL-3.0-or-later.
     `cargo deny check licenses` enforces an allowlist (Apache-2.0, MIT, BSD-2/3-Clause,
     ISC, MPL-2.0, Unicode-DFS-2016, AGPL-3.0, GPL-3.0). Proprietary or unspecified
     licenses are rejected.
   - **No proprietary Azure SDKs** — this is the paired half of cloud
     [REPO_BOUNDARY R2](../nexus-cloud-console/docs/REPO_BOUNDARY.md#r2-the-core-repo-must-not-import-any-azure-sdk).
     All Azure I/O (Blob PUT, Service Bus, Key Vault) happens through cloud-side services
     reached via the wire protocol. The edge negotiates SAS URLs and PUTs blobs with
     `reqwest` only — never with `azure_storage_blobs` or any other azure-* crate.
   - **ONNX weights are data, not linked code.** Models loaded by `ort` from
     [models/](models/) at runtime do NOT trigger AGPL copyleft on the weights themselves.
     This separation is what lets us ship third-party permissively-licensed weights
     (DINOv2-S Apache-2.0, OSNet MIT, YOLO* GPL/AGPL upstream) under the engine's AGPL.
2. **Model-license discipline (`xtask check-models`).** Every file referenced in
   [models/models-manifest.json](models/models-manifest.json) MUST declare two fields:
   - `license` — resolves to an allowlist `Apache-2.0`, `MIT`, `BSD-3-Clause`,
     `Apache-2.0 WITH LLVM-exception`. Build fails on `non-commercial`, `research-only`,
     `unknown`, or any other value.
   - `weights_dataset_license` — the license of the training dataset (e.g.
     `LVIS:CC-BY-4.0`, `COCO:CC-BY-4.0`, `LAION-5B:CC-BY-4.0`, `DigiFace-1M:research`).
     Datasets tagged `research`-only on the dataset side disqualify the weights from
     shipping, even if the model code itself is permissively licensed.

   **HARD product invariant — no face-specific extractor at the edge in v1.** The
   following model names (case-insensitive substring match) fail the check unconditionally:
   `AdaFace`, `ArcFace`, `InsightFace`, `Buffalo` (the InsightFace bundle), `FaceNet`,
   `SphereFace`, `CosFace`, `MagFace`. Rationale: (a) MS1MV2 / MS-Celeb-1M dataset
   retractions taint pretrained weights; (b) InsightFace's 2023 non-commercial relicense;
   (c) face recognition undermines the cloud's pseudonymous-by-default identity vault
   (see [WEDGE_PLAN.md](../nexus-cloud-console/docs/product/WEDGE_PLAN.md)). Body +
   clothing appearance is the v1 substrate (DINOv2-S default, OSNet-x1.0 opt-in).
3. **Repo boundary is sacred.** This repo MUST NOT import any cloud-side crate or Azure
   SDK. The cloud repo MUST NOT depend on this one. The only sanctioned cross-repo
   artifact is the generated Rust view of the wire schema vendored into
   [crates/nexus-cloud-protocol/src/v1.rs](crates/nexus-cloud-protocol/src/v1.rs)
   alongside a SHA-256 checksum (`v1.CHECKSUM`) that CI verifies against the cloud-side
   source of truth at
   [nexus-cloud-console/proto/v1.json](../nexus-cloud-console/proto/v1.json). The edge
   itself does NOT carry a copy of `proto/v1.json` — only the generated bindings. See
   [REPO_BOUNDARY R1–R3 in the cloud repo](../nexus-cloud-console/docs/REPO_BOUNDARY.md).
4. **Wire protocol version pinned to the cloud's `v`.** The engine speaks the version
   declared in the generated `crates/nexus-cloud-protocol/src/v1.rs`. Breaking changes
   happen in the cloud repo and propagate into this one via
   `cargo xtask sync-cloud-protocol --core <path>` (run from the cloud repo, which writes
   the regenerated file + a fresh `v1.CHECKSUM` into this repo). Never hand-edit the
   vendored copy. See
   [WIRE_PROTOCOL.md](../nexus-cloud-console/docs/WIRE_PROTOCOL.md).
5. **Fail-open locally.** The engine MUST continue to detect, record, evaluate rules,
   and serve its local admin/UI without any cloud connectivity (see
   [REPO_BOUNDARY R6](../nexus-cloud-console/docs/REPO_BOUNDARY.md#r6-edges-fail-open-locally-when-the-cloud-is-gone)).
   Any new feature that requires cloud reachability MUST gracefully degrade to a local-only
   mode, never block the pipeline.
6. **No camera credentials over the tunnel.** RTSP URLs, ONVIF secrets, and any per-camera
   credential MUST stay edge-resident. Camera creation that arrives from the cloud as an
   `rpc_call` is treated as opaque pass-through to the local admin API; the cloud never
   sees the secret. Paired with [REPO_BOUNDARY R5b](../nexus-cloud-console/docs/REPO_BOUNDARY.md#r5b-camera-credentials-never-cross-the-tunnel-into-the-cloud).
7. **Privacy invariants for the identity / re-ID pipeline (Wedge Phase 4–5).**
   - The future `crates/nexus-reid` extractor produces **appearance embeddings only**
     (DINOv2-S default, OSNet-x1.0 opt-in). It MUST NOT produce face-recognition
     embeddings. Code review and `xtask check-models` enforce model selection at build.
   - Embeddings travel to the cloud as `entity_sighting` envelopes (additive on wire `v=1`
     — see [WIRE_PROTOCOL.md §4](../nexus-cloud-console/docs/WIRE_PROTOCOL.md#4-message-catalog)).
     The edge tags every sighting with a per-core opaque `entity_local_id`; cloud
     assigns the global identity via its linker. The edge MUST NOT call any
     identity-resolution API itself.
   - The local SQLite store MUST NOT persist a `name`, `email`, `phone`, or any other
     personal identifier alongside `entity_local_id`. Operator-supplied labels (when the
     M6 admin surface adds them) live in a separate operator-only table that never
     replicates to the cloud.
8. **Capability split: `nexus-engine` and `nexus-updater` never share authority.** The
   engine does NOT have access to Docker / systemd-write paths; the OTA updater does NOT
   have access to camera streams or the local admin DB. See
   [REPO_BOUNDARY R8](../nexus-cloud-console/docs/REPO_BOUNDARY.md#r8-capability-split-on-the-edge-nexus-engine-and-nexus-updater-never-share-authority).

## Conventions

- **Rust workspace pinned to `rust-toolchain.toml`** (kept in sync with the cloud repo's
  toolchain so codegen produces identical artifacts).
- **Crate naming:** `nexus-<domain>` (e.g. `nexus-engine`, `nexus-pipeline`,
  `nexus-inference`, `nexus-tracker`, `nexus-rules`, `nexus-sinks`, `nexus-storage`,
  `nexus-store`, `nexus-cloud-client`, future `nexus-reid`). Each crate has a single
  responsibility; cross-crate APIs land in `nexus-types` or `nexus-bus`.
- **Features gate optional hardware.** GStreamer (`gstreamer`), ONNX-Runtime EPs
  (`ep-cpu`, `ep-coreml`, `ep-cuda`, `ep-openvino`, `ep-tensorrt`), WebRTC
  (`gstreamer-webrtc`), test injection (`test-injection`). NEVER add a feature gate via
  `cfg(debug_assertions)` for anything testing-related — use an explicit Cargo feature.
- **Frame contract is per-camera:** the supervisor (analysis) frame is RGB,
  16:9, derived from the resolved detector input width (640→640×360, 960→960×540,
  1280→1280×720). See `supervisor_frame_for(detector_width)` in
  [crates/nexus-pipeline/src/source.rs](crates/nexus-pipeline/src/source.rs).
  Detector / tracker / re-ID all share one camera's resolution. Clip recording is
  a separate passthrough chain at native camera resolution; bbox coords need
  scaling when overlaying on the MP4 — read per-clip `frame_width`/`frame_height`
  off the tracks API rather than hardcoding any value.
- **UI is `ui/` (Vite + TS + vanilla `h()` helper).** Per-tab modules live in
  `ui/src/ui/`; new tabs register in `ui/src/main.ts` `TABS` array. Forbidden:
  `style: "string"` props (use object); arbitrary DOM-property assignment for getter-only
  attributes like `list` / `form` (use `setAttribute`).

## Workflow

1. Pick a step from the wedge plan or a milestone doc in the cloud repo's docs index
   (linked above). Cross-repo work that lands in both repos in the same PR pair is
   common — open companion PRs.
2. Branch + PR per logical change. Title: `[<crate>] <verb> <object>` for engine-only;
   `[Phase N · Step M] <verb> <object>` for wedge work that maps to a phase number.
3. CI gates that must be green: `cargo fmt --check`, `cargo clippy --workspace
   --all-targets -- -D warnings`, `cargo test --workspace`, `cargo deny check`,
   `cargo xtask check-models`. The GStreamer + ORT integration jobs run on Linux.
4. macOS-local clippy does NOT catch every Linux-only clippy issue (`#[cfg(target_os
   = "linux")]` gates, `nix` integer width). If your change touches a Linux-gated
   block, expect at least one CI round-trip.

## Out of scope (do not propose without discussion)

- Face-recognition models at the edge in v1 (hard product invariant — see Rule 2).
- Any direct Azure SDK dependency (use the cloud tunnel instead).
- Any feature that requires permanent cloud connectivity (must degrade to local).
- New non-trivial Rust dependencies without a license + binary-size justification in
  the PR description.
- Persisting personal identifiers in the local SQLite store outside the M6 operator
  labels table.
- Bypassing the GStreamer pipeline contract (e.g. introducing a parallel frame source
  that doesn't honour the per-camera supervisor frame).

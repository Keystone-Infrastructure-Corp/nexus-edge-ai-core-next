# AGENTS.md — Guidance for Coding Agents

**Preferred model:** Claude Opus 5 when available, unless the host or operator picks another.

## What this repo is

The **edge engine** for Nexus Edge AI: a Rust workspace for on-premises camera appliances
(GStreamer pipeline, ONNX-Runtime inference, tracking, rules, clip recording, local
admin/UI, WSS tunnel client). It works **without any cloud**. The cloud control plane is
the sibling repo `nexus-cloud-console`; the only contract is the wire protocol in
[nexus-cloud-protocol](crates/nexus-cloud-protocol) and
[nexus-cloud-client](crates/nexus-cloud-client).

Edge docs: [INSTALL](docs/INSTALL.md), [HARDWARE_MATRIX](docs/HARDWARE_MATRIX.md),
[DEV_NOTES](docs/DEV_NOTES.md). Architecture, milestones and the wedge plan live in the
cloud repo's `docs/edge-core/` and `docs/product/`.

## Setup

Clone `nexus-home` beside this repo (`../nexus-home`), then run
`../nexus-home/bin/nexus-setup` once.

## Hard rules

`R1`–`R8` are rules in the cloud repo's `docs/REPO_BOUNDARY.md`.

1. **License: AGPL-3.0-or-later** ([LICENSE](LICENSE), workspace `Cargo.toml`). New
   top-level deps must be AGPL-compatible: review enforces Apache-2.0, MIT,
   BSD-2/3-Clause, ISC, MPL-2.0, Unicode-DFS-2016, AGPL-3.0, GPL-3.0 and rejects
   proprietary or unspecified licenses. There is no `cargo deny` gate (no `deny.toml`).
   - **No Azure SDKs** (`azure_storage_blobs` or any azure-\* crate; R2). Azure I/O goes
     through cloud services over the wire; blobs are PUT to SAS URLs with `reqwest` only.
   - ONNX weights loaded by `ort` from [models/](models/) at runtime are data, not linked
     code, so AGPL copyleft does not reach them.
2. **Model licenses (`cargo xtask check-models`).** Every file in
   [models-manifest.json](models/models-manifest.json) declares `license` (exactly one of
   `Apache-2.0`, `MIT`, `BSD-3-Clause`, `Apache-2.0 WITH LLVM-exception`, `AGPL-3.0`,
   `GPL-3.0`) and `weights_dataset_license` (e.g. `COCO:CC-BY-4.0`). A dataset tagged
   `research`, `academic`, `non-commercial` or another denylisted token is always an
   error. A weights file in `--models-dir` that no manifest `path` names fails too.
   **No face-recognition model at the edge in v1 (hard product invariant).** Names
   containing (any case) `AdaFace`, `ArcFace`, `InsightFace`, `Buffalo`, `FaceNet`,
   `SphereFace`, `CosFace` or `MagFace` fail unconditionally (MS-Celeb-1M retractions,
   InsightFace's non-commercial relicense, the cloud's pseudonymous identity vault). Body
   and clothing appearance (DINOv2-S default, OSNet-x1.0 opt-in) is the substrate.
3. **Repo boundary is sacred (R1–R3).** No cloud-side crate here; the cloud never
   depends on this repo. The only cross-repo artifact is the generated
   [v1.rs](crates/nexus-cloud-protocol/src/v1.rs) plus `v1.CHECKSUM` of the cloud's
   `proto/v1.json` (not carried here). Only the cloud's `check-wire-parity` checks it: it
   reads our `main` and fails when the cloud accepts an edge message or bus topic that
   our mirror or non-test source lacks.
4. **Wire version follows the cloud's `v`.** Breaking changes start in the cloud and
   arrive via `cargo xtask sync-cloud-protocol --core <path>`, run from the cloud repo.
   Never hand-edit the vendored copy. See the cloud's `docs/WIRE_PROTOCOL.md`.
5. **Fail-open locally (R6).** Detection, recording, rules and local admin/UI work
   without the cloud; cloud features degrade to local-only and never block the pipeline.
6. **No camera credentials over the tunnel (R5b).** RTSP URLs, ONVIF secrets and
   per-camera credentials stay on the edge. A cloud `rpc_call` that creates a camera is
   opaque pass-through to the local admin API.
7. **Re-ID privacy (Wedge Phases 4–5).** The future `crates/nexus-reid` emits appearance
   embeddings only, never face embeddings. Sightings go up as `entity_sighting` envelopes
   tagged with a per-core opaque `entity_local_id`; the cloud assigns identity, and the
   edge never calls an identity-resolution API. Local SQLite never stores `name`,
   `email`, `phone` or any personal identifier with `entity_local_id`; M6 operator labels
   live in a separate table that never replicates.
8. **One `nexus-engine` process; privileged work is sudoers-gated (R8).** The engine runs
   as the unprivileged `nexus` user under `nexus-engine.service`: no Docker, no sidecar
   updater, no shared socket. OTA privileged work goes only through the root-owned
   applier `/usr/local/sbin/nexus-apply-release`, the single argv-independent grant in
   [nexus-update](deploy/sudoers.d/nexus-update). Never add per-command sudoers rules or
   general `apt`, `tar`, `ln`, `systemctl` or `rm` power. See
   [OTA update handler](docs/DEV_NOTES.md#ota-update-handler-phase-9).

## Code Review Rules

Apply `REVIEW.md`. Any break of hard rules 1–8 is at least P1: check each changed
dependency, model, wire file, cloud-dependent path, credential or identifier store, and
privileged command against them. Also flag test gates on `cfg(debug_assertions)`.

## Coding principles

1. **Think first.** State assumptions; ask when the answer changes the design
   (`implement-loop` records it and continues).
2. **Simplicity.** Nothing speculative; on an appliance every abstraction costs size and
   startup.
3. **Surgical changes.** Touch only what the task needs, in the surrounding style.
   Mention unrelated dead code (above all under `#[cfg(...)]` gates), don't delete it.
4. **Goal-driven.** Plan as `step → verify:` pairs that name a real gate from Commands.

### Debugging and review

Follow `systematic-debugging`, `requesting-code-review` and `receiving-code-review`.
These bind even when no skill loads:

- **No fix without a root cause.** Reproduce first. Instrument each stage boundary once
  (camera → GStreamer → inference → tracker → rules → sinks, or engine → tunnel → cloud)
  to find where it breaks; write the failing test before the fix. **Three failed fixes
  means the design is wrong.** Once root-caused, look for siblings.
- **Review is not self-review.** A fresh reviewer subagent gets the base/head SHAs and
  requirements, never this session's history. Verify each finding, push back with
  reasons, clarify every unclear item first. No performative agreement.

Context budget: search before opening whole files, read definitions before consumers,
and don't load large generated files or re-read unchanged ones.

## Commands

The local admin console is the npm project [ui/](ui/). It uses npm and
`package-lock.json`, never pnpm (CI runs `npm ci` on Node 22).

- Rust: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace`, `cargo xtask check-models`, `cargo xtask
  check-span-cardinality`, and `cargo test -p nexus-types --features ts`, after which
  `ui/src/api/types/` must show no diff.
- UI, from `ui/`: `npm ci`, `npm run dev`, `npm test` (Vitest), `npm run lint`, `npm run
  typecheck`, `npm run build`, `npm run e2e` (Playwright; `npm run e2e:install` once).
  E2E serves `ui/dist` from `target/debug/nexus-engine`: first `npm run build` and, from
  the root, `cargo build -p nexus-engine --features test-injection`.

**CI gates:** all of the above except `dev`, plus `cargo check`. Clippy uses
`[workspace.lints.clippy]` in the root `Cargo.toml` (`clippy.toml` allows `unwrap`/`expect`/`panic`/prints in tests); `-D warnings` fails warn-level lints.
`system-libs` (GStreamer + ORT on Linux, `models-v6` smoke tests) runs on pushes to
`main` and on PRs labelled `system-libs`.

**Linux-gated code:** macOS clippy misses `#[cfg(target_os = "linux")]` blocks and `nix`
integer widths. If you touch them, run CI's clippy in a Linux container (e.g.
`rust:1-slim-bookworm` fed a `git archive`) before pushing.

## Conventions

- `rust-toolchain.toml` matches the cloud repo's so codegen is identical.
- Crates are `nexus-<domain>`, one responsibility each; cross-crate APIs go in
  `nexus-types` or `nexus-bus`.
- Cargo features gate optional hardware (`gstreamer`, `ep-*`, `gstreamer-webrtc`) and
  `test-injection`. NEVER gate testing behaviour on `cfg(debug_assertions)`.
- **Per-camera frame contract:** one RGB 16:9 supervisor frame on the stride-32 ladder
  (512×288 / 1024×576 / 1536×864) feeds detector, tracker and re-ID. Models are native
  16:9: no stretch, no letterbox. Clips record at native resolution, so scale bboxes by
  each clip's `frame_width`/`frame_height` from the tracks API; never hard-code. See
  `supervisor_frame_for` in [source.rs](crates/nexus-pipeline/src/source.rs) and
  [yolo.rs](crates/nexus-inference/src/yolo.rs).
- **UI** (React, Tailwind, shadcn/ui): TanStack Router in
  `ui/src/router.tsx`, one page per route in `ui/src/pages/`, typed clients in
  `ui/src/api/`. Server state uses TanStack Query, never hand-rolled `useEffect` +
  `fetch`. Forms `react-hook-form` + `zod`, toasts `sonner`, icons `lucide-react`.
  Playwright (`ui/e2e/`) stays at `workers: 1`: admin settings are a global singleton.

## Workflow

1. **Rebase before committing:** `git fetch && git rebase origin/main`; never a merge
   `git pull`. `implement-loop` units work from a pinned SHA and never rebase or merge.
2. Pick a step from the wedge plan or a milestone doc; cross-repo work gets companion PRs.
3. One PR per logical change, titled `[<crate>] <verb> <object>`, or
   `[Phase N · Step M] <verb> <object>` for wedge work.
4. **Vault:** none here. ADR-, SPEC- or BUG-worthy changes get a record in the cloud
   repo's `.obsidian-vault/` in the same PR pair; follow `nexus-vault`.

## Field notes

Before debugging environment, CI or test trouble, scan `.agents/memory/INDEX.md`. Record
a new trap there in the same PR.

## Terminal output

Never stream unbounded output into chat; follow `nexus-terminal-output`.
`scripts/run-capped.sh` defaults to a shared log, so set `LOG=` per checkout:
`LOG=$(mktemp -t nexus-cmd) scripts/run-capped.sh cargo test --workspace`. SSH to edge
boxes: `ssh-remote-ops`.

## Out of scope (do not propose without discussion)

Anything against the hard rules, a frame source that bypasses the supervisor frame, and
new non-trivial deps without a license and binary-size justification in the PR.

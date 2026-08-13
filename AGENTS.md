# AGENTS.md — Guidance for Coding Agents

> Read this before editing any file in this repository.

**Preferred model:** Use Claude Opus 5 for coding-agent work when it is available.
Model selection is configured by the agent host; this preference does not override an
unavailable model or a model explicitly selected by the operator.

## What this repo is

The **edge engine** for Nexus Edge AI: a Rust workspace that runs on-premises camera
appliances (Intel iGPU, Intel NPU, Hailo, AMD, and NVIDIA hardware profiles). It owns the GStreamer pipeline, ONNX-Runtime
inference, multi-object tracking, rules evaluation, motion-clip recording, the local
admin/UI server, and the WSS tunnel client to the cloud. The engine is **functional in
total isolation from any cloud** — the cloud control plane is an optional companion.

The companion cloud-side control plane lives in
[nexus-cloud-console](../nexus-cloud-console). The two repos communicate through exactly
one contract: the wire protocol vendored under
[crates/nexus-cloud-protocol](crates/nexus-cloud-protocol) and
[crates/nexus-cloud-client](crates/nexus-cloud-client).

This repo keeps only **install + dev** docs:

- [docs/INSTALL.md](docs/INSTALL.md) — bring-up on each hardware profile
- [docs/HARDWARE_MATRIX.md](docs/HARDWARE_MATRIX.md) — vendor/capability matrix
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
8. **Edge runs as a single `nexus-engine` process; privileged work is sudoers-gated.**
   The engine runs as the unprivileged `nexus` system user under `nexus-engine.service`
   (systemd). There is no Docker on the edge, no sidecar updater, no shared socket. The
   small amount of privileged work an OTA needs (extract into
   `/opt/nexus/releases/<version>/`, install that release's declared apt runtime
   deps + journald cap, ensure the `nexus` service user's `systemd-journal` group
   membership (so the engine can read its own journal for the diagnostics bundle),
   flip `/opt/nexus/current`, run `systemctl restart
   nexus-engine`, prune stale releases) is performed by a SINGLE pinned, root-owned
   applier `/usr/local/sbin/nexus-apply-release` (modes `apply`/`reflip`/`prune`,
   delegating deps to `/usr/local/sbin/nexus-apply-deps`). The
   `/etc/sudoers.d/nexus-update` entry in [deploy/sudoers.d/](deploy/sudoers.d/)
   grants exactly ONE command — a stable, argv-independent wildcard on that applier —
   so the engine's privileged behaviour can change without ever editing sudoers again
   (the old per-argv rules coupled sudoers byte-for-byte to the engine's `Command`
   calls, and drift there could brick an OTA). Both wrappers live outside the
   OTA-writable tree and enforce their own arg/package allowlists, so the grant never
   confers general `apt`, `tar`, `ln`, `systemctl`, or `rm`. See
   [REPO_BOUNDARY R8](../nexus-cloud-console/docs/REPO_BOUNDARY.md#r8-edge-runs-as-a-single-nexus-engine-process-privileged-work-is-sudoers-gated).

## Coding principles

Adapted from the [Karpathy-inspired guidelines](https://github.com/multica-ai/andrej-karpathy-skills).
They bias toward caution over speed — for a typo or an obvious one-liner, use judgment.

1. **Think before coding.** Don't assume, don't hide confusion, surface tradeoffs. State
   assumptions explicitly. Where a requirement has more than one reading, present the
   readings instead of silently picking one. If a simpler approach exists, say so and push
   back. If something is genuinely unclear and the answer changes the design, stop and ask
   — the exception is an autonomous run under the `implement-loop` skill (in the cloud
   repo), which records the assumption and keeps going rather than yielding.
2. **Simplicity first.** The minimum code that solves the problem, nothing speculative. No
   features beyond what was asked, no abstraction for single-use code, no configurability
   nobody requested, no error handling for states that cannot occur. If 200 lines could be
   50, rewrite it. The test: would a senior engineer call this overcomplicated? This
   compounds on an edge binary — every speculative abstraction is size and startup cost on
   an appliance.
3. **Surgical changes.** Touch only what you must; clean up only your own mess. Don't
   "improve" adjacent code, comments, or formatting; don't refactor what isn't broken;
   match the surrounding style even where you'd write it differently. Unrelated dead code
   gets mentioned, not deleted — particularly under `#[cfg(...)]` hardware gates you cannot
   compile locally. Imports and bindings that *your* change orphaned do get removed. The
   test: every changed line traces directly to the request.
4. **Goal-driven execution.** Define success criteria, then loop until they verify.
   Turn imperative tasks into verifiable goals — "fix the bug" becomes "write a test that
   reproduces it, then make it pass". For multi-step work state the plan as `step → verify:`
   pairs, where each verify names a real gate (`cargo test --workspace`, `cargo clippy
   --workspace --all-targets -- -D warnings`, `cargo xtask check-models`, the `nexus-types`
   TS-binding drift check) rather than "make it work". Remember macOS-local clippy does not
   cover the Linux-gated paths — a green local run is a weaker verify than CI.

## Context Budget

- Search for the relevant symbol or route before opening a full file
- Read the file that defines something before files that only consume it
- Don't re-read files already in context unless they may have changed
- Avoid loading large generated or schema files in full unless the task needs it

## Commands

The repo root is a Cargo workspace; the local admin console is a separate npm
project under [ui/](ui/). **`ui` uses npm and `package-lock.json`, not pnpm.**
CI runs `npm ci` on Node 22 (`.github/workflows/ci.yml`); running `pnpm install`
there replaces `node_modules` with a pnpm-resolved tree that does not match the
lockfile.

Rust, from the repo root:

- Format: `cargo fmt --all -- --check`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Test: `cargo test --workspace`
- Model licences: `cargo xtask check-models`
- Generated TS bindings: `cargo test -p nexus-types --features ts`, then confirm
  `ui/src/api/types/` has no diff — CI fails on drift

UI, from `ui/`:

- Install: `npm ci`
- Dev: `npm run dev`
- Unit tests: `npm test` (Vitest)
- Lint: `npm run lint`
- Typecheck: `npm run typecheck`
- Build: `npm run build`
- End-to-end: `npm run e2e` (Playwright; `npm run e2e:install` once)

Note that CI's `ui` job runs only `typecheck` and `build` — `lint`, `npm test`,
and Playwright are local gates, so a lint or unit-test regression will not be
caught for you.

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
  16:9, on the native ladder (512×288 / 1024×576 / 1536×864 — exact
  16:9 ∩ stride-32, W=512k / H=288k). By default its width equals the resolved
  detector input width; a camera may analyse at a *larger* rung via
  `CameraBehavior::supervisor_width` (decoupled from the model input) so the tile
  grid divides the frame into exact model-sized tiles. See
  `supervisor_frame_for(width)` in
  [crates/nexus-pipeline/src/source.rs](crates/nexus-pipeline/src/source.rs).
  Detector / tracker / re-ID all share one camera's resolution. Clip recording is
  a separate passthrough chain at native camera resolution; bbox coords need
  scaling when overlaying on the MP4 — read per-clip `frame_width`/`frame_height`
  off the tracks API rather than hardcoding any value.
- **The shipped detector models are native 16:9, matching the supervisor frame
  — no stretch, no letterbox.** Preprocessing is a plain bilinear resize of the
  16:9 supervisor frame to the model's (`input_w` × `input_h`) 16:9 input, with
  box coords scaled back by the per-axis `image_dim / input_dim` factors (see the
  module docs in
  [crates/nexus-inference/src/yolo.rs](crates/nexus-inference/src/yolo.rs)). The
  exact-16:9 ∩ stride-32 ladder means:
  - No invented rows: the model input is 16:9, so the whole tensor is real
    pixels (the old square inputs stretched a 640×360 frame into 640×640 — ~44%
    invented rows — that is gone).
  - Geometry is undistorted: real-world shape / aspect reasoning from model-space
    coords is now valid.
  - Tiles divide the frame exactly on the ladder — `grid_cells(1536, 864, G3x3)`
    yields nine pixel-identical 512×288 tiles (== the 512×288 model input), zero
    resampling.

  Note the distinct, genuinely-letterboxed step upstream: `videoscale
  add-borders=true` letterboxes a non-16:9 *camera* into the 16:9 supervisor
  frame. The native-16:9 model shapes and the supervisor-decoupling design are in
  [M_NATIVE_ASPECT.md](../nexus-cloud-console/docs/edge-core/M_NATIVE_ASPECT.md).
- **UI is `ui/` (Vite 5 + React 18 + TypeScript 5 + Tailwind 3).** Entry point is
  `ui/src/main.tsx`; routes are code-defined with TanStack Router in
  `ui/src/router.tsx`. Layout:
  - `ui/src/pages/` — one `.tsx` per route (`cameras.tsx`, `admin-server.tsx`, …)
  - `ui/src/components/ui/` — shadcn/ui primitives;
    `ui/src/components/` — shared widgets; `ui/src/components/layout/` — chrome
  - `ui/src/api/` — typed fetch clients, with wire types in `ui/src/api/types.ts`
  - `ui/src/lib/` — framework-free helpers; `ui/src/hooks/` — React hooks

  Server state goes through TanStack Query (`useQuery` / `useMutation`) — do not
  hand-roll `useEffect` + `fetch`. Forms use `react-hook-form` + `zod`; toasts use
  `sonner`; icons are `lucide-react`. Unit tests are Vitest + Testing Library;
  e2e is Playwright under `ui/e2e/`, pinned to `workers: 1` because admin settings
  are a global singleton and parallel specs race on them.

## Engineering vault (ADR / SPEC / BUG) — lives in the cloud repo

This repo has no local Obsidian vault. The engineering vault lives at
[nexus-cloud-console/.obsidian-vault/](../nexus-cloud-console/.obsidian-vault/), since
only that repo's root carries `.obsidian/`. Add or update a record **there**, not here, in
the same PR whenever your change in this repo is:

- **ADR-worthy** — an edge architectural decision with stated alternatives and
  consequences (e.g. the trait-pool fail-soft pattern, the native-aspect ladder).
- **SPEC-worthy** — a milestone (`M2`, `M3`, `M6`, `M7`, `M_ADMIN`, `M_OTA`, …) with a
  goal and acceptance criteria; one spec per milestone doc is the norm.
- **BUG-worthy** — a concrete defect with a stated symptom, root cause, and resolution.

Use the templates at
`../nexus-cloud-console/.obsidian-vault/templates/_template-{adr,spec,bug}.md`, continue
the existing `ADR-NNN`/`SPEC-NNN`/`BUG-NNN` numbering found in
`../nexus-cloud-console/.obsidian-vault/{decisions,specs,bugs}/`, and cite this repo's
docs with `[[docs/edge-core/<file>#Heading]]` wikilinks — those paths are relative to the
cloud repo, since `docs/edge-core/` physically lives there, not here. Verify heading text
is exact (grep `^#{1,6} ` in the source doc) and that every link resolves before
considering the change done. Same duplication-avoidance and judgment rules as the cloud
repo's AGENTS.md apply: don't spawn a near-duplicate record, don't convert routine
implementation notes, do cross-link ADR/SPEC/BUG records that relate to each other.

## Workflow

0. **Always rebase before committing.** Before staging any commit, run
   `git fetch && git rebase origin/main` (or `git pull --rebase origin main`). Never
   `git pull` (default merge) into a working branch — it creates noisy merge commits
   that the squash-merge model cannot collapse cleanly. If you have local work in
   progress, `git stash` first or use `git pull --rebase --autostash`. Resolve any
   rebase conflicts before continuing.
1. Pick a step from the wedge plan or a milestone doc in the cloud repo's docs index
   (linked above). Cross-repo work that lands in both repos in the same PR pair is
   common — open companion PRs.
2. Branch + PR per logical change. Title: `[<crate>] <verb> <object>` for engine-only;
   `[Phase N · Step M] <verb> <object>` for wedge work that maps to a phase number.
3. CI gates that must be green: `cargo fmt --check`, `cargo check` + `cargo clippy
   --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
   `cargo xtask check-models`, and the `nexus-types` TS-binding drift check. The
   GStreamer + ORT integration jobs run on Linux. There is **no `cargo deny`
   gate here** — this repo has no `deny.toml`; licence discipline (Rule 1) is
   enforced by review, unlike the cloud repo where `cargo deny check` is CI.
4. macOS-local clippy does NOT catch every Linux-only clippy issue (`#[cfg(target_os
   = "linux")]` gates, `nix` integer width). If your change touches a Linux-gated
   block, expect at least one CI round-trip.
5. If the change is decision/spec/bug-worthy, add or update the corresponding record in
   the cloud repo's engineering vault (see "Engineering vault" above) before considering
   it merged.

## Terminal output discipline (cost control)

Terminal output is the single largest driver of token spend in this workspace —
multi-hundred-kilobyte command dumps (full build logs, `journalctl` tails, remote
edge-box diagnostics) pasted into the chat transcript routinely dominate the context
budget. Keep command output small and deliberate:

1. **Never stream a full, unbounded command into chat.** Builds, test runs,
   `journalctl`, `cargo`, `npm`, package installs, and remote diagnostics MUST be
   capped. Redirect the firehose to a file and surface only the tail:

   ```bash
   <noisy-cmd> > /tmp/nexus-cmd.log 2>&1; rc=$?; tail -n 40 /tmp/nexus-cmd.log; exit $rc
   ```

   or use the helper [scripts/run-capped.sh](scripts/run-capped.sh):

   ```bash
   scripts/run-capped.sh cargo test --workspace
   ```

2. **Filter at the source.** Prefer `grep -E`, `rg`, `awk`, `tail -n`, `head -n`,
   `-q`/`--quiet`, `wc -l`, and `gh ... --json ... --jq` over dumping everything and
   reading it back in chat. Ask for the few lines you need, not the thousands you
   don't.

3. **Never use follow / watch modes inside an agent turn.** No `tail -f`,
   `journalctl -f`, `gh ... --watch` without a `| tail`, `cargo watch`, or a
   foreground dev server — they pin the terminal and stream unbounded output. Poll
   once and stop, or background the process.

4. **For remote (SSH) edge-box work, cap on the box, not in chat.** Redirect remote
   output to a log on the remote host (`bash /tmp/do.sh > /tmp/do.log 2>&1`) and
   `tail` it in a separate follow-up call — never let a remote command's full stdout
   cross the tunnel into the transcript. (Complements the SSH rules below.)

5. **One chat per task.** Start a fresh chat when you switch to an unrelated task so
   a long transcript doesn't ride along as dead context into the next piece of work.

## SSH to operator boxes (sudo, one prompt per turn)

When an agent needs to run *more than one* command on a remote host in the same
turn (e.g. EQR7 at `192.168.1.183` for install + verify), it MUST set up one
persistent SSH connection and batch sudo work into one remote script. The
operator should type the SSH password (or unlock the key) at most **once per
agent turn**, and the sudo password at most **once per agent turn** — not
once per remote command.

**THE ZEROTH RULE — ONE SSH SESSION PER HOST. PERIOD.** Open one interactive
ssh shell to the box (or reuse the operator's existing pane if they already
have one open) and ride that single connection for EVERY subsequent remote
command via `send_to_terminal`. Do NOT spawn a fresh `ssh user@host '<cmd>'`
one-shot for each command — not for recon, not for verification, not for
"just a quick check", not ever. Each one-shot opens a new session, fights for
the operator's terminal, and can kill or race their interactive shell. The
ControlMaster socket is NOT a license to open parallel `ssh host '…'` calls;
it just suppresses the password prompt on each one — they are still separate
sessions. Rule of thumb: if you've typed `ssh user@host '…'` twice in the
same turn, you're doing it wrong — go back to the existing shell.

**THE HARD RULE: EVERY `ssh` AND `scp` INVOCATION MUST CARRY
`-o "ControlPath=$HOME/.ssh/cm/%r@%h:%p"`.** No exceptions. `scp` without
`-o ControlPath` opens a fresh TCP+SSH session and prompts for the password
again, even when a ControlMaster socket is alive. Same for `rsync -e ssh`,
`git push` over ssh, `ssh-copy-id`, etc. — they all need the option (or an
`~/.ssh/config` Host stanza, see step 0 below).

0. **(One-time per workstation) make ControlMaster the default for the box.**
   Adding a stanza to `~/.ssh/config` removes the need to remember `-o
   ControlPath=…` on every invocation — all `ssh`, `scp`, `rsync -e ssh`,
   `git@…` calls inherit it automatically. Agents SHOULD assume this is set
   up; if it isn't, propose it once and ask the operator to add it (the file
   is sensitive, do not edit it without confirmation):
   ```
   # ~/.ssh/config
   Host eqr7
       HostName 192.168.1.183
       User nexus-admin
       ControlMaster auto
       ControlPath ~/.ssh/cm/%r@%h:%p
       ControlPersist 2h
   ```
   With this in place, every `ssh eqr7 …` / `scp foo eqr7:bar` rides one
   socket automatically and the explicit `-o ControlPath` is redundant (but
   still harmless if used).

1. **Open / reuse one OpenSSH ControlMaster at the start of each turn.**
   Run this BEFORE any other remote command in the turn; if the master
   was opened in a prior turn the socket persists for the `ControlPersist`
   window and this call is a no-op (no password prompt):
   ```bash
   mkdir -p "$HOME/.ssh/cm"
   ssh -o ControlMaster=auto \
       -o ControlPath="$HOME/.ssh/cm/%r@%h:%p" \
       -o ControlPersist=2h \
       nexus-admin@192.168.1.183 true
   ```
   Every subsequent `ssh` / `scp` / `rsync -e ssh` MUST pass
   `-o ControlPath="$HOME/.ssh/cm/%r@%h:%p"` (or use the matching
   `~/.ssh/config` Host stanza from step 0) so it rides the existing socket.

2. **Batch sudo into ONE remote script** so the sudo password prompt fires
   exactly once per turn. Stage the script with `scp` + run it with `ssh -tt`
   so sudo has a real TTY — and BOTH calls carry `-o ControlPath`:
   ```bash
   cat >/tmp/do.sh <<'EOF'
   set -e
   # …commands that need root…
   EOF
   scp -o "ControlPath=$HOME/.ssh/cm/%r@%h:%p" \
       /tmp/do.sh nexus-admin@192.168.1.183:/tmp/do.sh
   ssh -tt -o "ControlPath=$HOME/.ssh/cm/%r@%h:%p" \
       nexus-admin@192.168.1.183 'sudo bash /tmp/do.sh'
   ```
   The `ssh <host> 'sudo bash -s' <<EOF` heredoc pattern **does not work** —
   the heredoc replaces stdin, so `ssh` can't allocate a PTY and sudo bails
   with "a terminal is required to read the password". Always go via
   `scp` + `ssh -tt <host> 'sudo bash /tmp/<script>.sh'`.
3. **Focus the terminal before issuing a sudo-bearing command** so the
   operator sees the password prompt without hunting for the pane. Call
   `run_vscode_command` with `workbench.action.terminal.focus`
   (`skipCheck=true`) immediately before the `ssh -tt … sudo …` invocation.
   Also: do **not** pipe the `ssh -tt … sudo …` call through `| tail -N`
   or any pager — `tail` buffers stdin until EOF, so the sudo password
   prompt is hidden and the operator sees an empty terminal. Run the
   ssh call unbuffered; if the remote output is long, redirect it to a
   file on the box (`bash /tmp/do.sh > /tmp/do.log 2>&1`) and `tail` the
   log from a separate follow-up `ssh` call after sudo finishes.
4. **Never route passwords / passphrases / API tokens through
   `vscode_askQuestions`** — those answers flow through the model. Have the
   operator type secrets directly into the focused terminal.
5. ControlMaster sockets occasionally go stale (network change, laptop sleep).
   If `ssh -O check` reports "Control socket connect: No such file or
   directory" or a command hangs, `rm -f "$HOME/.ssh/cm/<user>@<host>:<port>"`
   and re-establish per step 1.

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

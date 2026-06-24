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
   `/opt/nexus/releases/<version>/`, flip `/opt/nexus/current`, run
   `systemctl restart nexus-engine`) is gated through a single
   `/etc/sudoers.d/nexus-update` entry in [deploy/sudoers.d/](deploy/sudoers.d/) that
   whitelists only those exact commands. See
   [REPO_BOUNDARY R8](../nexus-cloud-console/docs/REPO_BOUNDARY.md#r8-edge-runs-as-a-single-nexus-engine-process-privileged-work-is-sudoers-gated).

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
3. CI gates that must be green: `cargo fmt --check`, `cargo clippy --workspace
   --all-targets -- -D warnings`, `cargo test --workspace`, `cargo deny check`,
   `cargo xtask check-models`. The GStreamer + ORT integration jobs run on Linux.
4. macOS-local clippy does NOT catch every Linux-only clippy issue (`#[cfg(target_os
   = "linux")]` gates, `nix` integer width). If your change touches a Linux-gated
   block, expect at least one CI round-trip.

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

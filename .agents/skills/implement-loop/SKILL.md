---
name: implement-loop
description: Edge extension of the shared Nexus implement-loop — edge worktree and target paths, pinned unit bases, the Nexus Edge Coding agent, cloud-owned engineering records, hardware evidence, E2E harness ownership, and the edge gates. Read the shared core first. Use to implement, finish, or harden an edge feature to spec without hand-holding.
---

# Implement Loop — Edge extension

This extends the shared implement-loop core at
`../nexus-home/.agents/skills/implement-loop/SKILL.md` (the Nexus Home Repo, beside this
repo). Read the core first, then this file; if the core is missing, stop and ask for it.
Where this file is stricter than the core, it wins for this repo.

Read `AGENTS.md` and `.github/workflows/ci.yml` before planning. Their hardware,
privacy, licence, wire, security, validation, accessibility, and error-handling rules
remain binding. Do not ask again for an action the user already authorized.

## Workspace

```bash
TOP=/tmp/nx-$SLUG-edge
TARGET=/tmp/nx-$SLUG-target-edge
```

- Base: `origin/main`. Every Rust worktree has a distinct `CARGO_TARGET_DIR`; a unit's is
  `/tmp/nx-<slug>-target-u-edge-<unit>`. Never share compiled artifacts between trees.
- Ledger: `/tmp/nx-$SLUG-ledger.md` for an edge-only task; a cross-repo task uses the cloud
  ledger. Record canonical path, branch, base, target directory, and owned processes.
- Refresh/rebase the integration tree onto `origin/main` before dispatch and before every
  top-level commit, per `AGENTS.md`, preserving local work; resolve conflicts and rerun
  affected gates if the base changes.
- Record each original dispatched base SHA in the ledger; it never changes for that unit
  even if origin/main or the integration tree advances. Units never rebase or merge;
  redispatch from a fresh pinned base when needed. When accepting a report, the ancestor
  check uses the ORIGINAL dispatched base from the ledger — never substitute an
  agent-reported replacement base — and every commit in
  `<original-dispatched-base>..<unit-head>` must be allowlisted.

## Agents and top-level-only files

- Strongest-tier implementation units go to `Nexus Edge Coding`
  (`.github/agents/nexus-edge-coding.agent.md`). It pins Claude Opus 5 in its frontmatter,
  so a per-dispatch `model` does not reach it — a cheap-tier unit goes to the default agent
  instead. Use strong judgment for design, security, wire/hardware work and review.
- Top-level only, never in an allowlist: generated wire bindings, generated TS types
  (`ui/src/api/types/`), lockfiles, and cloud vault records.

## Unit prompt lines

Add these to the core's unit prompt:

```text
   TARGET=/tmp/nx-<slug>-target-u-edge-<unit>  # append -r<n> for a re-dispatch
   Rust verifies run inside the worktree as `CARGO_TARGET_DIR="$TARGET" cargo <verify args>`.
   Read AGENTS.md. Keep the supplied base fixed: never rebase or merge, per AGENTS.md's
   unit exception. Cap terminal output.
   A fresh worktree has no node_modules: run `npm --prefix "$WT/ui" ci` (Node 22) before any
   npm verify. Install Playwright browsers once per machine with
   `npm --prefix "$WT/ui" run e2e:install` before its first E2E run.
   Edge UI E2E: `E2E_PORT=<port> npm --prefix "$WT/ui" run e2e`, following the E2E
   ownership rules in the edge extension (the top level serializes edge E2E).
   Edge UI unit tests: `npm --prefix "$WT/ui" run test`.
   Report also: skipped gates, assumptions, and open findings.
```

## Criteria details

1. **Spec/ADR conformance** — satisfy matching cloud vault records, acceptance criteria,
   and every applicable edge hard rule; cite requirements and source evidence.
2. **Mock/visual fidelity** — UI matches the approved mock in layout, behavior, loading,
   empty/error states, themes, and responsive viewports. Locate or extend the mock first;
   compare rendered implementation and mock at matching states.
3. **Regression coverage** — for bugs establish root cause and write a reproducer before
   fixing. Skipped hardware/model smoke tests and unavailable runtimes are unverified.
   Default features and macOS do not prove Linux, GStreamer, ONNX/EP, Hailo, or real
   hardware correctness.
5. **Engineering record** — edge has no local vault: find the cloud repository and use its
   own isolated worktree, rules, numbering, and templates under `.obsidian-vault/`. Never
   fabricate a vault in edge or edit the cloud primary checkout. Record-worthy edge work
   needs a paired cloud deliverable; update it in each behavior-changing pass and carry
   both SHAs.

## Gates

Edge repo gates, from its `$TOP`, with the tree's private `CARGO_TARGET_DIR` (use the exact
commands in `AGENTS.md` and `.github/workflows/ci.yml`):
`cargo fmt --all -- --check` · `cargo check --locked --workspace --all-targets` ·
`cargo clippy --locked --workspace --all-targets -- -D warnings` ·
`cargo test --locked --workspace --no-fail-fast` · `cargo xtask check-models` ·
`cargo xtask check-span-cardinality` ·
`cargo test --locked -p nexus-types --features ts` ·
`git diff --exit-code -- ui/src/api/types/` ·
`test -z "$(git ls-files --others -- ui/src/api/types/)"`.
Include CI's ORT session tuning/engine prewarm tests, linked Hailo lint, and release EP
feature lint; consult the workflow for exact feature sets. Linux-gated code needs Linux
verification. For changes touching GStreamer/ORT feature paths or Linux-only code, require
the label-gated `system-libs` CI job (add its PR label); otherwise report that gate as
unverified. It runs the `nexus-pipeline` `gstreamer`/`gstreamer-webrtc`,
`nexus-inference` `ort,ep-cpu`/`ort,ep-cuda`, `nexus-engine` `gstreamer`/`gstreamer-webrtc`,
and `nexus-reid` `ep-cpu` `cargo check --locked` variants from `.github/workflows/ci.yml`.
No cloud `cargo deny`, cloud RLS, migration, or Azure gates are invented for edge; licence
review still applies.
Edge UI: first `npm --prefix "$TOP/ui" ci` (Node 22), then
`npm --prefix "$TOP/ui" run lint` · `npm --prefix "$TOP/ui" run typecheck` ·
`npm --prefix "$TOP/ui" run test` · `npm --prefix "$TOP/ui" run build` ·
run `npm --prefix "$TOP/ui" run e2e:install` once per machine before first E2E, then
`E2E_PORT=<private-port> npm --prefix "$TOP/ui" run e2e` under the ownership rules below.

## E2E ownership

For E2E build `ui/dist` and `cargo build --locked -p nexus-engine --features test-injection`
from the same tree. The current harness hardcodes `$WT/target/debug/nexus-engine`; if
`$WT/target` is absent, a run-owned symlink to that tree's private target is acceptable.
Verify an existing path belongs to this tree before using it; never replace unknown
contents or use another tree's binary. Use a unique `E2E_PORT=<port>` and confirm the
test base URL and engine bind match. Edge E2E is a shared harness: serialize it across all
worktrees, because the harness uses the shared `/tmp/nexus-e2e-sidecar.json` even with
separate ports. Preserve this constraint until the harness itself supports isolated state.
Stop owned engines after tests and verify their ports are no longer listening.

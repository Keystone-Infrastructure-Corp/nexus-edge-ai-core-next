---
name: implement-loop
description: Edge extension of the Nexus implement-loop; use to implement, finish, or harden an edge feature to spec without hand-holding.
---

# Implement Loop — Edge extension

Entry point for edge work. Read `AGENTS.md` and `.github/workflows/ci.yml` before planning;
their hardware, privacy, licence, wire, security, validation, accessibility, and
error-handling rules bind, and you do not re-ask for an action the user already authorized.
Then read the shared core at `../nexus-home/.agents/skills/implement-loop-core/SKILL.md` (or
`$NEXUS_HOME/.agents/skills/implement-loop-core/SKILL.md`). With no nexus-home checkout, clone
it and run its `bin/nexus-setup`; do not run the loop from memory. Run
`nx-il check-core --config .agents/implement-loop.config` (`nx-il` is on PATH under the Claude
plugin; elsewhere `$NEXUS_HOME/bin/nx-il` when set, else `../nexus-home/bin/nx-il`). Where this file is stricter than the core, it
wins for this repo.

## Workspace

```bash
nx-il start . <slug>   # from the edge checkout: /tmp/nx-<slug>-edge, branch, ledger
```

`$TOP` below is the worktree path `nx-il start` prints (`/tmp/nx-<slug>-edge`); set `TOP=<that path>`
in every shell that runs a gate, since each command starts in a fresh shell.

- Base `origin/main`. Every Rust worktree has a distinct `CARGO_TARGET_DIR` (the `export`
  line it prints, `/tmp/nx-<slug>-target-edge`); a unit's is
  `/tmp/nx-<slug>-target-u-edge-<unit>`. Never share compiled artifacts between trees.
- Ledger: `$TOP/.implement-loop/<slug>.md` (gitignored) for an edge-only task; a cross-repo
  task uses the cloud ledger. Record canonical path, branch, base, target directory, and
  owned processes.
- Refresh/rebase the integration tree onto `origin/main` before dispatch and before every
  top-level commit, per `AGENTS.md`, preserving local work; resolve conflicts and rerun
  affected gates if the base changes. A PR follow-up or `@claude` run never rebases,
  per the core.
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
- Top-level only, never in an allowlist (enforced by `nx-il verify-report` from
  `top-level-only` in `.agents/implement-loop.config`): generated wire bindings, generated TS
  types (`ui/src/api/types/`), lockfiles, and cloud vault records.

## Unit prompt

Render with `nx-il unit-prompt --short edge --extra .agents/implement-loop.unit-lines ...`;
that file holds this repo's lines (target directory, base pinning, `npm ci`, E2E, report
fields). When verifying a report, pass the ORIGINAL dispatched base from the ledger as
`--base`, never one the agent reports.

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
Edge UI: first `(cd "$TOP/ui" && npm ci)` (Node 22), then
`npm --prefix "$TOP/ui" run lint` · `npm --prefix "$TOP/ui" run typecheck` ·
`npm --prefix "$TOP/ui" run test` · `npm --prefix "$TOP/ui" run build` ·
run `npm --prefix "$TOP/ui" run e2e:install` once per machine before first E2E, then
`E2E_PORT=<private-port> npm --prefix "$TOP/ui" run e2e` under the ownership rules in `e2e-ownership.md` beside this file.

---
name: implement-loop
description: Self-verifying edge implementation loop with isolated worktrees, independent review, actual regression gates, restrained changes, cloud-owned engineering records, and an explicit installed Ponytail handoff. Use to implement, finish, or harden an edge feature to spec without hand-holding.
---

# Implement Loop — Edge

Adapted from the companion cloud repository's implement-loop; this is the edge-local
workflow. Read `AGENTS.md` and `.github/workflows/ci.yml` before planning. Their hardware,
privacy, licence, wire, security, validation, accessibility, and error-handling rules
remain binding. Restate the task in one sentence and run the loop to completion.

Record ambiguity as `Ruling: <decision> — <why> — <cost if wrong>` and continue within
approved scope. Stop for destructive or irreversible operations, security-sensitive
actions, unauthorized external side effects, or requirements too vague to choose a
defensible path. Required design approvals remain required; reuse prior answers.
Run-owned cleanup below is authorized after its preservation checks. Do not ask again
for an action the user already authorized.

## Pass 0: own the workspace and context

Never edit the primary checkout. Inspect `git worktree list` and the run ledger before
creating anything; resume a matching owned worktree instead of duplicating it. For a new
run, fetch origin and create the top worktree from `origin/main`:

```bash
SLUG=<task-slug>
TOP=/tmp/nx-$SLUG-edge
TARGET=/tmp/nx-$SLUG-target-edge
git -C <edge-repo> fetch -q origin
git -C <edge-repo> worktree add -b feat/implement-loop-$SLUG "$TOP" origin/main
BASE=$(git -C "$TOP" rev-parse HEAD)
```

Record canonical path, branch, base, target directory, and owned processes in
`/tmp/nx-$SLUG-ledger.md`; edge does not ignore `.implement-loop/`. All repository
commands use `$TOP` explicitly. Every Rust worktree has a distinct `CARGO_TARGET_DIR`;
never share compiled artifacts between trees. Cross-repo work gets a separate isolated
top worktree and target per repo. Transfer work only as commits, never by copying files.

Before implementation, honor an explicit user mode, including `off`; default to `full`.
With `off`, record it and skip Ponytail discovery, loading, and review. Otherwise resolve
installed Ponytail through the host's skill catalog/tools, using read-only filesystem
discovery of installed skill sources if no catalog is exposed. Read the full resolved source
and record its actual path, exposed version (or `unavailable`), and effective mode in the
ledger. Never hardcode cache paths or versions, install anything, or mutate host or repo
configuration for this handoff. If discovery or source access fails, disclose and record
the reason, then continue the original loop. Lifecycle hooks are not evidence of loading.

Apply Ponytail within approved scope: prefer existing project helpers/components, then
standard-library/platform facilities, then installed dependencies, then minimum new code.
Record concrete reuse and simplification evidence, accepted shortcuts and tradeoffs.
Simplification cannot weaken required tests, design approvals, allowlists, security,
validation, accessibility, error handling, hardware correctness, or repository hard rules.
Even `ultra` cannot reopen approved scope or invent approvals. No line-count target,
additional score, automatic whole-repository audit, or extra reviewer is introduced.

## Units and integration

The top level owns planning, scores, integration, engineering records, and final gates.
Delegate only substantial independent work. Each implementation agent owns a worktree at
the current integrated top HEAD, branch `il/<slug>/<unit>`, path
`/tmp/nx-<slug>-u-edge-<unit>`, and target `/tmp/nx-<slug>-target-u-edge-<unit>`.
Redispatch with a new `-r<n>` suffix on all three. Disjoint file allowlists may run in
parallel; overlapping units run sequentially from the preceding integrated result.
Generated wire bindings, generated TS types, lockfiles, and cloud vault records have
one top-level writer; agents report required changes instead of independently writing them.

Give every implementation agent this complete contract:

```text
Task and requirements: <one step; spec/ADR/BUG paths; prior art; approved scope>
Repo, base SHA, owned worktree/branch/target: <exact values>
Step → verify: <exact commands; expected evidence>
Allowlist: <edit no other paths>
Ponytail: <mode; resolved source path/version or unavailable reason;
  full source contents if inaccessible; accepted shortcuts/tradeoffs; open findings>
Read the applicable resolved skill source explicitly before work, or its supplied full
contents if inaccessible. Do not rely on lifecycle hooks. Off skips the handoff.
UI context if applicable: <design skill/context; approved mock; tokens/components;
  viewports/themes/states; confirmed decisions; pending approvals; artifact owner>
Read AGENTS.md. Create and use your own worktree from the supplied base. Echo canonical
path/base/branch before editing. Use your own CARGO_TARGET_DIR and E2E_PORT, respecting
the E2E serialization and engine-path constraints below. Cap terminal output.
Before staging each commit fetch/rebase origin/main per AGENTS.md, preserving local work.
Commit only allowlisted files. Report canonical path, actual base/head/commit SHAs,
changed paths, exact verification results, skipped gates, assumptions, and open findings.
Never push, merge, deploy, or edit another tree.
```

Choose the model for the unit, recording the reason; honor the operator's explicit model
and the repository preference when available. Use strong judgment for design, security,
wire/hardware work and review; mechanical work may use a cheaper available model.
Record fallback to the host default when unavailable. Two failed cheap-tier attempts
require a stronger approach rather than a third identical attempt.

Before accepting a report, compare canonical paths (macOS `/tmp` may be `/private/tmp`),
verify branch head equals reported SHA and base is its ancestor, and inspect every commit
for allowlist compliance. Reject a mismatched report. Cherry-pick accepted commits into
`$TOP` in dependency order; resolve conflicts there or redispatch, never copy source.
Rerun each unit's verification in the integrated tree; only that evidence counts.
Before every top-level commit fetch/rebase `origin/main` per `AGENTS.md`; preserve local
work, resolve conflicts, and rerun affected gates if the base changes.

## Five criteria: score 1–10 each pass

1. **Spec/ADR conformance:** satisfy matching cloud vault records, acceptance criteria,
   and every applicable edge hard rule; cite requirements and source evidence.
2. **Mock/visual fidelity:** UI matches the approved mock in layout, behavior, loading,
   empty/error states, themes, and responsive viewports. Locate or extend the mock first;
   compare rendered implementation and mock at matching states. Load applicable design
   instructions and preserve required approvals and finish checks. Missing design context
   does not authorize a redesign. Record N/A with reason for nonvisual work.
3. **Actual regression coverage:** exercise new behavior, error paths, invariants, and
   meaningful regression cases. For bugs establish root cause and write a reproducer
   before fixing. Required commands must actually run green; skipped hardware/model
   smoke tests and unavailable runtimes are unverified. Default features and macOS do
   not prove Linux, GStreamer, ONNX/EP, Hailo, or real hardware correctness.
4. **Restraint:** every changed line serves the task. Name existing helpers/components
   searched and the reuse/simplification selected (or why unsuitable), plus accepted
   shortcuts and their tradeoffs. Avoid speculative features, single-use abstractions,
   unnecessary dependencies, drive-by cleanup, and impossible-state handling. Remove
   only orphans created by this change. Ponytail informs this criterion without replacing
   #1–3 or imposing line-count targets.
5. **Engineering record current:** pass 1 resolves an existing matching ADR/SPEC/BUG or
   establishes why the work is routine and N/A. Edge has no local vault: find the cloud
   repository and use its own isolated worktree, rules, numbering, and templates under
   `.obsidian-vault/`. Never fabricate a vault in edge or edit the cloud primary checkout.
   Reuse records rather than duplicating them. Record-worthy edge work needs a paired
   cloud deliverable; update it in each behavior-changing pass and carry both SHAs.
   At FINAL verify status, acceptance criteria, symptom/root cause/resolution or decision
   consequences, cross-links, and exact target headings. Routine docs and implementation
   notes need no record unless genuinely ADR/SPEC/BUG-worthy; log the N/A reason once.

## Repeat in the same run

1. **PLAN:** state `step → verify:` pairs, allowlists, owners, dependencies, and evidence.
   Start with the diff; inspect connected callers/source when contracts or flows require.
2. **DO:** implement or dispatch, integrate commits, update applicable engineering records,
   and run affected gates. Keep output capped using `scripts/run-capped.sh` or log tails.
3. **COMPACT:** persist the task, paths/branches/base/head/targets, scores and evidence,
   files touched, open worktrees/processes, model choices, assumptions/rulings, vault
   paths or N/A reason, design approvals/context/screenshots, and next action in the ledger.
   Persist Ponytail mode, resolved source/version or unavailable reason, shortcuts and
   tradeoffs, and open findings/severity. Resume from this ledger after compaction.
4. **VERIFY AS A TESTER:** score the integrated tree on disk with fresh evidence and
   file:line weaknesses. No evidence means at most 5; untested code scores at most 5 on
   #3, UI without rendered comparison at most 5 on #2, stale records at most 5 on #5.
5. **DECIDE:** all applicable scores at least 8 plus final gates/review below permits
   `FINAL`. Otherwise print `ITERATING`, name the lowest score, and immediately begin
   the next PLAN. Three consecutive low passes demand a different approach, not another
   version of the same fix. Exhausted passes alone are not a stopping point.

## Edge verification and E2E ownership

Read the current CI conditions; unconditional PR gates must be green before FINAL.
Use the exact commands in `AGENTS.md` and `.github/workflows/ci.yml`, including:

- `cargo fmt --all -- --check`
- `cargo check --locked --workspace --all-targets`
- `cargo clippy --locked --workspace --all-targets -- -D warnings`
- `cargo test --locked --workspace --no-fail-fast`
- `cargo xtask check-models` and `cargo xtask check-span-cardinality`
- `cargo test --locked -p nexus-types --features ts`, followed by no diff in
  `ui/src/api/types/`
- In `ui/`, `npm ci` (Node 22), `npm run lint`, `npm test`, `npm run typecheck`,
  `npm run build`, and Playwright `npm run e2e` (`npm run e2e:install` once per host).

Run Rust commands with the tree's private `CARGO_TARGET_DIR`. Include CI's ORT session
tuning/engine prewarm tests, linked Hailo lint, and release EP feature lint; consult the
workflow for exact feature sets. Linux-gated code needs Linux verification. Conditional
`system-libs` jobs cover GStreamer/ORT/model-backed paths; report unrun jobs as unverified
and obtain them when required by changed behavior. No cloud `cargo deny`, cloud RLS,
migration, or Azure gates are invented for edge; licence review still applies.

For E2E build `ui/dist` and `cargo build --locked -p nexus-engine --features test-injection`
from the same tree. The current harness hardcodes `$WT/target/debug/nexus-engine`; if
`$WT/target` is absent, a run-owned symlink to that tree's private target is acceptable.
Verify an existing path belongs to this tree before using it; never replace unknown
contents or use another tree's binary. Use a unique `E2E_PORT=<port>` and confirm the
test base URL and engine bind match. Serialize edge E2E across all worktrees: the harness
uses the shared `/tmp/nexus-e2e-sidecar.json` even with separate ports. Preserve this
constraint until the harness itself supports isolated state. Stop owned engines after
tests and verify their ports are no longer listening.

## Independent review and final cleanup

After gates, require a clean tracked/untracked tree and commit all reviewed work. Pin
`REVIEW_BASE` and `REVIEW_HEAD` to exact SHAs; generate the patch with
`git -C "$TOP" diff "$REVIEW_BASE"..."$REVIEW_HEAD" > <run-owned-patch>`. A moving ref
is not a pinned review. Use one patch per repo for paired changes.

Dispatch the existing `requesting-code-review` reviewer as a fresh agent without session
history. Give requirements, pinned base/head/patch, and a detached read-only worktree at
review HEAD. It reads repository review instructions and, when enabled, the installed
`ponytail-review` source resolved through host discovery. Record the actual source and
version or unavailable reason; require explicit source reading, supplying full contents
if the path is inaccessible. Give effective mode, accepted shortcuts/tradeoffs, and open
findings. Resolve the review source independently: an unavailable main Ponytail source
does not prove its review source is missing. Missing Ponytail-review is disclosed and
recorded; normal review continues.
Both instruction sets evaluate the same pinned patch and HEAD; do not add a reviewer or
whole-repository audit. Require echoed patch path, canonical worktree, and actual HEAD.

Verify findings against the integrated tree per `receiving-code-review`. Ask about
ambiguities that materially change implementation; do not guess away a review finding.
Map Ponytail findings by actual impact to existing critical/important/minor gates.
Critical and important findings reopen the loop; minor findings go in the ledger, with a
vault update only when record-worthy. A `Ship` verdict bypasses no required gate or open
material finding. Fixes require affected gates and review evidence for the new pinned HEAD.

Clean up only worktrees/branches/processes created by this run. For accepted units,
`git cherry HEAD <unit-branch>` must show only `-` lines before removal. For rejected
units, preserve reports/SHAs and explicitly determine the work is unneeded. Inspect both
normal and ignored/untracked status; preserve unknown or uncommitted work outside the
tree before removal. Remove only verified disposable owned build outputs and symlinks,
then require clean status and `git worktree remove <path>` without force. Delete owned
branches only after integration or explicit rejection. Review/exploration trees follow
the same preservation checks. Never remove a pre-existing worktree, branch, or file.

Confirm `git worktree list --porcelain` has no remaining run-owned unit/read-only trees
and no owned preview/engine is listening. `$TOP` and its branch remain for the PR; final
scores, gates, review SHAs, unresolved minor findings, and record deliverables stay in the
ledger. Push, shared-branch merge, or deploy requires existing user authorization.

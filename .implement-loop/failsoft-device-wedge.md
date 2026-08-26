# Task

Make `fail_soft` survive a runtime execution-provider/device failure on the edge.
Repo: `nexus-edge-ai-core-next`. Vault record in `nexus-cloud-console`.

## State: PASS 4 done, awaiting CI on `cac5b39`

- Edge PR **#308**, branch `fix/failsoft-survives-device-wedge`
  - `f46ca61` first pass — **CI fully green** (check/clippy/test/EP-demotion/check-models)
  - `cac5b39` review fixes — CI run `32915432473`: check OK, clippy OK, `cargo test` running
- Cloud PR **#682**, branch `docs/bug-133-failsoft-device-wedge`, head `a90b960`

## Vault record

`nexus-cloud-console/.obsidian-vault/bugs/BUG-133 A Wedged Accelerator Keeps Its Worker Ready So Fail-Soft Never Engages.md`
`status: In Progress` -> flip to `Resolved` at FINAL. Already matches the reviewed design.

## Shipped design (after independent review)

1. `backends.rs` — demote to `Failed` when `error_streak >= DEVICE_FAILURE_STREAK (32)`
   AND the streak has lasted `DEVICE_FAILURE_WINDOW (15s)`. Streak cleared on demotion;
   state change edge-triggered; `streak_started: Mutex<Option<Instant>>` set as streak 0->1.
2. `lib.rs` — `fail_soft_cfg` rewrites the fallback chain to `["cpu"]` unless the chain
   names `hailo` (case-insensitive, trimmed).
3. `session_tuning.rs` — `build_or_demote_to_cpu` checks `ACCELERATOR_WEDGED` BEFORE the
   CPU-only short-circuit, so a `["cpu"]` fallback stays bounded post-wedge.
4. `pool.rs` — behaviour unchanged; one characterization test added.

## Review findings all addressed

BLOCKER (unbounded fallback build post-wedge) done; fallback guard inverted/Hailo wrong
done; `starts_with("cpu")` case-sensitivity done; `==` threshold latch + missing reset
done; frames-not-time threshold done; `.gitignore` scope creep reverted.
Reviewer confirmed the `fetch_add` concurrency question is NOT a bug.

## Known gaps (recorded in BUG-133, deliberately not fixed)

- M1 session factory still a stub -> demoted slot cannot rebuild; permanent until restart.
- `nexus-doctor` check 9.5 `backends_ready` fails for process lifetime after a demotion.
- `fail_soft = false` + all workers demoted -> permanent pool error (default is `true`).
- No wiring test for the `build_detector(&fail_soft_cfg(cfg))` call site (needs a real model).

## Remaining steps

1. Confirm CI green on `cac5b39`, and grep the job log for the test names to prove they
   ran — a green job is not proof of execution.
2. Flip BUG-133 `status: Resolved`, push to #682.
3. Update PR #308 body for the count+window rule and the known gaps.
4. Merge both — ask first, shared branches.

## Constraint

**No Rust toolchain on this workstation.** CI is the only gate. Never claim a local green.

## Scores (pass 4, pre-CI)

1 Spec/ADR 9 · 2 Mock N/A · 3 Regression 7 (pending CI proof) · 4 Restraint 8 · 5 Vault 9

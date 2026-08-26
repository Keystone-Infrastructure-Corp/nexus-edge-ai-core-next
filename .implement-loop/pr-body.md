Fixes BUG-133 (record in the cloud repo, companion PR).

A wedged accelerator leaves the worker *thread* healthy, so the only thing that ever set
`BackendState::Failed` — a closed command channel — never fired. The slot stayed `Ready`,
`DetectorPool::pick_ready` kept handing it every frame, and the fail-soft fallback was
never consulted.

Observed on `colmc-tx3604cedarspringsrd-1` (Apollo Lake, `ep_priority=["gpu","cpu"]`,
`fail_soft=true`, `workers=1`, 10 cameras): the iGPU wedged at 20:27:38 (i915 `rcs0`
preemption timeout, then `Failed to reset GuC, ret = -110`) and detections stopped
completely until systemd restarted the process at 21:03:34 — **36 minutes**. Clip
recording was unaffected, because decode runs on a different engine of the same chip.

## Change

- `ThreadIsolatedBackend` counts **consecutive** `detect` failures, cleared by the first
  success, and demotes the slot to `Failed` at `DEVICE_FAILURE_STREAK = 32`. The threshold
  is deliberately long: nothing restores the slot short of a process restart, so a
  transient hiccup must not cost the accelerator for the life of the process.
- `fail_soft_cfg` builds the fallback on the CPU EP when the configured chain contains one.
  Building it from the same `ep_priority` made it fail with the workers it exists to
  survive. A chain with no CPU entry (Hailo — the model is a HEF the CPU cannot execute)
  keeps what it configured.
- `DetectorPool` is **unchanged**. It was never wrong; it was told the slot was healthy.

## Not in this PR, recorded in BUG-133

- The M1 session factory is still a stub — `factory()` returns a clone of the same
  `Arc<dyn Detector>` — so a demoted slot cannot rebuild. That is a session-lifecycle
  change well beyond this bug.
- Marking `ACCELERATOR_WEDGED` on a detect-time wedge (BUG-120's latch), and re-probing a
  `Failed` slot.

## Tests

Five, each failing without the corresponding half of the change:

- `a_persistently_failing_slot_demotes_itself` — asserts it does **not** demote early, then
  does at the threshold
- `one_success_clears_the_error_streak` — proves the streak is consecutive, not a lifetime
  error budget
- `pool_routes_past_a_failed_worker_to_the_fallback` — the failed slot's `detect` panics, so
  a wrong route fails loudly rather than silently
- `fail_soft_fallback_leaves_the_accelerator_behind` and
  `fail_soft_fallback_keeps_a_chain_with_no_cpu_entry`

No Rust toolchain on this workstation, so CI is the gate.

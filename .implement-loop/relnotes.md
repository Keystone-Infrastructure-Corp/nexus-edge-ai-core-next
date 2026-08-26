Fixes **BUG-133** — a wedged accelerator kept its worker `Ready`, so fail-soft never engaged.

## What was broken

On `colmc-tx3604cedarspringsrd-1` (Apollo Lake, `ep_priority = ["gpu","cpu"]`, `fail_soft = true`, one worker, ten cameras) the Intel iGPU wedged at runtime — i915 `rcs0` preemption timeout, then `Failed to reset GuC, ret = -110` and `Failed to reset chip`. The ORT/OpenVINO session had already built successfully and was serving; it then returned `CL_OUT_OF_RESOURCES` on every frame.

**Detection stopped completely for 36 minutes** (20:27:38 → 21:03:34) until systemd restarted the process. Clip recording was unaffected throughout — decode runs on a different engine of the same chip — so 327 clips wrote normally while nothing was being detected.

Three compounding defects:

1. `ThreadIsolatedBackend::detect` logged `"worker returned error; not yet restarting"` and never demoted. `BackendState::Failed` was set only when the command *channel* closed, i.e. on thread death. A wedged *device* leaves the thread healthy, so `DetectorPool::pick_ready` kept feeding it every frame and the fallback was never consulted.
2. The worker's restart loop is unreachable after the first successful init — and its factory is still an M0 stub that returns a clone of the same poisoned `Arc<dyn Detector>`, so a session could never be recycled.
3. The fail-soft fallback was built from the same `cfg`, so it sat on the same wedged GPU.

## What changed

- A slot demotes itself to `Failed` after `DEVICE_FAILURE_STREAK = 32` consecutive `detect` failures **that have also lasted `DEVICE_FAILURE_WINDOW = 15s`**. Both conditions matter: 32 frames is under a second when one worker serves ten cameras, demotion is permanent until restart, and the earlier i915 reset that night recovered on its own — a count-only rule would have permanently demoted a healthy accelerator over it.
- The fail-soft fallback is built on the CPU EP unless the chain names `hailo`, so the last resort no longer shares the device that just failed.
- `build_or_demote_to_cpu` now checks the `ACCELERATOR_WEDGED` latch **before** its CPU-only short-circuit, so the CPU fallback build stays bounded after a BUG-120 wedge instead of hanging startup.
- `DetectorPool` is unchanged — it was never wrong, it was told the slot was healthy.

Net effect: a runtime device wedge costs a demotion and continues on the CPU EP — slower, which is what ADR-006 promises — instead of stopping detection until systemd notices.

## Known behaviour worth watching

- `nexus-doctor` check 9.5 `backends_ready` will report a hard failure for the life of the process after a demotion, while the box serves correctly on CPU.
- With `fail_soft = false` and every worker demoted the pool returns a permanent error; `fail_soft` defaults to `true`.
- After demotion all cameras serialise through one in-process CPU session — the degraded path is now hot rather than cold.
- Nothing restores a demoted slot short of a restart; finishing the M1 session factory is the follow-up.

Also includes everything in v0.1.206.

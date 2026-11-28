---
name: edge-e2e-shared-sidecar
description: "Concurrent edge ui e2e runs on one machine share $TMPDIR/nexus-e2e-sidecar.json; give each run its own TMPDIR"
metadata:
  type: project
---

`ui/e2e/global-setup.ts` and `ui/e2e/helpers.ts` keep the engine sidecar at
`$TMPDIR/nexus-e2e-sidecar.json`. Two edge Playwright runs on one machine, even on
different `E2E_PORT`s, share that file. One run's teardown deletes it, and the
other run then fails with `ENOENT` on the sidecar and orphans its engine.

**Why:** a parking UI run failed 24 tests this way while another worktree's edge
e2e finished; the same specs passed once the run had its own `TMPDIR`.

**How to apply:** give each concurrent run its own directory, for example
`mkdir -p /tmp/<run>/tmp && TMPDIR=/tmp/<run>/tmp E2E_PORT=<port> npx playwright test ...`.
After a run that hit the error, kill any engine it left behind.

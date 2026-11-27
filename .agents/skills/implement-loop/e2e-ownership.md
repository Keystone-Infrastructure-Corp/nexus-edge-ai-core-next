# E2E ownership

Read only for units or gates that run edge UI E2E.

For E2E build `ui/dist` and `cargo build --locked -p nexus-engine --features test-injection`
from the same tree. The current harness hardcodes `$WT/target/debug/nexus-engine`; if
`$WT/target` is absent, a run-owned symlink to that tree's private target is acceptable.
Verify an existing path belongs to this tree before using it; never replace unknown
contents or use another tree's binary. Use a unique `E2E_PORT=<port>` and confirm the
test base URL and engine bind match. Edge E2E is a shared harness: serialize it across all
worktrees, because the harness uses the shared `/tmp/nexus-e2e-sidecar.json` even with
separate ports. Preserve this constraint until the harness itself supports isolated state.
Stop owned engines after tests and verify their ports are no longer listening.

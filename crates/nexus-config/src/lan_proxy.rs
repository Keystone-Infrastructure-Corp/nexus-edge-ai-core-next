//! Phase 7.6.6 — generic LAN device proxy (REPO_BOUNDARY R5c)

use serde::{Deserialize, Serialize};

/// `[lan_proxy]` block. **Disabled by default.** When `enabled = true`,
/// the audited, SSRF-bounded `POST /api/v1/admin/proxy` admin route
/// lets the operator console reach a non-ONVIF device on the edge's own
/// LAN (camera web UIs, NVRs). The whole feature is an explicit,
/// narrowly-scoped exception to the credential-boundary rule, so it is
/// opt-in per deployment (R5c §5: ships off). Every other R5c
/// constraint (SSRF guard, discovery allowlist, per-call audit, no
/// credential persistence) is enforced unconditionally in
/// `crate::lan_proxy` whenever this is on.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LanProxyConfig {
    /// Master switch. Defaults `false`.
    #[serde(default)]
    pub enabled: bool,
}

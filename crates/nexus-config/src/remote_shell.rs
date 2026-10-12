//! Remote shell

use serde::{Deserialize, Serialize};

/// `[remote_access]` block. **Disabled by default.**
///
/// When `enabled = true`, a `shell_session_open` from the cloud makes
/// the engine dial a second WSS side channel back to the SAME host its
/// control tunnel already talks to, and pipe those bytes to [`Self::target`].
///
/// Two properties are load-bearing and deliberately live here rather
/// than on the wire:
///
/// * **The destination is local config, never a wire field.** If the
///   cloud could name the target, a compromised control plane could use
///   any core as a pivot into its owner's LAN. The `shell_session_open`
///   payload has no `target` field at all.
/// * **Consent is local.** An org admin grants access in the console,
///   but the box owner still had to switch this on. Neither party can
///   act alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteAccessConfig {
    /// Master switch. Defaults `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Where the byte pipe terminates. Defaults to the local sshd.
    /// Must be a loopback or private address — see
    /// [`Self::target_is_local`].
    #[serde(default = "default_shell_target")]
    pub target: String,
    /// Ceiling the engine enforces on its own, independent of whatever
    /// deadline the cloud sends. A session is torn down at the earlier
    /// of the two, so a wedged or hostile broker cannot hold a shell
    /// open past what the box owner agreed to.
    #[serde(default = "default_shell_max_session_secs")]
    pub max_session_secs: u64,
}

fn default_shell_target() -> String {
    "127.0.0.1:22".to_string()
}

const fn default_shell_max_session_secs() -> u64 {
    1800
}

impl Default for RemoteAccessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            target: default_shell_target(),
            max_session_secs: default_shell_max_session_secs(),
        }
    }
}

impl RemoteAccessConfig {
    /// Whether [`Self::target`] resolves to loopback or an RFC1918 /
    /// unique-local address.
    ///
    /// The pipe exists to reach a service on the appliance itself. A
    /// target on the public internet would make the appliance a relay,
    /// which is not a thing anyone asked for, so it is refused at
    /// config-validation time rather than at session time.
    #[must_use]
    pub fn target_is_local(&self) -> bool {
        let Some((host, _port)) = self.target.rsplit_once(':') else {
            return false;
        };
        let host = host.trim_matches(['[', ']']);
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        match host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(v4)) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local()
            }
            Ok(std::net::IpAddr::V6(v6)) => {
                v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00
            }
            Err(_) => false,
        }
    }
}

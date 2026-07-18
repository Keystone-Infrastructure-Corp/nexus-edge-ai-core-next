//! Interface-role assignment — which NIC cameras live on (LAN) and
//! which NIC carries outbound internet traffic (WAN).
//!
//! This is a **higher-level policy** than the per-NIC netplan plan in
//! [`super::plan`]: the plan says *how* each interface is addressed;
//! the roles say *what each interface is for*. Two roles, both
//! optional:
//!
//! * **`camera`** — the interface cameras are reachable on. Enforced
//!   **hard** on the discovery path the engine controls: discovery
//!   probe sockets are source-bound to this NIC's IPv4 (see
//!   [`camera_bind_ipv4`]), and its subnet is the suggested default
//!   for a CIDR sweep. RTSP media pulled by GStreamer follows kernel
//!   routing — on a correctly-segmented install the camera subnet is
//!   only reachable via this NIC, so ingest naturally egresses here.
//! * **`internet`** — the interface used for outbound cloud traffic
//!   (WSS tunnel, clip upload, alert sinks). Enforced **soft**: the
//!   resolved source IP is surfaced as the preferred egress address
//!   and label. No socket is forcibly bound (the engine runs
//!   unprivileged and a hard bind would need `SO_BINDTODEVICE` /
//!   policy routing via the privileged helper).
//!
//! Persisted as JSON in `engine_runtime_settings.iface_roles_json`.
//! Both roles absent (the default) means "let the OS decide" — the
//! prior behaviour, so this is fully backward-compatible.

use std::net::Ipv4Addr;

use nexus_store::Store;
use serde::{Deserialize, Serialize};

use super::enumerate::{list_interfaces, NetworkInterface};

/// `engine_runtime_settings` key the JSON blob lives under.
pub const KEY_IFACE_ROLES: &str = "iface_roles_json";

/// Operator-assigned interface roles. Either field may be `None`
/// when unassigned; the engine then falls back to OS default
/// routing for egress and requires an explicit discovery CIDR.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterfaceRoles {
    /// NIC name (`eno1`, `eno1.20`) cameras are reachable on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<String>,
    /// NIC name used for outbound internet traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internet: Option<String>,
}

impl InterfaceRoles {
    /// Normalise: trim whitespace and coalesce empty strings to
    /// `None` so the UI can send `""` to clear a role.
    pub fn normalised(mut self) -> Self {
        self.camera = self.camera.and_then(non_empty);
        self.internet = self.internet.and_then(non_empty);
        self
    }
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// A role resolved against the live NIC table. Surfaces exactly what
/// the assignment means *right now* so the UI can show the operator
/// the concrete address / subnet the role points at (and flag a
/// dangling assignment when the NIC has gone away).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResolvedRole {
    /// The assigned interface name.
    pub interface: String,
    /// `true` when a NIC by that name currently exists.
    pub present: bool,
    /// `true` when the NIC is up with a carrier.
    pub up: bool,
    /// Primary IPv4 bound to the NIC, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<Ipv4Addr>,
    /// CIDR prefix length of the primary IPv4.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix_len: Option<u8>,
    /// Network subnet in CIDR form (`192.168.10.0/24`), derived
    /// from the primary IPv4 + prefix. The camera role uses this as
    /// the suggested discovery sweep range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subnet_cidr: Option<String>,
}

/// First IPv4 (address, prefix) bound to a NIC. `addrs` is sorted
/// IPv4-first by [`list_interfaces`], so this is the "primary".
fn primary_ipv4(nic: &NetworkInterface) -> Option<(Ipv4Addr, u8)> {
    nic.addrs.iter().find_map(|a| match a.addr {
        std::net::IpAddr::V4(v4) => Some((v4, a.prefix_len)),
        std::net::IpAddr::V6(_) => None,
    })
}

/// Mask an address to its network and format as CIDR.
fn subnet_cidr(ip: Ipv4Addr, prefix: u8) -> Option<String> {
    ipnet::Ipv4Net::new(ip, prefix)
        .ok()
        .map(|n| n.trunc().to_string())
}

/// A NIC counts as "up" when the kernel reports operstate `up` and
/// hasn't explicitly flagged a missing carrier. Mirrors the badge
/// logic in the admin-network UI.
fn is_up(nic: &NetworkInterface) -> bool {
    nic.operstate.as_deref() == Some("up") && nic.carrier != Some(false)
}

/// Resolve one assigned interface name against a NIC table.
pub fn resolve_role(name: &str, nics: &[NetworkInterface]) -> ResolvedRole {
    match nics.iter().find(|n| n.name == name) {
        Some(nic) => {
            let ip4 = primary_ipv4(nic);
            ResolvedRole {
                interface: name.to_string(),
                present: true,
                up: is_up(nic),
                ipv4: ip4.map(|(a, _)| a),
                prefix_len: ip4.map(|(_, p)| p),
                subnet_cidr: ip4.and_then(|(a, p)| subnet_cidr(a, p)),
            }
        }
        None => ResolvedRole {
            interface: name.to_string(),
            present: false,
            up: false,
            ipv4: None,
            prefix_len: None,
            subnet_cidr: None,
        },
    }
}

/// Validate that an assignment names a real, non-loopback NIC. The
/// UI only offers real NICs, but a stale form or a NIC that was
/// removed between the GET and the PUT must be rejected loudly.
pub fn validate_role_name(name: &str, nics: &[NetworkInterface]) -> Result<(), String> {
    let nic = nics
        .iter()
        .find(|n| n.name == name)
        .ok_or_else(|| format!("interface `{name}` not found"))?;
    if nic.is_loopback {
        return Err(format!(
            "interface `{name}` is loopback and cannot be assigned a role"
        ));
    }
    Ok(())
}

/// Load the persisted roles. Missing / malformed rows resolve to the
/// empty (both-unassigned) default so callers never fail on a bad
/// blob — the worst case is falling back to OS default routing.
pub async fn load_roles(store: &Store) -> InterfaceRoles {
    match store.read_runtime_setting(KEY_IFACE_ROLES).await {
        Ok(Some(Some(json))) => serde_json::from_str::<InterfaceRoles>(&json)
            .map(InterfaceRoles::normalised)
            .unwrap_or_default(),
        _ => InterfaceRoles::default(),
    }
}

/// The IPv4 the camera-role NIC is bound to, if a camera role is set
/// and the NIC currently has an IPv4. Discovery source-binds its
/// probe sockets to this address (the hard camera enforcement).
/// `None` — no role, missing NIC, or no IPv4 — means "don't bind",
/// preserving the prior default-routing behaviour.
pub async fn camera_bind_ipv4(store: &Store) -> Option<Ipv4Addr> {
    let name = load_roles(store).await.camera?;
    let nics = list_interfaces().ok()?;
    resolve_role(&name, &nics).ipv4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::enumerate::{InterfaceAddr, InterfaceKind};
    use std::net::IpAddr;

    fn nic(name: &str, ip: Option<(&str, u8)>, loopback: bool, up: bool) -> NetworkInterface {
        let addrs = ip
            .map(|(a, p)| {
                vec![InterfaceAddr {
                    addr: a.parse::<IpAddr>().unwrap(),
                    prefix_len: p,
                    family: "ipv4",
                }]
            })
            .unwrap_or_default();
        NetworkInterface {
            name: name.to_string(),
            mac: None,
            addrs,
            is_loopback: loopback,
            operstate: Some(if up { "up" } else { "down" }.to_string()),
            carrier: Some(up),
            mtu: Some(1500),
            kind: if loopback {
                InterfaceKind::Loopback
            } else {
                InterfaceKind::Physical
            },
            parent: None,
            vlan_id: None,
        }
    }

    #[test]
    fn resolve_present_nic_yields_subnet() {
        let nics = vec![nic("eno1", Some(("192.168.10.20", 24)), false, true)];
        let r = resolve_role("eno1", &nics);
        assert!(r.present);
        assert!(r.up);
        assert_eq!(r.ipv4, Some("192.168.10.20".parse().unwrap()));
        assert_eq!(r.prefix_len, Some(24));
        assert_eq!(r.subnet_cidr.as_deref(), Some("192.168.10.0/24"));
    }

    #[test]
    fn resolve_missing_nic_is_absent() {
        let nics = vec![nic("eno1", Some(("192.168.10.20", 24)), false, true)];
        let r = resolve_role("eno2", &nics);
        assert!(!r.present);
        assert!(!r.up);
        assert!(r.ipv4.is_none());
        assert!(r.subnet_cidr.is_none());
    }

    #[test]
    fn validate_rejects_missing_and_loopback() {
        let nics = vec![
            nic("eno1", Some(("192.168.10.20", 24)), false, true),
            nic("lo", Some(("127.0.0.1", 8)), true, true),
        ];
        assert!(validate_role_name("eno1", &nics).is_ok());
        assert!(validate_role_name("lo", &nics).is_err());
        assert!(validate_role_name("nope", &nics).is_err());
    }

    #[test]
    fn normalise_coalesces_blank_to_none() {
        let roles = InterfaceRoles {
            camera: Some("  ".to_string()),
            internet: Some(" eno1 ".to_string()),
        }
        .normalised();
        assert_eq!(roles.camera, None);
        assert_eq!(roles.internet.as_deref(), Some("eno1"));
    }

    #[test]
    fn roundtrip_json_omits_absent_roles() {
        let roles = InterfaceRoles {
            camera: Some("eno1".to_string()),
            internet: None,
        };
        let json = serde_json::to_string(&roles).unwrap();
        assert_eq!(json, r#"{"camera":"eno1"}"#);
        let back: InterfaceRoles = serde_json::from_str(&json).unwrap();
        assert_eq!(back, roles);
    }
}

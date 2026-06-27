//! ONVIF network-settings client (`ver10/device`).
//!
//! Phase 7.6.9. The "how is this box wired into the LAN" service:
//! the read side ([`get_network_interfaces`],
//! [`get_network_default_gateway`], [`get_dns`], [`get_hostname`],
//! [`get_network_protocols`], [`get_zero_configuration`]) is always
//! safe, the **safe** write side ([`set_dns`], [`set_hostname`],
//! [`set_network_protocols`]) reconfigures plumbing that cannot
//! isolate the camera, and the **dangerous** write side
//! ([`set_network_interfaces`], [`set_network_default_gateway`]) can
//! sever the edge's own ingest path — those two are owner-only +
//! type-token confirmed and drive a lockstep `ingest.url` /
//! `onvif_endpoint` rewrite + re-probe in
//! `crate::device_control` (there is no transactional ONVIF revert).
//!
//! NTP (`GetNTP` / `SetNTP`) is a network setting too, but it already
//! lives in [`super::onvif_device`]; the device-control read handler
//! folds it into the same response.
//!
//! Every request funnels through [`onvif_soap::DEVICE`]; the request
//! builders and response parsers are pure functions so they unit-test
//! against recorded fixtures without a network.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use super::onvif_soap::{first_text, local_name, xml_escape, DEVICE};

// ---------------------------------------------------------------------------
// Read-side types
// ---------------------------------------------------------------------------

/// One IPv4 address bound to an interface, tagged with how it was
/// acquired (`manual` / `dhcp` / `link_local`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Ipv4Address {
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix_length: Option<u8>,
    /// `manual`, `dhcp`, or `link_local`.
    pub origin: String,
}

/// One network interface (`GetNetworkInterfaces`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NetworkInterface {
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hw_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4_dhcp: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ipv4_addresses: Vec<Ipv4Address>,
}

/// The default gateway(s) (`GetNetworkDefaultGateway`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NetworkGateway {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ipv4: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ipv6: Vec<String>,
}

/// DNS client configuration (`GetDNS`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DnsInformation {
    pub from_dhcp: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub search_domain: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<String>,
}

/// Hostname configuration (`GetHostname`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HostnameInformation {
    pub from_dhcp: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One network-protocol toggle (`GetNetworkProtocols`): `HTTP`,
/// `HTTPS`, or `RTSP`, whether it's enabled, and the port(s) it
/// listens on.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NetworkProtocol {
    pub name: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
}

/// Zero-configuration (link-local / Bonjour) state
/// (`GetZeroConfiguration`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ZeroConfiguration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
}

// ---------------------------------------------------------------------------
// Write-side request shapes
// ---------------------------------------------------------------------------

/// The IPv4 reconfiguration applied by [`set_network_interfaces`].
/// `dhcp == true` ignores `address` / `prefix_length` and lets the
/// camera take its lease from DHCP.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ipv4InterfaceSet {
    pub token: String,
    pub enabled: Option<bool>,
    pub mtu: Option<u32>,
    pub dhcp: bool,
    pub address: Option<String>,
    pub prefix_length: Option<u8>,
}

/// One protocol toggle applied by [`set_network_protocols`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetworkProtocolSet {
    pub name: String,
    pub enabled: bool,
    pub port: Option<u16>,
}

// ---------------------------------------------------------------------------
// Read operations
// ---------------------------------------------------------------------------

/// Read every network interface.
pub async fn get_network_interfaces(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<Vec<NetworkInterface>, String> {
    let resp = DEVICE
        .call(
            endpoint,
            username,
            password,
            "GetNetworkInterfaces",
            "<tds:GetNetworkInterfaces/>",
        )
        .await?;
    Ok(parse_network_interfaces(&resp))
}

/// Read the configured default gateway(s).
pub async fn get_network_default_gateway(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<NetworkGateway, String> {
    let resp = DEVICE
        .call(
            endpoint,
            username,
            password,
            "GetNetworkDefaultGateway",
            "<tds:GetNetworkDefaultGateway/>",
        )
        .await?;
    Ok(parse_default_gateway(&resp))
}

/// Read the DNS client configuration.
pub async fn get_dns(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<DnsInformation, String> {
    let resp = DEVICE
        .call(endpoint, username, password, "GetDNS", "<tds:GetDNS/>")
        .await?;
    Ok(parse_dns(&resp))
}

/// Read the hostname configuration.
pub async fn get_hostname(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<HostnameInformation, String> {
    let resp = DEVICE
        .call(
            endpoint,
            username,
            password,
            "GetHostname",
            "<tds:GetHostname/>",
        )
        .await?;
    Ok(parse_hostname(&resp))
}

/// Read the network-protocol (HTTP / HTTPS / RTSP) port + enable
/// state.
pub async fn get_network_protocols(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<Vec<NetworkProtocol>, String> {
    let resp = DEVICE
        .call(
            endpoint,
            username,
            password,
            "GetNetworkProtocols",
            "<tds:GetNetworkProtocols/>",
        )
        .await?;
    Ok(parse_network_protocols(&resp))
}

/// Read the zero-configuration (link-local) state.
pub async fn get_zero_configuration(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<ZeroConfiguration, String> {
    let resp = DEVICE
        .call(
            endpoint,
            username,
            password,
            "GetZeroConfiguration",
            "<tds:GetZeroConfiguration/>",
        )
        .await?;
    Ok(parse_zero_configuration(&resp))
}

// ---------------------------------------------------------------------------
// Safe write operations
// ---------------------------------------------------------------------------

/// Set the DNS servers. When `from_dhcp` is true the manual servers
/// are ignored and the camera takes DNS from DHCP.
pub async fn set_dns(
    endpoint: &str,
    username: &str,
    password: &str,
    from_dhcp: bool,
    servers: &[String],
    search_domain: Option<&str>,
) -> Result<(), String> {
    let body = set_dns_body(from_dhcp, servers, search_domain);
    DEVICE
        .call(endpoint, username, password, "SetDNS", &body)
        .await
        .map(|_| ())
}

/// Set the device hostname.
pub async fn set_hostname(
    endpoint: &str,
    username: &str,
    password: &str,
    name: &str,
) -> Result<(), String> {
    let body = format!(
        "<tds:SetHostname><tds:Name>{}</tds:Name></tds:SetHostname>",
        xml_escape(name)
    );
    DEVICE
        .call(endpoint, username, password, "SetHostname", &body)
        .await
        .map(|_| ())
}

/// Set the network-protocol (HTTP / HTTPS / RTSP) enable state +
/// port. A port change here is the "safe" half of 7.6.9 — the camera
/// stays at the same IP — but the caller still rewrites the stored
/// `ingest.url` / `onvif_endpoint` port in lockstep.
pub async fn set_network_protocols(
    endpoint: &str,
    username: &str,
    password: &str,
    protocols: &[NetworkProtocolSet],
) -> Result<(), String> {
    let body = set_network_protocols_body(protocols);
    DEVICE
        .call(endpoint, username, password, "SetNetworkProtocols", &body)
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Dangerous write operations
// ---------------------------------------------------------------------------

/// Reconfigure an interface's IPv4 (DHCP↔static, address, MTU).
/// Returns the camera's `RebootNeeded` flag. This can sever the
/// edge's ingest path — the caller (device-control) is responsible
/// for the owner-only gate, the lockstep endpoint rewrite, the
/// re-probe at the new address, and failing loud.
pub async fn set_network_interfaces(
    endpoint: &str,
    username: &str,
    password: &str,
    set: &Ipv4InterfaceSet,
) -> Result<bool, String> {
    let body = set_network_interfaces_body(set);
    let resp = DEVICE
        .call(endpoint, username, password, "SetNetworkInterfaces", &body)
        .await?;
    Ok(parse_reboot_needed(&resp))
}

/// Set the IPv4 default gateway(s). Dangerous for the same reason as
/// [`set_network_interfaces`].
pub async fn set_network_default_gateway(
    endpoint: &str,
    username: &str,
    password: &str,
    ipv4_gateways: &[String],
) -> Result<(), String> {
    let body = set_default_gateway_body(ipv4_gateways);
    DEVICE
        .call(
            endpoint,
            username,
            password,
            "SetNetworkDefaultGateway",
            &body,
        )
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Request-body builders (pure)
// ---------------------------------------------------------------------------

fn set_dns_body(from_dhcp: bool, servers: &[String], search_domain: Option<&str>) -> String {
    let search = search_domain
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!("<tds:SearchDomain>{}</tds:SearchDomain>", xml_escape(s)))
        .unwrap_or_default();
    let manual = if from_dhcp {
        String::new()
    } else {
        servers
            .iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!("<tds:DNSManual>{}</tds:DNSManual>", ip_type_xml(s)))
            .collect::<String>()
    };
    format!("<tds:SetDNS><tds:FromDHCP>{from_dhcp}</tds:FromDHCP>{search}{manual}</tds:SetDNS>")
}

fn set_network_protocols_body(protocols: &[NetworkProtocolSet]) -> String {
    let entries = protocols
        .iter()
        .map(|p| {
            let port = p
                .port
                .map(|n| format!("<tt:Port>{n}</tt:Port>"))
                .unwrap_or_default();
            format!(
                "<tds:NetworkProtocols><tt:Name>{}</tt:Name><tt:Enabled>{}</tt:Enabled>{}</tds:NetworkProtocols>",
                xml_escape(&p.name),
                p.enabled,
                port,
            )
        })
        .collect::<String>();
    format!("<tds:SetNetworkProtocols>{entries}</tds:SetNetworkProtocols>")
}

fn set_network_interfaces_body(set: &Ipv4InterfaceSet) -> String {
    let enabled = set
        .enabled
        .map(|e| format!("<tt:Enabled>{e}</tt:Enabled>"))
        .unwrap_or_default();
    let mtu = set
        .mtu
        .map(|m| format!("<tt:MTU>{m}</tt:MTU>"))
        .unwrap_or_default();
    let manual = if set.dhcp {
        String::new()
    } else if let Some(addr) = set.address.as_deref().filter(|a| !a.trim().is_empty()) {
        let prefix = set.prefix_length.unwrap_or(24);
        format!(
            "<tt:Manual><tt:Address>{}</tt:Address><tt:PrefixLength>{}</tt:PrefixLength></tt:Manual>",
            xml_escape(addr),
            prefix,
        )
    } else {
        String::new()
    };
    format!(
        "<tds:SetNetworkInterfaces><tds:InterfaceToken>{token}</tds:InterfaceToken><tds:NetworkInterface>{enabled}{mtu}<tt:IPv4><tt:Enabled>true</tt:Enabled><tt:DHCP>{dhcp}</tt:DHCP>{manual}</tt:IPv4></tds:NetworkInterface></tds:SetNetworkInterfaces>",
        token = xml_escape(&set.token),
        dhcp = set.dhcp,
    )
}

fn set_default_gateway_body(ipv4_gateways: &[String]) -> String {
    let entries = ipv4_gateways
        .iter()
        .filter(|g| !g.trim().is_empty())
        .map(|g| format!("<tds:IPv4Address>{}</tds:IPv4Address>", xml_escape(g)))
        .collect::<String>();
    format!("<tds:SetNetworkDefaultGateway>{entries}</tds:SetNetworkDefaultGateway>")
}

/// Build the `tt:Type` + address element pair an IPv4 literal vs a
/// DNS name decides — shared by the DNS-manual entries.
fn ip_type_xml(value: &str) -> String {
    if value.parse::<std::net::Ipv4Addr>().is_ok() {
        format!(
            "<tt:Type>IPv4</tt:Type><tt:IPv4Address>{}</tt:IPv4Address>",
            xml_escape(value)
        )
    } else if value.parse::<std::net::Ipv6Addr>().is_ok() {
        format!(
            "<tt:Type>IPv6</tt:Type><tt:IPv6Address>{}</tt:IPv6Address>",
            xml_escape(value)
        )
    } else {
        format!(
            "<tt:Type>IPv4</tt:Type><tt:IPv4Address>{}</tt:IPv4Address>",
            xml_escape(value)
        )
    }
}

// ---------------------------------------------------------------------------
// Response parsers (pure)
// ---------------------------------------------------------------------------

fn attr_str(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key.as_bytes())
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

fn parse_network_interfaces(body: &str) -> Vec<NetworkInterface> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out: Vec<NetworkInterface> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut cur: Option<NetworkInterface> = None;
    // The IPv4 address currently being assembled + its origin tag.
    let mut cur_addr: Option<Ipv4Address> = None;
    let mut capture: Option<String> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(&e.name());
                match n.as_str() {
                    "NetworkInterfaces" => {
                        cur = Some(NetworkInterface {
                            token: attr_str(&e, "token").unwrap_or_default(),
                            ..NetworkInterface::default()
                        });
                    }
                    "Manual" if cur.is_some() => {
                        cur_addr = Some(Ipv4Address {
                            origin: "manual".into(),
                            ..Ipv4Address::default()
                        });
                    }
                    "LinkLocal" if cur.is_some() => {
                        cur_addr = Some(Ipv4Address {
                            origin: "link_local".into(),
                            ..Ipv4Address::default()
                        });
                    }
                    "FromDHCP" if cur.is_some() => {
                        cur_addr = Some(Ipv4Address {
                            origin: "dhcp".into(),
                            ..Ipv4Address::default()
                        });
                    }
                    "Enabled" | "Name" | "HwAddress" | "MTU" | "DHCP" | "Address"
                    | "PrefixLength" => {
                        capture = Some(n.clone());
                        acc.clear();
                    }
                    _ => {}
                }
                stack.push(n);
            }
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let n = local_name(&e.name());
                if let Some(field) = capture.take() {
                    if field == n {
                        let v = acc.trim().to_string();
                        apply_interface_field(&field, &v, &stack, cur.as_mut(), cur_addr.as_mut());
                    }
                }
                acc.clear();
                match n.as_str() {
                    "Manual" | "LinkLocal" | "FromDHCP" => {
                        if let (Some(c), Some(a)) = (cur.as_mut(), cur_addr.take()) {
                            if !a.address.is_empty() {
                                c.ipv4_addresses.push(a);
                            }
                        }
                    }
                    "NetworkInterfaces" => {
                        if let Some(c) = cur.take() {
                            out.push(c);
                        }
                    }
                    _ => {}
                }
                stack.pop();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Route one captured leaf value into the interface / address under
/// construction, using the open-element stack to disambiguate
/// (`Enabled` under `IPv4` is the v4-enable flag, `Enabled` under
/// `NetworkInterfaces` is the interface enable flag; `Address` under
/// `Manual`/`LinkLocal`/`FromDHCP` is an IP).
fn apply_interface_field(
    field: &str,
    value: &str,
    stack: &[String],
    iface: Option<&mut NetworkInterface>,
    addr: Option<&mut Ipv4Address>,
) {
    let Some(iface) = iface else { return };
    // `stack` still has the leaf element on top; its parent is at
    // len-2.
    let parent = stack.len().checked_sub(2).map(|i| stack[i].as_str());
    match field {
        "Enabled" => match parent {
            Some("IPv4") => iface.ipv4_enabled = parse_bool(value),
            _ => iface.enabled = parse_bool(value),
        },
        "Name" => iface.name = non_empty(value),
        "HwAddress" => iface.hw_address = non_empty(value),
        "MTU" => iface.mtu = value.parse().ok(),
        "DHCP" => iface.ipv4_dhcp = parse_bool(value),
        "Address" => {
            if let Some(a) = addr {
                a.address = value.to_string();
            }
        }
        "PrefixLength" => {
            if let Some(a) = addr {
                a.prefix_length = value.parse().ok();
            }
        }
        _ => {}
    }
}

fn parse_default_gateway(body: &str) -> NetworkGateway {
    let mut out = NetworkGateway::default();
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut capture: Option<&'static str> = None;
    let mut acc = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(&e.name()).as_str() {
                "IPv4Address" => {
                    capture = Some("v4");
                    acc.clear();
                }
                "IPv6Address" => {
                    capture = Some("v6");
                    acc.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(_)) => {
                if let Some(kind) = capture.take() {
                    let v = acc.trim().to_string();
                    if !v.is_empty() {
                        match kind {
                            "v4" => out.ipv4.push(v),
                            "v6" => out.ipv6.push(v),
                            _ => {}
                        }
                    }
                }
                acc.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn parse_dns(body: &str) -> DnsInformation {
    let mut out = DnsInformation::default();
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut capture: Option<&'static str> = None;
    let mut acc = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(&e.name()).as_str() {
                "FromDHCP" => {
                    capture = Some("from_dhcp");
                    acc.clear();
                }
                "SearchDomain" => {
                    capture = Some("search");
                    acc.clear();
                }
                "IPv4Address" | "IPv6Address" => {
                    capture = Some("server");
                    acc.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(_)) => {
                if let Some(kind) = capture.take() {
                    let v = acc.trim().to_string();
                    match kind {
                        "from_dhcp" => out.from_dhcp = parse_bool(&v).unwrap_or(false),
                        "search" if !v.is_empty() => out.search_domain.push(v),
                        "server" if !v.is_empty() => out.servers.push(v),
                        _ => {}
                    }
                }
                acc.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn parse_hostname(body: &str) -> HostnameInformation {
    HostnameInformation {
        from_dhcp: first_text(body, "FromDHCP")
            .and_then(|v| parse_bool(&v))
            .unwrap_or(false),
        name: first_text(body, "Name"),
    }
}

fn parse_network_protocols(body: &str) -> Vec<NetworkProtocol> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out: Vec<NetworkProtocol> = Vec::new();
    let mut cur: Option<NetworkProtocol> = None;
    let mut capture: Option<&'static str> = None;
    let mut acc = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(&e.name()).as_str() {
                "NetworkProtocols" => cur = Some(NetworkProtocol::default()),
                "Name" if cur.is_some() => {
                    capture = Some("name");
                    acc.clear();
                }
                "Enabled" if cur.is_some() => {
                    capture = Some("enabled");
                    acc.clear();
                }
                "Port" if cur.is_some() => {
                    capture = Some("port");
                    acc.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let n = local_name(&e.name());
                if let Some(kind) = capture.take() {
                    if let Some(c) = cur.as_mut() {
                        let v = acc.trim().to_string();
                        match kind {
                            "name" => c.name = v,
                            "enabled" => c.enabled = parse_bool(&v).unwrap_or(false),
                            "port" => {
                                if let Ok(p) = v.parse::<u16>() {
                                    c.ports.push(p);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                acc.clear();
                if n == "NetworkProtocols" {
                    if let Some(c) = cur.take() {
                        if !c.name.is_empty() {
                            out.push(c);
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn parse_zero_configuration(body: &str) -> ZeroConfiguration {
    let mut out = ZeroConfiguration::default();
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut capture: Option<&'static str> = None;
    let mut acc = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(&e.name()).as_str() {
                "InterfaceToken" => {
                    capture = Some("token");
                    acc.clear();
                }
                "Enabled" => {
                    capture = Some("enabled");
                    acc.clear();
                }
                "Addresses" => {
                    capture = Some("address");
                    acc.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(_)) => {
                if let Some(kind) = capture.take() {
                    let v = acc.trim().to_string();
                    match kind {
                        "token" if !v.is_empty() => out.interface_token = Some(v),
                        "enabled" => out.enabled = parse_bool(&v),
                        "address" if !v.is_empty() => out.addresses.push(v),
                        _ => {}
                    }
                }
                acc.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn parse_reboot_needed(body: &str) -> bool {
    first_text(body, "RebootNeeded")
        .and_then(|v| parse_bool(&v))
        .unwrap_or(false)
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn non_empty(v: &str) -> Option<String> {
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

// ---------------------------------------------------------------------------
// Lockstep URL-rewrite helpers (pure)
// ---------------------------------------------------------------------------
//
// A dangerous IP change rewrites the stored `ingest.url` +
// `onvif_endpoint` to the new address atomically with the apply (the
// edge would otherwise lose the camera even though the change
// "succeeded"); a safe protocol port change rewrites just the port.
// These are pure so they unit-test without a store or a network.

/// Rewrite the host of a URL in place, preserving scheme / port /
/// path / userinfo. `new_host` is an IP literal or DNS name.
pub fn set_url_host(url: &mut url::Url, new_host: &str) -> Result<(), String> {
    url.set_host(Some(new_host))
        .map_err(|e| format!("invalid host {new_host}: {e}"))
}

/// Rewrite the host of an endpoint URL string (the stored
/// `onvif_endpoint`), returning the new string.
pub fn endpoint_with_host(endpoint: &str, new_host: &str) -> Result<String, String> {
    let mut u =
        url::Url::parse(endpoint).map_err(|e| format!("invalid endpoint {endpoint}: {e}"))?;
    set_url_host(&mut u, new_host)?;
    Ok(u.to_string())
}

/// Rewrite the port of an endpoint URL string (the stored
/// `onvif_endpoint`), returning the new string.
pub fn endpoint_with_port(endpoint: &str, port: u16) -> Result<String, String> {
    let mut u =
        url::Url::parse(endpoint).map_err(|e| format!("invalid endpoint {endpoint}: {e}"))?;
    u.set_port(Some(port))
        .map_err(|_| format!("cannot set port {port} on {endpoint}"))?;
    Ok(u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(inner: &str) -> String {
        format!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><s:Body>{inner}</s:Body></s:Envelope>"#
        )
    }

    #[test]
    fn parses_static_interface_with_address_and_mtu() {
        let xml = wrap(
            r#"<tds:GetNetworkInterfacesResponse><tds:NetworkInterfaces token="eth0">
              <tt:Enabled>true</tt:Enabled>
              <tt:Info><tt:Name>eth0</tt:Name><tt:HwAddress>AA:BB:CC:DD:EE:FF</tt:HwAddress><tt:MTU>1500</tt:MTU></tt:Info>
              <tt:IPv4><tt:Enabled>true</tt:Enabled><tt:Config>
                <tt:Manual><tt:Address>192.168.1.50</tt:Address><tt:PrefixLength>24</tt:PrefixLength></tt:Manual>
                <tt:DHCP>false</tt:DHCP>
              </tt:Config></tt:IPv4>
            </tds:NetworkInterfaces></tds:GetNetworkInterfacesResponse>"#,
        );
        let ifaces = parse_network_interfaces(&xml);
        assert_eq!(ifaces.len(), 1);
        let i = &ifaces[0];
        assert_eq!(i.token, "eth0");
        assert_eq!(i.enabled, Some(true));
        assert_eq!(i.name.as_deref(), Some("eth0"));
        assert_eq!(i.hw_address.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(i.mtu, Some(1500));
        assert_eq!(i.ipv4_enabled, Some(true));
        assert_eq!(i.ipv4_dhcp, Some(false));
        assert_eq!(i.ipv4_addresses.len(), 1);
        assert_eq!(i.ipv4_addresses[0].address, "192.168.1.50");
        assert_eq!(i.ipv4_addresses[0].prefix_length, Some(24));
        assert_eq!(i.ipv4_addresses[0].origin, "manual");
    }

    #[test]
    fn parses_dhcp_interface_with_leased_address() {
        let xml = wrap(
            r#"<tds:GetNetworkInterfacesResponse><tds:NetworkInterfaces token="eth0">
              <tt:Enabled>true</tt:Enabled>
              <tt:IPv4><tt:Enabled>true</tt:Enabled><tt:Config>
                <tt:FromDHCP><tt:Address>10.0.0.23</tt:Address><tt:PrefixLength>8</tt:PrefixLength></tt:FromDHCP>
                <tt:DHCP>true</tt:DHCP>
              </tt:Config></tt:IPv4>
            </tds:NetworkInterfaces></tds:GetNetworkInterfacesResponse>"#,
        );
        let ifaces = parse_network_interfaces(&xml);
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].ipv4_dhcp, Some(true));
        assert_eq!(ifaces[0].ipv4_addresses[0].origin, "dhcp");
        assert_eq!(ifaces[0].ipv4_addresses[0].address, "10.0.0.23");
    }

    #[test]
    fn parses_default_gateway() {
        let xml = wrap(
            r#"<tds:GetNetworkDefaultGatewayResponse><tds:NetworkGateway>
              <tt:IPv4Address>192.168.1.1</tt:IPv4Address>
            </tds:NetworkGateway></tds:GetNetworkDefaultGatewayResponse>"#,
        );
        let gw = parse_default_gateway(&xml);
        assert_eq!(gw.ipv4, vec!["192.168.1.1".to_string()]);
        assert!(gw.ipv6.is_empty());
    }

    #[test]
    fn parses_dns_manual() {
        let xml = wrap(
            r#"<tds:GetDNSResponse><tds:DNSInformation>
              <tt:FromDHCP>false</tt:FromDHCP>
              <tt:SearchDomain>example.com</tt:SearchDomain>
              <tt:DNSManual><tt:Type>IPv4</tt:Type><tt:IPv4Address>8.8.8.8</tt:IPv4Address></tt:DNSManual>
              <tt:DNSManual><tt:Type>IPv4</tt:Type><tt:IPv4Address>1.1.1.1</tt:IPv4Address></tt:DNSManual>
            </tds:DNSInformation></tds:GetDNSResponse>"#,
        );
        let dns = parse_dns(&xml);
        assert!(!dns.from_dhcp);
        assert_eq!(dns.search_domain, vec!["example.com".to_string()]);
        assert_eq!(
            dns.servers,
            vec!["8.8.8.8".to_string(), "1.1.1.1".to_string()]
        );
    }

    #[test]
    fn parses_hostname() {
        let xml = wrap(
            r#"<tds:GetHostnameResponse><tds:HostnameInformation>
              <tt:FromDHCP>false</tt:FromDHCP><tt:Name>cam-front</tt:Name>
            </tds:HostnameInformation></tds:GetHostnameResponse>"#,
        );
        let h = parse_hostname(&xml);
        assert!(!h.from_dhcp);
        assert_eq!(h.name.as_deref(), Some("cam-front"));
    }

    #[test]
    fn parses_network_protocols() {
        let xml = wrap(
            r#"<tds:GetNetworkProtocolsResponse>
              <tds:NetworkProtocols><tt:Name>HTTP</tt:Name><tt:Enabled>true</tt:Enabled><tt:Port>80</tt:Port></tds:NetworkProtocols>
              <tds:NetworkProtocols><tt:Name>RTSP</tt:Name><tt:Enabled>true</tt:Enabled><tt:Port>554</tt:Port></tds:NetworkProtocols>
            </tds:GetNetworkProtocolsResponse>"#,
        );
        let protos = parse_network_protocols(&xml);
        assert_eq!(protos.len(), 2);
        assert_eq!(protos[0].name, "HTTP");
        assert_eq!(protos[0].ports, vec![80]);
        assert_eq!(protos[1].name, "RTSP");
        assert!(protos[1].enabled);
        assert_eq!(protos[1].ports, vec![554]);
    }

    #[test]
    fn parses_zero_configuration() {
        let xml = wrap(
            r#"<tds:GetZeroConfigurationResponse><tds:ZeroConfiguration>
              <tt:InterfaceToken>eth0</tt:InterfaceToken><tt:Enabled>false</tt:Enabled>
              <tt:Addresses>169.254.1.2</tt:Addresses>
            </tds:ZeroConfiguration></tds:GetZeroConfigurationResponse>"#,
        );
        let z = parse_zero_configuration(&xml);
        assert_eq!(z.interface_token.as_deref(), Some("eth0"));
        assert_eq!(z.enabled, Some(false));
        assert_eq!(z.addresses, vec!["169.254.1.2".to_string()]);
    }

    #[test]
    fn parses_reboot_needed() {
        let yes = wrap("<tds:SetNetworkInterfacesResponse><tds:RebootNeeded>true</tds:RebootNeeded></tds:SetNetworkInterfacesResponse>");
        let no = wrap("<tds:SetNetworkInterfacesResponse><tds:RebootNeeded>false</tds:RebootNeeded></tds:SetNetworkInterfacesResponse>");
        assert!(parse_reboot_needed(&yes));
        assert!(!parse_reboot_needed(&no));
    }

    #[test]
    fn set_dns_body_emits_manual_only_when_not_from_dhcp() {
        let manual = set_dns_body(false, &["8.8.8.8".into()], Some("example.com"));
        assert!(manual.contains("<tds:FromDHCP>false</tds:FromDHCP>"));
        assert!(manual.contains("<tt:Type>IPv4</tt:Type><tt:IPv4Address>8.8.8.8</tt:IPv4Address>"));
        assert!(manual.contains("<tds:SearchDomain>example.com</tds:SearchDomain>"));

        let dhcp = set_dns_body(true, &["8.8.8.8".into()], None);
        assert!(dhcp.contains("<tds:FromDHCP>true</tds:FromDHCP>"));
        assert!(!dhcp.contains("DNSManual"));
    }

    #[test]
    fn set_interfaces_body_static_carries_address_dhcp_omits_it() {
        let stat = set_network_interfaces_body(&Ipv4InterfaceSet {
            token: "eth0".into(),
            enabled: Some(true),
            mtu: Some(1400),
            dhcp: false,
            address: Some("192.168.1.77".into()),
            prefix_length: Some(24),
        });
        assert!(stat.contains("<tds:InterfaceToken>eth0</tds:InterfaceToken>"));
        assert!(stat.contains("<tt:MTU>1400</tt:MTU>"));
        assert!(stat.contains("<tt:DHCP>false</tt:DHCP>"));
        assert!(stat.contains("<tt:Address>192.168.1.77</tt:Address>"));
        assert!(stat.contains("<tt:PrefixLength>24</tt:PrefixLength>"));

        let dhcp = set_network_interfaces_body(&Ipv4InterfaceSet {
            token: "eth0".into(),
            dhcp: true,
            address: Some("192.168.1.77".into()),
            ..Ipv4InterfaceSet::default()
        });
        assert!(dhcp.contains("<tt:DHCP>true</tt:DHCP>"));
        assert!(!dhcp.contains("<tt:Manual>"));
    }

    #[test]
    fn set_protocols_body_includes_port_when_present() {
        let body = set_network_protocols_body(&[NetworkProtocolSet {
            name: "RTSP".into(),
            enabled: true,
            port: Some(8554),
        }]);
        assert!(body.contains("<tt:Name>RTSP</tt:Name>"));
        assert!(body.contains("<tt:Enabled>true</tt:Enabled>"));
        assert!(body.contains("<tt:Port>8554</tt:Port>"));
    }

    #[test]
    fn set_default_gateway_body_filters_blank() {
        let body = set_default_gateway_body(&["192.168.1.1".into(), "  ".into()]);
        assert_eq!(body.matches("<tds:IPv4Address>").count(), 1);
        assert!(body.contains("<tds:IPv4Address>192.168.1.1</tds:IPv4Address>"));
    }

    #[test]
    fn rewrite_rtsp_host_preserves_port_path_and_userinfo() {
        let mut u = url::Url::parse("rtsp://admin:secret@192.168.1.50:554/Streaming/101").unwrap();
        set_url_host(&mut u, "10.9.9.9").unwrap();
        assert_eq!(u.host_str(), Some("10.9.9.9"));
        assert_eq!(u.port(), Some(554));
        assert_eq!(u.path(), "/Streaming/101");
        assert_eq!(u.username(), "admin");
        assert_eq!(u.password(), Some("secret"));
    }

    #[test]
    fn rewrite_onvif_endpoint_host() {
        let out =
            endpoint_with_host("http://192.168.1.50:80/onvif/device_service", "10.9.9.9").unwrap();
        assert_eq!(out, "http://10.9.9.9/onvif/device_service");
    }

    #[test]
    fn rewrite_rtsp_endpoint_port() {
        let out = endpoint_with_port("rtsp://192.168.1.50:554/Streaming/101", 8554).unwrap();
        assert_eq!(out, "rtsp://192.168.1.50:8554/Streaming/101");
    }
}

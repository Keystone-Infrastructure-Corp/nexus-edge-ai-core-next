//! WS-Discovery (ONVIF) probe over UDP multicast.
//!
//! Sends two SOAP `<wsd:Probe>` envelopes to
//! `239.255.255.250:3702` — one scoped to
//! `dn:NetworkVideoTransmitter` (ONVIF NVTs) and one scoped to
//! `tds:Device` (broader ONVIF devices) — then listens on the
//! same UDP socket for 5 seconds. Each incoming `ProbeMatch` SOAP
//! envelope is parsed with `quick-xml` for:
//!
//! * `<wsa:Address>` — opaque URN used as the de-dup key.
//! * `<wsd:XAddrs>` — space-separated list of device-service URLs.
//!   The first `http://<host>:<port>/onvif/device_service`
//!   contributes the IP + port to the [`DiscoveredDevice`].
//! * `<wsd:Scopes>` — space-separated `onvif://www.onvif.org/...`
//!   URIs that ONVIF vendors stuff with name / hardware / MAC.
//!
//! No replies = empty `found` list + `state: Done` (the network
//! has no ONVIF cameras OR the host's outbound multicast is
//! firewalled). Send-side failures are surfaced as
//! `state: Error`.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use quick_xml::events::Event;
use quick_xml::Reader;
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{debug, warn};
use url::Url;
use uuid::Uuid;

use super::{mark_finished, DeviceKind, DiscoveredDevice, DiscoverySessions, SessionInner};

const WS_DISCOVERY_ADDR: &str = "239.255.255.250:3702";

/// Total listen window for ProbeMatch replies. ONVIF cameras
/// usually answer within ~1 s; 5 s is a safe upper bound that
/// also catches sleepy hardware.
const LISTEN_WINDOW: Duration = Duration::from_secs(5);

/// Per-recv timeout. Smaller than `LISTEN_WINDOW` so the loop
/// can quickly notice the deadline.
const RECV_POLL: Duration = Duration::from_millis(250);

pub(crate) async fn run_session(
    _sessions: DiscoverySessions,
    _session_id: Uuid,
    inner: Arc<Mutex<SessionInner>>,
) {
    match probe_network().await {
        Ok(devices) => {
            {
                let mut guard = inner.lock();
                for d in devices {
                    guard.found.push(d);
                }
            }
            mark_finished(&inner, None);
        }
        Err(err) => {
            warn!(error = %err, "ONVIF WS-Discovery probe failed");
            mark_finished(&inner, Some(err));
        }
    }
}

/// Send the two Probe envelopes, listen `LISTEN_WINDOW`, return
/// the de-duplicated [`DiscoveredDevice`] list.
async fn probe_network() -> Result<Vec<DiscoveredDevice>, String> {
    // Bind on the wildcard with an OS-assigned port. On macOS
    // and Linux we don't need to join the WS-Discovery group to
    // receive unicast replies aimed at our ephemeral port.
    let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
        .await
        .map_err(|e| format!("udp bind failed: {e}"))?;
    socket
        .set_multicast_loop_v4(false)
        .map_err(|e| format!("set_multicast_loop_v4 failed: {e}"))?;

    // Two scoped probes go out back-to-back. Same MessageID would
    // confuse downstream replies; mint a fresh uuid each time.
    // (`Uuid::now_v7` is what the workspace `uuid` feature set
    // exposes; the WS-Discovery spec only requires uniqueness,
    // not v4 specifically.)
    let probe_nvt = build_probe_envelope(&Uuid::now_v7(), "dn:NetworkVideoTransmitter");
    let probe_dev = build_probe_envelope(&Uuid::now_v7(), "tds:Device");
    let target: SocketAddr = WS_DISCOVERY_ADDR
        .parse()
        .expect("WS-Discovery address is a valid literal SocketAddr");

    socket
        .send_to(probe_nvt.as_bytes(), target)
        .await
        .map_err(|e| format!("udp send (NVT probe) failed: {e}"))?;
    socket
        .send_to(probe_dev.as_bytes(), target)
        .await
        .map_err(|e| format!("udp send (Device probe) failed: {e}"))?;

    // Listen until LISTEN_WINDOW elapses, collecting unique
    // ProbeMatches keyed by the `wsa:Address` URN.
    let deadline = tokio::time::Instant::now() + LISTEN_WINDOW;
    let mut seen: HashSet<String> = HashSet::new();
    let mut devices: Vec<DiscoveredDevice> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024]; // SOAP envelopes are small but headroom is free.

    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let poll = remaining.min(RECV_POLL);
        let (n, _peer) = match timeout(poll, socket.recv_from(&mut buf)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                debug!(error = %e, "ws-discovery recv error (continuing)");
                continue;
            }
            Err(_) => continue, // poll timeout — loop checks deadline
        };
        let body = match std::str::from_utf8(&buf[..n]) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let Some(parsed) = parse_probe_match(body) else {
            continue;
        };
        if !seen.insert(parsed.address.clone()) {
            continue;
        }
        if let Some(d) = parsed.into_device() {
            devices.push(d);
        }
    }

    Ok(devices)
}

/// Build a single Probe envelope scoped to one Type. The
/// MessageID must be a fresh UUID per spec; we use `Uuid::new_v4`.
fn build_probe_envelope(message_id: &Uuid, types: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing"
            xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
            xmlns:dn="http://www.onvif.org/ver10/network/wsdl"
            xmlns:tds="http://www.onvif.org/ver10/device/wsdl">
  <s:Header>
    <a:MessageID>uuid:{message_id}</a:MessageID>
    <a:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To>
    <a:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</a:Action>
  </s:Header>
  <s:Body>
    <d:Probe>
      <d:Types>{types}</d:Types>
    </d:Probe>
  </s:Body>
</s:Envelope>"#
    )
}

/// Intermediate representation of a parsed `<d:ProbeMatch>`.
/// `address` is the URN used for de-dup. `xaddrs` and `scopes`
/// are the raw whitespace-joined fields straight from the
/// envelope.
#[derive(Debug, Default)]
struct ProbeMatch {
    address: String,
    xaddrs: String,
    scopes: String,
}

impl ProbeMatch {
    /// Turn a parsed ProbeMatch into a [`DiscoveredDevice`]. Drops
    /// matches with no usable XAddrs (the URN alone is not enough
    /// — we need an IP to talk to).
    fn into_device(self) -> Option<DiscoveredDevice> {
        let (host, port) = first_xaddr_host_port(&self.xaddrs)?;
        let scopes = parse_scopes(&self.scopes);
        // Preserve the verbatim XAddrs so the ONVIF Media probe
        // can talk SOAP to the exact endpoint the camera
        // advertised — some vendors put the device service on
        // a custom path (e.g. `/onvif/Device`, not
        // `/onvif/device_service`) and re-deriving the URL from
        // host + port would break those.
        let xaddrs = if self.xaddrs.trim().is_empty() {
            None
        } else {
            Some(self.xaddrs.clone())
        };
        Some(DiscoveredDevice {
            ip: host,
            port,
            kind: DeviceKind::Onvif,
            // WS-Discovery only tells us the ONVIF web port (XAddrs
            // is always the HTTP device_service URL). RTSP lives
            // elsewhere — leave None and let the UI default to 554.
            rtsp_port: None,
            vendor: scopes.vendor,
            model: scopes.name.or_else(|| scopes.hardware.clone()),
            hardware: scopes.hardware,
            firmware: scopes.firmware,
            mac: scopes.mac,
            onvif_xaddrs: xaddrs,
            rtsp_paths: Vec::new(),
            codec: None,
        })
    }
}

/// Walk the envelope with `quick-xml`. We're looking for three
/// leaf elements anywhere under a `<d:ProbeMatch>` (namespace
/// suffix match is fine; vendors disagree on the prefix):
///
/// * `EndpointReference/Address`
/// * `XAddrs`
/// * `Scopes`
fn parse_probe_match(body: &str) -> Option<ProbeMatch> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut in_match = false;
    let mut current: Option<&'static str> = None;
    let mut acc = ProbeMatch::default();

    loop {
        let evt = reader.read_event_into(&mut buf).ok()?;
        match evt {
            Event::Start(e) => {
                let name = local_name(&e.name());
                if name.eq_ignore_ascii_case("ProbeMatch") {
                    in_match = true;
                } else if in_match {
                    current = match name.as_str() {
                        "Address" => Some("address"),
                        "XAddrs" => Some("xaddrs"),
                        "Scopes" => Some("scopes"),
                        _ => None,
                    };
                }
            }
            Event::Text(t) if in_match && current.is_some() => {
                let Ok(raw) = t.unescape() else {
                    continue;
                };
                let value = raw.to_string();
                match current {
                    Some("address") if acc.address.is_empty() => acc.address = value,
                    Some("xaddrs") if acc.xaddrs.is_empty() => acc.xaddrs = value,
                    Some("scopes") if acc.scopes.is_empty() => acc.scopes = value,
                    _ => {}
                }
            }
            Event::End(e) => {
                let name = local_name(&e.name());
                if name.eq_ignore_ascii_case("ProbeMatch") {
                    in_match = false;
                }
                current = None;
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    if acc.address.is_empty() {
        None
    } else {
        Some(acc)
    }
}

/// `quick_xml::name::QName` carries the full `prefix:local` form;
/// strip the prefix so we can match on `Address` / `XAddrs` /
/// `Scopes` regardless of whether the vendor used `wsa:` or
/// `d:` or no prefix at all.
fn local_name(qname: &quick_xml::name::QName) -> String {
    let raw = qname.as_ref();
    let s = std::str::from_utf8(raw).unwrap_or("");
    match s.find(':') {
        Some(idx) => s[idx + 1..].to_string(),
        None => s.to_string(),
    }
}

/// Pull the host + port from the first usable XAddrs URL.
/// `<wsd:XAddrs>` is whitespace-separated; we want the first
/// `http://<host>:<port>/...` entry.
fn first_xaddr_host_port(xaddrs: &str) -> Option<(String, u16)> {
    for token in xaddrs.split_whitespace() {
        let url = match Url::parse(token) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let host = url.host_str()?.to_string();
        let port = url.port().unwrap_or_else(|| match url.scheme() {
            "https" => 443,
            _ => 80,
        });
        return Some((host, port));
    }
    None
}

#[derive(Debug, Default)]
struct ParsedScopes {
    vendor: Option<String>,
    name: Option<String>,
    hardware: Option<String>,
    firmware: Option<String>,
    mac: Option<String>,
}

/// Parse `<wsd:Scopes>` into vendor / model / MAC. Scope URIs
/// look like `onvif://www.onvif.org/<category>/<value>` —
/// vendors are inconsistent about which categories they ship
/// but the common ones are documented in the ONVIF Core
/// Specification §7.3.1.2 (Default scopes).
fn parse_scopes(scopes: &str) -> ParsedScopes {
    let mut out = ParsedScopes::default();
    for token in scopes.split_whitespace() {
        let Some(rest) = token.strip_prefix("onvif://www.onvif.org/") else {
            continue;
        };
        // `category/value` — but some vendors emit `value` with
        // further slashes (a model name containing `/` is legal
        // per spec). Split on the FIRST slash only.
        let Some((category, value)) = rest.split_once('/') else {
            continue;
        };
        let value = url_decode(value);
        match category.to_ascii_lowercase().as_str() {
            "name" => {
                // Some vendors (Hikvision) put `Hikvision-DS-2CD…`
                // in name and nothing in hardware; treat name as
                // the model AND infer vendor from the prefix when
                // possible.
                if out.vendor.is_none() {
                    if let Some(first) = value.split('-').next() {
                        if !first.is_empty() {
                            out.vendor = Some(first.to_string());
                        }
                    }
                }
                out.name = Some(value);
            }
            "manufacturer" | "vendor" => {
                out.vendor = Some(value);
            }
            "hardware" => {
                out.hardware = Some(value);
            }
            "firmware" => {
                out.firmware = Some(value);
            }
            "mac" => {
                out.mac = Some(value);
            }
            _ => {}
        }
    }
    out
}

/// Minimal `application/x-www-form-urlencoded`-style decoder
/// that only handles `%HH`. Scope values are short and never
/// contain `+`; pulling in a full URL-encoding crate is overkill.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PROBE_MATCH: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
            xmlns:wsd="http://schemas.xmlsoap.org/ws/2005/04/discovery"
            xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
  <s:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</wsa:Action>
    <wsa:RelatesTo>uuid:e1bc99a7-ef34-4030-a3a3-3c5c3f4ed3a8</wsa:RelatesTo>
  </s:Header>
  <s:Body>
    <wsd:ProbeMatches>
      <wsd:ProbeMatch>
        <wsa:EndpointReference>
          <wsa:Address>urn:uuid:11111111-2222-3333-4444-555555555555</wsa:Address>
        </wsa:EndpointReference>
        <wsd:Types>dn:NetworkVideoTransmitter</wsd:Types>
        <wsd:Scopes>onvif://www.onvif.org/Profile/Streaming onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/name/Hikvision-DS-2CD2042WD-I onvif://www.onvif.org/hardware/DS-2CD2042WD-I onvif://www.onvif.org/firmware/V5.5.0_build_170725 onvif://www.onvif.org/MAC/aa:bb:cc:dd:ee:ff</wsd:Scopes>
        <wsd:XAddrs>http://192.168.1.100:80/onvif/device_service</wsd:XAddrs>
        <wsd:MetadataVersion>1</wsd:MetadataVersion>
      </wsd:ProbeMatch>
    </wsd:ProbeMatches>
  </s:Body>
</s:Envelope>"#;

    #[test]
    fn parses_full_probe_match_with_xaddrs_and_scopes() {
        let pm = parse_probe_match(SAMPLE_PROBE_MATCH).expect("envelope is a probe match");
        assert_eq!(pm.address, "urn:uuid:11111111-2222-3333-4444-555555555555");
        assert!(pm.xaddrs.starts_with("http://192.168.1.100:80/onvif"));
        assert!(pm.scopes.contains("/hardware/DS-2CD2042WD-I"));

        let device = pm.into_device().expect("xaddrs produced an ip:port");
        assert_eq!(device.ip, "192.168.1.100");
        assert_eq!(device.port, 80);
        assert_eq!(device.kind, DeviceKind::Onvif);
        assert_eq!(device.vendor.as_deref(), Some("Hikvision"));
        assert_eq!(device.model.as_deref(), Some("Hikvision-DS-2CD2042WD-I"));
        assert_eq!(device.hardware.as_deref(), Some("DS-2CD2042WD-I"));
        assert_eq!(device.firmware.as_deref(), Some("V5.5.0_build_170725"));
        assert_eq!(device.mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
    }

    #[test]
    fn parses_probe_match_without_namespace_prefix() {
        // Some cheap NVRs ship envelopes that omit the wsa: / wsd:
        // prefixes entirely and rely on default xmlns. We still
        // need to extract the same fields.
        let body = r#"<?xml version="1.0"?>
<Envelope xmlns="http://www.w3.org/2003/05/soap-envelope">
  <Body>
    <ProbeMatches xmlns="http://schemas.xmlsoap.org/ws/2005/04/discovery">
      <ProbeMatch>
        <EndpointReference xmlns="http://schemas.xmlsoap.org/ws/2004/08/addressing">
          <Address>urn:uuid:cheap-nvr-1</Address>
        </EndpointReference>
        <XAddrs>http://10.0.0.7:8080/onvif/device_service</XAddrs>
        <Scopes>onvif://www.onvif.org/name/CheapNVR</Scopes>
      </ProbeMatch>
    </ProbeMatches>
  </Body>
</Envelope>"#;
        let pm = parse_probe_match(body).unwrap();
        let device = pm.into_device().unwrap();
        assert_eq!(device.ip, "10.0.0.7");
        assert_eq!(device.port, 8080);
        assert_eq!(device.model.as_deref(), Some("CheapNVR"));
    }

    #[test]
    fn parses_https_xaddr_with_no_explicit_port() {
        // Default-port fallback: https → 443.
        let body = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
            xmlns:wsd="http://schemas.xmlsoap.org/ws/2005/04/discovery">
  <s:Body>
    <wsd:ProbeMatches>
      <wsd:ProbeMatch>
        <wsa:EndpointReference><wsa:Address>urn:uuid:x</wsa:Address></wsa:EndpointReference>
        <wsd:XAddrs>https://cam.local/onvif/device_service</wsd:XAddrs>
        <wsd:Scopes></wsd:Scopes>
      </wsd:ProbeMatch>
    </wsd:ProbeMatches>
  </s:Body>
</s:Envelope>"#;
        let pm = parse_probe_match(body).unwrap();
        let device = pm.into_device().unwrap();
        assert_eq!(device.ip, "cam.local");
        assert_eq!(device.port, 443);
    }

    #[test]
    fn url_decode_handles_percent_escapes() {
        assert_eq!(url_decode("Hikvision%20Camera"), "Hikvision Camera");
        assert_eq!(url_decode("plain"), "plain");
        // Malformed % sequences pass through.
        assert_eq!(url_decode("ab%2"), "ab%2");
        assert_eq!(url_decode("ab%ZZ"), "ab%ZZ");
    }

    #[test]
    fn parse_scopes_extracts_manufacturer_and_hardware() {
        let scopes = "onvif://www.onvif.org/manufacturer/Axis \
                      onvif://www.onvif.org/hardware/M3025-VE \
                      onvif://www.onvif.org/Profile/G";
        let parsed = parse_scopes(scopes);
        assert_eq!(parsed.vendor.as_deref(), Some("Axis"));
        assert_eq!(parsed.hardware.as_deref(), Some("M3025-VE"));
        assert_eq!(parsed.name, None);
        assert_eq!(parsed.mac, None);
    }

    #[test]
    fn build_probe_envelope_has_unique_message_id_and_type() {
        let id = Uuid::now_v7();
        let env = build_probe_envelope(&id, "dn:NetworkVideoTransmitter");
        assert!(env.contains(&format!("uuid:{id}")));
        assert!(env.contains("dn:NetworkVideoTransmitter"));
        assert!(env.contains("<s:Envelope"));
    }

    #[test]
    fn parse_probe_match_returns_none_for_non_match_envelope() {
        let body = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body><Hello/></s:Body>
</s:Envelope>"#;
        assert!(parse_probe_match(body).is_none());
    }
}

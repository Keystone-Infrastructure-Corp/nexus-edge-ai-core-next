//! Bounded-concurrency CIDR sweep.
//!
//! Per host we race three TCP probes (defaults: 554, 80, 8080) and
//! one UDP SIP probe:
//!
//! * **Port 554 (RTSP)** — send `OPTIONS rtsp://<host>:554/
//!   RTSP/1.0\r\nCSeq: 1\r\n\r\n` and look for a `Public:` line
//!   in the reply. Confirms an RTSP server is listening.
//! * **Port 80 / 8080 (ONVIF SOAP)** — POST a minimal
//!   `<tds:GetDeviceInformation>` envelope to
//!   `/onvif/device_service`. A `200` with a SOAP body, OR a
//!   `401 WWW-Authenticate: Digest realm="…"`, both confirm an
//!   ONVIF endpoint. Plain HTML server banners are intentionally
//!   NOT counted (avoids false positives from random web servers).
//! * **UDP 5060 (SIP)** — a SIP `OPTIONS` ping. Any final reply marks
//!   an IP speaker (they do not answer WS-Discovery); it is reported
//!   in the session's `speakers`, not `found`.
//!
//! Concurrency is bounded by a [`tokio::sync::Semaphore`]; default
//! is 64 inflight probes. The semaphore holds **outstanding hosts**,
//! not outstanding sockets — each in-flight host opens up to
//! `ports.len()` sockets in parallel.
//!
//! Per-probe RTT budget is 2 s, connect timeout 1.5 s. A host
//! whose every port times out contributes `scanned += 1` and no
//! `DiscoveredDevice` to the session.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::debug;
use uuid::Uuid;

use super::{
    mark_finished, DeviceKind, DiscoveredDevice, DiscoveredSpeaker, DiscoverySessions, ScanPlan,
    SessionInner,
};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(1_500);
const READ_TIMEOUT: Duration = Duration::from_millis(2_000);
const SIP_PORT: u16 = 5060;

/// Drive the sweep for one session. Updates `inner` live; marks
/// it finished when every host has been probed.
pub(crate) async fn run_session(
    sessions: DiscoverySessions,
    session_id: Uuid,
    inner: Arc<Mutex<SessionInner>>,
    plan: ScanPlan,
) {
    let sem = Arc::new(Semaphore::new(plan.concurrency));
    let ports = Arc::new(plan.ports);
    let bind_source = plan.bind_source;
    let mut joins = Vec::with_capacity(plan.total_targets as usize);

    for host in plan.cidr.hosts() {
        let permit_sem = sem.clone();
        let ports = ports.clone();
        let inner = inner.clone();
        joins.push(tokio::spawn(async move {
            // `acquire_owned` returns once a slot is free. The
            // permit drops at end-of-block (i.e. when the host's
            // probes finish), releasing the slot for the next
            // queued host.
            let _permit = match permit_sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let (device, speaker) =
                probe_host(IpAddr::V4(host), &ports, bind_source, SIP_PORT).await;
            let mut guard = inner.lock();
            guard.scanned = guard.scanned.saturating_add(1);
            if let Some(dev) = device {
                guard.found.push(dev);
            }
            if let Some(sp) = speaker {
                guard.speakers.push(sp);
            }
        }));
    }

    for j in joins {
        let _ = j.await;
    }

    mark_finished(&inner, None);
    // Touch the registry to keep it hot — and to be explicit
    // that we keep a strong ref to it for the duration of the
    // task even though we no longer write to it after this point.
    let _ = sessions.get(&session_id);
}

/// Race the per-port probes (plus the SIP ping) for a single host.
/// The device is `None` if every TCP probe failed (closed, refused,
/// or timed out). Otherwise it is the highest-fidelity result: ONVIF
/// beats RTSP-only (an NVT always speaks RTSP too, so reporting it
/// as ONVIF is strictly more informative). The speaker is `Some`
/// iff the host answered SIP `OPTIONS` on `sip_port`, independent
/// of the device result.
async fn probe_host(
    host: IpAddr,
    ports: &[u16],
    bind: Option<Ipv4Addr>,
    sip_port: u16,
) -> (Option<DiscoveredDevice>, Option<DiscoveredSpeaker>) {
    // shortcut: `options_ping` cannot source-bind its UDP socket, so a
    // sweep pinned to the camera NIC skips it rather than let a probe
    // leave by OS routing; lift once the helper takes a bind address.
    let sip = bind
        .is_none()
        .then(|| tokio::spawn(probe_sip(host, sip_port)));
    let mut tasks = Vec::with_capacity(ports.len());
    for &port in ports {
        tasks.push(tokio::spawn(probe_one(host, port, bind)));
    }
    let mut best: Option<DiscoveredDevice> = None;
    // Remember the RTSP port even when ONVIF wins the merge.
    // Cameras with both services open lose their RTSP entry when
    // we pick the ONVIF one as `best`; without this side-channel
    // the UI would later send Probe to port 80 and get 405 from
    // the camera's HTTP listener for every candidate path.
    let mut rtsp_port_seen: Option<u16> = None;
    for t in tasks {
        let r = match t.await {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let Some(dev) = r {
            if dev.kind == DeviceKind::Rtsp {
                rtsp_port_seen = Some(dev.port);
            }
            match (&best, dev.kind) {
                (None, _) => best = Some(dev),
                (Some(cur), DeviceKind::Onvif) if cur.kind == DeviceKind::Rtsp => {
                    best = Some(dev);
                }
                _ => {}
            }
        }
    }
    let device = best.map(|mut dev| {
        if dev.rtsp_port.is_none() {
            dev.rtsp_port = rtsp_port_seen.or(if dev.kind == DeviceKind::Rtsp {
                Some(dev.port)
            } else {
                None
            });
        }
        dev
    });
    let speaker = match sip {
        Some(t) => t.await.ok().flatten(),
        None => None,
    };
    (device, speaker)
}

/// SIP `OPTIONS` ping, bounded by the same 2 s budget as a TCP
/// probe's read and run concurrently with them, so it adds nothing
/// to the per-host time.
async fn probe_sip(host: IpAddr, port: u16) -> Option<DiscoveredSpeaker> {
    let ip = host.to_string();
    let ua = crate::talkdown::sip::options_ping(&ip, port, READ_TIMEOUT).await?;
    Some(DiscoveredSpeaker {
        ip,
        port,
        user_agent: Some(ua).filter(|u| !u.is_empty()),
    })
}

/// One TCP probe. Dispatches on port to RTSP vs ONVIF flavour.
async fn probe_one(host: IpAddr, port: u16, bind: Option<Ipv4Addr>) -> Option<DiscoveredDevice> {
    match port {
        554 => probe_rtsp_options(host, port, bind).await,
        80 | 8080 => probe_onvif_soap(host, port, bind).await,
        // Unknown ports get the RTSP probe — most non-standard
        // RTSP deployments live on :8554 / :10554 / similar.
        _ => probe_rtsp_options(host, port, bind).await,
    }
}

async fn probe_rtsp_options(
    host: IpAddr,
    port: u16,
    bind: Option<Ipv4Addr>,
) -> Option<DiscoveredDevice> {
    let req = format!(
        "OPTIONS rtsp://{host}:{port}/ RTSP/1.0\r\n\
         CSeq: 1\r\n\
         User-Agent: nexus-discovery/1\r\n\r\n"
    );
    let buf = match request_response(host, port, req.as_bytes(), bind).await {
        Ok(b) => b,
        Err(e) => {
            debug!(%host, port, error = %e, "RTSP OPTIONS probe failed");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&buf);

    // Accept any RTSP/1.x reply that includes a `Public:` line.
    // Some IP-camera firmwares return `401 Unauthorized` with the
    // `Public:` header before challenging — count those too.
    if !(text.starts_with("RTSP/1.") && text.to_ascii_lowercase().contains("\npublic:")) {
        return None;
    }

    let server = extract_header(&text, "Server");
    let vendor = server.as_deref().and_then(parse_vendor_from_server);

    Some(DiscoveredDevice {
        ip: host.to_string(),
        port,
        kind: DeviceKind::Rtsp,
        rtsp_port: Some(port),
        vendor,
        model: server.clone(),
        hardware: None,
        firmware: None,
        mac: None,
        // CIDR scan only does an RTSP OPTIONS probe — no SOAP
        // service discovery on this path. The ONVIF Media query
        // is skipped for these devices; brute-force path sweep
        // remains the only option.
        onvif_xaddrs: None,
        rtsp_paths: Vec::new(),
        codec: None,
    })
}

/// Minimal ONVIF probe. We POST a tiny `GetDeviceInformation`
/// envelope and accept the endpoint as ONVIF iff:
///
/// * the reply is a `200 OK` with `GetDeviceInformationResponse`
///   in the body, OR
/// * the reply is a `401` with a `WWW-Authenticate: Digest`
///   header (a SOAP endpoint demanding HTTP Digest is, in
///   practice, always an IP camera).
///
/// We deliberately do NOT count a 200 with an HTML body — that's
/// just a web server on :80.
async fn probe_onvif_soap(
    host: IpAddr,
    port: u16,
    bind: Option<Ipv4Addr>,
) -> Option<DiscoveredDevice> {
    const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:tds="http://www.onvif.org/ver10/device/wsdl">
  <s:Body><tds:GetDeviceInformation/></s:Body>
</s:Envelope>"#;
    let req = format!(
        "POST /onvif/device_service HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Content-Type: application/soap+xml; charset=utf-8; \
         action=\"http://www.onvif.org/ver10/device/wsdl/GetDeviceInformation\"\r\n\
         Content-Length: {len}\r\n\
         User-Agent: nexus-discovery/1\r\n\
         Connection: close\r\n\r\n{body}",
        len = BODY.len(),
        body = BODY,
    );
    let buf = match request_response(host, port, req.as_bytes(), bind).await {
        Ok(b) => b,
        Err(e) => {
            debug!(%host, port, error = %e, "ONVIF SOAP probe failed");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&buf);
    let lower = text.to_ascii_lowercase();

    let status_line_ok = text.starts_with("HTTP/1.");
    let is_200_with_soap_body = lower.contains(" 200 ")
        && (lower.contains("getdeviceinformationresponse")
            || lower.contains("envelope")
            || lower.contains("soap"));
    let is_401_with_digest =
        lower.contains(" 401 ") && lower.contains("www-authenticate:") && lower.contains("digest");

    if !(status_line_ok && (is_200_with_soap_body || is_401_with_digest)) {
        return None;
    }

    let server = extract_header(&text, "Server");
    let vendor = server.as_deref().and_then(parse_vendor_from_server);

    Some(DiscoveredDevice {
        ip: host.to_string(),
        port,
        kind: DeviceKind::Onvif,
        // Leave None — the merge in `probe_host` will fill this
        // in if the parallel 554 probe also succeeded, otherwise
        // the UI defaults to 554 (correct for ~all IP cameras).
        rtsp_port: None,
        vendor,
        model: server.clone(),
        hardware: None,
        firmware: None,
        mac: None,
        // Synthesise the standard ONVIF service URL from
        // (host, port) so the inline `onvif-streams` probe has
        // something to point at. Vendors that put device_service
        // on a non-standard path won't be reachable via this
        // CIDR-scan find — they need WS-Discovery (which
        // captures the verbatim XAddrs) to populate the real
        // URL. Acceptable: CIDR scan is the fallback path.
        onvif_xaddrs: Some(format!("http://{host}:{port}/onvif/device_service")),
        rtsp_paths: Vec::new(),
        codec: None,
    })
}

/// Connect → write → read a single reply (up to 4 KiB or
/// `READ_TIMEOUT`). The reply size is intentionally bounded — we
/// only need the response head; the full SOAP / SDP body is
/// pulled by the inline Verify probe.
///
/// When `bind` is `Some`, the connecting socket is source-bound to
/// that local IPv4 (the assigned camera interface) so the probe can
/// only leave via the camera NIC. A v6 target with a v4 bind (or a
/// bind IP the kernel rejects) surfaces as a normal connect error
/// and the host is simply counted as "no device".
async fn request_response(
    host: IpAddr,
    port: u16,
    payload: &[u8],
    bind: Option<Ipv4Addr>,
) -> io::Result<Vec<u8>> {
    let addr = SocketAddr::new(host, port);
    let connect = async {
        match bind {
            Some(src) if addr.is_ipv4() => {
                let socket = TcpSocket::new_v4()?;
                socket.bind(SocketAddr::new(IpAddr::V4(src), 0))?;
                socket.connect(addr).await
            }
            _ => TcpStream::connect(addr).await,
        }
    };
    let mut stream = timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timeout"))??;
    stream.set_nodelay(true).ok();

    timeout(READ_TIMEOUT, stream.write_all(payload))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "write timeout"))??;

    let mut buf = vec![0u8; 4096];
    let n = timeout(READ_TIMEOUT, stream.read(&mut buf))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "read timeout"))??;
    buf.truncate(n);
    Ok(buf)
}

/// Pull the value of a header from an HTTP/RTSP response head,
/// case-insensitively. Returns `None` if missing.
fn extract_header(text: &str, name: &str) -> Option<String> {
    let needle = format!("\n{}:", name.to_ascii_lowercase());
    let lower = text.to_ascii_lowercase();
    let start = lower.find(&needle)? + needle.len();
    let line_end = lower[start..]
        .find('\n')
        .map(|p| start + p)
        .unwrap_or(text.len());
    Some(
        text[start..line_end]
            .trim()
            .trim_end_matches('\r')
            .to_string(),
    )
}

/// Cheap vendor guess from a `Server:` banner. Used to populate
/// the live table's Vendor column before the inline Verify probe
/// runs against the device.
fn parse_vendor_from_server(server: &str) -> Option<String> {
    let s = server.to_ascii_lowercase();
    for (needle, label) in [
        ("hikvision", "Hikvision"),
        ("dahua", "Dahua"),
        ("axis", "Axis"),
        ("amcrest", "Amcrest"),
        ("reolink", "Reolink"),
        ("uniview", "Uniview"),
        ("vivotek", "Vivotek"),
        ("avigilon", "Avigilon"),
        ("bosch", "Bosch"),
        ("foscam", "Foscam"),
        ("tp-link", "TP-Link"),
        ("ubiquiti", "Ubiquiti"),
        ("wyze", "Wyze"),
    ] {
        if s.contains(needle) {
            return Some(label.to_string());
        }
    }
    // Fallback: take the first whitespace-delimited token of the
    // raw Server: banner. Better than nothing for unknown vendors.
    server.split_whitespace().next().map(|t| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::DiscoveryKind;

    #[test]
    fn extract_header_finds_server_line() {
        let text = "RTSP/1.0 200 OK\r\nCSeq: 1\r\nServer: Hikvision-DS2CD/V5.5\r\n\r\n";
        assert_eq!(
            extract_header(text, "Server").as_deref(),
            Some("Hikvision-DS2CD/V5.5")
        );
    }

    #[test]
    fn extract_header_is_case_insensitive() {
        let text = "HTTP/1.1 200 OK\r\nserver: Dahua-IPC/2.420\r\n\r\n";
        assert_eq!(
            extract_header(text, "Server").as_deref(),
            Some("Dahua-IPC/2.420")
        );
    }

    #[test]
    fn extract_header_missing_returns_none() {
        let text = "HTTP/1.1 200 OK\r\n\r\n";
        assert_eq!(extract_header(text, "Server"), None);
    }

    #[test]
    fn parse_vendor_recognises_canonical_brands() {
        assert_eq!(
            parse_vendor_from_server("Hikvision-DS2CD/V5.5").as_deref(),
            Some("Hikvision")
        );
        assert_eq!(
            parse_vendor_from_server("dahua-IPC/2.420").as_deref(),
            Some("Dahua")
        );
        assert_eq!(
            parse_vendor_from_server("Axis OS 1.2.3").as_deref(),
            Some("Axis")
        );
    }

    #[test]
    fn parse_vendor_falls_back_to_first_token() {
        assert_eq!(
            parse_vendor_from_server("ACME-IPCam/1.0").as_deref(),
            Some("ACME-IPCam/1.0")
        );
    }

    /// Loopback SIP UAS: answers one OPTIONS with `200 OK`, echoing the
    /// headers `options_ping` correlates on, plus `User-Agent: TSIP`.
    async fn fake_sip_speaker() -> u16 {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = sock.recv_from(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let mut out = String::from("SIP/2.0 200 OK\r\n");
            for line in req.lines().filter(|l| {
                let l = l.to_ascii_lowercase();
                ["via:", "from:", "to:", "call-id:", "cseq:"]
                    .iter()
                    .any(|h| l.starts_with(h))
            }) {
                out.push_str(line);
                out.push_str("\r\n");
            }
            out.push_str("User-Agent: TSIP\r\nContent-Length: 0\r\n\r\n");
            sock.send_to(out.as_bytes(), from).await.unwrap();
        });
        port
    }

    #[tokio::test]
    async fn sip_responder_is_reported_as_speaker_with_user_agent() {
        let port = fake_sip_speaker().await;
        // No TCP ports: only the SIP probe runs.
        let (device, speaker) = probe_host("127.0.0.1".parse().unwrap(), &[], None, port).await;
        assert!(device.is_none());
        let speaker = speaker.expect("SIP responder must be listed");
        assert_eq!(speaker.ip, "127.0.0.1");
        assert_eq!(speaker.port, port);
        assert_eq!(speaker.user_agent.as_deref(), Some("TSIP"));
    }

    #[tokio::test]
    async fn host_without_sip_responder_yields_no_speaker() {
        // Bind then drop: the port is closed, so the probe gets ICMP
        // unreachable (or silence) and must report nothing.
        let port = {
            let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            sock.local_addr().unwrap().port()
        };
        let (device, speaker) = probe_host("127.0.0.1".parse().unwrap(), &[], None, port).await;
        assert!(device.is_none());
        assert!(speaker.is_none());
    }

    #[tokio::test]
    async fn camera_nic_pinned_sweep_skips_the_unbindable_sip_probe() {
        let port = fake_sip_speaker().await;
        let pinned = Some(Ipv4Addr::LOCALHOST);
        let (_, speaker) = probe_host("127.0.0.1".parse().unwrap(), &[], pinned, port).await;
        assert!(speaker.is_none());
    }

    /// Bench check against a real speaker (NEXUS_TALKDOWN_HW=<ip>), driven
    /// through the same session machinery the API uses.
    #[tokio::test]
    #[ignore = "needs a SIP speaker on the LAN: NEXUS_TALKDOWN_HW=<ip>"]
    async fn discovery_hw_cidr_scan_lists_sip_speaker() {
        let ip = std::env::var("NEXUS_TALKDOWN_HW").expect("set NEXUS_TALKDOWN_HW=<speaker ip>");
        let plan = ScanPlan {
            cidr: format!("{ip}/32").parse().unwrap(),
            ports: vec![554, 80, 8080],
            concurrency: 4,
            total_targets: 1,
            bind_source: None,
        };
        let sessions = DiscoverySessions::new();
        let (id, inner) = sessions.start(DiscoveryKind::Scan, Some(1));
        run_session(sessions.clone(), id, inner, plan).await;
        let view = sessions.get(&id).unwrap();
        println!("found={:?} speakers={:?}", view.found, view.speakers);
        let speaker = view
            .speakers
            .iter()
            .find(|s| s.ip == ip)
            .expect("speaker must be listed");
        assert_eq!(speaker.user_agent.as_deref(), Some("TSIP"));
    }
}

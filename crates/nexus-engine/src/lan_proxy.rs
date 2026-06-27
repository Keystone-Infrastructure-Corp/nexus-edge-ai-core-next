//! Phase 7.6.6 — generic LAN device proxy (REPO_BOUNDARY R5c).
//!
//! `POST /api/v1/admin/proxy` lets the operator console reach a
//! non-ONVIF device on the edge's own LAN (camera web UIs, NVRs) by
//! riding the existing cloud → edge `rpc_call` substrate and executing
//! the HTTP request *in-process on the edge*. The cloud never opens an
//! outbound socket to the edge and never learns the device's IP or
//! credentials.
//!
//! This is an explicit, narrowly-scoped exception to the "no credential
//! convenience features" boundary, so every R5c constraint is enforced
//! here unconditionally:
//!
//! * **Opt-in, default off** ([`crate::api::ApiState::lan_proxy_enabled`],
//!   sourced from `[lan_proxy] enabled`). When off, every call is
//!   rejected.
//! * **SSRF-bounded.** Only RFC1918 IPv4 is allowed. Loopback,
//!   link-local `169.254.0.0/16` (including the cloud-metadata IP
//!   `169.254.169.254`), multicast, broadcast, public, and all IPv6 are
//!   rejected.
//! * **DNS-rebind-proof.** The `target` host MUST be a literal IP — a
//!   hostname is rejected outright, so there is no name to re-resolve
//!   between the check and the connection. Redirects are not followed
//!   (a `3xx` cannot bounce the request off the pinned IP).
//! * **Allowlisted to discovered devices** (R5c §4). The target IP must
//!   already be known to the edge: a configured camera's IP or an IP
//!   surfaced by a recent discovery scan.
//! * **Audited + capped** (R5c §1, §5). One `audit_log` row per call
//!   records the actor, target, method, and outcome — **never** the
//!   request headers or body, so a device credential the operator
//!   supplies for the target never lands in any log. Response size and
//!   request time are capped.
//! * **Admin-only** (the cloud gates owner/admin; the actor_token is
//!   verified upstream by the rpc dispatcher per R4c since this is a
//!   `POST`).

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use nexus_store::audit::AuditOutcome;
use nexus_types::Role;

use crate::api::{ApiError, ApiState};
use crate::auth::admin_audit::audit_admin_action;
use crate::auth::require_role::{SessionContext, SessionRejection};

/// Cap on the proxied response body. LAN device web UIs are small; this
/// protects the edge + tunnel from a hostile / unbounded response.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Per-request timeout for the LAN round-trip.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Deserialize)]
pub struct ProxyRequest {
    /// `http(s)://<literal-ip>[:port]/path` — host MUST be a literal IP.
    target: String,
    /// HTTP method to use against the LAN device.
    method: String,
    /// Optional request headers (e.g. the device's own auth). Passed to
    /// the device and discarded; never logged or persisted.
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    /// Optional request body. Never logged.
    #[serde(default)]
    body: Option<String>,
}

#[derive(Serialize)]
struct ProxyResponseEnvelope {
    /// The LAN device's HTTP status.
    status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    /// Base64 of the (capped) response body — keeps arbitrary device
    /// output (HTML / JSON / binary) intact across the JSON tunnel.
    body_base64: String,
}

// ---------------------------------------------------------------------------
// Pure SSRF classification + vetting (no DNS, no I/O — heavily tested)
// ---------------------------------------------------------------------------

/// Classify an IPv4 for the SSRF guard. Only `"lan"` (RFC1918 private)
/// is permitted; every other class is denied with the reason surfaced
/// for the audit row + the operator error.
fn classify_v4(v4: Ipv4Addr) -> &'static str {
    if v4.is_unspecified() {
        "unspecified"
    } else if v4.is_loopback() {
        "loopback"
    } else if v4.is_link_local() {
        // 169.254.0.0/16 — includes the cloud-metadata IP 169.254.169.254.
        "link-local"
    } else if v4.is_broadcast() {
        "broadcast"
    } else if v4.is_multicast() {
        "multicast"
    } else if v4.is_private() {
        "lan"
    } else {
        "public"
    }
}

/// The single allow predicate: RFC1918 IPv4 only.
fn ip_is_lan(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if classify_v4(v4) == "lan")
}

/// Vet a (literal) target IP against the feature flag, the SSRF LAN
/// guard, and the discovery allowlist. Pure — no DNS, no I/O. `Err`
/// carries an operator-facing reason. The allowlist is consulted only
/// AFTER the SSRF class check, so allowlisting can never re-admit a
/// loopback / metadata / public address.
fn vet_target(enabled: bool, ip: IpAddr, allowlist: &HashSet<IpAddr>) -> Result<(), String> {
    if !enabled {
        return Err("LAN device proxy is disabled".to_string());
    }
    if !ip_is_lan(ip) {
        let reason = match ip {
            IpAddr::V4(v4) => format!(
                "target {ip} is {}, not a private LAN address",
                classify_v4(v4)
            ),
            IpAddr::V6(_) => format!("target {ip} is IPv6, which is not allowed"),
        };
        return Err(reason);
    }
    if !allowlist.contains(&ip) {
        return Err(format!("target {ip} is not a discovered LAN device"));
    }
    Ok(())
}

#[derive(Debug)]
struct ParsedTarget {
    scheme: String,
    ip: IpAddr,
    port: u16,
    path_and_query: String,
}

/// Parse + validate the target into scheme / ip / port / path. The host
/// MUST be a literal IP: hostnames are rejected outright, the strongest
/// DNS-rebinding defence (no name to re-resolve between the check and
/// the connection — R5c §3).
fn parse_target(target: &str) -> Result<ParsedTarget, String> {
    let url = url::Url::parse(target).map_err(|e| format!("invalid target URL: {e}"))?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err("target scheme must be http or https".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "target has no host".to_string())?;
    let ip: IpAddr = host.parse().map_err(|_| {
        "target host must be a literal IP address (hostnames are not allowed)".to_string()
    })?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "target has no port".to_string())?;
    let path_and_query = url[url::Position::BeforePath..].to_string();
    Ok(ParsedTarget {
        scheme: scheme.to_string(),
        ip,
        port,
        path_and_query,
    })
}

fn method_from_str(m: &str) -> Result<reqwest::Method, String> {
    reqwest::Method::from_bytes(m.trim().to_ascii_uppercase().as_bytes())
        .map_err(|_| format!("invalid HTTP method: {m}"))
}

/// Build the allowlist of reachable IPs: every IP surfaced by a recent
/// discovery scan, plus every configured camera's ingest / ONVIF IP
/// (the persistent half — cameras were themselves added via discovery).
async fn build_allowlist(s: &ApiState) -> HashSet<IpAddr> {
    let mut set = s.discovery_sessions.discovered_ips();
    if let Ok(cams) = s.store.list_cameras().await {
        for c in cams {
            if let Some(ip) = c
                .ingest
                .url
                .host_str()
                .and_then(|h| h.parse::<IpAddr>().ok())
            {
                set.insert(ip);
            }
            if let Some(ip) = c.onvif.endpoint.as_deref().and_then(|ep| {
                url::Url::parse(ep)
                    .ok()
                    .and_then(|u| u.host_str().and_then(|h| h.parse::<IpAddr>().ok()))
            }) {
                set.insert(ip);
            }
        }
    }
    set
}

fn rbac(ctx: &SessionContext) -> Result<(), ApiError> {
    ctx.require(Role::Admin).map_err(|r| match r {
        SessionRejection::InsufficientRole { .. } => {
            ApiError(StatusCode::FORBIDDEN, "insufficient role".to_string())
        }
        _ => ApiError(
            StatusCode::UNAUTHORIZED,
            "authentication required".to_string(),
        ),
    })
}

/// `POST /v1/admin/proxy` — audited, SSRF-bounded LAN device proxy.
pub async fn proxy_request(
    State(s): State<ApiState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(req): Json<ProxyRequest>,
) -> Result<Response, ApiError> {
    rbac(&ctx)?;
    let ip = peer.ip();

    let parsed = match parse_target(&req.target) {
        Ok(p) => p,
        Err(e) => {
            audit_proxy(
                &s,
                &ctx,
                &headers,
                ip,
                None,
                &req.method,
                AuditOutcome::Denied,
            )
            .await;
            return Err(ApiError(StatusCode::BAD_REQUEST, e));
        }
    };
    let resource = format!("{}:{}", parsed.ip, parsed.port);

    let allowlist = build_allowlist(&s).await;
    if let Err(reason) = vet_target(s.lan_proxy_enabled, parsed.ip, &allowlist) {
        audit_proxy(
            &s,
            &ctx,
            &headers,
            ip,
            Some(&resource),
            &req.method,
            AuditOutcome::Denied,
        )
        .await;
        return Err(ApiError(StatusCode::FORBIDDEN, reason));
    }

    let method = match method_from_str(&req.method) {
        Ok(m) => m,
        Err(e) => {
            audit_proxy(
                &s,
                &ctx,
                &headers,
                ip,
                Some(&resource),
                &req.method,
                AuditOutcome::Denied,
            )
            .await;
            return Err(ApiError(StatusCode::BAD_REQUEST, e));
        }
    };

    match execute(&parsed, method, req.headers.as_ref(), req.body.as_deref()).await {
        Ok(env) => {
            audit_proxy(
                &s,
                &ctx,
                &headers,
                ip,
                Some(&resource),
                &req.method,
                AuditOutcome::Success,
            )
            .await;
            Ok((StatusCode::OK, Json(env)).into_response())
        }
        Err(e) => {
            audit_proxy(
                &s,
                &ctx,
                &headers,
                ip,
                Some(&resource),
                &req.method,
                AuditOutcome::Failure,
            )
            .await;
            Err(ApiError(StatusCode::BAD_GATEWAY, e))
        }
    }
}

async fn execute(
    parsed: &ParsedTarget,
    method: reqwest::Method,
    hdrs: Option<&BTreeMap<String, String>>,
    body: Option<&str>,
) -> Result<ProxyResponseEnvelope, String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // LAN appliances commonly ship self-signed certs.
        .danger_accept_invalid_certs(true)
        // Never follow redirects: a 3xx could bounce the request off the
        // pinned LAN IP to an attacker-chosen host (SSRF escape).
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client build failed: {e}"))?;

    // Connect to the pinned IP literal — no DNS lookup, no rebind.
    let url = format!(
        "{}://{}:{}{}",
        parsed.scheme, parsed.ip, parsed.port, parsed.path_and_query
    );
    let mut rb = client.request(method, &url);
    if let Some(h) = hdrs {
        for (k, v) in h {
            // reqwest manages these itself; forwarding them breaks framing.
            let kl = k.to_ascii_lowercase();
            if kl == "host" || kl == "content-length" || kl == "connection" {
                continue;
            }
            rb = rb.header(k, v);
        }
    }
    if let Some(b) = body {
        rb = rb.body(b.to_string());
    }

    let resp = rb
        .send()
        .await
        .map_err(|e| format!("LAN request failed: {e}"))?;
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("reading LAN response failed: {e}"))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "LAN response too large: {} bytes (cap {MAX_RESPONSE_BYTES})",
            bytes.len()
        ));
    }
    Ok(ProxyResponseEnvelope {
        status,
        content_type,
        body_base64: B64.encode(&bytes),
    })
}

/// One audit row per proxy call. Records the actor, target, method, and
/// outcome ONLY — never the request headers or body, so no device
/// credential the operator supplied can land in the audit log (R5c §1).
async fn audit_proxy(
    s: &ApiState,
    ctx: &SessionContext,
    headers: &HeaderMap,
    ip: IpAddr,
    resource: Option<&str>,
    method: &str,
    outcome: AuditOutcome,
) {
    let detail = serde_json::json!({ "method": method.trim().to_ascii_uppercase() }).to_string();
    audit_admin_action(
        &s.store,
        Some(ctx),
        headers,
        ip,
        "lan_proxy.request",
        "lan_device",
        resource,
        outcome,
        None,
        Some(&detail),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn classify_v4_buckets_every_range() {
        assert_eq!(classify_v4(v4("127.0.0.1")), "loopback");
        assert_eq!(classify_v4(v4("169.254.1.1")), "link-local");
        // The cloud-metadata IP is link-local — the headline SSRF target.
        assert_eq!(classify_v4(v4("169.254.169.254")), "link-local");
        assert_eq!(classify_v4(v4("224.0.0.1")), "multicast");
        assert_eq!(classify_v4(v4("255.255.255.255")), "broadcast");
        assert_eq!(classify_v4(v4("0.0.0.0")), "unspecified");
        assert_eq!(classify_v4(v4("8.8.8.8")), "public");
        assert_eq!(classify_v4(v4("192.168.1.50")), "lan");
        assert_eq!(classify_v4(v4("10.0.0.5")), "lan");
        assert_eq!(classify_v4(v4("172.16.5.5")), "lan");
        // Just outside RFC1918 172.16/12 is public.
        assert_eq!(classify_v4(v4("172.32.0.1")), "public");
    }

    #[test]
    fn ip_is_lan_only_for_rfc1918_v4() {
        assert!(ip_is_lan(ip("192.168.1.1")));
        assert!(ip_is_lan(ip("10.1.2.3")));
        assert!(!ip_is_lan(ip("127.0.0.1")));
        assert!(!ip_is_lan(ip("169.254.169.254")));
        assert!(!ip_is_lan(ip("8.8.8.8")));
        // IPv6 is denied wholesale, including ULA + loopback.
        assert!(!ip_is_lan(ip("::1")));
        assert!(!ip_is_lan(ip("fd00::1")));
    }

    fn allowlist(items: &[&str]) -> HashSet<IpAddr> {
        items.iter().map(|s| ip(s)).collect()
    }

    #[test]
    fn vet_rejects_when_feature_disabled() {
        let al = allowlist(&["192.168.1.50"]);
        let err = vet_target(false, ip("192.168.1.50"), &al).unwrap_err();
        assert!(err.contains("disabled"), "{err}");
    }

    #[test]
    fn vet_rejects_loopback_even_if_allowlisted() {
        // Allowlist membership must NOT override the SSRF class check.
        let al = allowlist(&["127.0.0.1"]);
        let err = vet_target(true, ip("127.0.0.1"), &al).unwrap_err();
        assert!(err.contains("loopback"), "{err}");
    }

    #[test]
    fn vet_rejects_cloud_metadata_ip() {
        let al = allowlist(&["169.254.169.254"]);
        let err = vet_target(true, ip("169.254.169.254"), &al).unwrap_err();
        assert!(err.contains("link-local"), "{err}");
    }

    #[test]
    fn vet_rejects_public_target() {
        let al = allowlist(&["8.8.8.8"]);
        let err = vet_target(true, ip("8.8.8.8"), &al).unwrap_err();
        assert!(err.contains("public"), "{err}");
    }

    #[test]
    fn vet_rejects_lan_ip_not_in_allowlist() {
        let al = allowlist(&["192.168.1.50"]);
        let err = vet_target(true, ip("192.168.1.99"), &al).unwrap_err();
        assert!(err.contains("not a discovered"), "{err}");
    }

    #[test]
    fn vet_allows_discovered_lan_ip() {
        let al = allowlist(&["192.168.1.50"]);
        assert!(vet_target(true, ip("192.168.1.50"), &al).is_ok());
    }

    #[test]
    fn parse_target_accepts_ip_literal() {
        let p = parse_target("http://192.168.1.50:8080/cgi-bin/status?x=1").unwrap();
        assert_eq!(p.scheme, "http");
        assert_eq!(p.ip, ip("192.168.1.50"));
        assert_eq!(p.port, 8080);
        assert_eq!(p.path_and_query, "/cgi-bin/status?x=1");
    }

    #[test]
    fn parse_target_defaults_port_by_scheme() {
        assert_eq!(parse_target("http://192.168.1.50/").unwrap().port, 80);
        assert_eq!(parse_target("https://192.168.1.50/").unwrap().port, 443);
    }

    #[test]
    fn parse_target_rejects_hostname_dns_rebind() {
        // A hostname could re-resolve to a hostile IP — reject outright.
        let err = parse_target("http://evil.example.com/x").unwrap_err();
        assert!(err.contains("literal IP"), "{err}");
    }

    #[test]
    fn parse_target_rejects_non_http_scheme() {
        let err = parse_target("ftp://192.168.1.50/").unwrap_err();
        assert!(err.contains("http or https"), "{err}");
        let err = parse_target("file:///etc/passwd").unwrap_err();
        assert!(err.contains("http"), "{err}");
    }

    #[test]
    fn parse_target_metadata_ip_parses_but_vet_rejects() {
        // The metadata URL is structurally valid (link-local IP) — the
        // SSRF guard is what rejects it, not the parser.
        let p = parse_target("http://169.254.169.254/latest/meta-data/").unwrap();
        let al = allowlist(&["169.254.169.254"]);
        assert!(vet_target(true, p.ip, &al).is_err());
    }

    #[test]
    fn method_from_str_normalises_and_validates() {
        assert_eq!(method_from_str("get").unwrap(), reqwest::Method::GET);
        assert_eq!(method_from_str("POST").unwrap(), reqwest::Method::POST);
        assert!(method_from_str("not a method!").is_err());
    }
}

//! RTSP `OPTIONS` + `DESCRIBE` Verify probe with Digest fallback.
//!
//! The inline `POST /api/v1/admin/discovery/:session_id/probe-rtsp`
//! handler calls into [`probe`] to confirm that an
//! `(host, port, path, optional creds)` actually answers RTSP
//! and to surface the SDP track summary (codec + resolution)
//! before the operator clicks **Add** in the UI.
//!
//! Wire flow:
//!
//! 1. Open a TCP socket to `host:port` (1.5 s connect timeout).
//! 2. Send `OPTIONS rtsp://host:port/path RTSP/1.0`.
//!    Read the reply head (≤8 KiB). A `200` with a `Public:`
//!    header confirms an RTSP server is listening.
//! 3. Send `DESCRIBE rtsp://host:port/path RTSP/1.0` (no auth).
//!    * On `200` with a SDP body → parse and return.
//!    * On `401` with `WWW-Authenticate: Digest …` → reopen the
//!      socket, recompute the digest response per RFC 2617
//!      (MD5 only — Basic is intentionally not supported because
//!      the operator already typed the creds and we want to fail
//!      loudly on `WWW-Authenticate: Basic` rather than silently
//!      sending a plaintext password), and retry.
//!
//! Returns [`ProbeRtspResult`] with `ok=true` iff the second leg
//! ended with a `200` and a parseable SDP body.
//!
//! ## Path discovery
//!
//! When the operator's request leaves `path` empty or supplies a
//! bare `"/"` (the common case immediately after a CIDR scan, since
//! the scan only does OPTIONS and never learns vendor paths), the
//! handler iterates [`DEFAULT_PATHS`] — a curated list of the
//! ~10 paths that cover Hikvision, Dahua/Amcrest, Axis, Foscam,
//! Reolink, TP-Link, generic ONVIF and a `/` last-resort. The
//! first path that DESCRIBE-200s wins and is echoed back in
//! `ProbeRtspResult.path`. The UI then uses that string to build
//! the camera's final `rtsp://host:port<path>` URL on Add, so the
//! operator doesn't have to hand-type the vendor-specific suffix.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use md5::{Digest, Md5};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

use super::{ProbeRtspReq, ProbeRtspResult, ProbeStream, SdpStream};

/// Map an SDP `a=rtpmap:` codec name to [`nexus_types::CodecKind`].
///
/// Accepts both `H265` (RFC 7798 §7.1, common in vendor SDPs) and
/// `HEVC` (the IANA-registered name). Anything outside H.264 / H.265
/// (`JPEG`, `MPEG4-GENERIC`, audio codecs, future AV1) returns
/// `None` so the typed selector stays empty and the operator's UI
/// surfaces the raw string instead. Autodetect never emits the
/// `_plus` SVC variants; they're operator-supplied labels only.
fn codec_kind_from_rtpmap(name: &str) -> Option<nexus_types::CodecKind> {
    match name.to_ascii_uppercase().as_str() {
        "H264" => Some(nexus_types::CodecKind::H264),
        "H265" | "HEVC" => Some(nexus_types::CodecKind::H265),
        _ => None,
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_millis(1_500);
const READ_TIMEOUT: Duration = Duration::from_millis(3_000);
const MAX_RESPONSE: usize = 64 * 1024;

/// Vendor-default RTSP paths the probe tries when the operator
/// supplies an empty / `/` path. Ordered by approximate consumer-
/// market share so the common case completes in one TCP attempt:
///
///   * Hikvision / Annke / EZVIZ — `/Streaming/Channels/{101,102}`
///   * Dahua / Amcrest / Lorex — `/cam/realmonitor?channel=1&subtype={0,1}`
///   * Axis — `/axis-media/media.amp`
///   * Reolink — `/h264Preview_01_main`
///   * Foscam — `/videoMain`
///   * TP-Link Tapo / VIGI — `/stream1` (main), `/stream2` (sub)
///   * Generic ONVIF profile names — `/Streaming/Channels/1`, `/live`
///   * Last-resort `/` for everything else (the camera's own root,
///     used by some Bosch and budget devices).
///
/// Substream variants (`102`, `subtype=1`, `stream2`, …) live AFTER
/// the matching main-stream entry so the bandwidth-hungry main feed
/// is the default Add suggestion when both work.
const DEFAULT_PATHS: &[&str] = &[
    "/Streaming/Channels/101",
    "/Streaming/Channels/102",
    "/cam/realmonitor?channel=1&subtype=0",
    "/cam/realmonitor?channel=1&subtype=1",
    "/axis-media/media.amp",
    "/h264Preview_01_main",
    "/h264Preview_01_sub",
    "/videoMain",
    "/stream1",
    "/stream2",
    "/live",
    "/Streaming/Channels/1",
    "/",
];

pub(crate) async fn probe(req: &ProbeRtspReq) -> ProbeRtspResult {
    // If the operator supplied a real path (anything other than
    // "" or "/"), honour it verbatim. Otherwise iterate
    // [`DEFAULT_PATHS`] until one succeeds. This matches the UX
    // we want: a hand-typed path is treated as gospel, an empty
    // path triggers discovery.
    let requested = req.path.trim();
    let candidates: Vec<String> = if requested.is_empty() || requested == "/" {
        DEFAULT_PATHS.iter().map(|p| (*p).to_string()).collect()
    } else {
        vec![requested.to_string()]
    };

    // Probe every candidate in parallel. Per-path latency is
    // dominated by CONNECT_TIMEOUT (1.5s) + READ_TIMEOUT (3s)
    // worst case, so a 12-candidate sweep that ran sequentially
    // would cost up to ~50s — unacceptable for a synchronous
    // operator-facing endpoint. Running all 12 concurrent TCP
    // probes against one host is fine (each socket is
    // independent in the kernel) and bounds wall-clock to ~5s.
    let attempts = candidates.iter().map(|c| {
        let attempt = ProbeRtspReq {
            host: req.host.clone(),
            port: req.port,
            path: c.clone(),
            username: req.username.clone(),
            password: req.password.clone(),
        };
        let path = c.clone();
        async move { (path, probe_inner(&attempt).await) }
    });
    let results = futures::future::join_all(attempts).await;

    // Partition into (winning, auth-blocked, other-failure). The
    // winners list is returned to the UI so the operator sees
    // "main + sub" when both work; the failures determine the
    // status field surfaced when nothing answered.
    let mut winners: Vec<(String, Vec<SdpStream>)> = Vec::new();
    let mut auth_status: Option<u16> = None;
    let mut other_status: Option<u16> = None;
    // Per-path summary table for diagnostics. Logged at INFO so
    // the operator-facing error message can be cross-referenced
    // against the engine log when a probe sweep returns no
    // winners (e.g. all 12 vendor paths return 405 because the
    // camera is non-standard).
    let mut summary: Vec<(String, String)> = Vec::with_capacity(candidates.len());
    for (path, r) in results {
        match r {
            Ok(probe_result) => {
                let tag = if probe_result.ok {
                    format!("200 OK ({} sdp tracks)", probe_result.sdp_streams.len())
                } else {
                    format!("status {}", probe_result.status)
                };
                summary.push((path.clone(), tag));
                if probe_result.ok {
                    winners.push((path, probe_result.sdp_streams));
                } else if probe_result.status == 401 || probe_result.status == 403 {
                    auth_status.get_or_insert(probe_result.status);
                } else if probe_result.status != 0 {
                    other_status.get_or_insert(probe_result.status);
                }
            }
            Err(e) => {
                summary.push((path.clone(), format!("io error: {e}")));
                debug!(
                    host = %req.host,
                    port = req.port,
                    path = %path,
                    error = %e,
                    "rtsp Verify candidate failed"
                );
            }
        }
    }
    let summary_str = summary
        .iter()
        .map(|(p, s)| format!("{p} -> {s}"))
        .collect::<Vec<_>>()
        .join(" | ");
    // INFO when nothing answered with 200 (operator-actionable
    // failure — they want to see the per-path status); DEBUG
    // when at least one path won (sweep succeeded, summary is
    // noise unless someone is actively debugging).
    if winners.is_empty() {
        info!(
            host = %req.host,
            port = req.port,
            with_creds = req.username.is_some(),
            winners = winners.len(),
            candidates = candidates.len(),
            "rtsp probe sweep: {summary_str}"
        );
    } else {
        debug!(
            host = %req.host,
            port = req.port,
            with_creds = req.username.is_some(),
            winners = winners.len(),
            candidates = candidates.len(),
            "rtsp probe sweep: {summary_str}"
        );
    }

    if !winners.is_empty() {
        // Build the per-path summary in the candidate order so
        // the UI shows main-stream above sub-stream (DEFAULT_PATHS
        // is curated that way; explicit-path callers have only one).
        let streams: Vec<ProbeStream> = winners
            .iter()
            .map(|(path, sdp_streams)| {
                let video = sdp_streams
                    .iter()
                    .find(|s| {
                        let c = s.codec.to_ascii_uppercase();
                        c.starts_with("H26") || c == "AV1" || c == "VP9" || c == "VP8"
                    })
                    .or_else(|| sdp_streams.first());
                ProbeStream {
                    path: path.clone(),
                    codec: video.map(|s| s.codec.clone()),
                    codec_kind: video.and_then(|s| s.codec_kind),
                    resolution: video.and_then(|s| s.resolution.clone()),
                }
            })
            .collect();
        let top_codec_kind = streams.first().and_then(|s| s.codec_kind);
        let (first_path, first_sdp) = winners.into_iter().next().expect("non-empty");
        return ProbeRtspResult {
            ok: true,
            status: 200,
            sdp_streams: first_sdp,
            codec: top_codec_kind,
            path: Some(first_path),
            streams,
        };
    }

    // Nothing answered with 200. Prefer 401/403 over other
    // statuses since auth is the actionable failure the operator
    // can fix (typing creds), where 404/500 mostly mean
    // "wrong device or wrong path list".
    ProbeRtspResult {
        ok: false,
        status: auth_status.or(other_status).unwrap_or(0),
        sdp_streams: Vec::new(),
        codec: None,
        path: None,
        streams: Vec::new(),
    }
}

/// URL-based codec autodetect wrapper around [`probe`].
///
/// Parses host/port/path/creds out of an `rtsp://` or `rtsps://`
/// URL and returns the top observed [`nexus_types::CodecKind`], or
/// `None` if the probe couldn't reach a `200 OK` (auth challenge
/// the URL's embedded creds didn't satisfy, network failure, or a
/// codec outside our enum like JPEG / AV1).
///
/// Used by `POST /cameras` when the operator omits `ingest.codec`
/// — we attempt one autodetect pass against the supplied URL; if
/// it succeeds, the camera persists with the detected codec; if
/// not, we leave `codec=None` and the reconciler's spawn-time
/// warning kicks in.
pub async fn probe_codec_for_url(url: &url::Url) -> Option<nexus_types::CodecKind> {
    let scheme = url.scheme();
    if scheme != "rtsp" && scheme != "rtsps" {
        return None;
    }
    let host = url.host_str()?.to_string();
    let port = url.port().unwrap_or(554);
    let path = url.path().to_string();
    let username = if url.username().is_empty() {
        None
    } else {
        Some(url.username().to_string())
    };
    let password = url.password().map(|p| p.to_string());
    let req = ProbeRtspReq {
        host,
        port,
        path,
        username,
        password,
    };
    probe(&req).await.codec
}

async fn probe_inner(req: &ProbeRtspReq) -> io::Result<ProbeRtspResult> {
    let path = normalize_path(&req.path);
    let host = req.host.trim();
    let url = format!("rtsp://{host}:{port}{path}", port = req.port);

    // Keep ONE TCP connection open across every leg of this
    // probe (OPTIONS → DESCRIBE-noauth → DESCRIBE-with-auth).
    // Cheap embedded RTSP servers (verified on TP-Link
    // IP-Camera firmware ca. 2024 / 2026) bind their digest
    // nonces to the originating TCP connection: presenting a
    // nonce from leg-1 over a *fresh* socket makes the server
    // treat it as stale and reply 401 with a brand-new nonce,
    // forever. The same nonce sent over the same socket works
    // first try. Spec-pedants would point to RFC 2617 §3.3
    // (`stale=true`/`false`) — TP-Link omits the field, so we
    // have to compensate by keeping the socket alive.
    let mut conn = RtspConn::connect(host, req.port).await?;

    // OPTIONS — purely a liveness check. Even if the camera
    // requires auth on DESCRIBE, OPTIONS is usually open.
    let options_text = conn
        .send_recv(&build_request("OPTIONS", &url, 1, None, None))
        .await?;
    let (options_status, _options_headers, _) = split_response(&options_text);

    // DESCRIBE leg 1 — no auth.
    let describe_text = conn
        .send_recv(&build_request(
            "DESCRIBE",
            &url,
            2,
            None,
            Some("application/sdp"),
        ))
        .await?;
    let (status_1, headers_1, body_1) = split_response(&describe_text);
    debug!(
        host = %req.host, port = req.port, path = %path,
        options_status, status_1, body_1_len = body_1.len(),
        challenges = %find_all_header_values(headers_1, "WWW-Authenticate").join(" || "),
        "rtsp probe: leg-1 DESCRIBE (no auth) response"
    );

    if status_1 == 200 && !body_1.is_empty() {
        return Ok(ProbeRtspResult {
            ok: true,
            status: 200,
            sdp_streams: parse_sdp(body_1),
            codec: None,
            // Filled in by the caller in `probe()` once it knows
            // which candidate path won — leave None here.
            path: None,
            streams: Vec::new(),
        });
    }

    // 401 challenge — retry DESCRIBE with creds. Cameras vary
    // wildly here, so we try every advertised scheme rather than
    // just the first `WWW-Authenticate` line. Concretely:
    //
    //   * Many Hikvision / Dahua firmwares ship BOTH `Basic` and
    //     `Digest` lines in that order. Returning only the first
    //     match silently dropped Digest and made every probe
    //     report 401 even with valid creds.
    //   * Some firmware uses `algorithm=MD5-sess` instead of MD5
    //     (handled inside `build_digest_response`).
    //   * Budget IP cameras occasionally advertise only `Basic`.
    //
    // Strategy: collect every `WWW-Authenticate` line, prefer
    // Digest variants (most secure), fall back to Basic. Bail
    // only when every attempt returns non-200.
    if status_1 == 401 {
        if let (Some(user), Some(pw)) = (req.username.as_deref(), req.password.as_deref()) {
            let challenges = find_all_header_values(headers_1, "WWW-Authenticate");
            let mut last_status: u16 = 401;

            // Digest first, since cameras that support both
            // schemes still prefer it for security.
            for challenge in &challenges {
                let Some(params) = parse_digest_challenge(challenge) else {
                    continue;
                };
                let auth_header = build_digest_response(&params, user, pw, "DESCRIBE", &url, 3);
                debug!(
                    host = %req.host, port = req.port, path = %path,
                    realm = %params.realm, qop = ?params.qop, algorithm = ?params.algorithm,
                    auth_header_redacted = %redact_response(&auth_header),
                    "rtsp digest retry: sending DESCRIBE with computed response"
                );
                let body = build_request(
                    "DESCRIBE",
                    &url,
                    3,
                    Some(&auth_header),
                    Some("application/sdp"),
                );
                // Same socket as leg-1 — see RtspConn comment.
                let text = conn.send_recv(&body).await?;
                let (status_2, h2, body_2) = split_response(&text);
                debug!(
                    host = %req.host, port = req.port, path = %path,
                    status = status_2, body_len = body_2.len(),
                    headers = %h2.lines().take(12).collect::<Vec<_>>().join(" | "),
                    "rtsp digest retry: server response"
                );
                if status_2 == 200 && !body_2.is_empty() {
                    return Ok(ProbeRtspResult {
                        ok: true,
                        status: 200,
                        sdp_streams: parse_sdp(body_2),
                        codec: None,
                        path: None,
                        streams: Vec::new(),
                    });
                }
                last_status = status_2;
            }

            // Basic. Some cameras advertise only `Basic`; others
            // advertise both but the Digest leg failed because of
            // an unsupported `algorithm` value (e.g. SHA-256).
            // We try Basic if any challenge declares it.
            let basic_advertised = challenges
                .iter()
                .any(|c| c.trim_start().to_ascii_lowercase().starts_with("basic"));
            if basic_advertised {
                let auth_header = build_basic_response(user, pw);
                let body = build_request(
                    "DESCRIBE",
                    &url,
                    4,
                    Some(&auth_header),
                    Some("application/sdp"),
                );
                // Basic doesn't need socket reuse (no nonce),
                // but use the same conn for symmetry and so we
                // don't pay a second TCP handshake.
                let text = conn.send_recv(&body).await?;
                let (status_2, _h2, body_2) = split_response(&text);
                if status_2 == 200 && !body_2.is_empty() {
                    return Ok(ProbeRtspResult {
                        ok: true,
                        status: 200,
                        sdp_streams: parse_sdp(body_2),
                        codec: None,
                        path: None,
                        streams: Vec::new(),
                    });
                }
                last_status = status_2;
            }

            return Ok(ProbeRtspResult {
                ok: false,
                status: last_status,
                sdp_streams: Vec::new(),
                codec: None,
                path: None,
                streams: Vec::new(),
            });
        }
    }

    Ok(ProbeRtspResult {
        ok: false,
        // Surface the DESCRIBE status when OPTIONS was 200 (the
        // common "options ok, describe gated" case), else fall
        // back to whatever OPTIONS returned.
        status: if status_1 == 0 {
            options_status
        } else {
            status_1
        },
        sdp_streams: Vec::new(),
        codec: None,
        path: None,
        streams: Vec::new(),
    })
}

/// Persistent RTSP/TCP connection used for the duration of
/// one probe attempt (per candidate path). We keep this around
/// the whole probe so the camera's digest-auth nonce stays
/// valid across the OPTIONS / DESCRIBE-noauth / DESCRIBE-auth
/// sequence; see the comment in `probe_inner` for the
/// motivating TP-Link behaviour.
struct RtspConn {
    stream: TcpStream,
}

impl RtspConn {
    async fn connect(host: &str, port: u16) -> io::Result<Self> {
        // Resolve the host. `IpAddr::parse` first so we don't
        // pay for DNS when the operator already typed a
        // dotted-quad.
        let addr = if let Ok(ip) = host.parse::<IpAddr>() {
            SocketAddr::new(ip, port)
        } else {
            let mut iter = tokio::net::lookup_host((host, port)).await?;
            iter.next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no DNS result"))?
        };

        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timeout"))??;
        stream.set_nodelay(true).ok();
        Ok(Self { stream })
    }

    /// Write one RTSP request and read back exactly one reply
    /// (head + body bounded by `Content-Length`, or just head
    /// if no `Content-Length` advertised — RTSP servers don't
    /// chunk, so absence of CL means empty body).
    async fn send_recv(&mut self, req: &str) -> io::Result<String> {
        timeout(READ_TIMEOUT, self.stream.write_all(req.as_bytes()))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "write timeout"))??;

        let mut out = Vec::with_capacity(8 * 1024);
        let mut buf = [0u8; 4096];
        loop {
            let n = match timeout(READ_TIMEOUT, self.stream.read(&mut buf)).await {
                Ok(Ok(0)) => break, // server closed — return whatever we have
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(e),
                Err(_) => break, // read timeout — return whatever we have
            };
            out.extend_from_slice(&buf[..n]);
            if out.len() >= MAX_RESPONSE {
                break;
            }
            // Frame on `\r\n\r\n`. With Content-Length: wait for
            // CL body bytes after the head. Without: return
            // immediately — keeping the socket open for the
            // next request. (The old send_rtsp closed the
            // socket after each call so it could afford to
            // block on EOF; we cannot.)
            if let Some(head_end) = window_index(&out, b"\r\n\r\n") {
                let head = &out[..head_end];
                match parse_content_length(head) {
                    Some(cl) if out.len() - (head_end + 4) >= cl => break,
                    None => break,
                    _ => {}
                }
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

fn window_index(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_content_length(head: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(head).ok()?;
    for line in text.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("Content-Length") {
                return v.trim().parse::<usize>().ok();
            }
        }
    }
    None
}

fn build_request(
    method: &str,
    url: &str,
    cseq: u32,
    authorization: Option<&str>,
    accept: Option<&str>,
) -> String {
    let mut s = format!(
        "{method} {url} RTSP/1.0\r\n\
         CSeq: {cseq}\r\n\
         User-Agent: nexus-discovery/1\r\n"
    );
    if let Some(a) = accept {
        s.push_str("Accept: ");
        s.push_str(a);
        s.push_str("\r\n");
    }
    if let Some(h) = authorization {
        s.push_str("Authorization: ");
        s.push_str(h);
        s.push_str("\r\n");
    }
    s.push_str("\r\n");
    s
}

/// Path normalisation: ensure a leading `/`, accept an empty
/// path as `/`.
fn normalize_path(p: &str) -> String {
    let p = p.trim();
    if p.is_empty() {
        "/".to_string()
    } else if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

/// Split an RTSP/HTTP-style reply into `(status, headers, body)`.
/// `headers` is the raw header block (LF-separated lines incl.
/// the status line). `body` is everything after the blank line
/// terminator. Returns `(0, "", "")` for malformed replies.
fn split_response(text: &str) -> (u16, &str, &str) {
    let split = text.find("\r\n\r\n").map(|i| (i, 4));
    let (hi, sep) = match split {
        Some(v) => v,
        None => return (0, "", ""),
    };
    let head = &text[..hi];
    let body = &text[hi + sep..];
    let status_line = head.lines().next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    (status, head, body)
}

/// Collect every header value matching `name`. Required for
/// `WWW-Authenticate` since RFC 7235 explicitly allows multiple
/// challenges in one response — and consumer cameras routinely
/// ship `Basic` + `Digest` as separate lines. Returning only the
/// first match (as a "find first" helper would) would silently
/// drop the second scheme and report 401 even with valid
/// credentials.
fn find_all_header_values<'a>(headers: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    for line in headers.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                out.push(v.trim());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Digest auth (RFC 2617 — MD5 only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct DigestParams {
    realm: String,
    nonce: String,
    opaque: Option<String>,
    qop: Option<String>,
    algorithm: Option<String>,
}

/// Parse a `WWW-Authenticate: Digest …` header value. Tolerates
/// the comma-separated `k=v` / `k="v"` salad cameras actually
/// ship. Returns `None` for non-Digest schemes.
fn parse_digest_challenge(header: &str) -> Option<DigestParams> {
    let rest = header
        .strip_prefix("Digest ")
        .or_else(|| header.strip_prefix("digest "))?;
    let mut out = DigestParams::default();
    for raw in split_digest_params(rest) {
        let (k, v) = raw.split_once('=')?;
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim().trim_matches('"').to_string();
        match k.as_str() {
            "realm" => out.realm = v,
            "nonce" => out.nonce = v,
            "opaque" => out.opaque = Some(v),
            "qop" => out.qop = Some(v),
            "algorithm" => out.algorithm = Some(v),
            _ => {}
        }
    }
    if out.realm.is_empty() || out.nonce.is_empty() {
        return None;
    }
    Some(out)
}

/// Comma-split a Digest challenge while honouring quoted values
/// (`qop="auth,auth-int"` must not be split on the inner comma).
fn split_digest_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_quote = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                buf.push(ch);
            }
            ',' if !in_quote => {
                if !buf.trim().is_empty() {
                    out.push(buf.trim().to_string());
                }
                buf.clear();
            }
            _ => buf.push(ch),
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf.trim().to_string());
    }
    out
}

/// Build the RFC 2617 `Authorization: Digest …` header value.
/// Uses `qop=auth` when the server advertises it; falls back to
/// the legacy `MD5(HA1:nonce:HA2)` form otherwise.
///
/// Supports both `algorithm=MD5` (default if absent) and
/// `algorithm=MD5-sess` — Hikvision firmware ≥ 5.5.x and some
/// Reolink models advertise MD5-sess, which redefines HA1 as
/// `MD5(MD5(user:realm:pass) ":" nonce ":" cnonce)`. The
/// previous implementation always used the plain-MD5 HA1, which
/// the camera rejected with another 401 even though the
/// credentials were correct.
fn build_digest_response(
    params: &DigestParams,
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    nc: u32,
) -> String {
    let plain_ha1 = md5_hex(&format!(
        "{username}:{realm}:{password}",
        realm = params.realm
    ));
    let ha2 = md5_hex(&format!("{method}:{uri}"));

    let is_sess = params
        .algorithm
        .as_deref()
        .map(|a| a.eq_ignore_ascii_case("md5-sess"))
        .unwrap_or(false);

    // Pick qop=auth iff the server lists it (qop may be a CSV).
    let qop_auth = params
        .qop
        .as_deref()
        .map(|q| {
            q.split(',')
                .map(|s| s.trim())
                .any(|s| s.eq_ignore_ascii_case("auth"))
        })
        .unwrap_or(false);

    // MD5-sess REQUIRES a cnonce even outside qop=auth, since the
    // session-key HA1 mixes it in. When the camera advertises
    // MD5-sess but not qop=auth (rare but observed on some
    // Foscam firmware), we still need to generate cnonce.
    let need_cnonce = qop_auth || is_sess;
    let cnonce = if need_cnonce {
        format!(
            "{:016x}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        )
    } else {
        String::new()
    };

    let ha1 = if is_sess {
        md5_hex(&format!(
            "{plain_ha1}:{nonce}:{cnonce}",
            nonce = params.nonce,
        ))
    } else {
        plain_ha1
    };

    let (response, qop_part, cnonce_part, nc_part) = if qop_auth {
        let nc_str = format!("{nc:08x}");
        let r = md5_hex(&format!(
            "{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}",
            nonce = params.nonce,
            nc = nc_str,
        ));
        (
            r,
            ", qop=auth".to_string(),
            format!(", cnonce=\"{cnonce}\""),
            format!(", nc={nc_str}"),
        )
    } else {
        let r = md5_hex(&format!("{ha1}:{nonce}:{ha2}", nonce = params.nonce));
        // MD5-sess outside qop=auth: include cnonce so the camera
        // can reconstruct the same session-key HA1 we used (RFC
        // 2617 §3.2.2 — "If the directive 'algorithm' is set to
        // 'MD5-sess', the client must produce a cnonce value").
        let cnonce_hdr = if is_sess {
            format!(", cnonce=\"{cnonce}\"")
        } else {
            String::new()
        };
        (r, String::new(), cnonce_hdr, String::new())
    };

    let mut h = format!(
        r#"Digest username="{user}", realm="{realm}", nonce="{nonce}", uri="{uri}", response="{resp}""#,
        user = username,
        realm = params.realm,
        nonce = params.nonce,
        uri = uri,
        resp = response,
    );
    if let Some(opaque) = &params.opaque {
        h.push_str(&format!(", opaque=\"{opaque}\""));
    }
    if let Some(alg) = &params.algorithm {
        h.push_str(&format!(", algorithm={alg}"));
    }
    h.push_str(&qop_part);
    h.push_str(&nc_part);
    h.push_str(&cnonce_part);
    h
}

/// Build an RFC 7617 `Authorization: Basic …` header value. Used
/// when the camera advertises Basic in `WWW-Authenticate` (or
/// when our Digest leg failed because of an unsupported algorithm
/// like SHA-256). Plaintext-equivalent over the wire — fine for
/// the local-network Verify probe but never propagated into the
/// stored camera URL.
fn build_basic_response(username: &str, password: &str) -> String {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    let creds = format!("{username}:{password}");
    let b64 = B64.encode(creds.as_bytes());
    format!("Basic {b64}")
}

fn md5_hex(s: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(s.as_bytes());
    let bytes = hasher.finalize();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Replace the `response="..."` value in a Digest authorization
/// header with a short prefix + `...`, leaving every other field
/// (realm, nonce, uri, qop, nc, cnonce, algorithm) intact. Used
/// for debug logging — the response hash is the only field that
/// would let an observer brute-force the password offline; every
/// other field is already on the wire in cleartext. We keep the
/// 8-char prefix so two consecutive logs are visually
/// distinguishable for "did the response change between
/// attempts?" diagnosis.
fn redact_response(header: &str) -> String {
    let needle = "response=\"";
    let Some(start) = header.find(needle) else {
        return header.to_string();
    };
    let value_start = start + needle.len();
    let Some(end_offset) = header[value_start..].find('"') else {
        return header.to_string();
    };
    let value_end = value_start + end_offset;
    let prefix = &header[..value_start];
    let value = &header[value_start..value_end];
    let suffix = &header[value_end..];
    let short = value.chars().take(8).collect::<String>();
    format!("{prefix}{short}...{suffix}")
}

// ---------------------------------------------------------------------------
// SDP parser
// ---------------------------------------------------------------------------

/// Pull a per-track summary out of an SDP body. We care about
/// codec (from `a=rtpmap:`), resolution (from `a=framesize:`
/// when present, or `a=x-dimensions:` on some cameras), and the
/// control URL (from `a=control:`) — that last one is what the
/// camera-create form pre-fills into the RTSP URL path.
fn parse_sdp(body: &str) -> Vec<SdpStream> {
    let mut out = Vec::new();
    let mut current: Option<SdpStream> = None;
    for line in body.lines() {
        let line = line.trim();
        if let Some(media) = line.strip_prefix("m=") {
            // `m=video 0 RTP/AVP 96` — close the prior track, start a new one.
            if let Some(prev) = current.take() {
                out.push(prev);
            }
            let kind = media.split_whitespace().next().unwrap_or("?");
            current = Some(SdpStream {
                codec: kind.to_string(),
                codec_kind: None,
                resolution: None,
                control: None,
            });
        } else if let Some(track) = current.as_mut() {
            if let Some(rest) = line.strip_prefix("a=rtpmap:") {
                // `96 H264/90000` — pull the codec name.
                if let Some(codec) = rest.split_whitespace().nth(1) {
                    let name = match codec.split_once('/') {
                        Some((c, _)) => c.to_string(),
                        None => codec.to_string(),
                    };
                    track.codec_kind = codec_kind_from_rtpmap(&name);
                    track.codec = name;
                }
            } else if let Some(rest) = line.strip_prefix("a=framesize:") {
                // `96 1280-720` (some cameras use `-`, some `x`).
                if let Some(size) = rest.split_whitespace().nth(1) {
                    track.resolution = Some(size.replace('-', "x"));
                }
            } else if let Some(rest) = line.strip_prefix("a=x-dimensions:") {
                // `96,1920,1080`
                let parts: Vec<&str> = rest.splitn(2, ',').collect();
                if let Some(dims) = parts.get(1) {
                    let dims = dims.replace(',', "x");
                    track.resolution = Some(dims);
                }
            } else if let Some(rest) = line.strip_prefix("a=control:") {
                track.control = Some(rest.to_string());
            }
        }
    }
    if let Some(prev) = current.take() {
        out.push(prev);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_response_extracts_status_and_body() {
        let text = "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 4\r\n\r\nbody";
        let (status, headers, body) = split_response(text);
        assert_eq!(status, 200);
        assert!(headers.contains("Content-Length: 4"));
        assert_eq!(body, "body");
    }

    #[test]
    fn split_response_handles_no_body() {
        let text = "RTSP/1.0 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"x\", nonce=\"y\"\r\n\r\n";
        let (status, headers, body) = split_response(text);
        assert_eq!(status, 401);
        assert!(headers.to_lowercase().contains("www-authenticate"));
        assert_eq!(body, "");
    }

    #[test]
    fn parse_digest_challenge_extracts_required_fields() {
        let header =
            r#"Digest realm="IP Camera", nonce="abc123", qop="auth", opaque="opq", algorithm=MD5"#;
        let p = parse_digest_challenge(header).unwrap();
        assert_eq!(p.realm, "IP Camera");
        assert_eq!(p.nonce, "abc123");
        assert_eq!(p.qop.as_deref(), Some("auth"));
        assert_eq!(p.opaque.as_deref(), Some("opq"));
        assert_eq!(p.algorithm.as_deref(), Some("MD5"));
    }

    #[test]
    fn parse_digest_challenge_handles_quoted_csv_qop() {
        let header = r#"Digest realm="r", nonce="n", qop="auth,auth-int""#;
        let p = parse_digest_challenge(header).unwrap();
        assert_eq!(p.qop.as_deref(), Some("auth,auth-int"));
    }

    #[test]
    fn parse_digest_challenge_rejects_basic() {
        assert!(parse_digest_challenge("Basic realm=\"x\"").is_none());
    }

    #[test]
    fn parse_digest_challenge_requires_realm_and_nonce() {
        assert!(parse_digest_challenge("Digest realm=\"x\"").is_none());
        assert!(parse_digest_challenge("Digest nonce=\"y\"").is_none());
    }

    #[test]
    fn build_digest_response_matches_rfc_2617_example() {
        // RFC 2617 §3.5 example, legacy (no-qop) compatibility mode.
        // HA1 = MD5("Mufasa:testrealm@host.com:Circle Of Life")
        //     = 939e7578ed9e3c518a452acee763bce9
        // HA2 = MD5("GET:/dir/index.html")
        //     = 39aff3a2bab6126f332b942af96d3366
        // response = MD5("HA1:nonce:HA2")
        //          = 670fd8c2df070c60b045671b8b24ff02
        let params = DigestParams {
            realm: "testrealm@host.com".to_string(),
            nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093".to_string(),
            opaque: Some("5ccc069c403ebaf9f0171e9517f40e41".to_string()),
            qop: None,
            algorithm: None,
        };
        let header = build_digest_response(
            &params,
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            1,
        );
        assert!(header.contains(r#"username="Mufasa""#));
        assert!(header.contains(r#"realm="testrealm@host.com""#));
        assert!(header.contains(r#"nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093""#));
        assert!(header.contains(r#"uri="/dir/index.html""#));
        assert!(header.contains(r#"response="670fd8c2df070c60b045671b8b24ff02""#));
        assert!(header.contains(r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#));
        // No qop block when the server didn't ask for it.
        assert!(!header.contains("qop="));
        assert!(!header.contains("nc="));
    }

    #[test]
    fn build_digest_response_includes_qop_block_when_advertised() {
        let params = DigestParams {
            realm: "r".to_string(),
            nonce: "n".to_string(),
            opaque: None,
            qop: Some("auth".to_string()),
            algorithm: Some("MD5".to_string()),
        };
        let header = build_digest_response(&params, "u", "p", "DESCRIBE", "rtsp://x/", 1);
        assert!(header.contains("qop=auth"));
        assert!(header.contains("nc=00000001"));
        assert!(header.contains("cnonce="));
    }

    #[test]
    fn parse_sdp_extracts_h264_track_with_control() {
        let body = "v=0\r\n\
            o=- 0 0 IN IP4 0.0.0.0\r\n\
            s=Session streamed by GStreamer\r\n\
            c=IN IP4 0.0.0.0\r\n\
            t=0 0\r\n\
            m=video 0 RTP/AVP 96\r\n\
            a=rtpmap:96 H264/90000\r\n\
            a=framesize:96 1920-1080\r\n\
            a=control:trackID=0\r\n\
            m=audio 0 RTP/AVP 97\r\n\
            a=rtpmap:97 MPEG4-GENERIC/48000/2\r\n\
            a=control:trackID=1\r\n";
        let tracks = parse_sdp(body);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].codec, "H264");
        assert_eq!(tracks[0].codec_kind, Some(nexus_types::CodecKind::H264));
        assert_eq!(tracks[0].resolution.as_deref(), Some("1920x1080"));
        assert_eq!(tracks[0].control.as_deref(), Some("trackID=0"));
        assert_eq!(tracks[1].codec, "MPEG4-GENERIC");
        // MPEG4-GENERIC is audio — we don't enumerate it.
        assert_eq!(tracks[1].codec_kind, None);
    }

    #[test]
    fn parse_sdp_falls_back_to_x_dimensions() {
        let body = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H265/90000\r\na=x-dimensions:96,3840,2160\r\n";
        let tracks = parse_sdp(body);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].codec, "H265");
        assert_eq!(tracks[0].codec_kind, Some(nexus_types::CodecKind::H265));
        assert_eq!(tracks[0].resolution.as_deref(), Some("3840x2160"));
    }

    #[test]
    fn parse_sdp_accepts_hevc_alias_for_h265() {
        // RFC 7798 §7.1 — the IANA-registered name is `HEVC`.
        // Several Bosch / Hanwha firmwares emit that instead of `H265`.
        let body = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 HEVC/90000\r\n";
        let tracks = parse_sdp(body);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].codec_kind, Some(nexus_types::CodecKind::H265));
    }

    #[test]
    fn parse_sdp_lowercase_rtpmap_is_accepted() {
        // Some test SDPs and OBS-rtsp use lowercase — the
        // uppercase normalisation in `codec_kind_from_rtpmap`
        // should handle either.
        let body = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 h264/90000\r\n";
        let tracks = parse_sdp(body);
        assert_eq!(tracks[0].codec, "h264");
        assert_eq!(tracks[0].codec_kind, Some(nexus_types::CodecKind::H264));
    }

    #[test]
    fn normalize_path_adds_leading_slash() {
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path("/foo"), "/foo");
        assert_eq!(normalize_path("foo"), "/foo");
        assert_eq!(normalize_path("  /bar  "), "/bar");
    }

    #[test]
    fn find_all_header_values_returns_every_match() {
        // Camera firmwares routinely emit `Basic` + `Digest`
        // on two separate `WWW-Authenticate` lines. The Verify
        // probe must see both, otherwise it silently drops the
        // Digest challenge and falsely reports 401.
        let headers = "WWW-Authenticate: Basic realm=\"cam\"\r\n\
            WWW-Authenticate: Digest realm=\"cam\", nonce=\"abc\"\r\n\
            CSeq: 2";
        let values = find_all_header_values(headers, "WWW-Authenticate");
        assert_eq!(values.len(), 2);
        assert!(values[0].starts_with("Basic"));
        assert!(values[1].starts_with("Digest"));
    }

    #[test]
    fn build_digest_response_handles_md5_sess() {
        // MD5-sess HA1 = MD5(MD5(user:realm:pass):nonce:cnonce)
        // The header MUST include cnonce even when qop is unset,
        // since the session key depends on it. We verify the
        // structural invariants (cnonce present, algorithm
        // round-tripped) — the exact response hash varies with
        // the wall-clock-seeded cnonce.
        let params = DigestParams {
            realm: "r".to_string(),
            nonce: "n".to_string(),
            opaque: None,
            qop: None,
            algorithm: Some("MD5-sess".to_string()),
        };
        let header = build_digest_response(&params, "u", "p", "DESCRIBE", "rtsp://x/", 1);
        assert!(header.contains("algorithm=MD5-sess"));
        assert!(header.contains("cnonce="));
        // No qop block when the server didn't ask for it, even
        // though MD5-sess forced a cnonce.
        assert!(!header.contains("qop="));
        assert!(!header.contains("nc="));
    }

    #[test]
    fn build_basic_response_encodes_user_colon_pass() {
        // RFC 7617: token = base64(user ":" pass). The receiving
        // camera decodes back to "user:pass" — any deviation
        // (e.g. URL-encoding the colon) breaks the legacy Basic
        // path that we use as a fallback for cameras that don't
        // advertise Digest.
        let h = build_basic_response("admin", "secret123");
        // base64("admin:secret123") = YWRtaW46c2VjcmV0MTIz
        assert_eq!(h, "Basic YWRtaW46c2VjcmV0MTIz");
    }
}

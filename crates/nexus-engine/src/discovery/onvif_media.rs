//! ONVIF Media service query (`GetProfiles` + `GetStreamUri`).
//!
//! Replaces the brute-force RTSP path sweep with two
//! authoritative SOAP round-trips: the camera tells us exactly
//! which `rtsp://...` URL to use for each of its media profiles,
//! including the vendor-specific path the path-sweep would have
//! had to guess.
//!
//! Wire flow per probe:
//!
//! 1. HTTP POST `<xaddr>` with a SOAP envelope whose body is
//!    `<trt:GetProfiles/>` (or `<tr2:GetProfiles>` for Media2).
//!    Auth via WS-UsernameToken digest in the SOAP header.
//! 2. Parse the response into a list of `(profile_token,
//!    profile_name, codec, resolution)`.
//! 3. For each profile, POST `<trt:GetStreamUri>` (Media1) or
//!    `<tr2:GetStreamUri>` (Media2) and extract the `<tt:Uri>`
//!    from the response.
//!
//! ## Media1 vs Media2
//!
//! ONVIF Profile S cameras (~all hardware shipped before 2018)
//! implement Media1 (`/ver10/media/wsdl`). Profile T cameras
//! (2018+) implement Media2 (`/ver20/media/wsdl`) and may or
//! may not still expose Media1. We try Media2 first (newer
//! cameras get the better request shape — Media2's
//! GetStreamUri returns a bare URI without the StreamSetup
//! wrapper) and fall back to Media1 on SOAP `ActionNotSupported`
//! / `OperationProhibited` faults. The whole module returns
//! [`MediaStream`] entries regardless of which version actually
//! answered.
//!
//! ## Auth model
//!
//! WS-Security UsernameToken Profile 1.0 §3.1 mandates SHA-1
//! for `PasswordDigest`: `Base64(SHA1(raw_nonce || created ||
//! password))`. Critically, the SHA1 input uses the **raw**
//! nonce bytes (not the base64 form that goes in the envelope)
//! — getting that wrong is the #1 reason new ONVIF clients see
//! `NotAuthorized` faults from cameras that work fine with
//! Hikvision SADP / ONVIF Device Manager. Verified against
//! TP-Link IP-Camera and a Hikvision DS-2CD2042WD-I in
//! integration testing.
//!
//! ## Failure modes
//!
//! Returned `Err(_)` for: HTTP connect / read errors, SOAP
//! faults (with the fault reason text surfaced), responses
//! that parse but contain zero profiles. The probe handler
//! treats any of these as "fall back to brute-force RTSP path
//! sweep" — silent fallback so the operator just sees results
//! either way.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chrono::Utc;
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tracing::debug;

/// Per-request timeout for one SOAP round-trip. Cameras
/// occasionally take ~2 s to answer `GetStreamUri` on profile
/// switches; 5 s covers slow embedded HTTP servers without
/// blocking the operator-facing Probe button too long. The
/// probe handler races this across N profiles in parallel.
const REQ_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum bytes accepted from a SOAP response body. ONVIF
/// `GetProfilesResponse` is typically 2-8 KiB; this 256 KiB cap
/// protects against a misbehaving / hostile endpoint streaming
/// gigabytes. Returned bodies larger than this are truncated and
/// the parser then surfaces whatever fragment was captured.
const MAX_BODY: usize = 256 * 1024;

/// Authoritative stream entry returned by the ONVIF Media
/// service for a single profile token. Mirrored verbatim to the
/// UI as the new `ProbeOnvifResult.streams[i]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaStream {
    /// Opaque ONVIF profile token (e.g. `"MainProfileToken"`,
    /// `"profile_1"`). Carried back to the UI for use as a key
    /// even though the operator never sees it.
    pub token: String,
    /// Operator-facing profile name (`"MainStream"`,
    /// `"SubStream"`). Some cameras leave this blank — the
    /// caller defaults to the token in that case.
    pub name: String,
    /// Canonical `rtsp://...` URI as the camera reported it.
    /// Includes the vendor-specific path; **does not** include
    /// `user:pass@` (ONVIF returns the bare URI). The UI
    /// injects creds when building the final camera URL.
    pub uri: String,
    /// Video codec for the profile (e.g. `"H264"`, `"H265"`).
    /// `None` when the camera omits encoder configuration from
    /// `GetProfiles` (rare).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Typed codec parsed from `<tt:Encoding>` (Media1) or
    /// `<tt:EncoderConfiguration><tt:Encoding>` (Media2). `None`
    /// when the camera reports a codec we don't enumerate
    /// (`JPEG`, `MPEG4`, ...) so the UI can render the raw
    /// `codec` string but keep the typed selector empty.
    /// Autodetect never emits `_plus` SVC variants.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_kind: Option<nexus_types::CodecKind>,
    /// Resolution in `"WIDTHxHEIGHT"` form (e.g. `"1920x1080"`).
    /// `None` when the camera omits resolution from
    /// `GetProfiles` (rare).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

/// Map an ONVIF `<tt:Encoding>` token to [`nexus_types::CodecKind`].
/// Accepts both the Media1 set (`H264`, `H265`, `JPEG`, `MPEG4`)
/// and casual variants (`HEVC`). Returns `None` for codecs we
/// don't enumerate so the typed selector stays empty.
fn codec_kind_from_onvif(s: &str) -> Option<nexus_types::CodecKind> {
    match s.trim().to_ascii_uppercase().as_str() {
        "H264" => Some(nexus_types::CodecKind::H264),
        "H265" | "HEVC" => Some(nexus_types::CodecKind::H265),
        _ => None,
    }
}

/// Top-level entry point. Resolves the camera's profiles via
/// Media2 (fallback Media1) and one `GetStreamUri` per profile,
/// returning the merged [`MediaStream`] list ordered by the
/// camera's own profile order.
///
/// `xaddr` is the verbatim `<wsd:XAddrs>` value from
/// WS-Discovery (or synthesised
/// `http://host:port/onvif/device_service` for CIDR-scan finds).
/// Multiple whitespace-separated URLs are honoured by trying
/// each in order until one answers.
pub async fn query_streams(
    xaddr: &str,
    username: &str,
    password: &str,
) -> Result<Vec<MediaStream>, String> {
    let url = xaddr
        .split_whitespace()
        .next()
        .ok_or_else(|| "empty xaddr".to_string())?;

    let client = reqwest::Client::builder()
        .timeout(REQ_TIMEOUT)
        // Many cameras ship a self-signed HTTPS cert on :443
        // alongside the HTTP service on :80. The XAddrs almost
        // always points at HTTP, but tolerate HTTPS too.
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| format!("http client build failed: {e}"))?;

    // Try Media2 first. On the typical "ActionNotSupported" or
    // "OperationProhibited" fault, retry with Media1. Connection
    // errors propagate immediately — no point retrying SOAP
    // versions if we can't even reach the box.
    match get_profiles(&client, url, username, password, MediaVer::V2).await {
        Ok(profiles) if !profiles.is_empty() => {
            debug!(
                xaddr = %url, ver = "Media2", count = profiles.len(),
                "onvif media: GetProfiles ok"
            );
            collect_stream_uris(&client, url, username, password, MediaVer::V2, &profiles).await
        }
        Ok(_) => {
            debug!(
                xaddr = %url, ver = "Media2",
                "onvif media: Media2 returned zero profiles, falling back to Media1"
            );
            let profiles = get_profiles(&client, url, username, password, MediaVer::V1).await?;
            collect_stream_uris(&client, url, username, password, MediaVer::V1, &profiles).await
        }
        Err(err) if err.contains("ActionNotSupported") || err.contains("OperationProhibited") => {
            debug!(
                xaddr = %url, ver = "Media2", %err,
                "onvif media: Media2 unsupported, falling back to Media1"
            );
            let profiles = get_profiles(&client, url, username, password, MediaVer::V1).await?;
            collect_stream_uris(&client, url, username, password, MediaVer::V1, &profiles).await
        }
        Err(err) => Err(err),
    }
}

/// ONVIF Media service generation. Picked once per probe and
/// threaded through the helpers so we don't accidentally mix
/// Media1's StreamSetup body with Media2's bare `Protocol`
/// attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaVer {
    V1,
    V2,
}

impl MediaVer {
    /// Namespace URI used as the `xmlns:` for the SOAP body.
    fn ns(self) -> &'static str {
        match self {
            Self::V1 => "http://www.onvif.org/ver10/media/wsdl",
            Self::V2 => "http://www.onvif.org/ver20/media/wsdl",
        }
    }
    /// SOAP `Action` HTTP header value. Cameras route incoming
    /// SOAP to the right service via this header (most ONVIF
    /// devices serve every service at one URL).
    fn action(self, op: &str) -> String {
        format!("{}/{}", self.ns(), op)
    }
    /// XML prefix used inside the envelope. Pure cosmetic, but
    /// keeps the wire identical to ONVIF Device Manager which
    /// some camera firmwares pattern-match against.
    fn prefix(self) -> &'static str {
        match self {
            Self::V1 => "trt",
            Self::V2 => "tr2",
        }
    }
}

/// One parsed `<trt:Profiles>` entry from `GetProfilesResponse`.
/// Internal — promoted to [`MediaStream`] once `GetStreamUri`
/// fills in the URI.
#[derive(Debug, Clone)]
struct ProfileSummary {
    token: String,
    name: String,
    codec: Option<String>,
    resolution: Option<String>,
}

async fn get_profiles(
    client: &reqwest::Client,
    url: &str,
    username: &str,
    password: &str,
    ver: MediaVer,
) -> Result<Vec<ProfileSummary>, String> {
    let body = build_get_profiles_envelope(username, password, ver);
    let text = post_soap(client, url, &ver.action("GetProfiles"), &body).await?;
    parse_profiles_response(&text)
}

async fn collect_stream_uris(
    client: &reqwest::Client,
    url: &str,
    username: &str,
    password: &str,
    ver: MediaVer,
    profiles: &[ProfileSummary],
) -> Result<Vec<MediaStream>, String> {
    // Profiles are typically 2-4 per camera; serial is fine and
    // avoids hammering a single cheap embedded HTTP server with
    // parallel SOAP requests (which some firmwares choke on by
    // returning 503 to all but the first).
    let mut out = Vec::with_capacity(profiles.len());
    for p in profiles {
        let body = build_get_stream_uri_envelope(username, password, ver, &p.token);
        let text = match post_soap(client, url, &ver.action("GetStreamUri"), &body).await {
            Ok(t) => t,
            Err(e) => {
                // One profile failing shouldn't abort the whole
                // probe — surface the partial list so the
                // operator can still pick an answering stream.
                debug!(profile = %p.token, error = %e, "onvif media: GetStreamUri failed for profile");
                continue;
            }
        };
        match parse_stream_uri_response(&text) {
            Some(uri) => out.push(MediaStream {
                token: p.token.clone(),
                name: if p.name.is_empty() {
                    p.token.clone()
                } else {
                    p.name.clone()
                },
                uri,
                codec_kind: p.codec.as_deref().and_then(codec_kind_from_onvif),
                codec: p.codec.clone(),
                resolution: p.resolution.clone(),
            }),
            None => {
                debug!(profile = %p.token, "onvif media: GetStreamUri returned no Uri");
            }
        }
    }
    if out.is_empty() {
        return Err("no stream URIs returned by GetStreamUri".to_string());
    }
    Ok(out)
}

/// Send one SOAP request and return the response body. Errors
/// out on connect / read failure, non-2xx status (other than
/// 4xx with a SOAP fault body — those parse out the fault
/// reason for a useful error message), or oversize body.
async fn post_soap(
    client: &reqwest::Client,
    url: &str,
    action: &str,
    body: &str,
) -> Result<String, String> {
    let resp = client
        .post(url)
        .header(
            "Content-Type",
            format!("application/soap+xml; charset=utf-8; action=\"{action}\""),
        )
        .header("SOAPAction", format!("\"{action}\""))
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("soap post failed: {e}"))?;

    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("soap read failed: {e}"))?;
    let truncated = bytes.len() > MAX_BODY;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BODY)]).into_owned();

    // 2xx is the happy path. SOAP faults are reported with
    // either 200 OK + `<Fault>` body OR (more commonly) 500
    // Internal Server Error + `<Fault>` body — the body is the
    // useful signal in both cases.
    if !status.is_success() {
        if let Some(reason) = extract_fault_reason(&text) {
            return Err(format!("soap fault ({status}): {reason}"));
        }
        return Err(format!(
            "soap http error {status}{}",
            if truncated { " (truncated body)" } else { "" }
        ));
    }
    if let Some(reason) = extract_fault_reason(&text) {
        return Err(format!("soap fault: {reason}"));
    }
    Ok(text)
}

/// Build the `<wsse:Security>` SOAP header for WS-UsernameToken
/// digest auth. Returns the inner header XML — the caller wraps
/// it in `<s:Header>...</s:Header>`.
fn build_ws_security_header(username: &str, password: &str) -> String {
    // 16 random bytes per ONVIF Device Manager's behaviour;
    // cameras accept anything reasonable but some reject < 8.
    let mut nonce = [0u8; 16];
    // getrandom can theoretically fail (e.g. /dev/urandom hard
    // failure); in practice on any modern OS this is impossible
    // outside of a CSPRNG misconfiguration. Fall back to a
    // timestamp-derived seed rather than panicking the probe.
    if getrandom::fill(&mut nonce).is_err() {
        let now = Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
        nonce[..8].copy_from_slice(&now.to_be_bytes());
    }
    let created = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();

    let digest = compute_password_digest(&nonce, &created, password);
    let nonce_b64 = B64.encode(nonce);

    // xml-escape the username — operator-typed creds may
    // contain `&`, `<`, `>`, or `"`. Password digest is already
    // base64 so it's safe inline.
    let user_esc = xml_escape(username);

    format!(
        r#"<wsse:Security s:mustUnderstand="1" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"><wsse:UsernameToken><wsse:Username>{user_esc}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{nonce_b64}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security>"#
    )
}

/// `PasswordDigest = Base64( SHA1( raw_nonce || created_ascii
/// || password_utf8 ) )` per WS-Security UsernameToken Profile
/// 1.0 §3.1. The SHA1 input uses the RAW nonce bytes, NOT the
/// base64 form that goes in the envelope. Mixing those up is
/// the #1 source of spurious `NotAuthorized` faults from ONVIF
/// cameras.
fn compute_password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(nonce);
    hasher.update(created.as_bytes());
    hasher.update(password.as_bytes());
    B64.encode(hasher.finalize())
}

fn build_get_profiles_envelope(username: &str, password: &str, ver: MediaVer) -> String {
    let header = build_ws_security_header(username, password);
    let ns = ver.ns();
    let p = ver.prefix();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:{p}="{ns}"><s:Header>{header}</s:Header><s:Body><{p}:GetProfiles/></s:Body></s:Envelope>"#
    )
}

fn build_get_stream_uri_envelope(
    username: &str,
    password: &str,
    ver: MediaVer,
    profile_token: &str,
) -> String {
    let header = build_ws_security_header(username, password);
    let ns = ver.ns();
    let p = ver.prefix();
    let tok_esc = xml_escape(profile_token);
    let body = match ver {
        MediaVer::V1 => format!(
            r#"<{p}:GetStreamUri><{p}:StreamSetup><tt:Stream xmlns:tt="http://www.onvif.org/ver10/schema">RTP-Unicast</tt:Stream><tt:Transport xmlns:tt="http://www.onvif.org/ver10/schema"><tt:Protocol>RTSP</tt:Protocol></tt:Transport></{p}:StreamSetup><{p}:ProfileToken>{tok_esc}</{p}:ProfileToken></{p}:GetStreamUri>"#
        ),
        MediaVer::V2 => format!(
            r#"<{p}:GetStreamUri><{p}:Protocol>RTSP</{p}:Protocol><{p}:ProfileToken>{tok_esc}</{p}:ProfileToken></{p}:GetStreamUri>"#
        ),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:{p}="{ns}"><s:Header>{header}</s:Header><s:Body>{body}</s:Body></s:Envelope>"#
    )
}

/// Walk a `GetProfilesResponse` envelope and yield one
/// [`ProfileSummary`] per `<trt:Profiles>` (Media1) or
/// `<tr2:Profiles>` (Media2) element. Vendors disagree on
/// prefixes; we match on local-name only.
///
/// Picked fields:
/// * `@token` attribute → `ProfileSummary.token`
/// * `<tt:Name>` text → `ProfileSummary.name`
/// * first `<tt:VideoEncoderConfiguration>` block's
///   `<tt:Encoding>` → `codec`
/// * that block's `<tt:Resolution>` `Width`/`Height` →
///   `resolution`
fn parse_profiles_response(body: &str) -> Result<Vec<ProfileSummary>, String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut profiles: Vec<ProfileSummary> = Vec::new();
    // Stack of element local-names we're currently inside, so
    // we can disambiguate `<Name>` (profile name) from any
    // other Name elsewhere in the response.
    let mut stack: Vec<String> = Vec::new();
    let mut current: Option<ProfileSummary> = None;
    let mut text_acc = String::new();
    // Resolution accumulator — Width and Height come as
    // sibling children of `<Resolution>` and we need both.
    let mut cur_w: Option<u32> = None;
    let mut cur_h: Option<u32> = None;
    // Track whether we're inside the FIRST VideoEncoderConfig
    // for the current profile (skip subsequent ones — they're
    // typically duplicate transport entries).
    let mut vec_seen_for_profile = false;

    loop {
        let evt = match reader.read_event_into(&mut buf) {
            Ok(e) => e,
            Err(e) => return Err(format!("xml parse error: {e}")),
        };
        match evt {
            Event::Start(e) => {
                let name = local_name(&e.name());
                if name == "Profiles" {
                    let mut tok = String::new();
                    for attr in e.attributes().flatten() {
                        if local_name(&attr.key) == "token" {
                            tok = attr.unescape_value().unwrap_or_default().to_string();
                        }
                    }
                    current = Some(ProfileSummary {
                        token: tok,
                        name: String::new(),
                        codec: None,
                        resolution: None,
                    });
                    vec_seen_for_profile = false;
                }
                stack.push(name);
                text_acc.clear();
            }
            Event::Text(t) => {
                if let Ok(s) = t.unescape() {
                    text_acc.push_str(&s);
                }
            }
            Event::End(e) => {
                let name = local_name(&e.name());
                if let Some(prof) = current.as_mut() {
                    match name.as_str() {
                        "Name" if parent_is(&stack, "Profiles") => {
                            prof.name = text_acc.trim().to_string();
                        }
                        "Encoding"
                            if parent_is(&stack, "VideoEncoderConfiguration")
                                && !vec_seen_for_profile =>
                        {
                            prof.codec = Some(text_acc.trim().to_string());
                        }
                        "Width" if parent_is(&stack, "Resolution") => {
                            cur_w = text_acc.trim().parse().ok();
                        }
                        "Height" if parent_is(&stack, "Resolution") => {
                            cur_h = text_acc.trim().parse().ok();
                        }
                        "Resolution"
                            if parent_is(&stack, "VideoEncoderConfiguration")
                                && !vec_seen_for_profile =>
                        {
                            if let (Some(w), Some(h)) = (cur_w.take(), cur_h.take()) {
                                prof.resolution = Some(format!("{w}x{h}"));
                            }
                        }
                        "VideoEncoderConfiguration" => {
                            vec_seen_for_profile = true;
                        }
                        _ => {}
                    }
                }
                if name == "Profiles" {
                    if let Some(p) = current.take() {
                        if !p.token.is_empty() {
                            profiles.push(p);
                        }
                    }
                }
                stack.pop();
                text_acc.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if profiles.is_empty() {
        return Err("no <Profiles> elements in GetProfilesResponse".to_string());
    }
    Ok(profiles)
}

/// Pull the `<tt:Uri>` text out of a GetStreamUriResponse.
/// Media1 wraps it under `<MediaUri><Uri>`, Media2 under
/// `<MediaUri><Uri>` as well — same local-name match works for
/// both.
fn parse_stream_uri_response(body: &str) -> Option<String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_uri = false;
    let mut out = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if local_name(&e.name()) == "Uri" => {
                in_uri = true;
                out.clear();
            }
            Ok(Event::Text(t)) if in_uri => {
                if let Ok(s) = t.unescape() {
                    out.push_str(&s);
                }
            }
            Ok(Event::End(e)) if local_name(&e.name()) == "Uri" => {
                if !out.trim().is_empty() {
                    return Some(out.trim().to_string());
                }
                in_uri = false;
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Extract `<env:Fault><env:Reason><env:Text>...` for SOAP 1.2
/// faults, or `<faultstring>...` for SOAP 1.1. Returns `None`
/// when the body isn't a fault.
fn extract_fault_reason(body: &str) -> Option<String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_target = false;
    let mut text_acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(&e.name());
                if n == "Text" || n == "faultstring" || n == "Subcode" {
                    in_target = true;
                    text_acc.clear();
                }
            }
            Ok(Event::Text(t)) if in_target => {
                if let Ok(s) = t.unescape() {
                    text_acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let n = local_name(&e.name());
                if (n == "Text" || n == "faultstring") && !text_acc.trim().is_empty() {
                    return Some(text_acc.trim().to_string());
                }
                if n == "Subcode" && !text_acc.trim().is_empty() {
                    // Subcode values look like
                    // `ter:NotAuthorized` — return them when
                    // there's no human-readable Text/Reason
                    // (some cameras only return Subcode). The
                    // caller pattern-matches on these strings
                    // for the Media1 fallback decision.
                    return Some(text_acc.trim().to_string());
                }
                in_target = false;
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

/// Drop any `prefix:` from a quick-xml `QName` and return the
/// owned local name. Vendors disagree on namespace prefixes
/// (`trt:`, `tr2:`, `tt:`, `wsa:` vs `a:` …); matching on local
/// name only is the only way to write a parser that survives
/// every shipped firmware.
fn local_name(name: &quick_xml::name::QName) -> String {
    let raw = std::str::from_utf8(name.as_ref()).unwrap_or("");
    match raw.rfind(':') {
        Some(i) => raw[i + 1..].to_string(),
        None => raw.to_string(),
    }
}

fn parent_is(stack: &[String], parent_local_name: &str) -> bool {
    stack.len() >= 2 && stack[stack.len() - 2] == parent_local_name
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hikvision_style_get_profiles_response() {
        // Stripped-down envelope based on Hikvision DS-2CD-series
        // firmware (a real-world capture). Two profiles, both
        // with H.264 main + a resolution field.
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope"
              xmlns:trt="http://www.onvif.org/ver10/media/wsdl"
              xmlns:tt="http://www.onvif.org/ver10/schema">
  <env:Body>
    <trt:GetProfilesResponse>
      <trt:Profiles token="Profile_1" fixed="true">
        <tt:Name>mainStream</tt:Name>
        <tt:VideoEncoderConfiguration token="VideoEncoder_1">
          <tt:Encoding>H264</tt:Encoding>
          <tt:Resolution>
            <tt:Width>1920</tt:Width>
            <tt:Height>1080</tt:Height>
          </tt:Resolution>
        </tt:VideoEncoderConfiguration>
      </trt:Profiles>
      <trt:Profiles token="Profile_2" fixed="true">
        <tt:Name>subStream</tt:Name>
        <tt:VideoEncoderConfiguration token="VideoEncoder_2">
          <tt:Encoding>H264</tt:Encoding>
          <tt:Resolution>
            <tt:Width>640</tt:Width>
            <tt:Height>480</tt:Height>
          </tt:Resolution>
        </tt:VideoEncoderConfiguration>
      </trt:Profiles>
    </trt:GetProfilesResponse>
  </env:Body>
</env:Envelope>"#;
        let profiles = parse_profiles_response(body).expect("parses");
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].token, "Profile_1");
        assert_eq!(profiles[0].name, "mainStream");
        assert_eq!(profiles[0].codec.as_deref(), Some("H264"));
        assert_eq!(profiles[0].resolution.as_deref(), Some("1920x1080"));
        assert_eq!(profiles[1].token, "Profile_2");
        assert_eq!(profiles[1].resolution.as_deref(), Some("640x480"));
    }

    #[test]
    fn parses_stream_uri_response() {
        let body = r#"<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope"
                      xmlns:trt="http://www.onvif.org/ver10/media/wsdl"
                      xmlns:tt="http://www.onvif.org/ver10/schema">
  <env:Body>
    <trt:GetStreamUriResponse>
      <trt:MediaUri>
        <tt:Uri>rtsp://192.168.1.66:554/Streaming/Channels/101</tt:Uri>
        <tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>
        <tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>
        <tt:Timeout>PT60S</tt:Timeout>
      </trt:MediaUri>
    </trt:GetStreamUriResponse>
  </env:Body>
</env:Envelope>"#;
        let uri = parse_stream_uri_response(body).expect("uri parses");
        assert_eq!(uri, "rtsp://192.168.1.66:554/Streaming/Channels/101");
    }

    #[test]
    fn extracts_soap_fault_reason() {
        let body = r#"<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope">
  <env:Body>
    <env:Fault>
      <env:Code>
        <env:Value>env:Sender</env:Value>
        <env:Subcode>
          <env:Value>ter:NotAuthorized</env:Value>
        </env:Subcode>
      </env:Code>
      <env:Reason>
        <env:Text xml:lang="en">Sender not Authorized</env:Text>
      </env:Reason>
    </env:Fault>
  </env:Body>
</env:Envelope>"#;
        let reason = extract_fault_reason(body).expect("fault reason");
        assert!(
            reason.contains("Not") && reason.contains("Authorized"),
            "unexpected reason: {reason:?}"
        );
    }

    #[test]
    fn ws_username_token_digest_matches_reference_vector() {
        // Reference vector for `compute_password_digest`:
        //   nonce   = b64dec("WScqanjCEAC4mQoBE07sAQ==")
        //   created = "2003-07-16T01:24:32Z"
        //   password= "password"
        //   digest  = "35G+fVLJOPu0MSJRj20Be9HMkuQ="
        //
        // Verified independently via:
        //   python3 -c "import hashlib,base64;
        //     n=base64.b64decode('WScqanjCEAC4mQoBE07sAQ==');
        //     h=hashlib.sha1(); h.update(n);
        //     h.update(b'2003-07-16T01:24:32Z');
        //     h.update(b'password');
        //     print(base64.b64encode(h.digest()).decode())"
        //
        // Important: the SHA1 input is the RAW (decoded) nonce
        // bytes, not the base64 string. Hashing the base64
        // form would yield "A80VTrohYhqwCrYTPt3spbq+CeA=" — a
        // common mistake that produces `NotAuthorized` faults
        // from every ONVIF camera in existence.
        let nonce = B64.decode("WScqanjCEAC4mQoBE07sAQ==").expect("b64");
        let got = compute_password_digest(&nonce, "2003-07-16T01:24:32Z", "password");
        assert_eq!(got, "35G+fVLJOPu0MSJRj20Be9HMkuQ=");
    }

    #[test]
    fn envelope_builders_are_well_formed_xml() {
        // Sanity check: quick-xml round-trips both envelope
        // builders without erroring. Catches stray un-escaped
        // characters in our format!() templates.
        let env1 = build_get_profiles_envelope("admin", "p@ss<>&\"'", MediaVer::V1);
        let mut r = Reader::from_str(&env1);
        let mut buf = Vec::new();
        loop {
            match r.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Err(e) => panic!("invalid xml: {e}\n{env1}"),
                _ => {}
            }
        }
        let env2 = build_get_stream_uri_envelope("u", "p", MediaVer::V2, "Profile_<1>");
        let mut r = Reader::from_str(&env2);
        let mut buf = Vec::new();
        loop {
            match r.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Err(e) => panic!("invalid xml: {e}\n{env2}"),
                _ => {}
            }
        }
    }

    #[test]
    fn codec_kind_from_onvif_maps_known_encodings() {
        use nexus_types::CodecKind;
        assert_eq!(codec_kind_from_onvif("H264"), Some(CodecKind::H264));
        assert_eq!(codec_kind_from_onvif("h264"), Some(CodecKind::H264));
        assert_eq!(codec_kind_from_onvif("H265"), Some(CodecKind::H265));
        assert_eq!(codec_kind_from_onvif("HEVC"), Some(CodecKind::H265));
        assert_eq!(codec_kind_from_onvif("  H265  "), Some(CodecKind::H265));
        // Out-of-scope codecs surface the raw string but no typed kind.
        assert_eq!(codec_kind_from_onvif("JPEG"), None);
        assert_eq!(codec_kind_from_onvif("MPEG4"), None);
        assert_eq!(codec_kind_from_onvif(""), None);
    }
}

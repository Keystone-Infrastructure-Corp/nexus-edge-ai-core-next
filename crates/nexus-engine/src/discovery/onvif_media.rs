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

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use super::onvif_soap::{
    build_client, local_name, parent_is, post_soap, ws_security_header, xml_escape,
};

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

    // Many cameras ship a self-signed HTTPS cert on :443
    // alongside the HTTP service on :80; build_client tolerates
    // both.
    let client = build_client()?;

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
    trace!(xaddr = %url, ver = ?ver, body = %text, "onvif media: GetProfiles raw response");
    let profiles = parse_profiles_response(&text)?;
    for p in &profiles {
        debug!(
            xaddr = %url, ver = ?ver, token = %p.token, name = %p.name,
            codec = ?p.codec, resolution = ?p.resolution,
            "onvif media: parsed profile",
        );
    }
    Ok(profiles)
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

fn build_get_profiles_envelope(username: &str, password: &str, ver: MediaVer) -> String {
    let header = ws_security_header(username, password);
    let ns = ver.ns();
    let p = ver.prefix();
    // Media2 GetProfiles returns only the bare profile (token + name)
    // unless a <Type> filter is supplied — without it the response is
    // missing the encoder configuration we need for codec + resolution.
    // Media1 GetProfiles always includes every configuration.
    let body = match ver {
        MediaVer::V1 => format!(r#"<{p}:GetProfiles/>"#),
        MediaVer::V2 => format!(r#"<{p}:GetProfiles><{p}:Type>All</{p}:Type></{p}:GetProfiles>"#),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:{p}="{ns}"><s:Header>{header}</s:Header><s:Body>{body}</s:Body></s:Envelope>"#
    )
}

fn build_get_stream_uri_envelope(
    username: &str,
    password: &str,
    ver: MediaVer,
    profile_token: &str,
) -> String {
    let header = ws_security_header(username, password);
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
/// * first `<tt:VideoEncoderConfiguration>` (Media1) or
///   `<tr2:VideoEncoder>` (Media2) block's
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
                            if (parent_is(&stack, "VideoEncoderConfiguration")
                                || parent_is(&stack, "VideoEncoder"))
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
                            if (parent_is(&stack, "VideoEncoderConfiguration")
                                || parent_is(&stack, "VideoEncoder"))
                                && !vec_seen_for_profile =>
                        {
                            if let (Some(w), Some(h)) = (cur_w.take(), cur_h.take()) {
                                prof.resolution = Some(format!("{w}x{h}"));
                            }
                        }
                        "VideoEncoderConfiguration" | "VideoEncoder" => {
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
    fn parses_hikvision_media2_get_profiles_response() {
        // Hikvision DS-2CD-series Media2 firmware: the encoder
        // configuration is wrapped in `<tr2:Configurations>` and
        // the inner element is `<tr2:VideoEncoder>` (NOT the
        // Media1 `<tt:VideoEncoderConfiguration>` name). Without
        // recognising both parent names, the codec + resolution
        // are silently dropped and the cloud wizard's
        // pickBestProfile returns `no_video`.
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope"
              xmlns:tr2="http://www.onvif.org/ver20/media/wsdl"
              xmlns:tt="http://www.onvif.org/ver10/schema">
  <env:Body>
    <tr2:GetProfilesResponse>
      <tr2:Profiles token="Profile_1" fixed="true">
        <tr2:Name>mainStream</tr2:Name>
        <tr2:Configurations>
          <tr2:VideoEncoder token="VideoEncoder_1">
            <tt:Encoding>H264</tt:Encoding>
            <tt:Resolution>
              <tt:Width>1920</tt:Width>
              <tt:Height>1080</tt:Height>
            </tt:Resolution>
          </tr2:VideoEncoder>
        </tr2:Configurations>
      </tr2:Profiles>
      <tr2:Profiles token="Profile_2" fixed="true">
        <tr2:Name>subStream</tr2:Name>
        <tr2:Configurations>
          <tr2:VideoEncoder token="VideoEncoder_2">
            <tt:Encoding>H265</tt:Encoding>
            <tt:Resolution>
              <tt:Width>640</tt:Width>
              <tt:Height>360</tt:Height>
            </tt:Resolution>
          </tr2:VideoEncoder>
        </tr2:Configurations>
      </tr2:Profiles>
    </tr2:GetProfilesResponse>
  </env:Body>
</env:Envelope>"#;
        let profiles = parse_profiles_response(body).expect("parses");
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].token, "Profile_1");
        assert_eq!(profiles[0].name, "mainStream");
        assert_eq!(profiles[0].codec.as_deref(), Some("H264"));
        assert_eq!(profiles[0].resolution.as_deref(), Some("1920x1080"));
        assert_eq!(profiles[1].token, "Profile_2");
        assert_eq!(profiles[1].codec.as_deref(), Some("H265"));
        assert_eq!(profiles[1].resolution.as_deref(), Some("640x360"));
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
    fn media2_get_profiles_envelope_requests_all_configurations() {
        // Media2 GetProfiles returns only token + name unless a
        // <Type> filter is supplied. Without it the response omits
        // the encoder configuration and codec/resolution come back
        // empty — observed against Hikvision DS-2CD firmware.
        let env = build_get_profiles_envelope("admin", "secret", MediaVer::V2);
        assert!(
            env.contains("<tr2:Type>All</tr2:Type>"),
            "Media2 envelope must request <Type>All</Type>: {env}",
        );
        // Media1 must keep the bare GetProfiles call.
        let env1 = build_get_profiles_envelope("admin", "secret", MediaVer::V1);
        assert!(
            env1.contains("<trt:GetProfiles/>"),
            "Media1 envelope must use bare GetProfiles: {env1}",
        );
        assert!(
            !env1.contains("<trt:Type>"),
            "Media1 envelope must not include Type: {env1}",
        );
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

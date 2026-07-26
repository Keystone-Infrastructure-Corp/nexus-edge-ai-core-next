//! Shared ONVIF SOAP plumbing.
//!
//! Phase 7.6.2 expands the edge's ONVIF surface from "media
//! probe only" ([`super::onvif_media`]) to the full device-control
//! field set (PTZ / imaging / device / deviceio / encoder /
//! snapshot). Every one of those services speaks the same wire
//! dialect — a SOAP 1.2 envelope authenticated with a
//! WS-Security UsernameToken digest header — so the transport,
//! the auth header, the fault parsing, and the XML local-name
//! helpers all live here once and are shared by every service
//! module. This is a deliberate de-duplication of the helpers
//! that originally lived (privately) in [`super::onvif_media`];
//! the security-critical password-digest in particular MUST have
//! exactly one implementation.
//!
//! ## Auth model
//!
//! WS-Security UsernameToken Profile 1.0 §3.1 mandates SHA-1 for
//! `PasswordDigest`: `Base64(SHA1(raw_nonce || created ||
//! password))`. The SHA1 input uses the **raw** nonce bytes (not
//! the base64 form that goes in the envelope) — getting that
//! wrong is the #1 reason new ONVIF clients see `NotAuthorized`
//! faults from cameras that work fine with ONVIF Device Manager.
//!
//! ## Endpoint model
//!
//! Each service module is handed the per-camera ONVIF device
//! endpoint persisted in Phase 7.6.1 (`CameraOnvif::endpoint`,
//! the WS-Discovery `XAddrs` device-service URL). The vast
//! majority of ONVIF cameras serve *every* service at that one
//! URL and route the request by its `Action` / `SOAPAction`
//! header, which is what [`OnvifService::call`] sets. Cameras
//! that publish per-service XAddrs (some Axis firmware) are a
//! v1 limitation surfaced as a clean SOAP fault rather than a
//! silent failure; resolving per-service XAddrs via
//! `GetServices` is a future refinement.
//!
//! ## Credential discipline
//!
//! The username / password handed to these helpers are
//! edge-resident only ([AGENTS.md rule 6] / REPO_BOUNDARY R5b).
//! They are interpolated into a request envelope that never
//! leaves the edge and are never logged (the `trace!` bodies in
//! the service modules log *responses*, not request envelopes).

use std::collections::HashMap;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chrono::Utc;
use quick_xml::events::Event;
use quick_xml::Reader;
use sha1::{Digest, Sha1};

/// Per-request timeout for one SOAP round-trip. Live device
/// commands (PTZ move, imaging set) answer in well under a
/// second; 5 s tolerates a slow embedded HTTP server without
/// pinning the operator-facing request.
pub(crate) const REQ_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum bytes accepted from a SOAP response body. ONVIF
/// control responses are a few KiB; this 256 KiB cap protects
/// against a misbehaving / hostile endpoint streaming gigabytes.
pub(crate) const MAX_BODY: usize = 256 * 1024;

/// ONVIF schema namespace (`tt:` prefix) — carries the shared
/// value types (`PanTilt`, `Zoom`, `Resolution`, …) that the
/// per-service request bodies embed.
pub(crate) const TT_NS: &str = "http://www.onvif.org/ver10/schema";

/// One ONVIF SOAP service: its WSDL namespace + the XML prefix
/// we stamp on the request body. Kept identical to the prefixes
/// ONVIF Device Manager uses so firmware that pattern-matches on
/// them stays happy.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OnvifService {
    pub prefix: &'static str,
    pub ns: &'static str,
}

/// PTZ service (`ver20/ptz`). Continuous / relative / absolute
/// move, presets, home, auxiliary commands, node/config options.
pub(crate) const PTZ: OnvifService = OnvifService {
    prefix: "tptz",
    ns: "http://www.onvif.org/ver20/ptz/wsdl",
};

/// Imaging service (`ver20/imaging`). Brightness / contrast /
/// exposure / focus / white-balance / WDR / defog / etc.
pub(crate) const IMAGING: OnvifService = OnvifService {
    prefix: "timg",
    ns: "http://www.onvif.org/ver20/imaging/wsdl",
};

/// Core device service (`ver10/device`). Device info, system
/// time / NTP, system log, reboot, capabilities.
pub(crate) const DEVICE: OnvifService = OnvifService {
    prefix: "tds",
    ns: "http://www.onvif.org/ver10/device/wsdl",
};

/// Device I/O service (`ver10/deviceIO`). Relay outputs and
/// digital inputs.
pub(crate) const DEVICEIO: OnvifService = OnvifService {
    prefix: "tmd",
    ns: "http://www.onvif.org/ver10/deviceIO/wsdl",
};

/// Media1 service (`ver10/media`). Video-encoder configuration
/// + OSD + audio sources live here on Profile S cameras.
pub(crate) const MEDIA1: OnvifService = OnvifService {
    prefix: "trt",
    ns: "http://www.onvif.org/ver10/media/wsdl",
};

/// Media2 service (`ver20/media`). OSD lives here on Profile T
/// cameras (`GetOSDs` / `SetOSD` / `CreateOSD` / `DeleteOSD`).
pub(crate) const MEDIA2: OnvifService = OnvifService {
    prefix: "tr2",
    ns: "http://www.onvif.org/ver20/media/wsdl",
};

impl OnvifService {
    /// SOAP `Action` value (`<ns>/<op>`). Cameras route incoming
    /// SOAP to the right service via this header even when every
    /// service is served at one URL.
    pub(crate) fn action(&self, op: &str) -> String {
        format!("{}/{}", self.ns, op)
    }

    /// Wrap a request body in a full SOAP envelope with the
    /// WS-Security header. The envelope always declares `s`,
    /// `tt` (schema), and this service's prefix; bodies embed
    /// `tt:`-prefixed value types directly.
    pub(crate) fn envelope(&self, username: &str, password: &str, body: &str) -> String {
        let header = ws_security_header(username, password);
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="{TT_NS}" xmlns:{prefix}="{ns}"><s:Header>{header}</s:Header><s:Body>{body}</s:Body></s:Envelope>"#,
            prefix = self.prefix,
            ns = self.ns,
        )
    }

    /// Build a one-shot HTTPS client, post `body` (already an
    /// inner SOAP body fragment) wrapped in this service's
    /// envelope, and return the response text. The single entry
    /// point every service module funnels through.
    pub(crate) async fn call(
        &self,
        endpoint: &str,
        username: &str,
        password: &str,
        op: &str,
        body: &str,
    ) -> Result<String, String> {
        let client = build_client()?;
        let env = self.envelope(username, password, body);
        post_soap(&client, endpoint, &self.action(op), &env).await
    }
}

/// One-shot reqwest client tuned for embedded camera HTTP
/// servers (short timeout, tolerant of the self-signed HTTPS
/// certs cameras ship on :443).
pub(crate) fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(REQ_TIMEOUT)
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| format!("http client build failed: {e}"))
}

/// Send one SOAP request and return the response body. Errors on
/// connect / read failure, non-2xx status without a fault body,
/// or a parsed SOAP `<Fault>` (the fault reason is surfaced).
pub(crate) async fn post_soap(
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

    // SOAP faults arrive as 200 OK + `<Fault>` OR (more often)
    // 500 + `<Fault>`; the body is the useful signal either way.
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
/// digest auth. Returns the header XML — callers embed it inside
/// `<s:Header>...</s:Header>` (done by [`OnvifService::envelope`]).
pub(crate) fn ws_security_header(username: &str, password: &str) -> String {
    // 16 random bytes per ONVIF Device Manager's behaviour;
    // some cameras reject nonces < 8 bytes.
    let mut nonce = [0u8; 16];
    // getrandom can theoretically fail; fall back to a
    // timestamp-derived seed rather than panicking the command.
    if getrandom::fill(&mut nonce).is_err() {
        let now = Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
        nonce[..8].copy_from_slice(&now.to_be_bytes());
    }
    let created = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();

    let digest = compute_password_digest(&nonce, &created, password);
    let nonce_b64 = B64.encode(nonce);
    let user_esc = xml_escape(username);

    format!(
        r#"<wsse:Security s:mustUnderstand="1" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"><wsse:UsernameToken><wsse:Username>{user_esc}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{nonce_b64}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security>"#
    )
}

/// `PasswordDigest = Base64( SHA1( raw_nonce || created_ascii ||
/// password_utf8 ) )` per WS-Security UsernameToken Profile 1.0
/// §3.1. The SHA1 input uses the RAW nonce bytes, NOT the base64
/// form that goes in the envelope.
pub(crate) fn compute_password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(nonce);
    hasher.update(created.as_bytes());
    hasher.update(password.as_bytes());
    B64.encode(hasher.finalize())
}

/// Extract the most useful text from a SOAP `<Fault>`: the deepest
/// `<Subcode><Value>` ONVIF code combined with the human-readable
/// `<Reason><Text>` (SOAP 1.2) or `<faultstring>` (SOAP 1.1) — e.g.
/// `ter:InvalidParameter: The VideoSourceConfigurationToken is invalid`.
/// Falls back to whichever half is present. `None` when the body isn't a
/// fault.
pub(crate) fn extract_fault_reason(body: &str) -> Option<String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut capture = false;
    let mut acc = String::new();
    // The DEEPEST `<Subcode><Value>` (the most specific ONVIF error code —
    // e.g. `ter:InvalidParameter` nested under `ter:InvalidArgVal`) and the
    // human-readable `<Reason><Text>` / `<faultstring>`. Cameras name the
    // offending field in the Reason text, so surfacing BOTH turns a bare
    // `ter:InvalidParameter` into an actionable message
    // (`ter:InvalidParameter: The VideoSourceConfigurationToken is invalid`).
    let mut subcode: Option<String> = None;
    let mut reason: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(&e.name());
                let parent = stack.last().map(String::as_str);
                // `<Value>` directly under `<Code>` is the SOAP role
                // (soap:Sender/Receiver) — only the ones under `<Subcode>`
                // carry the ONVIF `ter:*` code.
                capture = (n == "Value" && parent == Some("Subcode"))
                    || (n == "Text" && parent == Some("Reason"))
                    || n == "faultstring";
                if capture {
                    acc.clear();
                }
                stack.push(n);
            }
            Ok(Event::Text(t)) if capture => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(_)) => {
                let n = stack.pop();
                if capture {
                    let v = acc.trim().to_string();
                    if !v.is_empty() {
                        match n.as_deref() {
                            // Outer subcode is emitted first, inner last, so
                            // last-write-wins yields the deepest (most
                            // specific) code.
                            Some("Value") => subcode = Some(v),
                            Some("Text") | Some("faultstring") if reason.is_none() => {
                                reason = Some(v);
                            }
                            _ => {}
                        }
                    }
                    capture = false;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    match (subcode, reason) {
        (Some(sc), Some(rs)) => Some(format!("{sc}: {rs}")),
        (Some(sc), None) => Some(sc),
        (None, Some(rs)) => Some(rs),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

/// Drop any `prefix:` from a quick-xml `QName` and return the
/// owned local name. Vendors disagree on namespace prefixes, so
/// matching on local name only is the one parser strategy that
/// survives every shipped firmware.
pub(crate) fn local_name(name: &quick_xml::name::QName) -> String {
    let raw = std::str::from_utf8(name.as_ref()).unwrap_or("");
    match raw.rfind(':') {
        Some(i) => raw[i + 1..].to_string(),
        None => raw.to_string(),
    }
}

/// `true` when the element directly enclosing the current one
/// has the given local name. `stack` is the running stack of
/// open-element local names maintained by a parser loop.
pub(crate) fn parent_is(stack: &[String], parent_local_name: &str) -> bool {
    stack.len() >= 2 && stack[stack.len() - 2] == parent_local_name
}

/// Minimal XML text/attribute escaper for values interpolated
/// into a request envelope.
pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Collect the trimmed text of the FIRST element matching each
/// requested local name into a map. Handy for flat responses
/// like `GetDeviceInformationResponse` (Manufacturer / Model /
/// FirmwareVersion / SerialNumber / HardwareId) where every
/// field is a distinct one-off element. Names not present in the
/// body are simply absent from the map.
pub(crate) fn collect_first_texts(body: &str, wanted: &[&str]) -> HashMap<String, String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out: HashMap<String, String> = HashMap::new();
    let mut cur: Option<String> = None;
    let mut text_acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(&e.name());
                cur = wanted.iter().find(|w| **w == n).map(|w| w.to_string());
                text_acc.clear();
            }
            Ok(Event::Text(t)) if cur.is_some() => {
                if let Ok(s) = t.unescape() {
                    text_acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                if let Some(name) = cur.take() {
                    if local_name(&e.name()) == name {
                        out.entry(name)
                            .or_insert_with(|| text_acc.trim().to_string());
                    }
                }
                text_acc.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Pull the trimmed text of the first element with the given
/// local name, anywhere in the body. `None` when absent or
/// empty.
pub(crate) fn first_text(body: &str, want: &str) -> Option<String> {
    collect_first_texts(body, &[want])
        .remove(want)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_username_token_digest_matches_reference_vector() {
        // Reference vector: the SHA1 input is the RAW (decoded)
        // nonce bytes, not the base64 string. Hashing the base64
        // form yields a wrong digest and `NotAuthorized` from
        // every ONVIF camera. Verified independently with python
        // hashlib.
        let nonce = B64.decode("WScqanjCEAC4mQoBE07sAQ==").expect("b64");
        let got = compute_password_digest(&nonce, "2003-07-16T01:24:32Z", "password");
        assert_eq!(got, "35G+fVLJOPu0MSJRj20Be9HMkuQ=");
    }

    #[test]
    fn ws_security_header_embeds_digest_not_plaintext_password() {
        // The header must carry a PasswordDigest, never the raw
        // password (credential discipline — even though the
        // envelope is edge-resident, plaintext-in-XML would be a
        // regression waiting to leak via a debug log).
        let h = ws_security_header("admin", "sup3r-secret");
        assert!(h.contains("PasswordDigest"), "{h}");
        assert!(
            !h.contains("sup3r-secret"),
            "plaintext password leaked: {h}"
        );
    }

    #[test]
    fn envelope_is_well_formed_and_escapes_credentials() {
        let env = PTZ.envelope("ad<min", "p@ss<>&\"'", "<tptz:Stop/>");
        let mut r = Reader::from_str(&env);
        let mut buf = Vec::new();
        loop {
            match r.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Err(e) => panic!("invalid xml: {e}\n{env}"),
                _ => {}
            }
        }
        assert!(env.contains("xmlns:tptz=\"http://www.onvif.org/ver20/ptz/wsdl\""));
        // The raw `<` in the username must have been escaped.
        assert!(!env.contains("ad<min"), "username not escaped: {env}");
    }

    #[test]
    fn action_header_is_namespace_slash_op() {
        assert_eq!(
            PTZ.action("ContinuousMove"),
            "http://www.onvif.org/ver20/ptz/wsdl/ContinuousMove"
        );
        assert_eq!(
            DEVICE.action("SystemReboot"),
            "http://www.onvif.org/ver10/device/wsdl/SystemReboot"
        );
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
    fn fault_reason_combines_deepest_subcode_and_text() {
        // ONVIF nests the specific code under a generic one; the human
        // message names the offending parameter. Both matter for OSD
        // debugging, where a bare `ter:InvalidParameter` is useless.
        let body = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body><s:Fault>
    <s:Code>
      <s:Value>s:Sender</s:Value>
      <s:Subcode>
        <s:Value>ter:InvalidArgVal</s:Value>
        <s:Subcode><s:Value>ter:InvalidParameter</s:Value></s:Subcode>
      </s:Subcode>
    </s:Code>
    <s:Reason><s:Text xml:lang="en">The VideoSourceConfigurationToken is invalid</s:Text></s:Reason>
  </s:Fault></s:Body>
</s:Envelope>"#;
        assert_eq!(
            extract_fault_reason(body).as_deref(),
            Some("ter:InvalidParameter: The VideoSourceConfigurationToken is invalid"),
        );
    }

    #[test]
    fn collect_first_texts_picks_named_fields() {
        let body = r#"<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope"
              xmlns:tds="http://www.onvif.org/ver10/device/wsdl">
  <env:Body>
    <tds:GetDeviceInformationResponse>
      <tds:Manufacturer>Hikvision</tds:Manufacturer>
      <tds:Model>DS-2CD2042WD-I</tds:Model>
      <tds:FirmwareVersion>V5.4.5</tds:FirmwareVersion>
      <tds:SerialNumber>DS-2CD2042WD-I20160101</tds:SerialNumber>
      <tds:HardwareId>88</tds:HardwareId>
    </tds:GetDeviceInformationResponse>
  </env:Body>
</env:Envelope>"#;
        let m = collect_first_texts(
            body,
            &[
                "Manufacturer",
                "Model",
                "FirmwareVersion",
                "SerialNumber",
                "HardwareId",
            ],
        );
        assert_eq!(m.get("Manufacturer").map(String::as_str), Some("Hikvision"));
        assert_eq!(m.get("Model").map(String::as_str), Some("DS-2CD2042WD-I"));
        assert_eq!(m.get("FirmwareVersion").map(String::as_str), Some("V5.4.5"));
        assert_eq!(m.get("HardwareId").map(String::as_str), Some("88"));
    }
}

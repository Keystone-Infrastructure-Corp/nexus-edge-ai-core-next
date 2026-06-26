//! ONVIF snapshot client (`ver10/media`).
//!
//! Phase 7.6.2. Resolves a media profile's still-image URL
//! ([`get_snapshot_uri`]) and fetches the JPEG behind it
//! ([`fetch_snapshot`]). The control-plane round-trip
//! (`GetSnapshotUri`) is a normal WS-Security SOAP call through
//! [`onvif_soap::MEDIA1`]; the image fetch itself is a plain
//! authenticated HTTP `GET` against the URL the camera returned.
//!
//! ## Auth on the image fetch
//!
//! The snapshot endpoint is guarded by HTTP auth, not WS-Security.
//! Cameras commonly accept HTTP Basic; HTTP Digest is also widely
//! used. `reqwest` (our only HTTP client — no new dependency is
//! permitted for this work) implements Basic natively but not
//! Digest, so v1 sends Basic credentials. A camera configured for
//! Digest-only on the snapshot path returns `401`, which is
//! surfaced as a clear error rather than a silent empty image.
//!
//! The public operations are consumed by the operator
//! admin-passthrough routes added in Phase 7.6.3; until those land
//! they have no in-crate caller, hence the module-level `dead_code`
//! allowance.
#![allow(dead_code)]

use super::onvif_soap::{build_client, first_text, xml_escape, MEDIA1};

/// Cap on a fetched snapshot. A 4K JPEG is well under this; the
/// cap protects against a misbehaving / hostile endpoint streaming
/// an unbounded body.
const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

/// Resolve the snapshot URL for a media profile.
pub async fn get_snapshot_uri(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
) -> Result<String, String> {
    let body = get_snapshot_uri_body(profile_token);
    let resp = MEDIA1
        .call(endpoint, username, password, "GetSnapshotUri", &body)
        .await?;
    first_text(&resp, "Uri").ok_or_else(|| "GetSnapshotUri response missing Uri".to_string())
}

/// Fetch the current still image for a media profile as JPEG
/// bytes. Resolves the snapshot URL first, then performs an
/// authenticated HTTP `GET`.
pub async fn fetch_snapshot(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
) -> Result<Vec<u8>, String> {
    let uri = get_snapshot_uri(endpoint, username, password, profile_token).await?;
    let client = build_client()?;
    let resp = client
        .get(&uri)
        .basic_auth(username, Some(password))
        .send()
        .await
        .map_err(|e| format!("snapshot GET failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("snapshot GET http error {status}"));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("snapshot read failed: {e}"))?;
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(format!(
            "snapshot too large: {} bytes (cap {MAX_SNAPSHOT_BYTES})",
            bytes.len()
        ));
    }
    Ok(bytes.to_vec())
}

fn get_snapshot_uri_body(profile_token: &str) -> String {
    format!(
        "<trt:GetSnapshotUri><trt:ProfileToken>{}</trt:ProfileToken></trt:GetSnapshotUri>",
        xml_escape(profile_token)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use quick_xml::events::Event;
    use quick_xml::Reader;

    fn wrap(inner: &str) -> String {
        format!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><s:Body>{inner}</s:Body></s:Envelope>"#
        )
    }

    fn assert_well_formed(xml: &str) {
        let mut r = Reader::from_str(xml);
        let mut buf = Vec::new();
        loop {
            match r.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Err(e) => panic!("invalid xml: {e}\n{xml}"),
                _ => {}
            }
        }
    }

    #[test]
    fn get_snapshot_uri_body_carries_profile_token() {
        let body = get_snapshot_uri_body("Profile_1");
        assert_well_formed(&wrap(&body));
        assert!(
            body.contains("<trt:ProfileToken>Profile_1</trt:ProfileToken>"),
            "{body}"
        );
    }

    #[test]
    fn parses_snapshot_uri_response() {
        let body = wrap(
            r#"<trt:GetSnapshotUriResponse><trt:MediaUri>
                <tt:Uri>http://192.168.1.64/onvif-http/snapshot?Profile_1</tt:Uri>
                <tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>
                <tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>
                <tt:Timeout>PT0S</tt:Timeout>
            </trt:MediaUri></trt:GetSnapshotUriResponse>"#,
        );
        assert_eq!(
            first_text(&body, "Uri").as_deref(),
            Some("http://192.168.1.64/onvif-http/snapshot?Profile_1")
        );
    }
}

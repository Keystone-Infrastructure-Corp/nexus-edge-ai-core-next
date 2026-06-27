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
//! admin-passthrough routes added in Phase 7.6.3
//! (`crate::device_control`).

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

/// PUT JPEG bytes to a cloud-minted single-blob **Write** SAS URL
/// (an Azure Block Blob). The image leaves the box straight to Blob
/// storage and never crosses the cloud gateway tunnel — Hard Rule 7
/// (SAS URLs for media). The cloud admin-proxy mints the SAS and the
/// matching short-TTL Read SAS it hands back to the browser.
///
/// Returns the number of bytes uploaded on success. The Azure block-
/// blob PUT requires the `x-ms-blob-type: BlockBlob` header; without
/// it the service returns `400`.
pub async fn put_snapshot_to_sas(sas_url: &str, jpeg: &[u8]) -> Result<usize, String> {
    let client = build_client()?;
    let resp = client
        .put(sas_url)
        .header("x-ms-blob-type", "BlockBlob")
        .header("content-type", "image/jpeg")
        .body(jpeg.to_vec())
        .send()
        .await
        .map_err(|e| format!("snapshot SAS PUT failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let preview: String = body.chars().take(256).collect();
        return Err(format!("snapshot SAS PUT http error {status}: {preview}"));
    }
    Ok(jpeg.len())
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

    #[tokio::test]
    async fn put_snapshot_to_sas_uploads_block_blob() {
        use std::sync::{Arc, Mutex};

        use axum::body::Bytes;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::put;
        use axum::Router;

        #[derive(Default)]
        struct Captured {
            blob_type: Option<String>,
            content_type: Option<String>,
            body: Vec<u8>,
        }
        let cap = Arc::new(Mutex::new(Captured::default()));
        let cap_handler = Arc::clone(&cap);

        let app = Router::new().route(
            "/snap",
            put(move |headers: HeaderMap, body: Bytes| {
                let cap = Arc::clone(&cap_handler);
                async move {
                    let mut c = cap.lock().expect("cap lock");
                    c.blob_type = headers
                        .get("x-ms-blob-type")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    c.content_type = headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    c.body = body.to_vec();
                    StatusCode::CREATED
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let url = format!("http://{addr}/snap");
        let n = put_snapshot_to_sas(&url, b"jpeg-bytes")
            .await
            .expect("upload ok");
        assert_eq!(n, 10);

        let c = cap.lock().expect("cap lock");
        assert_eq!(c.blob_type.as_deref(), Some("BlockBlob"));
        assert_eq!(c.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(c.body, b"jpeg-bytes");
    }

    #[tokio::test]
    async fn put_snapshot_to_sas_surfaces_http_error() {
        use axum::http::StatusCode;
        use axum::routing::put;
        use axum::Router;

        let app = Router::new().route(
            "/snap",
            put(|| async { (StatusCode::FORBIDDEN, "AuthenticationFailed") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let url = format!("http://{addr}/snap");
        let err = put_snapshot_to_sas(&url, b"x")
            .await
            .expect_err("must fail");
        assert!(err.contains("403"), "{err}");
    }
}

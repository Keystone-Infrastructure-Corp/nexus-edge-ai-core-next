//! ONVIF firmware-upgrade client (`ver10/device`, modern flow).
//!
//! Phase 7.6.8. Implements the **modern** `StartFirmwareUpgrade`
//! flow only ([`start_firmware_upgrade`]): the camera hands back an
//! `UploadUri` (plus best-effort `UploadDelay` / `ExpectedDownTime`
//! hints), the edge HTTP-POSTs the firmware blob to that URI
//! ([`upload_firmware`]), and then issues a `SystemReboot`
//! ([`super::onvif_device::system_reboot`]). The legacy MTOM
//! `UpgradeSystemFirmware` flow is intentionally **not** supported —
//! cameras that only offer it surface cleanly as "unsupported"
//! ([`is_unsupported_fault`]).
//!
//! The firmware blob itself never crosses the cloud gateway tunnel:
//! the edge pulls it straight from a cloud-minted single-blob **Read
//! SAS** over plain HTTPS ([`download_firmware`]) — Hard Rule 7 (SAS
//! URLs for media / large transfers). Two guards run **before** the
//! upgrade window is ever opened:
//!
//! 1. the blob's SHA-256 is verified against the caller-supplied
//!    digest ([`verify_checksum`]); and
//! 2. the camera's reported make / model is matched against the
//!    firmware's expected make / model ([`verify_make_model`]).
//!
//! There is no transactional ONVIF revert, so these pre-apply checks
//! plus the operator-facing owner-only + type-token confirmation are
//! the only safety net. The public operations are consumed by the
//! owner/admin firmware-upgrade route added in
//! `crate::device_control::firmware_upgrade`.

use std::time::Duration;

use sha2::{Digest, Sha256};

use super::onvif_device::DeviceInformation;
use super::onvif_soap::{collect_first_texts, DEVICE};

/// Cap on the firmware blob the edge will pull from the cloud SAS.
/// Camera firmware images are tens of MiB; this cap protects against
/// a misbehaving / hostile blob streaming an unbounded body.
const MAX_FIRMWARE_BYTES: usize = 256 * 1024 * 1024;

/// Transfer timeout for the firmware download (from Blob storage)
/// and the upload (to the camera). Deliberately far longer than the
/// 5 s SOAP `REQ_TIMEOUT` — a multi-tens-of-MiB transfer over a slow
/// LAN can legitimately take a couple of minutes.
const FIRMWARE_TIMEOUT: Duration = Duration::from_secs(180);

/// Parsed `StartFirmwareUpgrade` response.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct FirmwareUpgradeStart {
    /// The HTTP endpoint the firmware blob is POSTed to.
    pub upload_uri: String,
    /// `xs:duration` the client should wait before uploading
    /// (e.g. `PT5S`). Best-effort — absent on many cameras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_delay: Option<String>,
    /// `xs:duration` the camera expects to be offline for while it
    /// applies the firmware and reboots (e.g. `PT2M`). Drives the
    /// operator-facing countdown. Best-effort — absent on many
    /// cameras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_down_time: Option<String>,
}

// ---------------------------------------------------------------------------
// Pre-apply verification (pure)
// ---------------------------------------------------------------------------

/// Lower-case hex of the SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Verify the firmware blob's SHA-256 against `expected_hex`. The
/// digest is compared case-insensitively after trimming. An empty
/// expected digest is rejected — a checksum is mandatory, never
/// optional, for an irreversible firmware write.
pub fn verify_checksum(bytes: &[u8], expected_hex: &str) -> Result<(), String> {
    let want = expected_hex.trim().to_ascii_lowercase();
    if want.is_empty() {
        return Err("firmware checksum required (no sha256 supplied)".into());
    }
    let got = sha256_hex(bytes);
    if got == want {
        Ok(())
    } else {
        Err(format!(
            "firmware checksum mismatch: expected {want}, got {got}"
        ))
    }
}

/// Match the camera's reported identity against the firmware's
/// expected make / model. Both fields are compared case-insensitively
/// after trimming. A blank expected make or model is rejected — the
/// match is mandatory, never skipped, to keep an image for one model
/// off a different one.
pub fn verify_make_model(
    info: &DeviceInformation,
    expected_make: &str,
    expected_model: &str,
) -> Result<(), String> {
    let want_make = expected_make.trim();
    let want_model = expected_model.trim();
    if want_make.is_empty() || want_model.is_empty() {
        return Err("expected make and model required for firmware upgrade".into());
    }
    let got_make = info.manufacturer.trim();
    let got_model = info.model.trim();
    if got_make.eq_ignore_ascii_case(want_make) && got_model.eq_ignore_ascii_case(want_model) {
        Ok(())
    } else {
        Err(format!(
            "firmware make/model mismatch: image is for {want_make} / {want_model}, camera reports {got_make} / {got_model}"
        ))
    }
}

/// Heuristic: does a SOAP error from `StartFirmwareUpgrade` mean the
/// camera does not implement the modern firmware-upgrade flow (as
/// opposed to a transient / network failure)? Cameras signal this
/// with an `ActionNotSupported` / `NotSupported` SOAP fault or an
/// HTTP `501` / `405`. Used to surface a clean "unsupported" banner
/// instead of a generic gateway error.
pub fn is_unsupported_fault(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("actionnotsupported")
        || e.contains("not supported")
        || e.contains("notsupported")
        || e.contains("not implemented")
        || e.contains("notimplemented")
        || e.contains("optionnotsupported")
        || e.contains("http error 501")
        || e.contains("http error 405")
}

// ---------------------------------------------------------------------------
// SOAP control plane
// ---------------------------------------------------------------------------

/// Initiate the modern firmware upgrade and return the upload
/// coordinates. A SOAP fault (or an empty `UploadUri`) is surfaced as
/// an error; [`is_unsupported_fault`] then classifies whether it is a
/// "camera doesn't support this" signal.
pub async fn start_firmware_upgrade(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<FirmwareUpgradeStart, String> {
    let resp = DEVICE
        .call(
            endpoint,
            username,
            password,
            "StartFirmwareUpgrade",
            "<tds:StartFirmwareUpgrade/>",
        )
        .await?;
    let start = parse_start_firmware_upgrade(&resp);
    if start.upload_uri.trim().is_empty() {
        return Err(
            "StartFirmwareUpgrade response missing UploadUri (modern firmware flow not supported)"
                .into(),
        );
    }
    Ok(start)
}

fn parse_start_firmware_upgrade(body: &str) -> FirmwareUpgradeStart {
    let mut m = collect_first_texts(body, &["UploadUri", "UploadDelay", "ExpectedDownTime"]);
    FirmwareUpgradeStart {
        upload_uri: m.remove("UploadUri").unwrap_or_default(),
        upload_delay: m.remove("UploadDelay").filter(|s| !s.trim().is_empty()),
        expected_down_time: m
            .remove("ExpectedDownTime")
            .filter(|s| !s.trim().is_empty()),
    }
}

// ---------------------------------------------------------------------------
// Bulk transfer (download from SAS, upload to camera)
// ---------------------------------------------------------------------------

/// Build a long-timeout reqwest client for the bulk firmware
/// transfer. `accept_invalid_certs` is `true` for the upload to the
/// camera (which ships a self-signed cert on `:443`, like the SOAP
/// path) and `false` for the SAS download (Azure Blob storage serves
/// a valid public cert; the blob's integrity is independently pinned
/// by the SHA-256 verify regardless).
fn transfer_client(accept_invalid_certs: bool) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(FIRMWARE_TIMEOUT)
        .danger_accept_invalid_certs(accept_invalid_certs)
        .build()
        .map_err(|e| format!("firmware http client build failed: {e}"))
}

/// Download the firmware blob from a cloud-minted **Read SAS** URL.
/// Returns the raw bytes (capped at [`MAX_FIRMWARE_BYTES`]). The blob
/// is pulled straight from Blob storage, never through the gateway
/// tunnel (Hard Rule 7).
///
/// Errors are stripped of the request URL ([`reqwest::Error::without_url`])
/// so the SAS — which is a bearer credential — never lands in a log
/// line or an audit row.
pub async fn download_firmware(sas_get_url: &str) -> Result<Vec<u8>, String> {
    let client = transfer_client(false)?;
    let resp = client
        .get(sas_get_url)
        .send()
        .await
        .map_err(|e| format!("firmware download failed: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("firmware download http error {}", status.as_u16()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("firmware read failed: {}", e.without_url()))?;
    if bytes.len() > MAX_FIRMWARE_BYTES {
        return Err(format!(
            "firmware too large: {} bytes (cap {MAX_FIRMWARE_BYTES})",
            bytes.len()
        ));
    }
    Ok(bytes.to_vec())
}

/// HTTP-POST the firmware blob to the camera's `UploadUri` (raw
/// `application/octet-stream`, HTTP Basic auth — mirroring the
/// snapshot fetch, `reqwest` has no Digest support so a Digest-only
/// camera returns `401` with a clear error).
///
/// As with the download, the error is stripped of the request URL so
/// a credential-bearing upload URI never lands in an audit row.
pub async fn upload_firmware(
    upload_uri: &str,
    firmware: &[u8],
    username: &str,
    password: &str,
) -> Result<(), String> {
    let client = transfer_client(true)?;
    let resp = client
        .post(upload_uri)
        .basic_auth(username, Some(password))
        .header("content-type", "application/octet-stream")
        .body(firmware.to_vec())
        .send()
        .await
        .map_err(|e| format!("firmware upload failed: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("firmware upload http error {}", status.as_u16()));
    }
    Ok(())
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
    fn sha256_hex_matches_known_vector() {
        // SHA-256("") well-known value.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn verify_checksum_accepts_matching_digest() {
        let blob = b"firmware-payload";
        let digest = sha256_hex(blob);
        assert!(verify_checksum(blob, &digest).is_ok());
        // Case-insensitive + whitespace-tolerant.
        let padded = format!("  {}  ", digest.to_ascii_uppercase());
        assert!(verify_checksum(blob, &padded).is_ok());
    }

    #[test]
    fn verify_checksum_rejects_mismatch_and_empty() {
        let blob = b"firmware-payload";
        let err = verify_checksum(blob, "00").unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        let err = verify_checksum(blob, "   ").unwrap_err();
        assert!(err.contains("checksum required"), "{err}");
    }

    #[test]
    fn verify_make_model_accepts_case_insensitive_match() {
        let info = DeviceInformation {
            manufacturer: "Acme Optics".into(),
            model: "AX-200".into(),
            ..Default::default()
        };
        assert!(verify_make_model(&info, "acme optics", "ax-200").is_ok());
    }

    #[test]
    fn verify_make_model_rejects_mismatch_and_blank() {
        let info = DeviceInformation {
            manufacturer: "Acme Optics".into(),
            model: "AX-200".into(),
            ..Default::default()
        };
        let err = verify_make_model(&info, "Acme Optics", "ZZ-999").unwrap_err();
        assert!(err.contains("make/model mismatch"), "{err}");
        let err = verify_make_model(&info, "", "AX-200").unwrap_err();
        assert!(err.contains("required"), "{err}");
    }

    #[test]
    fn parses_start_firmware_upgrade_response() {
        let body = wrap(
            r#"<tds:StartFirmwareUpgradeResponse>
                <tds:UploadUri>http://192.168.1.64/onvif/upload</tds:UploadUri>
                <tds:UploadDelay>PT5S</tds:UploadDelay>
                <tds:ExpectedDownTime>PT2M</tds:ExpectedDownTime>
            </tds:StartFirmwareUpgradeResponse>"#,
        );
        let start = parse_start_firmware_upgrade(&body);
        assert_eq!(start.upload_uri, "http://192.168.1.64/onvif/upload");
        assert_eq!(start.upload_delay.as_deref(), Some("PT5S"));
        assert_eq!(start.expected_down_time.as_deref(), Some("PT2M"));
    }

    #[test]
    fn parses_start_firmware_upgrade_without_optional_hints() {
        let body = wrap(
            r#"<tds:StartFirmwareUpgradeResponse>
                <tds:UploadUri>http://cam/upload</tds:UploadUri>
            </tds:StartFirmwareUpgradeResponse>"#,
        );
        let start = parse_start_firmware_upgrade(&body);
        assert_eq!(start.upload_uri, "http://cam/upload");
        assert!(start.upload_delay.is_none());
        assert!(start.expected_down_time.is_none());
    }

    #[test]
    fn is_unsupported_fault_classifies_known_signals() {
        assert!(is_unsupported_fault(
            "soap fault: ter:ActionNotSupported the requested action is not supported"
        ));
        assert!(is_unsupported_fault("soap fault: Method Not Supported"));
        assert!(is_unsupported_fault("soap http error 501"));
        assert!(!is_unsupported_fault(
            "soap post failed: connection refused"
        ));
        assert!(!is_unsupported_fault("firmware download http error 403"));
    }

    #[tokio::test]
    async fn download_firmware_returns_blob_bytes() {
        use axum::routing::get;
        use axum::Router;

        let app = Router::new().route("/fw", get(|| async { "FIRMWARE-BYTES" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let url = format!("http://{addr}/fw");
        let bytes = download_firmware(&url).await.expect("download ok");
        assert_eq!(bytes, b"FIRMWARE-BYTES");
    }

    #[tokio::test]
    async fn download_firmware_does_not_leak_sas_credential_on_error() {
        use axum::http::StatusCode;
        use axum::routing::get;
        use axum::Router;

        // The blob endpoint denies access; the returned error must
        // NOT echo back the SAS signature (a bearer credential).
        let app = Router::new().route(
            "/fw",
            get(|| async { (StatusCode::FORBIDDEN, "AuthenticationFailed") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let url = format!("http://{addr}/fw?sv=2024-08-04&sig=SUPERSECRETSIGNATURE");
        let err = download_firmware(&url).await.expect_err("must fail");
        assert!(err.contains("403"), "{err}");
        assert!(
            !err.contains("SUPERSECRETSIGNATURE"),
            "SAS credential leaked into error: {err}"
        );
    }

    #[tokio::test]
    async fn download_firmware_strips_url_on_connection_error() {
        // Port 9 (discard) is closed → connection refused, the path
        // where reqwest's error would otherwise embed the request URL
        // (and thus the SAS signature). `without_url` must strip it.
        let url = "http://127.0.0.1:9/firmware.bin?sv=2024-08-04&sig=SUPERSECRETSIGNATURE";
        let err = download_firmware(url).await.expect_err("must fail");
        assert!(
            !err.contains("SUPERSECRETSIGNATURE"),
            "SAS credential leaked into connection error: {err}"
        );
    }

    #[tokio::test]
    async fn upload_firmware_posts_octet_stream() {
        use std::sync::{Arc, Mutex};

        use axum::body::Bytes;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::post;
        use axum::Router;

        #[derive(Default)]
        struct Captured {
            content_type: Option<String>,
            authorization: Option<String>,
            body: Vec<u8>,
        }
        let cap = Arc::new(Mutex::new(Captured::default()));
        let cap_handler = Arc::clone(&cap);

        let app = Router::new().route(
            "/upload",
            post(move |headers: HeaderMap, body: Bytes| {
                let cap = Arc::clone(&cap_handler);
                async move {
                    let mut c = cap.lock().expect("cap lock");
                    c.content_type = headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    c.authorization = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    c.body = body.to_vec();
                    StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let url = format!("http://{addr}/upload");
        upload_firmware(&url, b"fw-image", "admin", "secret")
            .await
            .expect("upload ok");

        let c = cap.lock().expect("cap lock");
        assert_eq!(c.content_type.as_deref(), Some("application/octet-stream"));
        assert!(
            c.authorization
                .as_deref()
                .unwrap_or_default()
                .starts_with("Basic "),
            "{:?}",
            c.authorization
        );
        assert_eq!(c.body, b"fw-image");
    }

    #[tokio::test]
    async fn upload_firmware_surfaces_http_error() {
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::Router;

        let app = Router::new().route(
            "/upload",
            post(|| async { (StatusCode::BAD_REQUEST, "rejected") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let url = format!("http://{addr}/upload");
        let err = upload_firmware(&url, b"x", "admin", "secret")
            .await
            .expect_err("must fail");
        assert!(err.contains("400"), "{err}");
    }
}

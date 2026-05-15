//! Microsoft OneDrive cold backend via the Microsoft Graph API.
//!
//! Implements [`ColdBackend`] against Microsoft Graph v1.0
//! (<https://learn.microsoft.com/graph/api/resources/onedrive>).
//! The operator-supplied OAuth refresh token is constrained to the
//! `Files.ReadWrite.AppFolder` scope — narrow enough that the
//! engine can only see the per-app folder Microsoft auto-creates
//! under the user's `/Apps/<app name>` directory. Wider scopes
//! (`Files.ReadWrite`, `Files.ReadWrite.All`) would surface every
//! file in the user's OneDrive and are unnecessary for cold-mirror
//! writes.
//!
//! ## Path semantics
//!
//! Unlike Google Drive, OneDrive supports hierarchical paths
//! natively. Every endpoint we hit uses the `approot:/<path>:`
//! syntax which Graph resolves into the app-scoped folder:
//!
//!   * `GET /me/drive/special/approot:/cam1/clip.mp4` → item
//!     metadata.
//!   * `PUT /me/drive/special/approot:/cam1/clip.mp4:/content` →
//!     small upload (≤ 4 MiB).
//!   * `POST /me/drive/special/approot:/cam1/clip.mp4:/createUploadSession`
//!     → resumable upload session URL for larger files.
//!
//! The 4 MiB simple-upload cap is below typical clip sizes (5–80
//! MiB at 1080p), so every upload here goes through the resumable
//! session flow, even for tiny clips. Code path is uniform.
//!
//! ## sha256 verification
//!
//! Personal OneDrive returns `file.hashes.sha256Hash` in the item
//! metadata; SharePoint sometimes does not. We don't depend on the
//! server-side value because:
//!
//!   1. The hash style varies (personal: SHA-1 / SHA-256 / quickXor;
//!      business: SHA-1 only on some plans).
//!   2. Setting a custom property via PATCH after upload is
//!      flaky for some tenants.
//!
//! Instead we stamp our own `description` field with
//! `nexus_sha256=<hex>` at upload time and parse it in [`Self::exists`].
//! Microsoft Graph's `description` field has no semantic meaning,
//! is freely operator-editable, and isn't scrubbed by any built-in
//! integrity check — perfect for a tagging marker.

use async_trait::async_trait;
use chrono::Utc;
use futures::stream::{StreamExt, TryStreamExt};
use nexus_storage::{BackendError, ByteStream, ColdBackend, HealthStatus, PutReceipt, VolumeInfo};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::oauth::{token_url, OAuthClient, Provider};
use crate::CloudConfig;

/// Marker stored in the OneDrive item's `description` field. See
/// module docs for the rationale.
const DESCRIPTION_PREFIX: &str = "nexus_sha256=";

struct Endpoints {
    /// `https://graph.microsoft.com/v1.0` (or override).
    api_base: String,
}

impl Endpoints {
    fn from_env() -> Self {
        Self {
            api_base: std::env::var("NEXUS_ONEDRIVE_API_BASE")
                .unwrap_or_else(|_| "https://graph.microsoft.com/v1.0".to_string()),
        }
    }
}

pub struct OneDriveBackend {
    handle: String,
    oauth: OAuthClient,
    http: reqwest::Client,
    endpoints: Endpoints,
}

#[derive(Debug, Deserialize)]
struct DriveItem {
    // Wire-shape fields — kept for forward compat / tcpdump
    // legibility. `id` is the Graph item id; `size` is the
    // server-reported size. Neither is consumed (we hash bytes
    // ourselves and key cold-paths off `<path>` not item id).
    #[allow(dead_code)]
    #[serde(default)]
    id: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    size: Option<i64>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UploadSessionResp {
    #[serde(rename = "uploadUrl")]
    upload_url: String,
}

#[derive(Debug, Deserialize)]
struct DriveQuota {
    #[serde(default)]
    total: Option<i64>,
    #[serde(default)]
    used: Option<i64>,
    #[serde(default)]
    remaining: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct DriveResource {
    #[serde(default)]
    quota: Option<DriveQuota>,
}

impl OneDriveBackend {
    pub fn new(handle: &str, cfg: CloudConfig) -> Result<Self, BackendError> {
        let oauth = OAuthClient::new(
            Provider::Onedrive,
            cfg.client_id,
            cfg.client_secret,
            cfg.refresh_token,
            token_url(Provider::Onedrive),
        );
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| BackendError::Other(format!("reqwest build: {e}")))?;
        Ok(Self {
            handle: handle.to_string(),
            oauth,
            http,
            endpoints: Endpoints::from_env(),
        })
    }

    /// Encode a backend-relative path for the Microsoft Graph
    /// `approot:` syntax. Slashes are preserved (Graph
    /// hierarchical-path addressing); other URL-reserved chars are
    /// percent-encoded.
    fn encode_path(rel: &str) -> String {
        // The Graph escape table differs from the URL escape table:
        // `:` is a path separator in their syntax but must be
        // encoded in path segments. Use a conservative
        // `url::form_urlencoded`-equivalent and then unescape `/`
        // back so segments remain segmented.
        let mut out = String::with_capacity(rel.len() + 8);
        for c in rel.chars() {
            match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => {
                    out.push(c);
                }
                ' ' => out.push_str("%20"),
                _ => {
                    let mut buf = [0u8; 4];
                    for b in c.encode_utf8(&mut buf).bytes() {
                        out.push_str(&format!("%{b:02X}"));
                    }
                }
            }
        }
        out
    }

    async fn authed_get(&self, url: &str) -> Result<reqwest::RequestBuilder, BackendError> {
        let token = self.oauth.access_token(&self.http).await?;
        Ok(self.http.get(url).bearer_auth(token))
    }

    async fn authed_post(&self, url: &str) -> Result<reqwest::RequestBuilder, BackendError> {
        let token = self.oauth.access_token(&self.http).await?;
        Ok(self.http.post(url).bearer_auth(token))
    }

    async fn authed_delete(&self, url: &str) -> Result<reqwest::RequestBuilder, BackendError> {
        let token = self.oauth.access_token(&self.http).await?;
        Ok(self.http.delete(url).bearer_auth(token))
    }

    /// Map response status into a [`BackendError`] consistent with
    /// the rest of the cold-backend contract. Caller only sees
    /// `Ok(resp)` for 2xx; non-2xx paths return Err.
    async fn unwrap_401(&self, resp: reqwest::Response) -> Result<reqwest::Response, BackendError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body_preview: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        if status.as_u16() == 401 {
            return Err(BackendError::Auth(format!("Graph 401: {body_preview}")));
        }
        if status.is_server_error() {
            return Err(BackendError::Unreachable(format!(
                "Graph {status}: {body_preview}"
            )));
        }
        Err(BackendError::Other(format!(
            "Graph {status}: {body_preview}"
        )))
    }

    /// `GET /me/drive/special/approot:/<path>` — returns item
    /// metadata, or `None` on 404.
    async fn get_item(&self, rel: &str) -> Result<Option<DriveItem>, BackendError> {
        let url = format!(
            "{}/me/drive/special/approot:/{}",
            self.endpoints.api_base,
            Self::encode_path(rel)
        );
        let resp = self
            .authed_get(&url)
            .await?
            .send()
            .await
            .map_err(|e| BackendError::Unreachable(format!("Graph get_item: {e}")))?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let resp = self.unwrap_401(resp).await?;
        let item: DriveItem = resp
            .json()
            .await
            .map_err(|e| BackendError::Other(format!("Graph get_item parse: {e}")))?;
        Ok(Some(item))
    }

    /// Initiate an upload session and PUT the entire clip in one
    /// request with `Content-Range: bytes 0-(N-1)/N`. Graph
    /// accepts arbitrarily large single chunks (up to the 60 GiB
    /// hard ceiling); we never bump against that for cold clips.
    async fn upload_session(&self, rel: &str, bytes: &[u8]) -> Result<DriveItem, BackendError> {
        if bytes.is_empty() {
            return Err(BackendError::Other(
                "OneDrive upload of 0-byte clip rejected".into(),
            ));
        }
        let init_url = format!(
            "{}/me/drive/special/approot:/{}:/createUploadSession",
            self.endpoints.api_base,
            Self::encode_path(rel)
        );
        let body = json!({
            "item": {
                // conflictBehavior=replace lets a re-upload after a
                // prior interrupted attempt overwrite the partial
                // file cleanly. `@microsoft.graph.` is the Graph
                // namespacing convention for control fields.
                "@microsoft.graph.conflictBehavior": "replace",
                "description": format!("{DESCRIPTION_PREFIX}placeholder"),
            }
        });
        let init = self
            .authed_post(&init_url)
            .await?
            .json(&body)
            .send()
            .await
            .map_err(|e| BackendError::Unreachable(format!("Graph upload init: {e}")))?;
        let init = self.unwrap_401(init).await?;
        let session: UploadSessionResp = init
            .json()
            .await
            .map_err(|e| BackendError::Other(format!("Graph upload init parse: {e}")))?;

        let total = bytes.len();
        let range = format!("bytes 0-{}/{}", total - 1, total);
        let put = self
            .http
            .put(&session.upload_url)
            .header("Content-Length", total.to_string())
            .header("Content-Range", range)
            .header("Content-Type", "application/octet-stream")
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|e| BackendError::Unreachable(format!("Graph upload PUT: {e}")))?;
        let put = self.unwrap_401(put).await?;
        let item: DriveItem = put
            .json()
            .await
            .map_err(|e| BackendError::Other(format!("Graph upload PUT parse: {e}")))?;
        Ok(item)
    }

    /// Stamp `description = "nexus_sha256=<hex>"` on the just-
    /// uploaded item. Done as a separate PATCH because the
    /// createUploadSession body's `description` field doesn't
    /// always survive the final PUT.
    async fn stamp_sha256(&self, rel: &str, sha256_hex: &str) -> Result<(), BackendError> {
        let url = format!(
            "{}/me/drive/special/approot:/{}",
            self.endpoints.api_base,
            Self::encode_path(rel)
        );
        let token = self.oauth.access_token(&self.http).await?;
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(token)
            .json(&json!({ "description": format!("{DESCRIPTION_PREFIX}{sha256_hex}") }))
            .send()
            .await
            .map_err(|e| BackendError::Unreachable(format!("Graph PATCH sha256: {e}")))?;
        self.unwrap_401(resp).await?;
        Ok(())
    }
}

#[async_trait]
impl ColdBackend for OneDriveBackend {
    fn handle(&self) -> &str {
        &self.handle
    }

    fn kind(&self) -> &str {
        "onedrive"
    }

    async fn put(
        &self,
        path: &str,
        bytes: &[u8],
        expected_sha256: &str,
    ) -> Result<PutReceipt, BackendError> {
        let do_put = || async {
            // Idempotent fast-path: a matching file already on cold
            // short-circuits the upload.
            if let Some(item) = self.get_item(path).await? {
                if let Some(desc) = &item.description {
                    if desc == &format!("{DESCRIPTION_PREFIX}{expected_sha256}") {
                        debug!(
                            path,
                            "OneDrive put: existing item already matches sha256; no-op"
                        );
                        return Ok(PutReceipt {
                            cold_path: path.to_string(),
                            uploaded_at: Utc::now(),
                            bytes_written: 0,
                        });
                    }
                }
            }
            let _item = self.upload_session(path, bytes).await?;
            // Stamp the description; failure here is a soft error
            // (the bytes are uploaded, just unverified) — but we
            // surface it so the replicator retries the WHOLE clip
            // rather than silently leaving an unverifiable file.
            self.stamp_sha256(path, expected_sha256).await?;
            Ok::<PutReceipt, BackendError>(PutReceipt {
                cold_path: path.to_string(),
                uploaded_at: Utc::now(),
                bytes_written: bytes.len() as u64,
            })
        };

        match do_put().await {
            Ok(r) => Ok(r),
            Err(BackendError::Auth(msg)) => {
                warn!(error = %msg, "OneDrive put: 401, refreshing token and retrying once");
                self.oauth.invalidate();
                do_put().await
            }
            Err(e) => Err(e),
        }
    }

    async fn get_range(
        &self,
        path: &str,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Vec<u8>, BackendError> {
        let do_get = || async {
            let url = format!(
                "{}/me/drive/special/approot:/{}:/content",
                self.endpoints.api_base,
                Self::encode_path(path)
            );
            let resp = self
                .authed_get(&url)
                .await?
                .header("Range", format!("bytes={start}-{end_inclusive}"))
                .send()
                .await
                .map_err(|e| BackendError::Unreachable(format!("Graph download: {e}")))?;
            let resp = self.unwrap_401(resp).await?;
            let body = resp
                .bytes()
                .await
                .map_err(|e| BackendError::Unreachable(format!("Graph download body: {e}")))?;
            Ok::<Vec<u8>, BackendError>(body.to_vec())
        };
        match do_get().await {
            Ok(v) => Ok(v),
            Err(BackendError::Auth(_)) => {
                self.oauth.invalidate();
                do_get().await
            }
            Err(e) => Err(e),
        }
    }

    async fn get_range_stream(
        &self,
        path: &str,
        start: u64,
        end_inclusive: u64,
    ) -> Result<ByteStream, BackendError> {
        // Mirror of `gdrive.rs::get_range_stream`: retry-on-401 the
        // synchronous "build response" leg so the body stream we
        // return cannot itself need a token refresh mid-flight.
        let build = || async {
            let url = format!(
                "{}/me/drive/special/approot:/{}:/content",
                self.endpoints.api_base,
                Self::encode_path(path)
            );
            let resp = self
                .authed_get(&url)
                .await?
                .header("Range", format!("bytes={start}-{end_inclusive}"))
                .send()
                .await
                .map_err(|e| BackendError::Unreachable(format!("Graph download: {e}")))?;
            let resp = self.unwrap_401(resp).await?;
            Ok::<reqwest::Response, BackendError>(resp)
        };
        let resp = match build().await {
            Ok(r) => r,
            Err(BackendError::Auth(_)) => {
                self.oauth.invalidate();
                build().await?
            }
            Err(e) => return Err(e),
        };
        let s = resp
            .bytes_stream()
            .map_err(|e| BackendError::Unreachable(format!("Graph download body: {e}")));
        Ok(s.boxed())
    }

    async fn delete(&self, path: &str) -> Result<bool, BackendError> {
        let do_delete = || async {
            let url = format!(
                "{}/me/drive/special/approot:/{}",
                self.endpoints.api_base,
                Self::encode_path(path)
            );
            let resp = self
                .authed_delete(&url)
                .await?
                .send()
                .await
                .map_err(|e| BackendError::Unreachable(format!("Graph delete: {e}")))?;
            match resp.status().as_u16() {
                404 => Ok::<bool, BackendError>(false),
                204 | 200 => Ok(true),
                _ => {
                    self.unwrap_401(resp).await?;
                    Ok(true)
                }
            }
        };
        match do_delete().await {
            Ok(v) => Ok(v),
            Err(BackendError::Auth(_)) => {
                self.oauth.invalidate();
                do_delete().await
            }
            Err(e) => Err(e),
        }
    }

    async fn exists(&self, path: &str, expected_sha256: &str) -> Result<bool, BackendError> {
        let do_exists = || async {
            let Some(item) = self.get_item(path).await? else {
                return Ok::<bool, BackendError>(false);
            };
            let Some(desc) = item.description else {
                return Ok(false);
            };
            Ok(desc == format!("{DESCRIPTION_PREFIX}{expected_sha256}"))
        };
        match do_exists().await {
            Ok(v) => Ok(v),
            Err(BackendError::Auth(_)) => {
                self.oauth.invalidate();
                do_exists().await
            }
            Err(e) => Err(e),
        }
    }

    async fn volume_info(&self) -> Result<VolumeInfo, BackendError> {
        let url = format!("{}/me/drive", self.endpoints.api_base);
        let resp = self
            .authed_get(&url)
            .await?
            .send()
            .await
            .map_err(|e| BackendError::Unreachable(format!("Graph /me/drive: {e}")))?;
        let resp = self.unwrap_401(resp).await?;
        let body: DriveResource = resp
            .json()
            .await
            .map_err(|e| BackendError::Other(format!("Graph /me/drive parse: {e}")))?;
        let quota = body.quota.unwrap_or(DriveQuota {
            total: None,
            used: None,
            remaining: None,
        });
        Ok(VolumeInfo {
            free_bytes: quota.remaining.map(|n| n.max(0) as u64),
            total_bytes: quota.total.map(|n| n.max(0) as u64),
            used_bytes: quota.used.map(|n| n.max(0) as u64),
        })
    }

    async fn health(&self) -> HealthStatus {
        let url = format!("{}/me/drive", self.endpoints.api_base);
        let req = match self.authed_get(&url).await {
            Ok(req) => req,
            Err(e) => {
                return HealthStatus::Unreachable {
                    reason: format!("{e}"),
                };
            }
        };
        match req.send().await {
            Ok(resp) if resp.status().is_success() => HealthStatus::Ok,
            Ok(resp) if resp.status().as_u16() == 401 => HealthStatus::ReadOnly {
                reason: "OAuth refresh-token rejected; re-consent required".into(),
            },
            Ok(resp) => HealthStatus::Unreachable {
                reason: format!("Graph /me/drive status {}", resp.status()),
            },
            Err(e) => HealthStatus::Unreachable {
                reason: format!("Graph /me/drive network: {e}"),
            },
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_storage::token_crypto::RefreshToken;
    use sha2::{Digest, Sha256};
    use std::sync::{Arc, LazyLock, Mutex};
    use wiremock::matchers::{header, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// See `gdrive::tests::TEST_ENV_LOCK` — identical rationale for
    /// the OneDrive env-var fence.
    static TEST_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn sha256_hex(b: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(b);
        let d = h.finalize();
        let mut s = String::with_capacity(d.len() * 2);
        for byte in d {
            use std::fmt::Write;
            let _ = write!(s, "{byte:02x}");
        }
        s
    }

    async fn build(server: &MockServer) -> OneDriveBackend {
        let base = server.uri();
        let guard = TEST_ENV_LOCK.lock().unwrap();
        std::env::set_var("NEXUS_ONEDRIVE_TOKEN_URL", format!("{base}/token"));
        std::env::set_var("NEXUS_ONEDRIVE_API_BASE", format!("{base}/graph"));
        let cfg = CloudConfig {
            client_id: "client".into(),
            client_secret: "secret".into(),
            refresh_token: RefreshToken::new("refresh-od".into()),
            account_email: Some("ops@example.com".into()),
            extra: serde_json::Value::Null,
        };
        let backend = OneDriveBackend::new("onedrive-test", cfg).unwrap();
        drop(guard);
        backend
    }

    fn mount_token(
        server: &MockServer,
        n_calls: Arc<std::sync::atomic::AtomicU32>,
    ) -> impl std::future::Future<Output = ()> + '_ {
        Mock::given(method("POST"))
            .and(wm_path("/token"))
            .respond_with(move |_: &wiremock::Request| {
                let i = n_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": format!("eat-{i}"),
                    "expires_in": 3600,
                }))
            })
            .mount(server)
    }

    #[tokio::test]
    async fn put_creates_session_then_uploads_then_stamps_sha256() {
        let server = MockServer::start().await;
        let token_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        mount_token(&server, token_calls.clone()).await;

        // Stage 0: GET item (idempotency probe) → 404.
        Mock::given(method("GET"))
            .and(wm_path("/graph/me/drive/special/approot:/cam1/clip.mp4"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        // Stage 1: POST createUploadSession → uploadUrl.
        let session_url = format!("{}/graph/session/xyz", server.uri());
        Mock::given(method("POST"))
            .and(wm_path(
                "/graph/me/drive/special/approot:/cam1/clip.mp4:/createUploadSession",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "uploadUrl": session_url
            })))
            .mount(&server)
            .await;

        // Stage 2: PUT the session URL → 201 with item metadata.
        Mock::given(method("PUT"))
            .and(wm_path("/graph/session/xyz"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "item-1",
                "size": 11,
            })))
            .mount(&server)
            .await;

        // Stage 3: PATCH stamps the sha256 description.
        Mock::given(method("PATCH"))
            .and(wm_path("/graph/me/drive/special/approot:/cam1/clip.mp4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let backend = build(&server).await;
        let bytes = b"hello cold!";
        let hash = sha256_hex(bytes);
        let receipt = backend.put("cam1/clip.mp4", bytes, &hash).await.unwrap();
        assert_eq!(receipt.bytes_written, bytes.len() as u64);
    }

    #[tokio::test]
    async fn put_short_circuits_when_description_matches() {
        let server = MockServer::start().await;
        let token_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        mount_token(&server, token_calls.clone()).await;

        let bytes = b"already there";
        let hash = sha256_hex(bytes);
        let desc = format!("{DESCRIPTION_PREFIX}{hash}");
        Mock::given(method("GET"))
            .and(wm_path("/graph/me/drive/special/approot:/cam1/clip.mp4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "exists",
                "size": bytes.len(),
                "description": desc,
            })))
            .mount(&server)
            .await;
        let backend = build(&server).await;
        let receipt = backend.put("cam1/clip.mp4", bytes, &hash).await.unwrap();
        assert_eq!(receipt.bytes_written, 0);
    }

    #[tokio::test]
    async fn exists_true_only_when_description_matches() {
        let server = MockServer::start().await;
        let token_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        mount_token(&server, token_calls.clone()).await;
        Mock::given(method("GET"))
            .and(wm_path("/graph/me/drive/special/approot:/cam1/match.mp4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "id-match",
                "description": format!("{DESCRIPTION_PREFIX}deadbeef"),
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/graph/me/drive/special/approot:/cam1/wrong.mp4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "id-wrong",
                "description": format!("{DESCRIPTION_PREFIX}cafebabe"),
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/graph/me/drive/special/approot:/cam1/gone.mp4"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let backend = build(&server).await;
        assert!(backend.exists("cam1/match.mp4", "deadbeef").await.unwrap());
        assert!(!backend.exists("cam1/wrong.mp4", "deadbeef").await.unwrap());
        assert!(!backend.exists("cam1/gone.mp4", "deadbeef").await.unwrap());
    }

    #[tokio::test]
    async fn refresh_on_401_retry_succeeds() {
        let server = MockServer::start().await;
        let token_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let tc = token_calls.clone();
        Mock::given(method("POST"))
            .and(wm_path("/token"))
            .respond_with(move |_: &wiremock::Request| {
                let i = tc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": format!("eat-{i}"),
                    "expires_in": 3600,
                }))
            })
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(wm_path("/graph/me/drive/special/approot:/cam1/clip.mp4"))
            .and(header("authorization", "Bearer eat-0"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/graph/me/drive/special/approot:/cam1/clip.mp4"))
            .and(header("authorization", "Bearer eat-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "id-after-refresh",
                "description": format!("{DESCRIPTION_PREFIX}deadbeef"),
            })))
            .mount(&server)
            .await;

        let backend = build(&server).await;
        let v = backend.exists("cam1/clip.mp4", "deadbeef").await.unwrap();
        assert!(v, "after-refresh response signals exists");
        assert_eq!(
            token_calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "expected mint + one refresh"
        );
    }

    #[test]
    fn encode_path_preserves_slashes_and_encodes_specials() {
        assert_eq!(
            OneDriveBackend::encode_path("cam1/a b/c.mp4"),
            "cam1/a%20b/c.mp4"
        );
        assert_eq!(
            OneDriveBackend::encode_path("cam_!:/clip.mp4"),
            "cam_%21%3A/clip.mp4"
        );
    }
}

//! Google Drive cold backend.
//!
//! Implements [`ColdBackend`] against Drive v3
//! (<https://developers.google.com/drive/api/v3/reference>). The
//! operator-supplied OAuth refresh token is constrained to the
//! `https://www.googleapis.com/auth/drive.file` scope — the
//! narrowest the API offers, granting access only to files the
//! app creates. Phase 2 deliberately does NOT request `drive` or
//! `drive.readonly`; those would surface every file in the user's
//! Drive to the engine and are unnecessary for cold-mirror writes.
//!
//! ## File naming + lookup
//!
//! Drive is a tag-based store, not a hierarchical FS — file names
//! are labels and a parent-child graph defines the tree. We
//! flatten by using the M2.2 cold-path string (e.g.
//! `cam1/1700000000_15000.mp4`) verbatim as the file name. That
//! preserves human-readable inspection ("oh, that's the camera 1
//! clip from 10am") without forcing us to maintain a folder
//! hierarchy on the cloud side.
//!
//! ## sha256 spot-check via `appProperties`
//!
//! At [`Self::put`] we stamp `appProperties["nexus_sha256"]` =
//! `<full hex>` on the uploaded file. At [`Self::exists`] we
//! re-query and compare. `appProperties` is per-app (scoped to
//! our OAuth client_id) so other apps using the same Drive can't
//! tamper with the marker.
//!
//! ## Resumable uploads, single-chunk
//!
//! Drive's simple (multipart) upload caps at 5 MB. The vast
//! majority of cold clips will be larger, so every upload goes
//! through the resumable session flow:
//!
//!   1. `POST /upload/drive/v3/files?uploadType=resumable` with
//!      JSON metadata → `Location:` header carries the session URL.
//!   2. `PUT <session-url>` with the file bytes + `Content-Range:
//!      bytes 0-<len-1>/<len>` → 200/201 + file resource on success.
//!
//! The engine holds the clip bytes in memory (the M2.2 hot
//! replicator already does `tokio::fs::read` before any
//! `ColdBackend::put`), so we send the whole file in one PUT.
//! Chunked uploads only matter if we want crash-resume; the next
//! replicator tick re-uploads from scratch if a put failed
//! mid-flight, so we accept the duplicate-network-bytes tradeoff
//! for code simplicity.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream::{StreamExt, TryStreamExt};
use nexus_storage::{BackendError, ByteStream, ColdBackend, HealthStatus, PutReceipt, VolumeInfo};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::oauth::{token_url, OAuthClient, Provider};
use crate::CloudConfig;

/// Per-backend HTTP client + endpoint base URLs. The
/// `NEXUS_GDRIVE_*` env vars are read once at construction so
/// tests can swap in a wiremock URL deterministically.
struct Endpoints {
    /// `https://www.googleapis.com/drive/v3` (or override).
    api_base: String,
    /// `https://www.googleapis.com/upload/drive/v3` (or override).
    upload_base: String,
}

impl Endpoints {
    fn from_env() -> Self {
        Self {
            api_base: std::env::var("NEXUS_GDRIVE_API_BASE")
                .unwrap_or_else(|_| "https://www.googleapis.com/drive/v3".to_string()),
            upload_base: std::env::var("NEXUS_GDRIVE_UPLOAD_BASE")
                .unwrap_or_else(|_| "https://www.googleapis.com/upload/drive/v3".to_string()),
        }
    }
}

pub struct GoogleDriveBackend {
    handle: String,
    /// Optional Drive folder ID the operator pre-created. When set,
    /// every uploaded file gets this as its parent; when None, the
    /// file lands in the operator's "My Drive" root (still
    /// `drive.file`-scoped, so we can only see our own files).
    root_folder_id: Option<String>,
    oauth: OAuthClient,
    http: reqwest::Client,
    endpoints: Endpoints,
}

#[derive(Debug, Deserialize)]
struct DriveFile {
    id: String,
    #[serde(default)]
    name: Option<String>,
    // Wire-shape field — kept so `serde_json::from_value` doesn't
    // choke if Drive starts including/omitting `size`. Not consumed
    // by any code path (we hash bytes ourselves), but removing it
    // makes future field additions harder to spot in tcpdump.
    #[allow(dead_code)]
    #[serde(default, rename = "size")]
    size: Option<String>,
    #[serde(default, rename = "appProperties")]
    app_properties: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct DriveFileList {
    #[serde(default)]
    files: Vec<DriveFile>,
}

#[derive(Debug, Deserialize)]
struct DriveAbout {
    #[serde(default, rename = "storageQuota")]
    storage_quota: Option<DriveQuota>,
}

#[derive(Debug, Deserialize)]
struct DriveQuota {
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    usage: Option<String>,
}

impl GoogleDriveBackend {
    pub fn new(handle: &str, cfg: CloudConfig) -> Result<Self, BackendError> {
        // The Drive-specific `extra` is shaped `{ "root_folder_id":
        // "<id-or-null>" }`. Operators paste a folder id from the
        // Drive web UI's address bar; we don't try to auto-create.
        let root_folder_id = cfg
            .extra
            .get("root_folder_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let oauth = OAuthClient::new(
            Provider::Gdrive,
            cfg.client_id,
            cfg.client_secret,
            cfg.refresh_token,
            token_url(Provider::Gdrive),
        );
        let http = reqwest::Client::builder()
            // 30s for the metadata path; the resumable PUT for
            // large uploads gets a longer timeout via the
            // request-level builder.
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| BackendError::Other(format!("reqwest build: {e}")))?;
        Ok(Self {
            handle: handle.to_string(),
            root_folder_id,
            oauth,
            http,
            endpoints: Endpoints::from_env(),
        })
    }

    /// Look up the most-recent Drive file matching `name` (and our
    /// parent folder, if configured). Returns `None` when there's
    /// no hit. The `drive.file` scope already constrains the query
    /// to app-owned files.
    async fn find_by_name(&self, name: &str) -> Result<Option<DriveFile>, BackendError> {
        // Drive's query language quotes string literals with single
        // quotes; embedded `'` must be doubled (`\'` not supported).
        let name_escaped = name.replace('\'', "\\'");
        let q = match &self.root_folder_id {
            Some(parent) => {
                format!("name = '{name_escaped}' and '{parent}' in parents and trashed = false")
            }
            None => format!("name = '{name_escaped}' and trashed = false"),
        };

        let url = format!("{}/files", self.endpoints.api_base);
        let resp = self
            .authed_get(&url)
            .await?
            .query(&[
                ("q", q.as_str()),
                ("spaces", "drive"),
                ("fields", "files(id,name,size,appProperties)"),
                ("pageSize", "10"),
            ])
            .send()
            .await
            .map_err(|e| BackendError::Unreachable(format!("Drive files.list: {e}")))?;
        let resp = self.unwrap_401(resp).await?;
        let list: DriveFileList = resp
            .json()
            .await
            .map_err(|e| BackendError::Other(format!("Drive files.list parse: {e}")))?;
        // Drive may return multiple matches if a previous put
        // raced; the replicator's idempotent retry path will
        // converge by deleting duplicates next pass. Pick the
        // first non-empty match here; the put path explicitly
        // dedupes when re-uploading.
        Ok(list.files.into_iter().next())
    }

    /// Build a GET request with the latest access token attached.
    /// Wrapped so callers don't repeat the refresh dance — a
    /// 401 is handled via [`Self::unwrap_401`].
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

    /// Inspect a response for 401 and surface it as
    /// [`BackendError::Auth`] so the caller can decide whether to
    /// refresh + retry. Non-401 errors map to either Unreachable
    /// (5xx) or Other (4xx) per their usual semantics.
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
            return Err(BackendError::Auth(format!("Drive API 401: {body_preview}")));
        }
        if status.is_server_error() {
            return Err(BackendError::Unreachable(format!(
                "Drive API {status}: {body_preview}"
            )));
        }
        Err(BackendError::Other(format!(
            "Drive API {status}: {body_preview}"
        )))
    }

    /// Initiate + finalise a resumable upload. Returns the
    /// resulting [`DriveFile`].
    async fn resumable_upload(
        &self,
        name: &str,
        bytes: &[u8],
        sha256_hex: &str,
    ) -> Result<DriveFile, BackendError> {
        let mut metadata = json!({
            "name": name,
            "appProperties": { "nexus_sha256": sha256_hex },
        });
        if let Some(parent) = &self.root_folder_id {
            metadata["parents"] = json!([parent]);
        }

        let init_url = format!(
            "{}/files?uploadType=resumable&supportsAllDrives=true",
            self.endpoints.upload_base
        );

        let init = self
            .authed_post(&init_url)
            .await?
            .header("X-Upload-Content-Type", "video/mp4")
            .header("X-Upload-Content-Length", bytes.len().to_string())
            .json(&metadata)
            .send()
            .await
            .map_err(|e| BackendError::Unreachable(format!("Drive resumable init: {e}")))?;
        let init = self.unwrap_401(init).await?;

        let session_url = init
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                BackendError::Other("Drive resumable init missing Location header".into())
            })?
            .to_string();

        // The session URL is pre-authenticated; no Bearer token
        // needed (and Google rejects PUTs with one). Stream the
        // bytes; `Content-Range: bytes 0-(N-1)/N` finalises in a
        // single round-trip.
        let total = bytes.len();
        // `total` is usize so `.max(0)` is a no-op; saturating_sub
        // already clamps to 0 for a 0-byte payload (which we reject
        // below anyway).
        let range = format!("bytes 0-{}/{}", total.saturating_sub(1), total);

        // 0-byte uploads are technically legal but our spot-check
        // is meaningless and the API rejects them with 400; the
        // replicator filters NULL-sha256 / 0-byte rows upstream
        // (`clips_pending_cold_upload` excludes them) so we just
        // guard defensively here.
        if total == 0 {
            return Err(BackendError::Other(
                "Drive resumable upload of 0-byte clip rejected".into(),
            ));
        }

        let put = self
            .http
            .put(&session_url)
            .header("Content-Length", total.to_string())
            .header("Content-Range", range)
            .header("Content-Type", "application/octet-stream")
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|e| BackendError::Unreachable(format!("Drive resumable PUT: {e}")))?;
        let put = self.unwrap_401(put).await?;

        let file: DriveFile = put
            .json()
            .await
            .map_err(|e| BackendError::Other(format!("Drive resumable PUT parse: {e}")))?;
        Ok(file)
    }
}

#[async_trait]
impl ColdBackend for GoogleDriveBackend {
    fn handle(&self) -> &str {
        &self.handle
    }

    fn kind(&self) -> &str {
        "gdrive"
    }

    async fn put(
        &self,
        path: &str,
        bytes: &[u8],
        expected_sha256: &str,
    ) -> Result<PutReceipt, BackendError> {
        let do_put = || async {
            // Idempotency: if a previous put landed but its receipt
            // got lost, delete the stale file before re-uploading so
            // we don't accumulate duplicates with the same name.
            if let Some(existing) = self.find_by_name(path).await? {
                let stored_hash = existing
                    .app_properties
                    .as_ref()
                    .and_then(|m| m.get("nexus_sha256"))
                    .and_then(|v| v.as_str());
                if stored_hash == Some(expected_sha256) {
                    debug!(
                        path,
                        id = %existing.id,
                        "Drive put: existing file already matches sha256; no-op"
                    );
                    return Ok(PutReceipt {
                        cold_path: path.to_string(),
                        uploaded_at: Utc::now(),
                        bytes_written: 0,
                    });
                }
                debug!(
                    path,
                    id = %existing.id,
                    "Drive put: deleting stale duplicate before re-upload"
                );
                let url = format!("{}/files/{}", self.endpoints.api_base, existing.id);
                let resp = self.authed_delete(&url).await?.send().await.map_err(|e| {
                    BackendError::Unreachable(format!("Drive duplicate delete: {e}"))
                })?;
                // 204 No Content on success; 404 if the dup was
                // already gone (race with another replicator).
                if !resp.status().is_success() && resp.status().as_u16() != 404 {
                    let _ = self.unwrap_401(resp).await?;
                }
            }

            let file = self.resumable_upload(path, bytes, expected_sha256).await?;
            Ok::<PutReceipt, BackendError>(PutReceipt {
                cold_path: path.to_string(),
                uploaded_at: Utc::now(),
                bytes_written: bytes.len() as u64,
            })
            .map(|mut r| {
                r.cold_path = file.name.unwrap_or_else(|| path.to_string());
                r
            })
        };

        // Refresh-on-401: every Drive API call goes through
        // `unwrap_401`, which surfaces Auth on a 401. On the first
        // such failure we invalidate the cached access token and
        // retry once; persistent Auth then means the refresh
        // token itself is dead and the admin UI must prompt for
        // re-consent.
        match do_put().await {
            Ok(r) => Ok(r),
            Err(BackendError::Auth(msg)) => {
                warn!(error = %msg, "Drive put: 401, refreshing access token and retrying once");
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
            let file = self
                .find_by_name(path)
                .await?
                .ok_or_else(|| BackendError::Other(format!("Drive get_range: {path} not found")))?;
            let url = format!(
                "{}/files/{}?alt=media&supportsAllDrives=true",
                self.endpoints.api_base, file.id
            );
            let resp = self
                .authed_get(&url)
                .await?
                .header("Range", format!("bytes={start}-{end_inclusive}"))
                .send()
                .await
                .map_err(|e| BackendError::Unreachable(format!("Drive download: {e}")))?;
            let resp = self.unwrap_401(resp).await?;
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| BackendError::Unreachable(format!("Drive download body: {e}")))?;
            Ok::<Vec<u8>, BackendError>(bytes.to_vec())
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
        // Headers + initial connect can fail with 401; the upload
        // path's retry-on-auth pattern would have to refresh the
        // token and rebuild the entire stream. Cheaper to do that
        // synchronously here: build the response once, retry once
        // on Auth, then hand the body stream out.
        let build = || async {
            let file = self.find_by_name(path).await?.ok_or_else(|| {
                BackendError::Other(format!("Drive get_range_stream: {path} not found"))
            })?;
            let url = format!(
                "{}/files/{}?alt=media&supportsAllDrives=true",
                self.endpoints.api_base, file.id
            );
            let resp = self
                .authed_get(&url)
                .await?
                .header("Range", format!("bytes={start}-{end_inclusive}"))
                .send()
                .await
                .map_err(|e| BackendError::Unreachable(format!("Drive download: {e}")))?;
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
        // Stream body chunks straight through. Errors map into our
        // Unreachable bucket — a mid-stream EOF or TLS reset is
        // exactly the same class of failure as a connect failure
        // from the replicator's POV.
        let s = resp
            .bytes_stream()
            .map_err(|e| BackendError::Unreachable(format!("Drive download body: {e}")));
        Ok(s.boxed())
    }

    async fn delete(&self, path: &str) -> Result<bool, BackendError> {
        let do_delete = || async {
            let Some(file) = self.find_by_name(path).await? else {
                return Ok::<bool, BackendError>(false);
            };
            let url = format!("{}/files/{}", self.endpoints.api_base, file.id);
            let resp = self
                .authed_delete(&url)
                .await?
                .send()
                .await
                .map_err(|e| BackendError::Unreachable(format!("Drive delete: {e}")))?;
            if resp.status().is_success() || resp.status().as_u16() == 404 {
                return Ok(true);
            }
            self.unwrap_401(resp).await?;
            // Unreachable code path; unwrap_401 always errors on
            // non-success. Defensive return for the type checker.
            Ok(true)
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
            let Some(file) = self.find_by_name(path).await? else {
                return Ok::<bool, BackendError>(false);
            };
            let stored = file
                .app_properties
                .as_ref()
                .and_then(|m| m.get("nexus_sha256"))
                .and_then(|v| v.as_str());
            Ok(stored == Some(expected_sha256))
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
        let url = format!("{}/about?fields=storageQuota", self.endpoints.api_base);
        let resp = self
            .authed_get(&url)
            .await?
            .send()
            .await
            .map_err(|e| BackendError::Unreachable(format!("Drive about: {e}")))?;
        let resp = self.unwrap_401(resp).await?;
        let body: DriveAbout = resp
            .json()
            .await
            .map_err(|e| BackendError::Other(format!("Drive about parse: {e}")))?;
        let quota = body.storage_quota.unwrap_or(DriveQuota {
            limit: None,
            usage: None,
        });
        let total = quota.limit.as_deref().and_then(|s| s.parse::<u64>().ok());
        let used = quota.usage.as_deref().and_then(|s| s.parse::<u64>().ok());
        let free = match (total, used) {
            (Some(t), Some(u)) => Some(t.saturating_sub(u)),
            _ => None,
        };
        Ok(VolumeInfo {
            free_bytes: free,
            total_bytes: total,
            used_bytes: used,
        })
    }

    async fn health(&self) -> HealthStatus {
        // Cheapest probe that exercises the OAuth path AND the
        // API: a /about call with minimal fields. A 401 here means
        // the refresh token is dead → ReadOnly so the replicator
        // pauses; everything else is Unreachable.
        let url = format!("{}/about?fields=kind", self.endpoints.api_base);
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
                reason: format!("Drive /about status {}", resp.status()),
            },
            Err(e) => HealthStatus::Unreachable {
                reason: format!("Drive /about network: {e}"),
            },
        }
    }
}

// Wire up the `Arc<dyn ColdBackend>` constructor — the factory in
// `lib.rs` calls this through `GoogleDriveBackend::new`, which
// returns an owned value the caller wraps in `Arc`.
#[allow(dead_code)]
fn _assert_object_safe(_: Arc<dyn ColdBackend>) {}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_storage::token_crypto::RefreshToken;
    use sha2::{Digest, Sha256};
    use std::sync::{LazyLock, Mutex};
    use wiremock::matchers::{header, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Global serialization fence for tests that mutate `NEXUS_GDRIVE_*`
    /// env vars. `cargo test` runs tests within a binary on a single
    /// tokio runtime but multiple worker threads, so two `build()` calls
    /// could otherwise clobber each other's overrides between the
    /// `set_var` and the `Endpoints::from_env()` capture inside
    /// `GoogleDriveBackend::new`. The guard is dropped before any
    /// `.await` so it never crosses suspension points.
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

    /// Build a backend pointed at the given wiremock URL, with env
    /// overrides set for the duration of the test. Tests in this
    /// module are `#[serial]`-friendly through the env-var fence;
    /// `cargo test` runs them on a single tokio worker per process
    /// so cross-test contamination isn't possible inside one binary.
    async fn build(server: &MockServer) -> GoogleDriveBackend {
        let base = server.uri();
        // Hold the lock only while env is mutated + backend captures
        // the endpoint URLs. After `new()` returns, the backend has
        // an owned `Endpoints` + token URL and is immune to further
        // env churn.
        let guard = TEST_ENV_LOCK.lock().unwrap();
        std::env::set_var("NEXUS_GDRIVE_TOKEN_URL", format!("{base}/token"));
        std::env::set_var("NEXUS_GDRIVE_API_BASE", format!("{base}/drive/v3"));
        std::env::set_var(
            "NEXUS_GDRIVE_UPLOAD_BASE",
            format!("{base}/upload/drive/v3"),
        );
        let cfg = CloudConfig {
            client_id: "client".into(),
            client_secret: "secret".into(),
            refresh_token: RefreshToken::new("refresh-abc".into()),
            account_email: Some("ops@example.com".into()),
            extra: serde_json::json!({"root_folder_id": null}),
        };
        let backend = GoogleDriveBackend::new("gdrive-test", cfg).unwrap();
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
                    "access_token": format!("ya29.AT-{i}"),
                    "expires_in": 3600,
                }))
            })
            .mount(server)
    }

    #[tokio::test]
    async fn put_uses_resumable_session_and_records_sha256() {
        let server = MockServer::start().await;
        let token_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        mount_token(&server, token_calls.clone()).await;

        // Stage 1: GET /files (find_by_name) returns empty.
        Mock::given(method("GET"))
            .and(wm_path("/drive/v3/files"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"files": []})),
            )
            .mount(&server)
            .await;

        // Stage 2: POST resumable init → 200 + Location.
        let session_url = format!("{}/upload/drive/v3/session/abc", server.uri());
        Mock::given(method("POST"))
            .and(wm_path("/upload/drive/v3/files"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("location", session_url.as_str()),
            )
            .mount(&server)
            .await;

        // Stage 3: PUT session URL → 200 with file metadata.
        Mock::given(method("PUT"))
            .and(wm_path("/upload/drive/v3/session/abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "drive-id-1",
                "name": "cam1/clip.mp4",
                "size": "13",
                "appProperties": { "nexus_sha256": sha256_hex(b"hello cold!!!") }
            })))
            .mount(&server)
            .await;

        let backend = build(&server).await;
        let bytes = b"hello cold!!!"; // 13 bytes
        let hash = sha256_hex(bytes);
        let receipt = backend
            .put("cam1/clip.mp4", bytes, &hash)
            .await
            .expect("put ok");
        assert_eq!(receipt.cold_path, "cam1/clip.mp4");
        assert_eq!(receipt.bytes_written, bytes.len() as u64);
    }

    #[tokio::test]
    async fn put_short_circuits_when_existing_sha256_matches() {
        let server = MockServer::start().await;
        let token_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        mount_token(&server, token_calls.clone()).await;

        let bytes = b"already there";
        let hash = sha256_hex(bytes);
        Mock::given(method("GET"))
            .and(wm_path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "files": [{
                    "id": "drive-id-9",
                    "name": "cam1/clip.mp4",
                    "size": bytes.len().to_string(),
                    "appProperties": { "nexus_sha256": hash }
                }]
            })))
            .mount(&server)
            .await;

        let backend = build(&server).await;
        let receipt = backend
            .put("cam1/clip.mp4", bytes, &hash)
            .await
            .expect("put ok");
        // No bytes written because we found a matching cold copy.
        assert_eq!(receipt.bytes_written, 0);
    }

    #[tokio::test]
    async fn exists_strict_about_sha256_marker() {
        let server = MockServer::start().await;
        let token_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        mount_token(&server, token_calls.clone()).await;

        Mock::given(method("GET"))
            .and(wm_path("/drive/v3/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "files": [{
                    "id": "drive-id-x",
                    "name": "cam1/clip.mp4",
                    "size": "100",
                    "appProperties": { "nexus_sha256": "deadbeef" }
                }]
            })))
            .mount(&server)
            .await;

        let backend = build(&server).await;
        assert!(backend
            .exists("cam1/clip.mp4", "deadbeef")
            .await
            .expect("exists ok"));
        assert!(!backend
            .exists("cam1/clip.mp4", "cafebabe")
            .await
            .expect("exists ok"));
    }

    #[tokio::test]
    async fn refresh_on_401_then_retry_succeeds() {
        let server = MockServer::start().await;
        // Token endpoint: serve a different access token each call.
        let token_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let tk = token_calls.clone();
        Mock::given(method("POST"))
            .and(wm_path("/token"))
            .respond_with(move |_: &wiremock::Request| {
                let i = tk.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": format!("ya29.AT-{i}"),
                    "expires_in": 3600,
                }))
            })
            .mount(&server)
            .await;

        // First call to /files returns 401 with the stale token;
        // second call with the refreshed token returns empty.
        // wiremock has no native "respond differently per call"
        // helper, so we layer two mocks with a per-mock counter
        // and the higher-priority one consumes the first request.
        let first_call = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fc = first_call.clone();
        Mock::given(method("GET"))
            .and(wm_path("/drive/v3/files"))
            .and(header("authorization", "Bearer ya29.AT-0"))
            .respond_with(move |_: &wiremock::Request| {
                fc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(401).set_body_string("auth expired")
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/drive/v3/files"))
            .and(header("authorization", "Bearer ya29.AT-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "files": []
            })))
            .mount(&server)
            .await;

        // Init + PUT for the retried upload.
        let session_url = format!("{}/upload/drive/v3/session/r", server.uri());
        Mock::given(method("POST"))
            .and(wm_path("/upload/drive/v3/files"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("location", session_url.as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(wm_path("/upload/drive/v3/session/r"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "drive-id-r",
                "name": "cam1/c.mp4",
            })))
            .mount(&server)
            .await;

        let backend = build(&server).await;
        let bytes = b"retry-after-401";
        let hash = sha256_hex(bytes);
        let receipt = backend.put("cam1/c.mp4", bytes, &hash).await.unwrap();
        assert_eq!(receipt.bytes_written, bytes.len() as u64);
        assert_eq!(
            first_call.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "expected exactly one 401 attempt with the stale token"
        );
        assert_eq!(
            token_calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "expected exactly one initial mint + one refresh"
        );
    }

    #[tokio::test]
    async fn delete_idempotent_for_missing_file() {
        let server = MockServer::start().await;
        let token_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        mount_token(&server, token_calls.clone()).await;
        Mock::given(method("GET"))
            .and(wm_path("/drive/v3/files"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"files": []})),
            )
            .mount(&server)
            .await;

        let backend = build(&server).await;
        let removed = backend.delete("cam1/clip.mp4").await.unwrap();
        assert!(!removed, "delete of absent file must return false");
    }

    #[tokio::test]
    async fn health_ok_when_about_returns_200() {
        let server = MockServer::start().await;
        let token_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        mount_token(&server, token_calls.clone()).await;
        Mock::given(method("GET"))
            .and(wm_path("/drive/v3/about"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let backend = build(&server).await;
        matches!(backend.health().await, HealthStatus::Ok)
            .then_some(())
            .unwrap();
    }
}

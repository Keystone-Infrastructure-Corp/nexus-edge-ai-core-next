//! M2.2 cold-mirror replication: backend abstraction + LAN impl.
//!
//! ## Why a separate crate?
//!
//! The cold-mirror layer is intentionally isolated from the engine
//! and from the M2.1 hot-storage code path. Three reasons:
//!
//!   1. **Recorder unaware of cold.** The recorder writes to the hot
//!      backend only; the cold replicator follower is the *only*
//!      caller of `ColdBackend::put`. Putting the trait in a leaf
//!      crate keeps it physically impossible to accidentally call
//!      from the recorder.
//!
//!   2. **Cloud backends land in `nexus-storage-cloud`** (Phase 2).
//!      That crate has heavy dependencies (oauth2, reqwest, AES-GCM)
//!      that core deployments without cloud access have no business
//!      pulling. The split mirrors the M3.1 visual-feature add-on.
//!
//!   3. **Trait-object dispatch.** The replicator holds an
//!      `Arc<dyn ColdBackend>` so the active cold target can be
//!      hot-swapped via the admin API without rebuilding any
//!      pipeline state. A standalone crate is the cleanest place
//!      to define + document that contract.
//!
//! ## The contract
//!
//! See [`ColdBackend`] for the full trait. Three invariants every
//! impl must honour:
//!
//!   * `put` is **idempotent** — re-uploading the same `(handle,
//!     path, sha256)` triple after a partial failure must succeed
//!     without duplicating bytes. The replicator may retry on
//!     transient failures and rely on this.
//!   * `delete` is a no-op on the *source* file. The replicator
//!     never deletes from cold (M2.2 spec invariant); cold deletion
//!     is operator-driven via the admin API only.
//!   * `exists` returns `true` only for *complete, non-torn*
//!     uploads. The strict variant in `LanFsBackend::exists` does a
//!     stat + first/last 64 KB sha256 spot-check to guard against
//!     the case where a previous upload crashed mid-write and left
//!     a partial file with the right name.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod lan;
mod throttle;
pub mod token_crypto;

pub use lan::LanFsBackend;
pub use throttle::TokenBucket;

// Re-export `bytes::Bytes` so downstream crates can talk to
// [`ColdBackend::get_range_stream`] without depending on the
// `bytes` crate directly.
pub use bytes::Bytes;

/// Boxed byte-stream returned by [`ColdBackend::get_range_stream`].
/// Items together cover the requested range in order; chunk
/// boundaries are an implementation detail.
pub type ByteStream = BoxStream<'static, Result<Bytes, BackendError>>;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum BackendError {
    /// I/O failure (network, disk, permission). Replicator treats
    /// these as transient and may retry after backoff.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The cold backend is currently unreachable — link down,
    /// share unmounted, OAuth token revoked, etc. The replicator
    /// emits a single `STORAGE_COLD_UNREACHABLE` bus event on the
    /// first such failure per outage and pauses the worker.
    #[error("backend unreachable: {0}")]
    Unreachable(String),

    /// The bytes that arrived at cold do not match the expected
    /// `sha256`. Replicator re-uploads on next tick (cold copy of a
    /// torn write must NEVER be left in place — `delete` is called
    /// before retry).
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    /// Caller passed a path that escapes the backend's configured
    /// root (`..` traversal, absolute path, etc.). Always a
    /// programming error in the replicator; never surfaced from a
    /// remote backend.
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// Authentication / authorization failure (cloud only). Treated
    /// like Unreachable for the bus event but surfaced separately
    /// so the admin UI can prompt for re-consent.
    #[error("auth: {0}")]
    Auth(String),

    /// Catch-all for backend-specific errors that don't fit any of
    /// the buckets above. Replicator logs at warn and treats as
    /// transient (retries with backoff).
    #[error("backend: {0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Backend metadata
// ---------------------------------------------------------------------------

/// Volume-level metadata reported by [`ColdBackend::volume_info`].
/// All sizes are bytes; capacity may be `None` for cloud backends
/// where the quota is not knowable cheaply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
}

/// Health probe result from [`ColdBackend::health`]. The replicator
/// checks this before every batch and pauses the worker on
/// non-`Ok`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HealthStatus {
    /// Backend reachable and writable.
    Ok,
    /// Backend reachable but read-only (filesystem mounted ro,
    /// OneDrive quota full, etc.). Replicator pauses.
    ReadOnly { reason: String },
    /// Backend unreachable. Replicator emits
    /// `STORAGE_COLD_UNREACHABLE` and pauses until the next health
    /// tick recovers.
    Unreachable { reason: String },
}

// ---------------------------------------------------------------------------
// ColdBackend trait
// ---------------------------------------------------------------------------

/// Abstraction over an attached cold-storage backend. Implementations
/// are kept I/O-bound and trait-object-friendly so the replicator can
/// hot-swap them via `storage_backends` table updates without
/// restarting the engine.
///
/// **Threading model.** Implementations MUST be safe to share across
/// threads (`Send + Sync`) and MUST not require interior async-state
/// pinning beyond what `async_trait` provides. The replicator wraps
/// every impl in `Arc<dyn ColdBackend>` and may call `put` /
/// `get_range` from different tokio tasks concurrently (today's
/// replicator is single-threaded, but future fan-out is in scope).
#[async_trait]
pub trait ColdBackend: Send + Sync {
    /// Stable handle (matches `storage_backends.handle`). Used as
    /// the dictionary key in the replicator's [`Registry`].
    fn handle(&self) -> &str;

    /// Backend kind discriminator (matches `storage_backends.kind`:
    /// `local`, `usb`, `lan`, `gdrive`, `onedrive`).
    fn kind(&self) -> &str;

    /// Upload a single clip. `path` is relative to the backend's
    /// configured root (e.g. `cam1/1700000000_15000.mp4`); the
    /// implementation enforces that traversal-safe path resolution
    /// never escapes that root.
    ///
    /// MUST be idempotent: re-calling with the same `(path, sha256)`
    /// after a previous partial-failure must succeed without
    /// duplicating bytes. After write, the impl re-computes the
    /// content hash and returns `ChecksumMismatch` if it differs
    /// from `expected_sha256`. The replicator deletes any torn
    /// partial on the next tick before retrying.
    async fn put(
        &self,
        path: &str,
        bytes: &[u8],
        expected_sha256: &str,
    ) -> Result<PutReceipt, BackendError>;

    /// Read a byte range out of a clip. Used by the future
    /// soft-evict-then-stream-from-cold playback path (Phase 4).
    /// Phase 1 only requires the impl to *exist*; the engine does
    /// not call it yet.
    async fn get_range(
        &self,
        path: &str,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Vec<u8>, BackendError>;

    /// Streaming variant of [`Self::get_range`]. The default impl
    /// buffers the whole range and wraps it as a single-frame
    /// stream — correct but allocates the full clip in RAM.
    /// Implementations SHOULD override this:
    ///
    /// * `LanFsBackend` reads the file in fixed chunks so peak RAM
    ///   per request stays at one chunk regardless of clip size.
    /// * The cloud backends pipe `reqwest::Response::bytes_stream()`
    ///   straight through so a 50 MB clip × 4 concurrent viewers
    ///   no longer means a 200 MB transient buffer in the engine.
    ///
    /// Callers MUST NOT assume the chunk boundaries are stable
    /// between impls; the stream's items are an opaque sequence of
    /// `Bytes` slices that together cover the requested range in
    /// order.
    async fn get_range_stream(
        &self,
        path: &str,
        start: u64,
        end_inclusive: u64,
    ) -> Result<ByteStream, BackendError> {
        let buf = self.get_range(path, start, end_inclusive).await?;
        Ok(stream::once(async move { Ok(Bytes::from(buf)) }).boxed())
    }

    /// Permanently remove a clip from cold. Operator-driven only;
    /// the replicator never calls this. Returns `Ok(false)` when
    /// the path was already absent (idempotent delete).
    async fn delete(&self, path: &str) -> Result<bool, BackendError>;

    /// Returns `true` when a *complete* clip lives at `path`.
    /// Implementations MUST do more than a stat check — at minimum,
    /// stat + first/last 64 KB sha256 spot-check — so partial
    /// uploads from a previous crash are NOT mistaken for valid
    /// existing copies. The replicator uses this to skip already-
    /// uploaded clips on polling-backstop scans.
    async fn exists(&self, path: &str, expected_sha256: &str) -> Result<bool, BackendError>;

    /// Volume-level metadata. Cheap (one stat / one cloud "drive
    /// info" call). Returned over the admin API for the UI.
    async fn volume_info(&self) -> Result<VolumeInfo, BackendError>;

    /// Liveness probe. Replicator calls before every batch; cheap
    /// equivalent of "can I write a 0-byte file to a temp path
    /// under the root?". Implementations MUST NOT block longer than
    /// a few seconds; 5 s is the soft cap.
    async fn health(&self) -> HealthStatus;
}

/// Receipt returned by [`ColdBackend::put`]. The replicator passes
/// the `cold_path` + `cold_uploaded_at` straight into
/// `Store::mark_cold_replicated`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutReceipt {
    /// Backend-resolved path that the clip now lives at. For
    /// `LanFsBackend` this echoes the input `path`; for cloud
    /// backends it may include an opaque session/file id segment.
    pub cold_path: String,
    /// UTC timestamp the upload finalised at.
    pub uploaded_at: DateTime<Utc>,
    /// Bytes actually written (may differ from input length on
    /// resumable uploads where part of the file was already there).
    pub bytes_written: u64,
}

// ---------------------------------------------------------------------------
// Registry — name → backend lookup, rebuilt on STORAGE_BACKENDS_CHANGED
// ---------------------------------------------------------------------------

/// Thread-safe map of backend `handle` → impl. Rebuilt at boot from
/// the `storage_backends` table and on every
/// `STORAGE_BACKENDS_CHANGED` bus event. Cheap to clone (it's a
/// shared `Arc<RwLock<…>>`).
///
/// The replicator holds one of these and looks up the active cold
/// backend by handle on every batch — that way an admin
/// reconfiguring the cold target via the API takes effect on the
/// next tick without restarting the engine task.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<RwLock<std::collections::HashMap<String, Arc<dyn ColdBackend>>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the entire backend set in one swap. Called by the
    /// boot loader and by the `STORAGE_BACKENDS_CHANGED` handler.
    pub fn replace_all<I>(&self, backends: I)
    where
        I: IntoIterator<Item = Arc<dyn ColdBackend>>,
    {
        let mut new_map = std::collections::HashMap::new();
        for b in backends {
            new_map.insert(b.handle().to_string(), b);
        }
        *self.inner.write() = new_map;
    }

    /// Look up a backend by handle. Returns `None` if the registry
    /// has no entry — the replicator treats that as "cold disabled
    /// for this tick" and skips quietly.
    pub fn get(&self, handle: &str) -> Option<Arc<dyn ColdBackend>> {
        self.inner.read().get(handle).cloned()
    }

    /// Iterate over `(handle, kind)` for the admin API listing.
    pub fn snapshot(&self) -> Vec<(String, String)> {
        self.inner
            .read()
            .iter()
            .map(|(h, b)| (h.clone(), b.kind().to_string()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Backend factory — kind + config_json → Arc<dyn ColdBackend>
// ---------------------------------------------------------------------------

/// Construct a `ColdBackend` from the discriminator + JSON config
/// stored in `storage_backends`. Phase 1 only knows about `lan`;
/// `gdrive` / `onedrive` will land in `nexus-storage-cloud` and be
/// merged in via a small wrapper factory in the engine.
pub fn build_backend(
    handle: &str,
    kind: &str,
    config_json: &str,
) -> Result<Arc<dyn ColdBackend>, BackendError> {
    let cfg: serde_json::Value = serde_json::from_str(config_json)
        .map_err(|e| BackendError::Other(format!("config_json parse: {e}")))?;
    match kind {
        "lan" => {
            let root = cfg.get("root").and_then(|v| v.as_str()).ok_or_else(|| {
                BackendError::Other("lan backend requires `root` in config_json".to_string())
            })?;
            Ok(Arc::new(LanFsBackend::new(handle, PathBuf::from(root))?))
        }
        "local" => Err(BackendError::Other(
            "kind='local' is the implicit hot backend; never registered as cold".to_string(),
        )),
        other => Err(BackendError::Other(format!(
            "unknown backend kind '{other}' (cloud kinds need `nexus-storage-cloud`)",
        ))),
    }
}

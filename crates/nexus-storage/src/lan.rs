//! `LanFsBackend` — cold mirror to a mounted filesystem path.
//!
//! "LAN" in the M2.2 spec is a misnomer; this impl works for any
//! filesystem-mounted destination (NFS share, SMB mount, USB stick,
//! second internal drive). Cloud backends with an HTTP control
//! plane live in `nexus-storage-cloud` (Phase 2).
//!
//! ## Idempotent put
//!
//! Each `put` writes to a temporary `<path>.partial` sibling first,
//! then atomically renames into place. That gives us crash-safety:
//! a partially-written file never appears at the final path, so the
//! strict `exists()` check below can trust that any file it sees
//! through `metadata()` corresponds to a *previously-completed*
//! upload. (Idempotent re-uploads simply overwrite the temp file
//! and re-rename.)
//!
//! ## Strict `exists`
//!
//! Plain `path.is_file()` is *not* enough — even with our atomic
//! rename, a power loss between the rename and the next operation
//! could leave a perfectly-named file with the wrong bytes (the
//! rename is journaled, the data may not be). We add a stat + first
//! 64 KB + last 64 KB sha256 spot-check: cheap on modern SSDs
//! (~tens of µs), catches the >99 % of torn-write cases, and is
//! correct *enough* for the cold-mirror contract (the replicator
//! re-uploads on any mismatch). A full sha256 of every clip on
//! every poll-backstop tick would amortise to gigabytes/hour for a
//! large camera fleet.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures::stream::{self, StreamExt};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::{debug, warn};

use super::{BackendError, ByteStream, ColdBackend, HealthStatus, PutReceipt, VolumeInfo};

const SPOT_CHECK_BYTES: u64 = 64 * 1024;

/// Chunk size handed back from [`LanFsBackend::get_range_stream`].
/// 64 KiB matches the kernel readahead window on every tier and
/// keeps the per-request transient buffer fixed regardless of
/// clip size.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

pub struct LanFsBackend {
    handle: String,
    root: PathBuf,
}

impl LanFsBackend {
    /// Construct a backend rooted at `root`. The directory MUST
    /// exist and be writable; we eagerly fail at construction so a
    /// misconfigured backend never silently lurks until the first
    /// upload.
    pub fn new(handle: impl Into<String>, root: PathBuf) -> Result<Self, BackendError> {
        if !root.is_dir() {
            return Err(BackendError::Other(format!(
                "lan backend root does not exist or is not a directory: {}",
                root.display()
            )));
        }
        Ok(Self {
            handle: handle.into(),
            root,
        })
    }

    /// Resolve a backend-relative path to an absolute filesystem
    /// path, rejecting any `..` traversal. Does NOT touch the
    /// filesystem.
    fn resolve(&self, rel: &str) -> Result<PathBuf, BackendError> {
        let p = Path::new(rel);
        if p.is_absolute() {
            return Err(BackendError::InvalidPath(format!(
                "backend path must be relative: {rel}"
            )));
        }
        for comp in p.components() {
            if matches!(comp, std::path::Component::ParentDir) {
                return Err(BackendError::InvalidPath(format!(
                    "backend path contains `..`: {rel}"
                )));
            }
        }
        Ok(self.root.join(p))
    }

    /// Hash the first + last `SPOT_CHECK_BYTES` of `abs` and
    /// compare against `expected_hex`. Used by the strict `exists`
    /// check; `false` covers both "wrong content" and "file shorter
    /// than 2 × SPOT_CHECK_BYTES so no spot-check region exists"
    /// (in the latter case we hash the whole file inline).
    async fn spot_check(abs: &Path, expected_hex: &str) -> Result<bool, BackendError> {
        let mut f = fs::File::open(abs).await?;
        let len = f.metadata().await?.len();
        let mut hasher = Sha256::new();

        if len <= 2 * SPOT_CHECK_BYTES {
            // Tiny clip — just hash the whole thing. Cheaper than
            // doing two seek+read calls for sub-128 KB files.
            let mut buf = Vec::with_capacity(len as usize);
            f.read_to_end(&mut buf).await?;
            hasher.update(&buf);
        } else {
            // First 64 KB
            let mut head = vec![0u8; SPOT_CHECK_BYTES as usize];
            f.read_exact(&mut head).await?;
            hasher.update(&head);
            // Last 64 KB
            f.seek(SeekFrom::End(-(SPOT_CHECK_BYTES as i64))).await?;
            let mut tail = vec![0u8; SPOT_CHECK_BYTES as usize];
            f.read_exact(&mut tail).await?;
            hasher.update(&tail);
            // And the file length, so two equally-sized files with
            // matching head+tail but different bodies don't collide.
            hasher.update(len.to_le_bytes());
        }

        let actual = hex_lower(&hasher.finalize());
        // The expected_hex is the FULL-file sha256; the spot-check
        // hash will not equal it. We're checking "spot-check this
        // file, hash the metadata signature, and store/reuse it
        // against the manifest". For now: store a parallel sidecar
        // (`<path>.sha256`) holding the spot-check digest at upload
        // time; on `exists` we re-spot-check and compare against
        // the sidecar. If the sidecar is missing we conservatively
        // return false so the replicator re-uploads.
        let sidecar = abs.with_extension(extend_ext(abs, "sha256"));
        let sidecar_bytes = match fs::read_to_string(&sidecar).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %abs.display(), "spot-check sidecar missing; treating clip as absent");
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        };
        let mut iter = sidecar_bytes.split_whitespace();
        let stored_full = iter.next().unwrap_or("").trim();
        let stored_spot = iter.next().unwrap_or("").trim();
        if stored_full != expected_hex {
            // The cold copy is for a different content hash; treat
            // as absent so the replicator overwrites it.
            return Ok(false);
        }
        Ok(stored_spot == actual)
    }
}

#[async_trait]
impl ColdBackend for LanFsBackend {
    fn handle(&self) -> &str {
        &self.handle
    }

    fn kind(&self) -> &str {
        "lan"
    }

    async fn put(
        &self,
        path: &str,
        bytes: &[u8],
        expected_sha256: &str,
    ) -> Result<PutReceipt, BackendError> {
        let abs = self.resolve(path)?;
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Verify caller's expected hash matches the bytes BEFORE
        // touching the disk. A mismatch here is always a programming
        // error (the replicator computed sha256 from the same source
        // bytes seconds earlier), but we'd rather fail fast than
        // write garbage.
        let actual_hex = sha256_hex(bytes);
        if actual_hex != expected_sha256 {
            return Err(BackendError::ChecksumMismatch {
                expected: expected_sha256.to_string(),
                actual: actual_hex,
            });
        }

        // Atomic write via `<path>.partial` + rename. Crash-safe.
        let tmp = abs.with_extension(extend_ext(&abs, "partial"));
        {
            let mut f = fs::File::create(&tmp).await?;
            f.write_all(bytes).await?;
            f.flush().await?;
            // fsync the data so the rename below cannot land before
            // the bytes hit the platter.
            f.sync_all().await?;
        }
        fs::rename(&tmp, &abs).await?;

        // Compute + persist the spot-check sidecar so subsequent
        // `exists` calls don't have to read the whole file.
        let spot_hex = compute_spot_check(&abs).await?;
        let sidecar = abs.with_extension(extend_ext(&abs, "sha256"));
        fs::write(&sidecar, format!("{expected_sha256} {spot_hex}\n")).await?;

        let bytes_written = bytes.len() as u64;
        Ok(PutReceipt {
            cold_path: path.to_string(),
            uploaded_at: Utc::now(),
            bytes_written,
        })
    }

    async fn get_range(
        &self,
        path: &str,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Vec<u8>, BackendError> {
        let abs = self.resolve(path)?;
        let mut f = fs::File::open(&abs).await?;
        f.seek(SeekFrom::Start(start)).await?;
        let len = end_inclusive
            .checked_sub(start)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| {
                BackendError::InvalidPath(format!("bad range {start}..={end_inclusive}"))
            })?;
        let mut buf = vec![0u8; len as usize];
        f.read_exact(&mut buf).await?;
        Ok(buf)
    }

    async fn get_range_stream(
        &self,
        path: &str,
        start: u64,
        end_inclusive: u64,
    ) -> Result<ByteStream, BackendError> {
        let abs = self.resolve(path)?;
        let mut f = fs::File::open(&abs).await?;
        f.seek(SeekFrom::Start(start)).await?;
        let remaining: u64 = end_inclusive
            .checked_sub(start)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| {
                BackendError::InvalidPath(format!("bad range {start}..={end_inclusive}"))
            })?;
        // try_unfold over (file, remaining). Each poll reads up to
        // STREAM_CHUNK_BYTES and yields a `Bytes` chunk; EOF arrives
        // exactly when remaining hits zero. We avoid `read_exact`
        // so a short read on the last chunk still flushes the data
        // we already have.
        let s = stream::try_unfold((f, remaining), |(mut f, remaining)| async move {
            if remaining == 0 {
                return Ok::<Option<(Bytes, _)>, BackendError>(None);
            }
            let want = std::cmp::min(remaining as usize, STREAM_CHUNK_BYTES);
            let mut buf = vec![0u8; want];
            let mut filled = 0usize;
            while filled < want {
                let n = f.read(&mut buf[filled..]).await?;
                if n == 0 {
                    // EOF mid-range — surface as IO error so callers
                    // know the range claim was wrong.
                    return Err(BackendError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "get_range_stream: short read at offset, expected {want} bytes, got {filled}"
                        ),
                    )));
                }
                filled += n;
            }
            buf.truncate(filled);
            let chunk = Bytes::from(buf);
            Ok(Some((chunk, (f, remaining - filled as u64))))
        });
        Ok(s.boxed())
    }

    async fn delete(&self, path: &str) -> Result<bool, BackendError> {
        let abs = self.resolve(path)?;
        match fs::remove_file(&abs).await {
            Ok(()) => {
                // Best-effort sidecar cleanup; if the sidecar is
                // missing, we ignore it.
                let sidecar = abs.with_extension(extend_ext(&abs, "sha256"));
                if let Err(e) = fs::remove_file(&sidecar).await {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        warn!(path = %sidecar.display(), error = %e, "delete sidecar failed");
                    }
                }
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn exists(&self, path: &str, expected_sha256: &str) -> Result<bool, BackendError> {
        let abs = self.resolve(path)?;
        match fs::metadata(&abs).await {
            Ok(m) if m.is_file() => Self::spot_check(&abs, expected_sha256).await,
            Ok(_) => Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn volume_info(&self) -> Result<VolumeInfo, BackendError> {
        // Cross-platform free/total bytes via `fs2` is one option,
        // but we already pull `nexus_engine::storage_safety` for
        // the hot-side stats. To avoid pulling another dep, return
        // None for both fields here; the admin API can fall back to
        // showing "unknown". Phase 2 polish: implement via libc /
        // `fs2` once a real customer asks for it.
        Ok(VolumeInfo {
            free_bytes: None,
            total_bytes: None,
            used_bytes: None,
        })
    }

    async fn health(&self) -> HealthStatus {
        // Cheap probe: stat the root. If it's gone the share is
        // unmounted. We deliberately do NOT try a write probe here
        // because that would amount to a poll-induced retry storm
        // when an SMB share blips for a few seconds; the next
        // `put` failure will surface the real issue.
        match fs::metadata(&self.root).await {
            Ok(m) if m.is_dir() => HealthStatus::Ok,
            Ok(_) => HealthStatus::Unreachable {
                reason: format!("{} is not a directory", self.root.display()),
            },
            Err(e) => HealthStatus::Unreachable {
                reason: format!("stat {} failed: {e}", self.root.display()),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex_lower(&h.finalize())
}

/// Append a sub-extension to an existing path. `extend_ext("a.mp4",
/// "partial")` → `"a.mp4.partial"`. Used so the temp file and
/// sidecar live next to the final clip and the orphan-file scanner
/// (Phase 4) can clean them up if the engine crashed mid-rename.
fn extend_ext(p: &Path, suffix: &str) -> std::ffi::OsString {
    let mut ext = p.extension().map(|e| e.to_owned()).unwrap_or_default();
    if !ext.is_empty() {
        ext.push(".");
    }
    ext.push(suffix);
    ext
}

async fn compute_spot_check(abs: &Path) -> Result<String, BackendError> {
    let mut f = fs::File::open(abs).await?;
    let len = f.metadata().await?.len();
    let mut hasher = Sha256::new();
    if len <= 2 * SPOT_CHECK_BYTES {
        let mut buf = Vec::with_capacity(len as usize);
        f.read_to_end(&mut buf).await?;
        hasher.update(&buf);
    } else {
        let mut head = vec![0u8; SPOT_CHECK_BYTES as usize];
        f.read_exact(&mut head).await?;
        hasher.update(&head);
        f.seek(SeekFrom::End(-(SPOT_CHECK_BYTES as i64))).await?;
        let mut tail = vec![0u8; SPOT_CHECK_BYTES as usize];
        f.read_exact(&mut tail).await?;
        hasher.update(&tail);
        hasher.update(len.to_le_bytes());
    }
    Ok(hex_lower(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fixture() -> (LanFsBackend, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let backend = LanFsBackend::new("lan-test", dir.path().to_path_buf()).unwrap();
        (backend, dir)
    }

    #[tokio::test]
    async fn put_get_round_trip() {
        let (backend, _dir) = fixture();
        let bytes = b"hello cold storage";
        let hash = sha256_hex(bytes);
        let receipt = backend.put("cam1/a.mp4", bytes, &hash).await.unwrap();
        assert_eq!(receipt.bytes_written, bytes.len() as u64);
        assert_eq!(receipt.cold_path, "cam1/a.mp4");

        let exists = backend.exists("cam1/a.mp4", &hash).await.unwrap();
        assert!(exists, "spot-check on freshly-put clip should pass");

        let read = backend
            .get_range("cam1/a.mp4", 0, (bytes.len() - 1) as u64)
            .await
            .unwrap();
        assert_eq!(read, bytes);
    }

    #[tokio::test]
    async fn put_rejects_traversal() {
        let (backend, _dir) = fixture();
        let bytes = b"x";
        let hash = sha256_hex(bytes);
        let err = backend
            .put("../escape.mp4", bytes, &hash)
            .await
            .unwrap_err();
        assert!(matches!(err, BackendError::InvalidPath(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn put_detects_caller_hash_mismatch() {
        let (backend, _dir) = fixture();
        let err = backend
            .put("a.mp4", b"hello", &"deadbeef".repeat(8))
            .await
            .unwrap_err();
        assert!(
            matches!(err, BackendError::ChecksumMismatch { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn exists_returns_false_when_absent() {
        let (backend, _dir) = fixture();
        let exists = backend.exists("nope.mp4", &"a".repeat(64)).await.unwrap();
        assert!(!exists);
    }

    #[tokio::test]
    async fn exists_returns_false_after_torn_write() {
        let (backend, dir) = fixture();
        // Simulate a torn write by dropping a file at the final
        // path with no sidecar. The strict `exists` should reject.
        std::fs::create_dir_all(dir.path().join("cam1")).unwrap();
        std::fs::write(dir.path().join("cam1/torn.mp4"), b"junk").unwrap();
        let exists = backend
            .exists("cam1/torn.mp4", &"a".repeat(64))
            .await
            .unwrap();
        assert!(!exists, "missing sidecar must read as `not present`");
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let (backend, _dir) = fixture();
        let bytes = b"x";
        let hash = sha256_hex(bytes);
        backend.put("a.mp4", bytes, &hash).await.unwrap();

        assert!(backend.delete("a.mp4").await.unwrap());
        assert!(
            !backend.delete("a.mp4").await.unwrap(),
            "second delete returns false"
        );
        assert!(!backend.exists("a.mp4", &hash).await.unwrap());
    }

    #[tokio::test]
    async fn health_ok_on_existing_root() {
        let (backend, _dir) = fixture();
        matches!(backend.health().await, HealthStatus::Ok)
            .then_some(())
            .expect("fresh tempdir backend should report Ok");
    }

    /// M2.2 perf P2 — `get_range_stream` must yield the same bytes
    /// as `get_range` but spread across multiple chunks for a
    /// payload larger than `STREAM_CHUNK_BYTES`. We assert both
    /// the content equality (must be identical to the buffered
    /// path) AND that at least two chunks arrived (otherwise the
    /// streaming win is theoretical only).
    #[tokio::test]
    async fn get_range_stream_chunks_large_payload() {
        use futures::stream::StreamExt;

        let (backend, _dir) = fixture();
        // 192 KiB → exactly 3 × 64 KiB chunks.
        let bytes: Vec<u8> = (0..3 * STREAM_CHUNK_BYTES)
            .map(|i| (i & 0xff) as u8)
            .collect();
        let hash = sha256_hex(&bytes);
        backend.put("cam1/big.mp4", &bytes, &hash).await.unwrap();

        let mut stream = backend
            .get_range_stream("cam1/big.mp4", 0, (bytes.len() - 1) as u64)
            .await
            .unwrap();

        let mut collected = Vec::new();
        let mut chunks = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            chunks += 1;
            collected.extend_from_slice(&chunk);
        }
        assert_eq!(collected, bytes, "streamed body must equal source");
        assert!(
            chunks >= 2,
            "expected multiple chunks for a payload > {STREAM_CHUNK_BYTES} bytes, got {chunks}"
        );
    }

    #[tokio::test]
    async fn get_range_stream_honours_subrange() {
        use futures::stream::StreamExt;

        let (backend, _dir) = fixture();
        let bytes: Vec<u8> = (0..1024u32).map(|i| (i & 0xff) as u8).collect();
        let hash = sha256_hex(&bytes);
        backend.put("a.mp4", &bytes, &hash).await.unwrap();

        let mut stream = backend.get_range_stream("a.mp4", 100, 199).await.unwrap();
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected, &bytes[100..=199]);
    }
}

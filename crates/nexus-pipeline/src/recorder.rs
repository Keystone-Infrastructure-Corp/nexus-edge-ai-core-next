//! Clip recorder — M2.1 Stage A.
//!
//! The recorder is the bridge between the tracker's lifecycle events
//! (`born` / `updated` / `died`) and the on-disk `motion_clips` rows
//! plus the GStreamer-managed mp4 files.
//!
//! Stage A (this module) ships:
//!
//! * [`ClipRecorder`] — the trait every recorder backend implements.
//! * [`StubClipRecorder`] — the always-available implementation that
//!   creates 0-byte placeholder files and writes the matching
//!   `motion_clips` row. Lets the rest of Stage A (motion-event
//!   emitter, watermark sampler, eviction loop, retention sweeper, API
//!   endpoints) land + be tested without a working GStreamer
//!   pipeline.
//! * [`OpenClip`] / [`ClipFinal`] — the data the supervisor passes
//!   into the recorder when a track is born / dies, and what comes
//!   back when the file is finalised.
//!
//! Stage B will add a real `GstClipRecorder` behind
//! `#[cfg(feature = "gstreamer")]` that owns a `splitmuxsink` per
//! camera and stamps real `duration_ms` / `size_bytes`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nexus_store::{ClipClose, ClipId, NewClip, Store};
use nexus_types::{CameraId, CodecKind};
use thiserror::Error;
use tokio::fs;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Minimum acceptable clip duration. Below this threshold the close
/// path deletes the file and cascade-deletes the metadata row, on
/// the theory that anything shorter is from a spurious detection
/// that flickered into existence and back out before the post-roll
/// timer could even arm. Per M2.1 spec.
pub const MIN_CLIP_DURATION_MS: i64 = 3_000;

/// Maximum acceptable clip duration. Anything past this gets a
/// rotation event from the supervisor (close current clip + open a
/// new one immediately) so file sizes stay bounded during sustained
/// activity. Per M2.1 spec.
pub const MAX_CLIP_DURATION_MS: i64 = 5 * 60 * 1_000;

/// Open a new clip for a camera. The path on disk is recorder-controlled
/// so we don't have to thread filesystem layout through every caller.
#[derive(Debug, Clone)]
pub struct OpenClip {
    pub camera_id: CameraId,
    pub started_at: DateTime<Utc>,
    /// Per-camera supervisor (analysis) RGB frame width in
    /// effect when the clip was opened. Recorded onto the
    /// `motion_clips` row so the `/api/v1/clips/:id/tracks`
    /// overlay can scale `motion_events` bboxes (which live in
    /// supervisor-frame coords) against the right denominator,
    /// even after the operator later changes the camera's
    /// detector input size. See
    /// [`crate::source::supervisor_frame_for`].
    pub frame_width: u32,
    /// Companion to [`Self::frame_width`].
    pub frame_height: u32,
}

/// Hand the closed handle back to the recorder so it can finalise the
/// file and stamp the row.
#[derive(Debug, Clone)]
pub struct ClipFinal {
    pub ended_at: DateTime<Utc>,
}

/// Handle returned by [`ClipRecorder::open`]. Opaque to callers; the
/// only thing they do is hand it back to [`ClipRecorder::close`].
#[derive(Debug, Clone, Copy)]
pub struct ClipHandle {
    pub clip_id: ClipId,
    pub camera_id: CameraId,
}

/// Final state stamped on the `motion_clips` row when [`ClipRecorder::close`]
/// completes. Returned for the supervisor + tests so they don't need a
/// second store roundtrip.
#[derive(Debug, Clone)]
pub struct ClipMeta {
    pub clip_id: ClipId,
    pub camera_id: CameraId,
    pub path: PathBuf,
    pub duration_ms: i64,
    pub size_bytes: i64,
    pub codec: String,
    pub container: String,
    /// True when the clip was **discarded at close time** because its
    /// duration fell below [`MIN_CLIP_DURATION_MS`]. The on-disk file
    /// has been unlinked and the `motion_clips` row + every
    /// CASCADE-linked child has been removed via
    /// [`Store::cascade_delete_clip_metadata`]. The supervisor treats
    /// this as a normal outcome (no clip ever existed for this
    /// motion burst); diagnostic fields above are populated for
    /// logging only.
    pub discarded: bool,
}

#[derive(Debug, Error)]
pub enum RecorderError {
    #[error("recorder is panic-paused; new clips refused")]
    Refused,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("store: {0}")]
    Store(#[from] nexus_store::StoreError),
    #[error("clip {0} is not open in this recorder")]
    UnknownClip(ClipId),
}

/// What every recorder implementation has to provide. Stage A only ships
/// [`StubClipRecorder`]; Stage B will add `GstClipRecorder` behind
/// `#[cfg(feature = "gstreamer")]` against the same trait.
#[async_trait]
pub trait ClipRecorder: Send + Sync {
    /// Begin a new clip for the camera. Inserts the `motion_clips` row
    /// and returns the handle the supervisor will hand back to
    /// [`Self::close`] when the track dies (plus post-roll grace).
    ///
    /// Returns [`RecorderError::Refused`] when the watermark sampler
    /// has flipped the recorder into panic mode. Callers MUST treat
    /// that as a normal outcome (drop the motion event entirely; do
    /// NOT emit a clipless `motion_events` row — the schema
    /// invariant is `clip_id NOT NULL`).
    async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError>;

    /// Finalise the clip: close the file, stamp `ended_at` /
    /// `duration_ms` / `size_bytes` on the row.
    async fn close(&self, handle: ClipHandle, args: ClipFinal) -> Result<ClipMeta, RecorderError>;

    /// Set or clear the panic flag. The watermark sampler calls
    /// `set_panic(true)` when free space is below
    /// `panic_watermark_pct` and `set_panic(false)` once eviction has
    /// recovered to the low+5pct threshold.
    fn set_panic(&self, panic: bool);

    /// True iff `set_panic(true)` was the last call (or set in
    /// constructor). Read by tests and by `/api/v1/storage/local`.
    fn is_panic(&self) -> bool;

    /// Identifier for OPS dashboards / health endpoints (`stub`,
    /// `gstreamer`, etc.).
    fn kind(&self) -> &'static str;

    /// Hot-add a per-camera ingester. Called by the engine's
    /// `config.changed` reconciler when a camera is added or its
    /// ingest URL changes, so the GStreamer recorder can start a
    /// fresh always-on `PreRollIngester` without a process restart.
    ///
    /// `max_fps` is the camera's configured detector framerate. The
    /// GStreamer recorder uses it to size the shared RGB tap's
    /// `videorate` cap so the detector sees frames at the same
    /// rate the old standalone `RtspSource` would have produced;
    /// other recorders ignore it.
    ///
    /// `rgb_w` / `rgb_h` are the per-camera supervisor-frame
    /// resolution the shared RGB tap should publish (derived from
    /// the camera's resolved detector input size via
    /// [`crate::source::supervisor_frame_for`] at the engine spawn
    /// site). The GStreamer recorder threads them into the
    /// pre-roll ingester's RGB tap pipeline string so the
    /// detector receives frames at exactly its model's input
    /// width with no extra resize; other recorders ignore.
    ///
    /// Default impl is a no-op so the stub recorder (and any future
    /// non-pre-roll backend) doesn't have to opt in. The GStreamer
    /// recorder overrides it; failure to build the ingester is
    /// logged + swallowed (the camera will refuse clips, but the
    /// rest of the engine keeps running).
    #[allow(unused_variables)]
    #[allow(clippy::too_many_arguments)]
    fn add_camera_ingester(
        &self,
        camera_id: CameraId,
        url: &str,
        pre_roll_secs: u32,
        max_fps: u32,
        rgb_w: u32,
        rgb_h: u32,
        codec: CodecKind,
    ) -> Result<(), RecorderError> {
        Ok(())
    }

    /// Hot-remove a per-camera ingester. Mirror of
    /// [`Self::add_camera_ingester`]. Called when a camera is
    /// deleted or disabled. Default impl is a no-op.
    #[allow(unused_variables)]
    fn remove_camera_ingester(&self, camera_id: CameraId) {}

    /// Return a [`crate::source::FrameSource`] that consumes
    /// decoded RGB frames from this recorder's per-camera ingester
    /// (sharing the one RTSP session). Default returns `None`; the
    /// GStreamer recorder overrides to return a
    /// [`crate::source::SharedRtspSource`] iff it has an ingester
    /// for `camera_id` and the ingester was built with the RGB tap.
    ///
    /// The supervisor's `build_source` calls this BEFORE falling
    /// back to a freshly-built `RtspSource`. When `Some`, the
    /// detector + recorder share one connection — required for
    /// single-session camera firmwares (InSight 192.168.1.66 et
    /// al). When `None`, the supervisor opens its own RTSP
    /// session.
    #[allow(unused_variables)]
    fn shared_frame_source(
        &self,
        camera_id: CameraId,
    ) -> Option<Box<dyn crate::source::FrameSource + Send>> {
        None
    }
}

// ---------------------------------------------------------------------------
// Path layout helpers
// ---------------------------------------------------------------------------

/// In-flight clip path per M2.1 spec layout, with a `.partial.mp4`
/// suffix until the recorder closes the file and renames to the final
/// `_{duration_ms}` form.
///
/// Layout: `{clips_dir}/{camera_id}/{YYYY-MM-DD}/{start_unix_ms}.partial.mp4`
///
/// `{YYYY-MM-DD}` is the UTC date of `started_at` so all clips for one
/// surveillance day share a directory regardless of the operator's
/// local timezone (matches how the API + retention sweeper reason
/// about "yesterday's clips").
pub fn inflight_clip_path(
    clips_dir: &Path,
    camera_id: CameraId,
    started_at: DateTime<Utc>,
) -> PathBuf {
    let day = started_at.format("%Y-%m-%d");
    let start_ms = started_at.timestamp_millis();
    clips_dir
        .join(camera_id.to_string())
        .join(day.to_string())
        .join(format!("{start_ms}.partial.mp4"))
}

/// Final clip path per M2.1 spec layout, set by the recorder at
/// close time once `duration_ms` is known.
///
/// Layout: `{clips_dir}/{camera_id}/{YYYY-MM-DD}/{start_unix_ms}_{duration_ms}.mp4`
pub fn final_clip_path(
    clips_dir: &Path,
    camera_id: CameraId,
    started_at: DateTime<Utc>,
    duration_ms: i64,
) -> PathBuf {
    let day = started_at.format("%Y-%m-%d");
    let start_ms = started_at.timestamp_millis();
    clips_dir
        .join(camera_id.to_string())
        .join(day.to_string())
        .join(format!("{start_ms}_{duration_ms}.mp4"))
}

/// Best-effort relative path from `clips_dir` to `abs`. Returns the
/// stripped relative path on success; falls back to the absolute
/// representation if `abs` is not under `clips_dir` (shouldn't happen
/// in practice but keeps the recorder defensive).
pub fn clip_rel_path(clips_dir: &Path, abs: &Path) -> String {
    abs.strip_prefix(clips_dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| abs.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// USB hot-plug resolver — M2.2 Phase 3
// ---------------------------------------------------------------------------

/// Inversion-of-control trait the recorder uses to ask "is this USB
/// label currently attached, and if so where under `clips_dir` is it
/// mounted?". Implemented by `nexus_engine::usb_watch::UsbRegistry`;
/// kept in this crate so the recorder doesn't depend on the engine.
///
/// `lookup` returns the USB volume's mount path **relative to the
/// engine's `clips_dir`** (e.g. `"usb/NEXUS_VAULT"`). Returning
/// `None` means "not attached right now" — the recorder treats that
/// as a fall-through to the local hot tier.
pub trait UsbResolver: Send + Sync {
    fn lookup(&self, label: &str) -> Option<PathBuf>;
}

/// Shared, hot-mutable handle to the operator-selected preferred USB
/// label. The engine constructs one of these at boot (seeded from
/// the `engine_runtime_settings` table, falling back to
/// `nexus.toml`) and shares clones with both the recorder and the
/// admin API. Every clip-open reads the *current* value, so a
/// `PUT /api/v1/admin/runtime/usb_preferred` takes effect on the
/// next clip without restarting the engine.
///
/// `arc-swap` gives us a wait-free atomic read on the hot recorder
/// path (`open()` reads the label once per clip — well off the
/// per-frame fast path, but still synchronous so we don't want a
/// `Mutex` here). Writes are admin-PUT-only and contend with no
/// readers in practice.
///
/// `From<Option<String>>` is implemented so existing test callsites
/// like `with_usb(resolver, Some("X".into()))` keep compiling — the
/// label gets wrapped in a fresh, per-test handle that nothing else
/// can mutate.
#[derive(Clone, Default)]
pub struct PreferredUsbLabel {
    inner: Arc<arc_swap::ArcSwapOption<String>>,
}

impl PreferredUsbLabel {
    /// Seed a new handle with the boot-time value.
    pub fn new(initial: Option<String>) -> Self {
        Self {
            inner: Arc::new(arc_swap::ArcSwapOption::from(initial.map(Arc::new))),
        }
    }

    /// Read the current label. Cheap — `arc-swap`'s `load_full`
    /// bumps the inner Arc's refcount but doesn't acquire a lock.
    pub fn get(&self) -> Option<String> {
        self.inner.load_full().as_deref().map(|s| s.to_owned())
    }

    /// Atomically replace the label. Takes effect on the next
    /// `open()` call across every clone.
    pub fn set(&self, label: Option<String>) {
        self.inner.store(label.map(Arc::new));
    }
}

impl From<Option<String>> for PreferredUsbLabel {
    fn from(initial: Option<String>) -> Self {
        Self::new(initial)
    }
}

impl std::fmt::Debug for PreferredUsbLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreferredUsbLabel")
            .field("current", &self.get())
            .finish()
    }
}

/// Resolve the clip-output directory + matching `hot_handle` for a
/// new clip. Pure function so it can be unit-tested without standing
/// up a recorder.
///
/// Truth table:
///
/// | preferred_label | resolver       | lookup      | result                                                         |
/// |-----------------|----------------|-------------|----------------------------------------------------------------|
/// | `None`          | *              | *           | `(clips_dir.into(), "local")`                                  |
/// | `Some(label)`   | `None`         | *           | `(clips_dir.into(), "local")`                                  |
/// | `Some(label)`   | `Some(r)`      | `None`      | `(clips_dir.into(), "local")`                                  |
/// | `Some(label)`   | `Some(r)`      | `Some(rel)` | `(clips_dir.join(rel), format!("usb-{label}"))`                |
pub fn effective_clips_dir(
    clips_dir: &Path,
    preferred_label: Option<&str>,
    resolver: Option<&dyn UsbResolver>,
) -> (PathBuf, String) {
    if let (Some(label), Some(r)) = (preferred_label, resolver) {
        if let Some(mount_relpath) = r.lookup(label) {
            return (clips_dir.join(mount_relpath), format!("usb-{label}"));
        }
    }
    (clips_dir.to_path_buf(), "local".to_string())
}

// ---------------------------------------------------------------------------
// StubClipRecorder
// ---------------------------------------------------------------------------

/// Recorder that writes 0-byte placeholder files and the matching
/// `motion_clips` row.
///
/// Behaviour matches [`ClipRecorder`] exactly so the rest of Stage A
/// (motion-event emitter, watermark sampler, eviction loop, retention
/// sweeper, API) can be wired + tested end-to-end without GStreamer.
/// The on-disk file is created so the safety floor's
/// `oldest_clip.path` unlink + the orphan-file scan both have a real
/// inode to work against.
pub struct StubClipRecorder {
    store: Arc<Store>,
    clips_dir: PathBuf,
    panic: parking_lot::RwLock<bool>,
    /// Tracks open clips so [`Self::close`] knows which `started_at` /
    /// `path` belongs to a handle without doing a second store
    /// roundtrip. Keyed by `clip_id`.
    open: Mutex<std::collections::HashMap<ClipId, OpenState>>,
    /// M2.2: optional bus for publishing `CLIP_CLOSED`. Stub clips
    /// are 0-byte so the cold replicator skips them anyway (the
    /// `mark_cold_replicated` path requires a non-NULL sha256), but
    /// we publish the event so integration tests covering the
    /// replicator wiring can use the stub recorder as a stand-in
    /// for the gst pipeline.
    bus: Option<Arc<dyn nexus_bus::Bus>>,
    /// M2.2 Phase 3: optional USB resolver + preferred label. When
    /// both are set and the resolver reports the label as attached
    /// at `open()` time, the new clip is routed under the USB
    /// volume's mount path and stamped with `hot_handle = "usb-<label>"`.
    /// In-flight clips never migrate — the choice is locked in
    /// at `open()` and recorded in [`OpenState`].
    ///
    /// `preferred_usb_label` is a [`PreferredUsbLabel`] handle
    /// rather than a plain `Option<String>` so the admin API can
    /// flip the label at runtime without restarting the engine
    /// (M2.2 Phase 3 closeout). A `Default` handle (label `None`)
    /// is functionally identical to the old static-`None` state.
    usb_resolver: Option<Arc<dyn UsbResolver>>,
    preferred_usb_label: PreferredUsbLabel,
}

#[derive(Debug, Clone)]
struct OpenState {
    camera_id: CameraId,
    started_at: DateTime<Utc>,
    path: PathBuf,
    /// Directory the in-flight + final files live under. Equal to
    /// `clips_dir` for `local` clips and to `clips_dir.join(mount_relpath)`
    /// for USB clips. Cached on `open()` so `close()` writes the
    /// final-name file in the same directory the partial was
    /// written to (in-flight clips finish where they started).
    effective_dir: PathBuf,
    /// Hot handle stamped on the row at open + repeated on the
    /// `CLIP_CLOSED` bus event. `"local"` or `"usb-<label>"`.
    hot_handle: String,
}

impl StubClipRecorder {
    pub fn new(store: Arc<Store>, clips_dir: impl AsRef<Path>) -> Self {
        Self {
            store,
            clips_dir: clips_dir.as_ref().to_path_buf(),
            panic: parking_lot::RwLock::new(false),
            open: Mutex::new(std::collections::HashMap::new()),
            bus: None,
            usb_resolver: None,
            preferred_usb_label: PreferredUsbLabel::default(),
        }
    }

    /// Attach a bus so the stub recorder publishes
    /// `topic::CLIP_CLOSED` on every successful close. Builder
    /// pattern keeps existing callsites that don't pass a bus
    /// working unchanged.
    pub fn with_bus(mut self, bus: Arc<dyn nexus_bus::Bus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// M2.2 Phase 3: attach a USB resolver + preferred-label so
    /// new clips can be routed to a hot-tier USB volume. Both
    /// arguments are required together — passing one without the
    /// other is a no-op. Builder pattern keeps existing callsites
    /// (and the gst-disabled fallback in `nexus-engine::main`)
    /// working unchanged.
    ///
    /// `preferred_label` accepts anything that converts into a
    /// [`PreferredUsbLabel`] — the engine passes a shared handle
    /// it also gave to the admin API; tests pass a bare
    /// `Some("X".to_string())` and get a fresh per-test handle
    /// via the `From<Option<String>>` impl.
    pub fn with_usb(
        mut self,
        resolver: Arc<dyn UsbResolver>,
        preferred_label: impl Into<PreferredUsbLabel>,
    ) -> Self {
        self.usb_resolver = Some(resolver);
        self.preferred_usb_label = preferred_label.into();
        self
    }

    /// Path the recorder would write for `(camera_id, started_at)`
    /// **on the local hot tier**. Per M2.1 spec layout — see
    /// [`inflight_clip_path`]. Stage A stub uses the same in-flight
    /// name as the GStreamer recorder so the rename-on-close logic
    /// exercises the same path.
    ///
    /// Note: when USB routing is active this is *not* the path the
    /// next clip will actually use; the USB-resolved directory is
    /// computed inside `open()` and cached on the `OpenState`.
    pub fn clip_path(&self, camera_id: CameraId, started_at: DateTime<Utc>) -> PathBuf {
        inflight_clip_path(&self.clips_dir, camera_id, started_at)
    }
}

#[async_trait]
impl ClipRecorder for StubClipRecorder {
    async fn open(&self, args: OpenClip) -> Result<ClipHandle, RecorderError> {
        if *self.panic.read() {
            return Err(RecorderError::Refused);
        }

        // Resolve the hot tier (local vs. USB) at open time. The
        // choice is locked into `OpenState` — close() will use the
        // same `effective_dir` even if the USB volume detaches
        // mid-recording. `preferred_usb_label.get()` reads the
        // *current* admin-supplied label; a concurrent PUT can
        // change it but won't affect any clip already open.
        let preferred = self.preferred_usb_label.get();
        let (effective_dir, hot_handle) = effective_clips_dir(
            &self.clips_dir,
            preferred.as_deref(),
            self.usb_resolver.as_deref().map(|r| r as &dyn UsbResolver),
        );

        let path = inflight_clip_path(&effective_dir, args.camera_id, args.started_at);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        // Touch a 0-byte file so the eviction unlink + orphan-file scan
        // both have a real inode to work against.
        fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .await?;

        // Path stored in DB is always relative to `clips_dir` (the
        // top-level), so a USB-rooted clip looks like
        // `usb/NEXUS_VAULT/<camera>/<date>/<file>.partial.mp4`. Reads
        // resolve via `clips_dir.join(rel)` exactly as before.
        let rel = clip_rel_path(&self.clips_dir, &path);

        let new = NewClip {
            camera_id: args.camera_id,
            started_at: args.started_at,
            hot_path: rel,
            codec: "stub".into(),
            container: "mp4".into(),
            hot_handle: hot_handle.clone(),
            frame_width: args.frame_width,
            frame_height: args.frame_height,
        };
        let clip_id = self.store.open_clip(&new).await?;

        self.open.lock().await.insert(
            clip_id,
            OpenState {
                camera_id: args.camera_id,
                started_at: args.started_at,
                path,
                effective_dir,
                hot_handle,
            },
        );
        debug!(
            camera_id = args.camera_id,
            clip_id, "stub recorder opened clip"
        );
        Ok(ClipHandle {
            clip_id,
            camera_id: args.camera_id,
        })
    }

    async fn close(&self, handle: ClipHandle, args: ClipFinal) -> Result<ClipMeta, RecorderError> {
        let state = self
            .open
            .lock()
            .await
            .remove(&handle.clip_id)
            .ok_or(RecorderError::UnknownClip(handle.clip_id))?;

        let duration_ms = (args.ended_at - state.started_at).num_milliseconds().max(0);

        // M2.1 spec: discard sub-3s clips. The stub never has live
        // bytes so the file is always 0 bytes; we still go through
        // the cascade-delete + unlink path so the test harness can
        // verify the discard is wired identically to the gst
        // recorder.
        if duration_ms < MIN_CLIP_DURATION_MS {
            warn!(
                camera_id = state.camera_id,
                clip_id = handle.clip_id,
                duration_ms,
                min_ms = MIN_CLIP_DURATION_MS,
                "stub recorder: clip too short -- discarding (delete file + cascade-delete metadata)"
            );
            if let Err(e) = fs::remove_file(&state.path).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        clip_id = handle.clip_id,
                        path = %state.path.display(),
                        error = %e,
                        "stub recorder: failed to unlink discarded short clip"
                    );
                }
            }
            self.store
                .cascade_delete_clip_metadata(handle.clip_id)
                .await?;
            return Ok(ClipMeta {
                clip_id: handle.clip_id,
                camera_id: state.camera_id,
                path: state.path,
                duration_ms,
                size_bytes: 0,
                codec: "stub".into(),
                container: "mp4".into(),
                discarded: true,
            });
        }

        // Normal close path: rename the in-flight file to the final
        // `{start_ms}_{duration_ms}.mp4` shape, stat for size, stamp
        // the row.
        let final_abs = final_clip_path(
            &state.effective_dir,
            state.camera_id,
            state.started_at,
            duration_ms,
        );
        if let Err(e) = fs::rename(&state.path, &final_abs).await {
            warn!(
                clip_id = handle.clip_id,
                from = %state.path.display(),
                to   = %final_abs.display(),
                error = %e,
                "stub recorder: rename to final path failed; row will keep in-flight path"
            );
        }
        // After rename, the file lives at final_abs whether or not
        // the rename succeeded (if it failed, state.path still
        // points at the old name and we keep it).
        let final_used = if final_abs.exists() {
            final_abs
        } else {
            state.path.clone()
        };
        let rel = clip_rel_path(&self.clips_dir, &final_used);
        let size_bytes = match fs::metadata(&final_used).await {
            Ok(meta) => meta.len() as i64,
            Err(e) => {
                warn!(
                    error = %e,
                    path = %final_used.display(),
                    "stub recorder could not stat clip; recording size_bytes=0"
                );
                0
            }
        };

        // Snapshot for the post-close bus publish (rel is moved into
        // ClipClose below).
        let rel_for_event = rel.clone();

        self.store
            .close_clip(
                handle.clip_id,
                &ClipClose {
                    ended_at: args.ended_at,
                    duration_ms,
                    size_bytes,
                    hot_path: Some(rel),
                    // Stub recorder skips the streaming hash —
                    // its 0-byte output is meaningless to replicate.
                    sha256: None,
                },
            )
            .await?;

        debug!(
            camera_id = state.camera_id,
            clip_id = handle.clip_id,
            duration_ms,
            "stub recorder closed clip"
        );

        // M2.2: best-effort CLIP_CLOSED publish; sha256 is null so
        // the cold replicator will skip until/unless an operator
        // opts into a one-shot rehash sweep.
        if let Some(bus) = &self.bus {
            let payload = serde_json::json!({
                "clip_id": handle.clip_id,
                "camera_id": state.camera_id,
                "hot_handle": state.hot_handle,
                "hot_path": rel_for_event,
                "size_bytes": size_bytes,
                "sha256": serde_json::Value::Null,
            });
            use nexus_bus::BusExt;
            if let Err(e) = bus.publish(nexus_bus::topic::CLIP_CLOSED, &payload).await {
                debug!(
                    clip_id = handle.clip_id,
                    error = %e,
                    "stub recorder: publish CLIP_CLOSED failed (replicator will catch up)"
                );
            }
        }
        Ok(ClipMeta {
            clip_id: handle.clip_id,
            camera_id: state.camera_id,
            path: final_used,
            duration_ms,
            size_bytes,
            codec: "stub".into(),
            container: "mp4".into(),
            discarded: false,
        })
    }

    fn set_panic(&self, panic: bool) {
        let mut guard = self.panic.write();
        if *guard != panic {
            tracing::info!(panic, "stub recorder panic flag changed");
        }
        *guard = panic;
    }

    fn is_panic(&self) -> bool {
        *self.panic.read()
    }

    fn kind(&self) -> &'static str {
        "stub"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nexus_config::{CameraConfig, StoreConfig};
    use nexus_store::Store;
    use std::path::PathBuf;
    use url::Url;

    async fn fresh_store_and_dir() -> (Arc<Store>, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let db_path = dir.path().join("nexus.db");
        let cfg = StoreConfig {
            url: format!("sqlite:{}?mode=rwc", db_path.display()),
            seed_from_config: false,
            duckdb_attach: false,
            duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
        };
        let store = Arc::new(Store::open(&cfg).await.expect("store"));
        store
            .upsert_camera(&CameraConfig {
                id: 1,
                name: "front".into(),
                ingest: nexus_config::CameraIngest {
                    url: Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                    enabled: true,
                    max_fps: 0,
                    codec: None,
                },
                detector: nexus_config::CameraDetector {
                    prompts: vec![],
                    visual_prompts: vec![],
                    model_override: None,
                },
                behavior: nexus_config::CameraBehavior {
                    parking_lot_mode: false,
                    anchor_ttl_secs: None,
                },
                zones: vec![],
            })
            .await
            .unwrap();
        let clips_dir = dir.path().join("clips");
        (store, dir, clips_dir)
    }

    #[tokio::test]
    async fn stub_recorder_open_close_round_trip() {
        let (store, _dir, clips_dir) = fresh_store_and_dir().await;
        let rec = StubClipRecorder::new(store.clone(), &clips_dir);

        let started = Utc::now();
        let handle = rec
            .open(OpenClip {
                camera_id: 1,
                started_at: started,
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();
        assert_eq!(rec.kind(), "stub");

        // File exists on disk + clip row exists in the DB.
        let row = store.get_clip(handle.clip_id).await.unwrap().unwrap();
        let hot_path = row.hot_path.as_deref().expect("in-flight clip is hot");
        let path_on_disk = clips_dir.join(hot_path);
        assert!(path_on_disk.exists(), "stub clip file should be created");
        assert!(
            hot_path.ends_with(".partial.mp4"),
            "in-flight clip should use the spec'd `.partial.mp4` suffix; got {hot_path}"
        );
        assert_eq!(row.codec, "stub");
        assert_eq!(row.hot_handle.as_deref(), Some("local"));
        assert!(
            row.cold_handle.is_none(),
            "new clip must be hot-only; cold filled by replicator"
        );
        assert!(row.ended_at.is_none(), "ended_at unset until close");

        let ended = started + chrono::Duration::seconds(7);
        let meta = rec
            .close(handle, ClipFinal { ended_at: ended })
            .await
            .unwrap();
        assert_eq!(meta.duration_ms, 7_000);
        assert_eq!(meta.size_bytes, 0); // touch-only file
        assert!(!meta.discarded, "7s clip must NOT be discarded");

        let row_closed = store.get_clip(handle.clip_id).await.unwrap().unwrap();
        assert_eq!(row_closed.duration_ms, 7_000);
        assert_eq!(row_closed.size_bytes, 0);
        assert!(row_closed.ended_at.is_some());
        let closed_hot = row_closed
            .hot_path
            .as_deref()
            .expect("closed clip is still hot");
        // The DB row's hot_path should be the FINAL renamed path
        // (`{start_ms}_{duration_ms}.mp4`), not the in-flight name.
        assert!(
            closed_hot.ends_with("_7000.mp4"),
            "closed clip should be renamed to the spec layout; got {closed_hot}"
        );
        assert!(
            !closed_hot.contains(".partial."),
            "closed clip path must not retain `.partial.`: {closed_hot}"
        );
        // The renamed file must exist on disk; the in-flight name must not.
        assert!(
            clips_dir.join(closed_hot).exists(),
            "renamed clip file should exist on disk"
        );
    }

    #[tokio::test]
    async fn stub_recorder_panic_mode_refuses_open_but_allows_close() {
        let (store, _dir, clips_dir) = fresh_store_and_dir().await;
        let rec = StubClipRecorder::new(store.clone(), &clips_dir);

        // Open one clip BEFORE panic so we have an in-flight handle.
        // Use 5s so the close stays above MIN_CLIP_DURATION_MS and
        // exercises the normal stamp path, not the discard path.
        let started = Utc::now();
        let handle = rec
            .open(OpenClip {
                camera_id: 1,
                started_at: started,
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();

        // Flip panic. New opens must be refused.
        rec.set_panic(true);
        assert!(rec.is_panic());
        let refused = rec
            .open(OpenClip {
                camera_id: 1,
                started_at: started + chrono::Duration::seconds(1),
                frame_width: 960,
                frame_height: 540,
            })
            .await;
        assert!(matches!(refused, Err(RecorderError::Refused)));

        // Existing handle MUST still close cleanly so the in-flight
        // clip's row gets ended_at + size stamped.
        let meta = rec
            .close(
                handle,
                ClipFinal {
                    ended_at: started + chrono::Duration::seconds(5),
                },
            )
            .await
            .unwrap();
        assert_eq!(meta.duration_ms, 5_000);
        assert!(!meta.discarded, "5s clip must not be discarded");

        // Clearing panic re-enables opens.
        rec.set_panic(false);
        assert!(!rec.is_panic());
        let _h2 = rec
            .open(OpenClip {
                camera_id: 1,
                started_at: started + chrono::Duration::seconds(6),
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stub_recorder_discards_sub_min_clips() {
        // M2.1 spec: clips shorter than MIN_CLIP_DURATION_MS are
        // unlinked + cascade-deleted at close. Caller observes
        // `meta.discarded == true`; the row + file are gone.
        let (store, _dir, clips_dir) = fresh_store_and_dir().await;
        let rec = StubClipRecorder::new(store.clone(), &clips_dir);

        let started = Utc::now();
        let handle = rec
            .open(OpenClip {
                camera_id: 1,
                started_at: started,
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();
        // Sanity: the file + row exist before close.
        assert!(store.get_clip(handle.clip_id).await.unwrap().is_some());

        // Close at start + 1s — well below MIN_CLIP_DURATION_MS=3000.
        let meta = rec
            .close(
                handle,
                ClipFinal {
                    ended_at: started + chrono::Duration::seconds(1),
                },
            )
            .await
            .unwrap();
        assert!(meta.discarded, "1s clip must be discarded");
        assert_eq!(meta.duration_ms, 1_000);

        // Row is GONE (cascade-deleted).
        assert!(
            store.get_clip(handle.clip_id).await.unwrap().is_none(),
            "discarded clip's metadata row must be cascade-deleted"
        );
        // File is GONE (unlinked).
        assert!(
            !meta.path.exists(),
            "discarded clip's file must be unlinked"
        );
    }

    #[tokio::test]
    async fn stub_recorder_close_unknown_handle_errors() {
        let (store, _dir, clips_dir) = fresh_store_and_dir().await;
        let rec = StubClipRecorder::new(store, &clips_dir);
        let bogus = ClipHandle {
            clip_id: 9999,
            camera_id: 1,
        };
        let res = rec
            .close(
                bogus,
                ClipFinal {
                    ended_at: Utc::now(),
                },
            )
            .await;
        assert!(matches!(res, Err(RecorderError::UnknownClip(9999))));
    }

    #[test]
    fn clip_path_is_stable_and_per_camera() {
        // Doesn't need the store, just exercises the path layout.
        let dummy_store = {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let cfg = StoreConfig {
                    url: format!("sqlite:{}?mode=rwc", dir.path().join("x.db").display()),
                    seed_from_config: false,
                    duckdb_attach: false,
                    duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
                };
                let s = Arc::new(Store::open(&cfg).await.unwrap());
                // Leak the tempdir so the file path stays valid for the
                // assertions that follow.
                std::mem::forget(dir);
                s
            })
        };
        let rec = StubClipRecorder::new(dummy_store, "/var/lib/nexus/clips");
        let t = chrono::DateTime::parse_from_rfc3339("2026-05-13T22:00:00.123Z")
            .unwrap()
            .with_timezone(&Utc);
        let p1 = rec.clip_path(1, t);
        let p2 = rec.clip_path(2, t);
        let s1 = p1.to_string_lossy().to_string();
        let s2 = p2.to_string_lossy().to_string();
        // Spec layout: {clips_dir}/{camera_id}/{YYYY-MM-DD}/{start_unix_ms}.partial.mp4
        assert!(s1.contains("/1/"), "missing camera_id segment: {s1}");
        assert!(s2.contains("/2/"), "missing camera_id segment: {s2}");
        assert!(s1.contains("/2026-05-13/"), "missing UTC date dir: {s1}");
        let expected_ms = t.timestamp_millis();
        assert!(
            s1.ends_with(&format!("{expected_ms}.partial.mp4")),
            "path tail wrong: {s1}"
        );
    }

    // ---- M2.2 Phase 3 — USB hot-plug routing ----

    /// Test fake for the [`UsbResolver`] trait. Tests mutate
    /// `attached` directly to simulate the `usb_watch` task seeing
    /// a volume appear or disappear between two recorder opens.
    #[derive(Default)]
    struct FakeUsbResolver {
        attached: parking_lot::RwLock<std::collections::HashMap<String, PathBuf>>,
    }

    impl FakeUsbResolver {
        fn attach(&self, label: &str, mount_relpath: PathBuf) {
            self.attached
                .write()
                .insert(label.to_string(), mount_relpath);
        }
        fn detach(&self, label: &str) {
            self.attached.write().remove(label);
        }
    }

    impl UsbResolver for FakeUsbResolver {
        fn lookup(&self, label: &str) -> Option<PathBuf> {
            self.attached.read().get(label).cloned()
        }
    }

    #[test]
    fn effective_clips_dir_truth_table() {
        let clips_dir = PathBuf::from("/var/lib/nexus/clips");
        let resolver = FakeUsbResolver::default();
        resolver.attach("NEXUS_VAULT", PathBuf::from("usb/NEXUS_VAULT"));

        // No preferred label → always local.
        assert_eq!(
            effective_clips_dir(&clips_dir, None, Some(&resolver)),
            (clips_dir.clone(), "local".to_string())
        );

        // Preferred label but no resolver → local (recorder built
        // without USB support, e.g. test harness).
        assert_eq!(
            effective_clips_dir(&clips_dir, Some("NEXUS_VAULT"), None),
            (clips_dir.clone(), "local".to_string())
        );

        // Preferred label set, resolver doesn't know about it →
        // local (volume not currently attached).
        assert_eq!(
            effective_clips_dir(&clips_dir, Some("NEXUS_OTHER"), Some(&resolver)),
            (clips_dir.clone(), "local".to_string())
        );

        // Preferred label set + attached → USB.
        let (effective, handle) =
            effective_clips_dir(&clips_dir, Some("NEXUS_VAULT"), Some(&resolver));
        assert_eq!(effective, clips_dir.join("usb/NEXUS_VAULT"));
        assert_eq!(handle, "usb-NEXUS_VAULT");
    }

    #[tokio::test]
    async fn stub_recorder_routes_to_usb_after_attach() {
        let (store, _dir, clips_dir) = fresh_store_and_dir().await;
        // Seed the storage_backends row the schema FK demands.
        // In production the `usb_watch` task does this when the
        // volume first appears; here we do it inline so the test
        // doesn't need to spin up the watcher.
        store
            .upsert_storage_backend("usb-NEXUS_VAULT", "usb", "{}")
            .await
            .unwrap();
        let resolver = Arc::new(FakeUsbResolver::default());
        let rec = StubClipRecorder::new(store.clone(), &clips_dir)
            .with_usb(resolver.clone(), Some("NEXUS_VAULT".to_string()));

        // Open #1: volume not yet attached → must land local.
        let t1 = Utc::now();
        let h1 = rec
            .open(OpenClip {
                camera_id: 1,
                started_at: t1,
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();
        let row1 = store.get_clip(h1.clip_id).await.unwrap().unwrap();
        assert_eq!(
            row1.hot_handle.as_deref(),
            Some("local"),
            "with no USB attached, hot_handle must be `local`"
        );
        let hot_path1 = row1.hot_path.as_deref().unwrap();
        assert!(
            !hot_path1.starts_with("usb/"),
            "local clip path must not start with usb/: {hot_path1}"
        );

        // Mid-stream: volume appears.
        resolver.attach("NEXUS_VAULT", PathBuf::from("usb/NEXUS_VAULT"));

        // Open #2: routed to USB.
        let t2 = t1 + chrono::Duration::seconds(1);
        let h2 = rec
            .open(OpenClip {
                camera_id: 1,
                started_at: t2,
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();
        let row2 = store.get_clip(h2.clip_id).await.unwrap().unwrap();
        assert_eq!(
            row2.hot_handle.as_deref(),
            Some("usb-NEXUS_VAULT"),
            "newly-attached USB volume must be picked up on the next open"
        );
        let hot_path2 = row2.hot_path.as_deref().unwrap();
        assert!(
            hot_path2.starts_with("usb/NEXUS_VAULT/"),
            "USB clip path must be relative to clips_dir under the mount: {hot_path2}"
        );
        // The on-disk file must exist under the USB-mount subdir
        // of clips_dir, NOT directly under clips_dir.
        assert!(
            clips_dir.join(hot_path2).exists(),
            "USB clip file should live under {:?}",
            clips_dir.join(hot_path2)
        );

        // Detach mid-recording must NOT migrate the in-flight clip.
        // close() must finish at the same path open() chose.
        resolver.detach("NEXUS_VAULT");
        let ended = t2 + chrono::Duration::seconds(5);
        let meta2 = rec.close(h2, ClipFinal { ended_at: ended }).await.unwrap();
        assert!(!meta2.discarded);
        let row2_closed = store.get_clip(h2.clip_id).await.unwrap().unwrap();
        let closed_hot = row2_closed.hot_path.as_deref().unwrap();
        assert!(
            closed_hot.starts_with("usb/NEXUS_VAULT/"),
            "in-flight USB clip must finish on USB even after detach: {closed_hot}"
        );
        assert_eq!(
            row2_closed.hot_handle.as_deref(),
            Some("usb-NEXUS_VAULT"),
            "hot_handle must NOT change at close time"
        );
        // And after detach, a brand-new open must fall back to local.
        let t3 = ended + chrono::Duration::seconds(1);
        let h3 = rec
            .open(OpenClip {
                camera_id: 1,
                started_at: t3,
                frame_width: 960,
                frame_height: 540,
            })
            .await
            .unwrap();
        let row3 = store.get_clip(h3.clip_id).await.unwrap().unwrap();
        assert_eq!(row3.hot_handle.as_deref(), Some("local"));
    }
}

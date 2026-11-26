//! SPEC-088 — which zone revision each camera's running supervisor applied.
//!
//! A revision is the SHA-256 of a camera's validated, ordered zone list. Each
//! save, reconcile pass and read tells the registry the revision saved now
//! and whether the camera is enabled ([`ZoneApplicationRegistry::request`]);
//! every change of either is a new request with a new [`RequestToken`], A→B→A
//! included, in commit order ([`ZoneApplicationRegistry::saves`]). Each supervisor start is a new generation
//! ([`ZoneApplicationRegistry::start`]) running the zones it froze. Before
//! each frame it takes the current request's token
//! ([`ZoneApplicationRegistry::current_request`]), which it gets only while it
//! runs the saved revision, and once the frame has gone through the zone
//! filters and everything downstream of them it acknowledges that token
//! ([`ZoneApplicationRegistry::acknowledge`]). Applied means the current
//! generation acknowledged the current request: a newer request, a stop or a
//! start takes that away whatever the digests say, and a callback carrying an
//! older token or generation counts for nothing. A disabled camera is
//! inactive, with no deadline. In memory only, so a process boot has applied
//! nothing; only a floor under the next boot's generations is persisted
//! ([`ZoneApplicationRegistry::load_generation_floor`]). An acknowledgement
//! says one frame was processed with the zones, not that analysis stays
//! healthy: that is the frame stats' job.
//!
//! SPEC-077's `parking_runtime` is a second registry, fed the
//! [`parking_revision`] of the same cameras at the same points, with its own
//! generations and floor: a zone acknowledgement never certifies parking.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use nexus_config::{CameraConfig, ZoneConfig};
use nexus_types::CameraId;
use parking_lot::Mutex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::warn;

/// How long an enabled camera has, from its latest request, start or stop,
/// to acknowledge its zones before it reads `failed` with
/// [`ZoneApplicationError::ApplicationTimeout`].
pub const APPLICATION_TIMEOUT: Duration = Duration::from_secs(30);

/// `sha256:` and the lowercase hex digest of `zones` as JSON, in saved order
/// and with every field the supervisor reads. Only the zones go in, so nothing
/// else about the camera, its credentials included, moves it or leaks into it.
/// A -0.0 vertex hashes as 0.0: the reconciler's key compares them equal, so
/// a save flipping that sign restarts nothing and must not change the revision.
#[expect(
    clippy::expect_used,
    reason = "a ZoneConfig is strings, floats, a unit enum and an Option<u32>, which always serialize to JSON"
)]
pub fn zone_revision(zones: &[ZoneConfig]) -> String {
    let mut zones = zones.to_vec();
    for (x, y) in zones.iter_mut().flat_map(|z| z.polygon.iter_mut()) {
        // -0.0 + 0.0 is 0.0; every other value is unchanged.
        *x += 0.0;
        *y += 0.0;
    }
    let json = serde_json::to_vec(&zones).expect("zones serialize to JSON");
    format!("sha256:{:x}", Sha256::digest(json))
}

/// SPEC-077 — `sha256:` and the lowercase hex digest of the parking tuple a
/// supervisor runs, as the JSON array `[zones, parking_lot_mode,
/// parking_policy_v2]`: the zones in saved order, and the whole optional
/// policy, `null` when absent. Nothing else about the camera moves it, its
/// credentials included, and it never equals a [`zone_revision`].
#[expect(
    clippy::expect_used,
    reason = "zones, a bool and an Option of four f64 always serialize to JSON"
)]
pub fn parking_revision(camera: &CameraConfig) -> String {
    let tuple = (
        &camera.zones,
        camera.behavior.parking_lot_mode,
        camera.behavior.parking_policy_v2,
    );
    let json = serde_json::to_vec(&tuple).expect("the parking tuple serializes to JSON");
    format!("sha256:{:x}", Sha256::digest(json))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneApplicationState {
    /// The saved camera is disabled: its zones apply when it is enabled.
    Inactive,
    Applying,
    Applied,
    Failed,
}

/// Why the running generation has not applied its zones. Bounded, so a reader
/// can know every value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneApplicationError {
    /// No frame finished processing within [`APPLICATION_TIMEOUT`].
    ApplicationTimeout,
    /// The supervisor ended, unasked, before any frame finished processing.
    SupervisorExited,
}

/// One request: one change of a camera's saved zones or enabled flag, as
/// this process saw it. Boot-scoped, like every acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestToken(u64);

/// SPEC-088's `zones_runtime`. Every key is always serialized, `null` included.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZonesRuntime {
    pub schema_version: u32,
    pub desired_revision: String,
    /// The last revision any generation of this camera acknowledged, and
    /// that generation: kept after it stops or a newer request comes, for
    /// comparison only. Equal digests here never mean applied on their own.
    pub applied_revision: Option<String>,
    /// The current generation. Before the camera's first start in this
    /// process, the newest one handed out when it was first requested: above
    /// every earlier boot's, and held by no supervisor of this camera.
    pub generation: u64,
    pub applied_generation: Option<u64>,
    pub state: ZoneApplicationState,
    pub applied_at: Option<DateTime<Utc>>,
    pub error_code: Option<ZoneApplicationError>,
}

struct Entry {
    /// The latest request: the revision saved now, and whether the camera is
    /// enabled.
    desired: String,
    enabled: bool,
    request: RequestToken,
    generation: u64,
    /// The revision the current generation started with; `None` before the
    /// camera's first start and once it began stopping, when nothing it
    /// reports counts any more.
    running: Option<String>,
    /// Monotonic, so a wall-clock step cannot time out early: when the
    /// latest request, start or stop began the deadline.
    since: Instant,
    /// The request the current generation acknowledged.
    acknowledged: Option<RequestToken>,
    /// Why the current generation cannot apply the current request.
    failed: Option<ZoneApplicationError>,
    /// The last acknowledgement: revision, generation and time.
    applied: Option<(String, u64, DateTime<Utc>)>,
}

impl Entry {
    /// The current request's token, while `generation` is the current one
    /// and runs the saved revision of an enabled camera.
    fn current(&self, generation: u64) -> Option<RequestToken> {
        (self.enabled
            && self.generation == generation
            && self.running.as_deref() == Some(self.desired.as_str()))
        .then_some(self.request)
    }

    /// Nothing reported so far counts, and the deadline runs from `now`.
    fn reset(&mut self, now: Instant) {
        self.since = now;
        self.acknowledged = None;
        self.failed = None;
    }
}

struct Inner {
    /// The last generation handed out, process-wide, so a number names one
    /// supervisor even across a camera's deletion and re-creation.
    last_generation: u64,
    /// The floor file and the highest generation it covers: none above that
    /// is handed out before a higher floor is written, or failed to be
    /// ([`reserve`]). `None` until
    /// [`ZoneApplicationRegistry::load_generation_floor`].
    floor: Option<(PathBuf, u64)>,
    last_request: u64,
    cameras: HashMap<CameraId, Entry>,
}

impl Inner {
    /// `camera_id`'s entry, with `revision` and `enabled` as its latest
    /// request: created if missing, and a new request if either changed.
    fn request(
        &mut self,
        camera_id: CameraId,
        revision: &str,
        enabled: bool,
        now: Instant,
    ) -> &mut Entry {
        let Inner {
            last_generation,
            last_request,
            cameras,
            ..
        } = self;
        let mut new = false;
        let e = cameras.entry(camera_id).or_insert_with(|| {
            new = true;
            Entry {
                desired: String::new(),
                enabled,
                request: RequestToken(0),
                generation: *last_generation,
                running: None,
                since: now,
                acknowledged: None,
                failed: None,
                applied: None,
            }
        });
        if new || e.desired != revision || e.enabled != enabled {
            *last_request += 1;
            e.request = RequestToken(*last_request);
            revision.clone_into(&mut e.desired);
            e.enabled = enabled;
            e.reset(now);
        }
        e
    }
}

/// The next generation, persisting a floor a block higher first when it
/// would pass the reserved one.
fn next_generation(last: &mut u64, floor: &mut Option<(PathBuf, u64)>) -> u64 {
    *last += 1;
    if let Some((path, reserved)) = floor.as_mut().filter(|(_, r)| *last > *r) {
        reserve(path, reserved, *last - 1 + FLOOR_BLOCK);
    }
    *last
}

/// The highest boot seed: 2^32 starts below 2^53 − 1, the largest integer a
/// JSON reader holds exactly. Unix-epoch ms × 1000 reaches it in 2255.
const MAX_BOOT_SEED: u64 = (1 << 53) - (1 << 32);

/// Generations reserved on disk at a time: one write per this many starts,
/// and small enough that a restart loop (5 s apart) never outruns the clock.
const FLOOR_BLOCK: u64 = 1 << 20;

/// Write `floor` to `path` through a synced temp file, a rename and a synced
/// directory, so a crash or power loss leaves the old floor or the new one.
fn write_floor(path: &Path, floor: u64) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    // A fresh install's state directory may not exist yet: bootstrap creates
    // it only for an admin secret it generates.
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(format!("{floor}\n").as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    match dir {
        Some(dir) => std::fs::File::open(dir)?.sync_all(),
        None => Ok(()),
    }
}

/// Persist `floor` to `path`, and only then record it as `reserved`. On a
/// failure the previous reservation stays, so the next start tries again,
/// and this process keeps handing out rising generations (fail open, R6):
/// the floor only guards the next boot.
fn reserve(path: &Path, reserved: &mut u64, floor: u64) {
    match write_floor(path, floor) {
        Ok(()) => *reserved = floor,
        Err(e) => warn!(path = %path.display(), error = %e,
            "could not persist the zone generation floor; a restart with the clock set back may read stale"),
    }
}

pub struct ZoneApplicationRegistry {
    inner: Mutex<Inner>,
    saves: tokio::sync::RwLock<()>,
}

impl Default for ZoneApplicationRegistry {
    fn default() -> Self {
        Self::booted_at(SystemTime::now())
    }
}

impl ZoneApplicationRegistry {
    /// A registry whose generations count up from `boot` in unix-epoch
    /// milliseconds × 1000, so a reader that saw an earlier process's
    /// generations sees this one's as newer (that process started at most
    /// 1000 supervisors per millisecond it ran). Without a floor
    /// ([`Self::load_generation_floor`]) this assumes the wall clock does not
    /// step back across a restart; if it does, the reader takes the lower
    /// generation for stale and shows applying until it reloads (SPEC-088).
    fn booted_at(boot: SystemTime) -> Self {
        let ms = boot.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let seed =
            u64::try_from(ms.saturating_mul(1000)).map_or(MAX_BOOT_SEED, |s| s.min(MAX_BOOT_SEED));
        Self {
            inner: Mutex::new(Inner {
                last_generation: seed,
                floor: None,
                last_request: 0,
                cameras: HashMap::new(),
            }),
            saves: tokio::sync::RwLock::new(()),
        }
    }

    /// Every writer of a camera row (a whole row carries the zones and the
    /// enabled flag) holds this for writing from before it reads what it
    /// writes back, or before its commit, until it has told [`Self::request`]
    /// (or [`Self::remove`]); readers that request what they read hold it for
    /// reading from before the read. So the registry sees every saved change,
    /// in commit order, and no stale read after it: a save racing another
    /// cannot find its change already requested by an earlier one.
    pub fn saves(&self) -> &tokio::sync::RwLock<()> {
        &self.saves
    }

    /// Keep generations above every earlier boot's, whatever the wall clock
    /// did: start above the floor in `floor_file` (or the clock seed, if
    /// higher), and persist a new floor a block higher, its directory created
    /// if missing, before handing out any generation; until one is persisted,
    /// every start tries again. Call before the first [`Self::start`]. A
    /// missing or corrupt file falls back to the clock seed and is rewritten.
    /// Remaining assumptions: the file survives restarts, or the clock does
    /// not step back across them ([`Self::booted_at`]); and the floor stays
    /// below the JSON-safe seed cap, which takes billions of restarts or a
    /// clock past 2255.
    pub fn load_generation_floor(&self, floor_file: PathBuf) {
        let mut inner = self.inner.lock();
        // What the file reserves: nothing once it is missing or corrupt.
        let on_disk = match std::fs::read_to_string(&floor_file).map(|s| s.trim().parse::<u64>()) {
            Ok(Ok(floor)) => floor,
            Ok(Err(e)) => {
                warn!(path = %floor_file.display(), error = %e,
                    "zone generation floor is corrupt; seeding from the wall clock");
                0
            }
            Err(e) => {
                warn!(path = %floor_file.display(), error = %e,
                    "zone generation floor unreadable; seeding from the wall clock");
                0
            }
        };
        // Capped like the clock seed, so a generation stays JSON-safe.
        inner.last_generation = inner.last_generation.max(on_disk.min(MAX_BOOT_SEED));
        let mut reserved = on_disk;
        reserve(
            &floor_file,
            &mut reserved,
            inner.last_generation + FLOOR_BLOCK,
        );
        inner.floor = Some((floor_file, reserved));
    }

    /// The camera's zones were saved as `revision`, enabled or not. A change
    /// of either is a new request, which nothing acknowledged before
    /// satisfies; an identical one changes nothing.
    pub fn request(&self, camera_id: CameraId, revision: &str, enabled: bool) {
        self.inner
            .lock()
            .request(camera_id, revision, enabled, Instant::now());
    }

    /// Start a new supervisor generation for `camera_id`, running the zones
    /// whose revision is `revision`, and return its number: higher than every
    /// one handed out before in this process, and in earlier ones
    /// ([`Self::load_generation_floor`], [`Self::booted_at`]). Nothing is
    /// applied until it acknowledges; the last acknowledgement is kept for
    /// comparison.
    pub fn start(&self, camera_id: CameraId, revision: &str) -> u64 {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let generation = next_generation(&mut inner.last_generation, &mut inner.floor);
        let now = Instant::now();
        if !inner.cameras.contains_key(&camera_id) {
            // Started before any request was seen: it was started for these.
            inner.request(camera_id, revision, true, now);
        }
        if let Some(e) = inner.cameras.get_mut(&camera_id) {
            e.generation = generation;
            e.running = Some(revision.to_owned());
            e.reset(now);
        }
        generation
    }

    /// `generation` began stopping, asked or not: what it applied no longer
    /// runs, so the camera reads applying, or failed once
    /// [`APPLICATION_TIMEOUT`] has passed since now, until the next
    /// generation acknowledges. Ignored unless it is the camera's current
    /// generation.
    pub fn stop(&self, camera_id: CameraId, generation: u64) {
        if let Some(e) = self
            .inner
            .lock()
            .cameras
            .get_mut(&camera_id)
            .filter(|e| e.generation == generation && e.running.is_some())
        {
            e.running = None;
            e.reset(Instant::now());
        }
    }

    /// The token `generation` acknowledges a frame under, taken before the
    /// frame is processed: `None` unless it is the camera's current, running
    /// generation, the camera is enabled and it runs the saved revision.
    pub fn current_request(&self, camera_id: CameraId, generation: u64) -> Option<RequestToken> {
        self.inner
            .lock()
            .cameras
            .get(&camera_id)?
            .current(generation)
    }

    /// `generation` processed a frame with `revision` under `request`.
    /// Ignored unless that is still the current request of the current,
    /// running generation.
    pub fn acknowledge(
        &self,
        camera_id: CameraId,
        generation: u64,
        request: RequestToken,
        revision: &str,
        at: DateTime<Utc>,
    ) {
        let mut inner = self.inner.lock();
        if let Some(e) = inner
            .cameras
            .get_mut(&camera_id)
            .filter(|e| e.current(generation) == Some(request) && e.desired == revision)
        {
            e.acknowledged = Some(request);
            e.failed = None;
            e.applied = Some((revision.to_owned(), generation, at));
        }
    }

    /// `generation` cannot apply its zones under `request`. Ignored unless
    /// that is still the current request of the current, running generation.
    pub fn fail(
        &self,
        camera_id: CameraId,
        generation: u64,
        request: RequestToken,
        code: ZoneApplicationError,
    ) {
        let mut inner = self.inner.lock();
        if let Some(e) = inner
            .cameras
            .get_mut(&camera_id)
            .filter(|e| e.current(generation) == Some(request))
        {
            e.failed = Some(code);
        }
    }

    /// Whether `generation` is the camera's current generation and still
    /// running. A supervisor kept running across a reconcile pass must be,
    /// or nothing it processes can be acknowledged: a camera deleted and
    /// re-created before the pass has a new entry no supervisor started.
    pub fn runs(&self, camera_id: CameraId, generation: u64) -> bool {
        self.inner
            .lock()
            .cameras
            .get(&camera_id)
            .is_some_and(|e| e.generation == generation && e.running.is_some())
    }

    /// The camera was deleted: it has nothing left to report.
    pub fn remove(&self, camera_id: CameraId) {
        self.inner.lock().cameras.remove(&camera_id);
    }

    /// The camera's status at monotonic `now`, with `desired` the revision
    /// saved now and `enabled` its saved flag: both are first a
    /// [`Self::request`], so a read straight after a save already answers
    /// for it. Inactive when disabled; applied only when the current
    /// generation acknowledged the current request.
    pub fn snapshot(
        &self,
        camera_id: CameraId,
        desired: &str,
        enabled: bool,
        now: Instant,
    ) -> ZonesRuntime {
        let mut inner = self.inner.lock();
        let e = inner.request(camera_id, desired, enabled, now);
        let applied = e.acknowledged.is_some() && e.acknowledged == e.current(e.generation);
        let error_code = if !enabled || applied {
            None
        } else {
            e.failed.or(
                (now.saturating_duration_since(e.since) >= APPLICATION_TIMEOUT)
                    .then_some(ZoneApplicationError::ApplicationTimeout),
            )
        };
        ZonesRuntime {
            schema_version: 1,
            desired_revision: desired.to_owned(),
            applied_revision: e.applied.as_ref().map(|(r, _, _)| r.clone()),
            generation: e.generation,
            applied_generation: e.applied.as_ref().map(|(_, g, _)| *g),
            state: if !enabled {
                ZoneApplicationState::Inactive
            } else if applied {
                ZoneApplicationState::Applied
            } else if error_code.is_some() {
                ZoneApplicationState::Failed
            } else {
                ZoneApplicationState::Applying
            },
            applied_at: e.applied.as_ref().map(|(_, _, at)| *at),
            error_code,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use nexus_config::ZoneKind;
    use ZoneApplicationError::{ApplicationTimeout, SupervisorExited};
    use ZoneApplicationState::{Applied, Applying, Failed, Inactive};

    fn zones(name: &str) -> Vec<ZoneConfig> {
        vec![ZoneConfig {
            id: "z1".into(),
            name: name.into(),
            polygon: vec![(0.1, 0.1), (0.9, 0.1), (0.5, 0.9)],
            kind: ZoneKind::Exclusion,
            min_bbox_area_px_override: Some(40),
        }]
    }

    fn at(secs: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 24, 16, 30, secs).unwrap()
    }

    type Read = (
        String,
        Option<String>,
        u64,
        Option<u64>,
        ZoneApplicationState,
        Option<ZoneApplicationError>,
    );

    /// (desired, applied, generation, applied generation, state, error) of
    /// camera 7, enabled with `desired` saved, read at `now`.
    fn read(r: &ZoneApplicationRegistry, desired: &str, now: Instant) -> Read {
        read_as(r, desired, true, now)
    }

    fn read_as(r: &ZoneApplicationRegistry, desired: &str, enabled: bool, now: Instant) -> Read {
        let s = r.snapshot(7, desired, enabled, now);
        (
            s.desired_revision,
            s.applied_revision,
            s.generation,
            s.applied_generation,
            s.state,
            s.error_code,
        )
    }

    /// Camera 7's `generation` processes a frame: it takes the current
    /// request first, as the supervisor does, and acknowledges it.
    fn frame(r: &ZoneApplicationRegistry, generation: u64, revision: &str, at: DateTime<Utc>) {
        let request = r
            .current_request(7, generation)
            .expect("the generation runs the saved revision");
        r.acknowledge(7, generation, request, revision, at);
    }

    #[test]
    fn the_revision_is_a_pinned_sha256_of_the_ordered_zones_only() {
        assert_eq!(
            zone_revision(&zones("Driveway")),
            "sha256:8eb69890b08b407eadcba9e87ea301b8da85fcaa11091bfe687c0c7adab49792",
            "the digest of this zone list must not change across processes or releases"
        );
        assert_eq!(zone_revision(&zones("A")), zone_revision(&zones("A")));
        let mut two = zones("A");
        two.extend(zones("B"));
        let mut swapped = zones("B");
        swapped.extend(zones("A"));
        assert_ne!(zone_revision(&two), zone_revision(&swapped), "order counts");
        let mut kind = zones("A");
        kind[0].kind = ZoneKind::Inclusion;
        let mut area = zones("A");
        area[0].min_bbox_area_px_override = None;
        let mut polygon = zones("A");
        polygon[0].polygon[0].0 = 0.2;
        let mut id = zones("A");
        id[0].id = "z2".into();
        for other in [kind, area, polygon, id, zones("B"), vec![]] {
            assert_ne!(zone_revision(&other), zone_revision(&zones("A")));
        }
    }

    /// Zeros and the frame's edges hash as they always have: folding -0.0
    /// into 0.0 changes no saved config's digest.
    #[test]
    fn a_config_with_zero_vertices_keeps_its_pinned_revision() {
        let mut edges = zones("Driveway");
        edges[0].polygon = vec![(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)];
        assert_eq!(
            zone_revision(&edges),
            "sha256:3865868024017ee787cdb03c9d3326f5f4e89816f3ad2f4158a34ce7e8537268"
        );
        assert_eq!(
            zone_revision(&[]),
            "sha256:4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
        );
    }

    /// A save that turns a 0.0 vertex into -0.0 passes validation and is the
    /// same zones to the reconciler's key (`==`), so nothing restarts: it is
    /// the same revision, and the running generation's receipt stays Applied.
    #[test]
    fn a_negative_zero_vertex_save_is_the_same_revision_and_stays_applied() {
        let camera = |x: &str| -> nexus_config::CameraConfig {
            serde_json::from_str(&format!(
                r#"{{"name":"c1","url":"rtsp://example/cam","zones":[{{"id":"z1","name":"door",
                "polygon":[[{x},0.0],[1.0,0.0],[0.0,1.0]],"kind":"exclusion"}}]}}"#
            ))
            .unwrap()
        };
        let (zero, negative) = (camera("0.0"), camera("-0.0"));
        assert!(negative.zones[0].polygon[0].0.is_sign_negative(), "fixture");
        assert_eq!(negative.validate_finite(), Ok(()));
        assert_eq!(negative.zones, zero.zones, "the reconciler would restart");
        let a = zone_revision(&zero.zones);
        assert_eq!(zone_revision(&negative.zones), a);

        let r = ZoneApplicationRegistry::default();
        let now = Instant::now();
        r.request(7, &a, true);
        let g = r.start(7, &a);
        frame(&r, g, &a, at(0));
        // The save requests what it stored; the reconciler restarts nothing.
        let saved = zone_revision(&negative.zones);
        r.request(7, &saved, true);
        assert_eq!(
            read(&r, &saved, now),
            (a.clone(), Some(a.clone()), g, Some(g), Applied, None)
        );
    }

    /// The plan's vector: A applied by g1, B started by g2 without an
    /// acknowledgement, then A requested again. Historical A/g1 cannot
    /// satisfy that request and B/g2 cannot acknowledge it; only a fresh A
    /// generation can, and a same-zones restart needs its own frame.
    #[test]
    fn only_a_fresh_a_generation_applies_a_requested_again_after_b_started() {
        let (a, b) = (zone_revision(&zones("A")), zone_revision(&zones("B")));
        let r = ZoneApplicationRegistry::default();
        let now = Instant::now();
        r.request(7, &a, true);
        let g1 = r.start(7, &a);
        let token_a = r.current_request(7, g1).expect("g1 runs A");
        r.acknowledge(7, g1, token_a, &a, at(0));
        assert_eq!(
            read(&r, &a, now),
            (a.clone(), Some(a.clone()), g1, Some(g1), Applied, None)
        );

        r.request(7, &b, true);
        r.stop(7, g1);
        let g2 = r.start(7, &b);
        let token_b = r.current_request(7, g2).expect("g2 runs B");
        r.request(7, &a, true);
        assert_eq!(r.current_request(7, g2), None, "g2 runs B, not A");
        let pending = (a.clone(), Some(a.clone()), g2, Some(g1), Applying, None);
        assert_eq!(
            read(&r, &a, now),
            pending,
            "historical A/g1 satisfied the new request for A"
        );
        r.acknowledge(7, g1, token_a, &a, at(1));
        r.acknowledge(7, g2, token_b, &b, at(1));
        assert_eq!(
            read(&r, &a, now),
            pending,
            "a stale acknowledgement counted"
        );
        assert_eq!(r.snapshot(7, &a, true, now).applied_at, Some(at(0)));

        r.stop(7, g2);
        let g3 = r.start(7, &a);
        frame(&r, g3, &a, at(2));
        assert_eq!(
            read(&r, &a, now),
            (a.clone(), Some(a.clone()), g3, Some(g3), Applied, None)
        );
        assert_eq!(r.snapshot(7, &a, true, now).applied_at, Some(at(2)));

        r.stop(7, g3);
        let g4 = r.start(7, &a);
        assert_eq!(
            read(&r, &a, now),
            (a.clone(), Some(a.clone()), g4, Some(g3), Applying, None),
            "a restart with the same zones applied before its own frame"
        );
        frame(&r, g4, &a, at(3));
        assert_eq!(
            read(&r, &a, now),
            (a.clone(), Some(a.clone()), g4, Some(g4), Applied, None)
        );
    }

    /// A→B→A while the generation running A keeps running: the A
    /// acknowledged before B no longer counts, and its next frame, taken
    /// under the new request, applies A again without a restart.
    #[test]
    fn a_to_b_to_a_before_a_restart_needs_an_acknowledgement_of_the_new_request() {
        let (a, b) = (zone_revision(&zones("A")), zone_revision(&zones("B")));
        let r = ZoneApplicationRegistry::default();
        let now = Instant::now();
        r.request(7, &a, true);
        let g1 = r.start(7, &a);
        let first = r.current_request(7, g1).expect("g1 runs A");
        r.acknowledge(7, g1, first, &a, at(0));

        r.request(7, &b, true);
        assert_eq!(r.current_request(7, g1), None, "g1 does not run B");
        r.request(7, &a, true);
        let again = r.current_request(7, g1).expect("g1 runs A again");
        assert_ne!(again, first, "A→B→A is a new request");
        let pending = (a.clone(), Some(a.clone()), g1, Some(g1), Applying, None);
        assert_eq!(read(&r, &a, now), pending, "the A before B still counts");
        r.acknowledge(7, g1, first, &a, at(1));
        assert_eq!(read(&r, &a, now), pending, "the earlier A token counted");
        assert_eq!(r.snapshot(7, &a, true, now).applied_at, Some(at(0)));

        r.acknowledge(7, g1, again, &a, at(2));
        assert_eq!(
            read(&r, &a, now),
            (a.clone(), Some(a.clone()), g1, Some(g1), Applied, None),
            "the running generation's next frame did not apply the new request"
        );
        assert_eq!(r.snapshot(7, &a, true, now).applied_at, Some(at(2)));
    }

    #[test]
    fn identical_requests_change_nothing() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        let now = Instant::now();
        r.request(7, &a, true);
        let g = r.start(7, &a);
        let token = r.current_request(7, g).expect("g runs A");
        r.request(7, &a, true);
        assert_eq!(
            r.current_request(7, g),
            Some(token),
            "a same-value save is a new request"
        );
        r.acknowledge(7, g, token, &a, at(0));
        r.request(7, &a, true);
        assert_eq!(
            read(&r, &a, now),
            (a.clone(), Some(a.clone()), g, Some(g), Applied, None)
        );
        assert_eq!(read(&r, &a, now).4, Applied, "a read undid the verdict");
        assert_eq!(r.current_request(7, g), Some(token));
    }

    #[test]
    fn a_stale_failure_cannot_fail_a_newer_request() {
        let (a, b) = (zone_revision(&zones("A")), zone_revision(&zones("B")));
        let r = ZoneApplicationRegistry::default();
        let now = Instant::now();
        r.request(7, &a, true);
        let g0 = r.start(7, &a);
        let old_generation = r.current_request(7, g0).expect("g0 runs A");
        r.stop(7, g0);
        let g1 = r.start(7, &a);
        let first = r.current_request(7, g1).expect("g1 runs A");
        r.request(7, &b, true);
        r.request(7, &a, true);
        let again = r.current_request(7, g1).expect("g1 runs A again");

        r.fail(7, g1, first, SupervisorExited);
        r.fail(7, g0, old_generation, SupervisorExited);
        r.fail(7, g0, again, SupervisorExited);
        assert_eq!(
            read(&r, &a, now),
            (a.clone(), None, g1, None, Applying, None),
            "a failure of an earlier request or generation failed the current one"
        );
        r.fail(7, g1, again, SupervisorExited);
        assert_eq!(
            read(&r, &a, now),
            (a.clone(), None, g1, None, Failed, Some(SupervisorExited))
        );
        assert_eq!(
            read(&r, &b, now),
            (b.clone(), None, g1, None, Applying, None),
            "the failure belongs to the request before"
        );
    }

    /// A applied, B saved, the generation running A begins stopping, and A
    /// is saved again before the replacement starts. Nothing runs A any
    /// more: applying, failed after the timeout if no replacement comes, and
    /// the stopping generation's late callbacks cannot bring it back.
    #[test]
    fn a_stopping_generation_no_longer_counts_as_applied() {
        let (a, b) = (zone_revision(&zones("A")), zone_revision(&zones("B")));
        let r = ZoneApplicationRegistry::default();
        r.request(7, &a, true);
        let g1 = r.start(7, &a);
        let token = r.current_request(7, g1).expect("g1 runs A");
        r.acknowledge(7, g1, token, &a, at(0));
        assert_eq!(read(&r, &b, Instant::now()).4, Applying);
        r.stop(7, g1);
        let stopped = Instant::now();
        assert_eq!(read(&r, &a, stopped).4, Applying);
        assert_eq!(
            r.current_request(7, g1),
            None,
            "a stopping generation runs nothing"
        );
        r.acknowledge(7, g1, token, &a, at(1));
        r.fail(7, g1, token, SupervisorExited);
        assert_eq!(
            read(&r, &a, stopped),
            (a.clone(), Some(a.clone()), g1, Some(g1), Applying, None),
            "a stopping generation's zones read applied"
        );
        assert_eq!(r.snapshot(7, &a, true, stopped).applied_at, Some(at(0)));
        assert_eq!(
            read(&r, &a, stopped + APPLICATION_TIMEOUT).5,
            Some(ApplicationTimeout),
            "a stop with no replacement fails after the timeout"
        );
        let g2 = r.start(7, &a);
        frame(&r, g2, &a, at(2));
        assert_eq!(read(&r, &a, Instant::now()).4, Applied);
    }

    #[test]
    fn a_fresh_process_has_applied_nothing() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        let now = Instant::now();
        assert_eq!(r.current_request(7, 1), None, "nothing requested yet");
        let s = r.snapshot(7, &a, true, now);
        assert_eq!(
            (
                s.applied_revision,
                s.applied_generation,
                s.applied_at,
                s.state
            ),
            (None, None, None, Applying),
            "an enabled camera with no generation is applying, not applied or inactive"
        );
        let g = r.start(7, &a);
        assert!(g > s.generation, "a start is a newer generation");
        let s = r.snapshot(7, &a, true, now);
        assert_eq!(
            (
                s.applied_revision,
                s.applied_generation,
                s.applied_at,
                s.state
            ),
            (None, None, None, Applying),
            "a started task is not an applied one"
        );
    }

    /// A reader treats a generation lower than one it saw as stale, so a
    /// restarted process must hand out higher generations than the last one.
    #[test]
    fn a_fresh_process_starts_above_every_earlier_boots_generations() {
        let earlier = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let r = ZoneApplicationRegistry::default();
        assert!(
            u128::from(r.start(7, &zone_revision(&[]))) > earlier * 1000,
            "a restart's first generation is below the earlier boot's"
        );
        assert!(
            u128::from(
                r.snapshot(8, &zone_revision(&[]), false, Instant::now())
                    .generation
            ) > earlier * 1000,
            "a camera with no generation yet reads one below the earlier boot's"
        );
    }

    #[test]
    fn generations_rise_within_a_boot_and_past_it_and_stay_json_safe() {
        let a = zone_revision(&[]);
        let boot = UNIX_EPOCH + Duration::from_millis(1_795_000_000_000);
        let first = ZoneApplicationRegistry::booted_at(boot);
        let mut last = 0;
        // A thousand starts in the boot's first millisecond: a boot one
        // millisecond later still starts above them.
        for camera in 0..1000 {
            let g = first.start(camera % 3, &a);
            assert!(g > last, "generation {g} after {last}");
            last = g;
        }
        let restarted = ZoneApplicationRegistry::booted_at(boot + Duration::from_millis(1));
        assert!(restarted.start(7, &a) > last, "the restart went back");

        let far = ZoneApplicationRegistry::booted_at(
            UNIX_EPOCH + Duration::from_secs(40_000 * 365 * 86_400),
        );
        assert!(far.start(7, &a) < 1 << 53, "beyond a JSON-safe integer");
    }

    fn floor_in(path: &std::path::Path) -> u64 {
        std::fs::read_to_string(path)
            .expect("the floor file exists")
            .trim()
            .parse()
            .expect("the floor file holds a number")
    }

    /// The review's clock rollback: the clock steps back an hour across a
    /// restart, and the restart still starts above every generation the
    /// earlier boot could hand out without persisting a higher floor first.
    #[test]
    fn a_restart_starts_above_the_earlier_boots_floor_even_when_the_clock_went_back() {
        let a = zone_revision(&[]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zone_generation_floor");
        let boot = UNIX_EPOCH + Duration::from_millis(1_795_000_000_000);
        let first = ZoneApplicationRegistry::booted_at(boot);
        first.load_generation_floor(path.clone());
        let mut last = 0;
        for camera in 0..1000 {
            last = first.start(camera % 3, &a);
        }
        let floor = floor_in(&path);
        assert!(
            last <= floor,
            "generation {last} handed out above floor {floor}"
        );

        let earlier = boot - Duration::from_secs(3600);
        assert!(
            ZoneApplicationRegistry::booted_at(earlier).start(7, &a) < last,
            "the clock alone would have gone back"
        );
        let restarted = ZoneApplicationRegistry::booted_at(earlier);
        restarted.load_generation_floor(path.clone());
        assert!(restarted.start(7, &a) > floor, "the restart went back");
    }

    #[test]
    fn a_missing_or_corrupt_floor_falls_back_to_the_clock_and_is_rewritten() {
        let a = zone_revision(&[]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zone_generation_floor");
        let boot = UNIX_EPOCH + Duration::from_millis(1_795_000_000_000);
        let seeded = ZoneApplicationRegistry::booted_at(boot).start(7, &a);
        for contents in [None, Some("not a number"), Some(""), Some("-5")] {
            if let Some(c) = contents {
                std::fs::write(&path, c).unwrap();
            }
            let r = ZoneApplicationRegistry::booted_at(boot);
            r.load_generation_floor(path.clone());
            let g = r.start(7, &a);
            assert_eq!(g, seeded, "{contents:?} does not seed from the clock");
            assert!(floor_in(&path) >= g, "{contents:?} was not rewritten");
            std::fs::remove_file(&path).unwrap();
        }
    }

    /// A state directory that cannot be written (a file stands in its place,
    /// which no user, root included, can write under) loses the floor across
    /// restarts but never this process's generations, past a block included.
    #[test]
    fn an_unwritable_floor_still_hands_out_rising_generations() {
        let a = zone_revision(&[]);
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("state");
        std::fs::write(&not_a_dir, "").unwrap();
        let path = not_a_dir.join("zone_generation_floor");
        let r = ZoneApplicationRegistry::default();
        r.load_generation_floor(path.clone());
        let mut last = 0;
        for _ in 0..FLOOR_BLOCK + 2 {
            let g = r.start(7, &a);
            assert!(g > last, "generation {g} after {last}");
            last = g;
        }
        assert!(!path.exists());
    }

    /// SPEC-088 review 11: with an external admin secret or OIDC, bootstrap
    /// leaves a fresh install's state directory uncreated, so the floor is the
    /// first thing written into it. It is persisted all the same, and a
    /// restart with the clock set back still starts above the first boot.
    #[test]
    fn a_fresh_install_persists_its_floor_before_the_state_dir_exists() {
        let a = zone_revision(&[]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("zone_generation_floor");
        let boot = UNIX_EPOCH + Duration::from_millis(1_795_000_000_000);
        let first = ZoneApplicationRegistry::booted_at(boot);
        first.load_generation_floor(path.clone());
        let last = first.start(7, &a);
        assert!(path.exists(), "the floor was not persisted");

        let restarted = ZoneApplicationRegistry::booted_at(boot - Duration::from_secs(3600));
        restarted.load_generation_floor(path.clone());
        assert!(restarted.start(7, &a) > last, "the restart went back");
    }

    /// SPEC-088 review 11: a floor that could not be persisted reserves
    /// nothing, so the next start persists one before it hands out a
    /// generation above it.
    #[test]
    fn a_floor_that_could_not_be_persisted_is_not_reserved() {
        let a = zone_revision(&[]);
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::write(&state, "").unwrap();
        let path = state.join("zone_generation_floor");
        let r = ZoneApplicationRegistry::default();
        r.load_generation_floor(path.clone());
        assert!(
            !path.exists(),
            "precondition: the floor could not be written"
        );

        std::fs::remove_file(&state).unwrap();
        std::fs::create_dir(&state).unwrap();
        let g = r.start(7, &a);
        assert!(
            path.exists() && floor_in(&path) >= g,
            "generation {g} was handed out above an unpersisted floor"
        );
    }

    #[test]
    fn handing_out_a_whole_block_persists_the_next_one_first() {
        let a = zone_revision(&[]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zone_generation_floor");
        let r = ZoneApplicationRegistry::default();
        r.load_generation_floor(path.clone());
        let floor = floor_in(&path);
        let mut g = 0;
        while g < floor {
            g = r.start(7, &a);
        }
        assert_eq!(
            (g, floor_in(&path)),
            (floor, floor),
            "the block was cut short"
        );
        assert_eq!(r.start(7, &a), floor + 1);
        assert_eq!(
            floor_in(&path),
            floor + FLOOR_BLOCK,
            "the next block was not persisted"
        );
    }

    #[test]
    fn a_deleted_and_recreated_camera_starts_over() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        let now = Instant::now();
        r.request(7, &a, true);
        let g1 = r.start(7, &a);
        let token = r.current_request(7, g1).expect("g1 runs A");
        r.acknowledge(7, g1, token, &a, at(0));
        assert_eq!(read(&r, &a, now).4, Applied);
        r.remove(7);
        let (_, applied, recreated, applied_generation, state, _) = read(&r, &a, now);
        assert_eq!((applied, applied_generation, state), (None, None, Applying));
        assert_eq!(
            r.current_request(7, g1),
            None,
            "g1 runs nothing of the new camera"
        );
        r.acknowledge(7, g1, token, &a, at(1));
        assert_eq!(
            read(&r, &a, now),
            (a.clone(), None, recreated, None, Applying, None),
            "the deleted camera's supervisor applied the new one"
        );
        let g2 = r.start(7, &a);
        assert!(g2 > g1, "a generation number is never reused");
        frame(&r, g2, &a, at(2));
        assert_eq!(read(&r, &a, now).4, Applied);
    }

    #[test]
    fn no_acknowledgement_in_30_s_fails_and_a_later_one_recovers() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        let before = Instant::now();
        r.request(7, &a, true);
        let g = r.start(7, &a);
        let after = Instant::now();
        assert_eq!(read(&r, &a, before + Duration::from_secs(29)).4, Applying);
        let late = after + APPLICATION_TIMEOUT;
        assert_eq!(
            read(&r, &a, late),
            (a.clone(), None, g, None, Failed, Some(ApplicationTimeout))
        );
        let json = serde_json::to_value(r.snapshot(7, &a, true, late)).unwrap();
        assert_eq!(
            (&json["state"], &json["error_code"]),
            (
                &serde_json::json!("failed"),
                &serde_json::json!("application_timeout")
            )
        );
        frame(&r, g, &a, at(0));
        assert_eq!(
            read(&r, &a, late),
            (a.clone(), Some(a.clone()), g, Some(g), Applied, None),
            "a valid acknowledgement after the timeout recovers"
        );
    }

    /// A generation that acknowledged and then ended unasked reads like a
    /// stopped one; a late stop of it leaves the newer generation alone.
    #[test]
    fn an_ended_generation_stops_reading_applied_and_only_itself() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        r.request(7, &a, true);
        let g1 = r.start(7, &a);
        let token = r.current_request(7, g1).expect("g1 runs A");
        r.acknowledge(7, g1, token, &a, at(0));
        r.stop(7, g1);
        let ended = Instant::now();
        r.acknowledge(7, g1, token, &a, at(1));
        assert_eq!(
            read(&r, &a, ended),
            (a.clone(), Some(a.clone()), g1, Some(g1), Applying, None)
        );
        assert_eq!(
            read(&r, &a, ended + APPLICATION_TIMEOUT).5,
            Some(ApplicationTimeout)
        );
        let g2 = r.start(7, &a);
        frame(&r, g2, &a, at(2));
        r.stop(7, g1);
        assert_eq!(
            read(&r, &a, Instant::now() + APPLICATION_TIMEOUT).4,
            Applied,
            "the ended generation stopped its replacement"
        );
    }

    #[test]
    fn only_the_current_generation_can_fail() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        let now = Instant::now();
        r.request(7, &a, true);
        let g1 = r.start(7, &a);
        let first = r.current_request(7, g1).expect("g1 runs A");
        let g2 = r.start(7, &a);
        let second = r.current_request(7, g2).expect("g2 runs A");
        r.fail(7, g1, first, SupervisorExited);
        assert_eq!(read(&r, &a, now).4, Applying);
        r.fail(7, g2, second, SupervisorExited);
        assert_eq!(
            read(&r, &a, now),
            (a.clone(), None, g2, None, Failed, Some(SupervisorExited))
        );
    }

    /// Enabled startup failure: a generation that ends before its first
    /// frame reads failed, and stays an application failure, never inactive,
    /// however long nothing replaces it.
    #[test]
    fn an_enabled_camera_that_fails_to_start_reads_failed_never_inactive() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        r.request(7, &a, true);
        let g = r.start(7, &a);
        let token = r.current_request(7, g).expect("g runs A");
        r.fail(7, g, token, SupervisorExited);
        assert_eq!(
            read(&r, &a, Instant::now() + 2 * APPLICATION_TIMEOUT),
            (a.clone(), None, g, None, Failed, Some(SupervisorExited))
        );
        r.stop(7, g);
        let stopped = Instant::now();
        assert_eq!(
            read(&r, &a, stopped + APPLICATION_TIMEOUT),
            (a.clone(), None, g, None, Failed, Some(ApplicationTimeout)),
            "an enabled camera with no working generation must time out"
        );
    }

    /// Save-while-disabled: a persisted disabled camera is inactive, with no
    /// deadline however long it stays so, and a save while disabled moves
    /// desired without asking any generation for anything.
    #[test]
    fn a_disabled_camera_is_inactive_without_a_deadline_even_when_saved_again() {
        let (a, b) = (zone_revision(&zones("A")), zone_revision(&zones("B")));
        let r = ZoneApplicationRegistry::default();
        let before = Instant::now();
        r.request(7, &a, false);
        let later = before + APPLICATION_TIMEOUT + Duration::from_secs(1);
        let (desired, applied, generation, applied_generation, state, error) =
            read_as(&r, &a, false, later);
        assert_eq!(
            (desired, applied, applied_generation, state, error),
            (a.clone(), None, None, Inactive, None)
        );
        r.request(7, &b, false);
        assert_eq!(
            read_as(&r, &b, false, Instant::now() + 2 * APPLICATION_TIMEOUT),
            (b.clone(), None, generation, None, Inactive, None),
            "a save while disabled started a deadline or a generation"
        );
        assert_eq!(r.current_request(7, generation), None);
    }

    /// Enabling starts applying with a fresh deadline, not one from the save
    /// while disabled, and only an acknowledgement of the enabled request
    /// applies.
    #[test]
    fn enabling_starts_a_fresh_deadline_and_needs_a_current_acknowledgement() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        r.request(7, &a, false);
        assert_eq!(
            read_as(&r, &a, false, Instant::now() + 2 * APPLICATION_TIMEOUT).4,
            Inactive
        );
        let before = Instant::now();
        r.request(7, &a, true);
        let after = Instant::now();
        assert_eq!(read(&r, &a, before + Duration::from_secs(29)).4, Applying);
        assert_eq!(
            read(&r, &a, after + APPLICATION_TIMEOUT).5,
            Some(ApplicationTimeout),
            "an enabled camera with no working generation must time out"
        );
        let g = r.start(7, &a);
        assert_eq!(
            read(&r, &a, Instant::now()),
            (a.clone(), None, g, None, Applying, None),
            "a start does not apply, and its deadline starts afresh"
        );
        frame(&r, g, &a, at(0));
        assert_eq!(
            read(&r, &a, Instant::now()),
            (a.clone(), Some(a.clone()), g, Some(g), Applied, None)
        );
    }

    /// Disabling while applying, or after applying, takes the verdict away
    /// at once and no late callback of the generation can turn inactive into
    /// applied or failed. Enabled again before that generation stopped, its
    /// next frame must acknowledge afresh.
    #[test]
    fn disabling_rejects_late_callbacks_and_enabling_needs_a_fresh_acknowledgement() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        r.request(7, &a, true);
        let g1 = r.start(7, &a);
        let applying = r.current_request(7, g1).expect("g1 runs A");
        r.request(7, &a, false);
        assert_eq!(
            r.current_request(7, g1),
            None,
            "a disabled camera requests nothing"
        );
        r.acknowledge(7, g1, applying, &a, at(0));
        r.fail(7, g1, applying, SupervisorExited);
        assert_eq!(
            read_as(&r, &a, false, Instant::now() + 2 * APPLICATION_TIMEOUT),
            (a.clone(), None, g1, None, Inactive, None),
            "a late callback changed an inactive camera"
        );

        r.request(7, &a, true);
        let applied = r.current_request(7, g1).expect("g1 runs A");
        r.acknowledge(7, g1, applied, &a, at(1));
        assert_eq!(read(&r, &a, Instant::now()).4, Applied);
        r.request(7, &a, false);
        assert_eq!(
            read_as(&r, &a, false, Instant::now()),
            (a.clone(), Some(a.clone()), g1, Some(g1), Inactive, None),
            "a disable kept the verdict"
        );
        r.request(7, &a, true);
        assert_eq!(
            read(&r, &a, Instant::now()).4,
            Applying,
            "an enable kept the old verdict"
        );
        r.acknowledge(7, g1, applied, &a, at(2));
        assert_eq!(
            read(&r, &a, Instant::now()).4,
            Applying,
            "a pre-disable token counted"
        );
        frame(&r, g1, &a, at(3));
        assert_eq!(read(&r, &a, Instant::now()).4, Applied);
        assert_eq!(
            r.snapshot(7, &a, true, Instant::now()).applied_at,
            Some(at(3))
        );
    }

    /// SPEC-077 — candidate values from the plan's calibration grid.
    fn policy(dwell_secs: f64) -> nexus_config::ParkingPolicyV2 {
        nexus_config::ParkingPolicyV2 {
            dwell_secs,
            max_speed_box_diagonals_per_sec: 0.05,
            max_displacement_box_diagonals: 0.10,
            max_observation_gap_secs: 3.0,
        }
    }

    /// Camera 7 in parking-lot mode with one parking zone and `policy`, and
    /// a credential in every field that can hold one.
    fn parked(policy: Option<nexus_config::ParkingPolicyV2>) -> CameraConfig {
        let mut camera: CameraConfig = serde_json::from_value(serde_json::json!({
            "id": 7,
            "name": "Lot",
            "url": "rtsp://user:secret@10.0.0.5/main",
            "analysis_url": "rtsp://user:secret@10.0.0.5/sub",
            "enabled": true,
            "onvif": {
                "endpoint": "http://10.0.0.5/onvif/device_service",
                "username": "user",
                "password": "secret",
            },
            "parking_lot_mode": true,
        }))
        .expect("a camera");
        camera.behavior.parking_policy_v2 = policy;
        camera.zones = vec![ZoneConfig {
            id: "stalls".into(),
            name: "Stalls".into(),
            polygon: vec![(0.1, 0.1), (0.9, 0.1), (0.5, 0.9)],
            kind: ZoneKind::Parking,
            min_bbox_area_px_override: None,
        }];
        camera
    }

    #[test]
    fn the_parking_revision_is_a_pinned_sha256_of_zones_mode_and_the_whole_policy() {
        let a = parking_revision(&parked(Some(policy(10.0))));
        assert_eq!(
            a, "sha256:a1c92de498bfeb8b4ff149ef25c61f67199dc3729a198e4d16cc3c43b2f1afc8",
            "the digest of [zones, true, policy] must not change across processes or releases"
        );
        let mut legacy = parked(None);
        legacy.behavior.parking_lot_mode = false;
        legacy.zones.clear();
        assert_eq!(
            parking_revision(&legacy),
            "sha256:01bd28a5e805c292e92bcba504a1a3b73cac6f037b6e421c7ebd4e8ca4116c00",
            "absence is part of the tuple: the digest of [[], false, null]"
        );
        assert_ne!(
            parking_revision(&legacy),
            zone_revision(&legacy.zones),
            "a zone-only digest can never stand for the parking tuple"
        );
        assert_ne!(a, zone_revision(&parked(Some(policy(10.0))).zones));

        type Edit = fn(&mut CameraConfig);
        let moves: [(&str, Edit); 9] = [
            ("dwell", |c| {
                c.behavior.parking_policy_v2 = Some(policy(20.0))
            }),
            ("speed", |c| {
                let mut p = policy(10.0);
                p.max_speed_box_diagonals_per_sec = 0.02;
                c.behavior.parking_policy_v2 = Some(p);
            }),
            ("displacement", |c| {
                let mut p = policy(10.0);
                p.max_displacement_box_diagonals = 0.20;
                c.behavior.parking_policy_v2 = Some(p);
            }),
            ("gap", |c| {
                let mut p = policy(10.0);
                p.max_observation_gap_secs = 6.0;
                c.behavior.parking_policy_v2 = Some(p);
            }),
            ("policy removed", |c| c.behavior.parking_policy_v2 = None),
            ("mode off", |c| c.behavior.parking_lot_mode = false),
            ("zone moved", |c| c.zones[0].polygon[0].0 = 0.2),
            ("zone kind", |c| c.zones[0].kind = ZoneKind::Exclusion),
            ("zone added", |c| {
                let mut z = c.zones[0].clone();
                z.id = "more".into();
                c.zones.push(z);
            }),
        ];
        for (what, edit) in moves {
            let mut c = parked(Some(policy(10.0)));
            edit(&mut c);
            assert_ne!(parking_revision(&c), a, "{what} kept the parking revision");
        }
        let mut swapped = parked(Some(policy(10.0)));
        let mut second = swapped.zones[0].clone();
        second.id = "more".into();
        swapped.zones.insert(0, second.clone());
        let mut ordered = parked(Some(policy(10.0)));
        ordered.zones.push(second);
        assert_ne!(
            parking_revision(&swapped),
            parking_revision(&ordered),
            "order counts"
        );

        let stays: [(&str, Edit); 7] = [
            ("name", |c| c.name = "Other".into()),
            ("url", |c| {
                c.ingest.url = "rtsp://u2:other@10.0.0.6/main".parse().expect("a url")
            }),
            ("analysis url", |c| c.ingest.analysis_url = None),
            ("onvif password", |c| {
                c.onvif.password = Some("other".into())
            }),
            ("enabled", |c| c.ingest.enabled = false),
            ("prompts", |c| c.detector.prompts = vec!["truck".into()]),
            ("anchor ttl", |c| c.behavior.anchor_ttl_secs = Some(60)),
        ];
        for (what, edit) in stays {
            let mut c = parked(Some(policy(10.0)));
            edit(&mut c);
            assert_eq!(parking_revision(&c), a, "{what} moved the parking revision");
        }
    }

    /// SPEC-077 — the parking receipt is this state machine in a registry of
    /// its own. A policy-only A→B→A leaves the zones unchanged, so the zone
    /// receipt is applied throughout; with B's generation started and never
    /// acknowledged, neither historical A nor B's generation can apply the A
    /// saved again, and only a fresh A generation does.
    #[test]
    fn a_policy_only_a_to_b_to_a_with_b_unacknowledged_needs_a_fresh_a_generation() {
        let (a_cam, b_cam) = (parked(Some(policy(10.0))), parked(Some(policy(20.0))));
        let z = zone_revision(&a_cam.zones);
        assert_eq!(
            z,
            zone_revision(&b_cam.zones),
            "precondition: zones unchanged"
        );
        let (a, b) = (parking_revision(&a_cam), parking_revision(&b_cam));
        let zones = ZoneApplicationRegistry::default();
        let parking = ZoneApplicationRegistry::default();
        let now = Instant::now();
        // What a save, a start and a stop do: tell both registries.
        let save = |revision: &str| {
            zones.request(7, &z, true);
            parking.request(7, revision, true);
        };
        let restart = |zg: u64, pg: u64, revision: &str| {
            zones.stop(7, zg);
            parking.stop(7, pg);
            (zones.start(7, &z), parking.start(7, revision))
        };

        save(&a);
        let (z1, p1) = (zones.start(7, &z), parking.start(7, &a));
        let token_a = parking.current_request(7, p1).expect("p1 runs A");
        frame(&zones, z1, &z, at(0));
        parking.acknowledge(7, p1, token_a, &a, at(0));
        assert_eq!(read(&parking, &a, now).4, Applied);

        save(&b);
        assert_eq!(
            (read(&zones, &z, now).4, read(&parking, &b, now).4),
            (Applied, Applying),
            "a policy-only save left parking applied, or took the zones' verdict"
        );
        let (z2, p2) = restart(z1, p1, &b);
        let token_b = parking.current_request(7, p2).expect("p2 runs B");
        frame(&zones, z2, &z, at(1));
        save(&a);
        assert_eq!(read(&zones, &z, now).4, Applied, "precondition");
        let pending = (a.clone(), Some(a.clone()), p2, Some(p1), Applying, None);
        assert_eq!(
            read(&parking, &a, now),
            pending,
            "the zone receipt or historical A/p1 certified parking A"
        );
        parking.acknowledge(7, p1, token_a, &a, at(2));
        parking.acknowledge(7, p2, token_b, &b, at(2));
        parking.fail(7, p1, token_a, SupervisorExited);
        assert_eq!(read(&parking, &a, now), pending, "a stale callback counted");

        let (_, p3) = restart(z2, p2, &a);
        assert!(p3 > p2 && p2 > p1);
        frame(&parking, p3, &a, at(3));
        assert_eq!(
            read(&parking, &a, now),
            (a.clone(), Some(a.clone()), p3, Some(p3), Applied, None)
        );
    }

    /// The edge always emits all eight keys, nulls explicit, so a reader can
    /// tell this receipt from an older one that lacks `applied_generation`
    /// (which it must treat as unverified, never applied).
    #[test]
    fn every_key_is_serialized_with_explicit_nulls() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        r.request(7, &a, false);
        let s = r.snapshot(7, &a, false, Instant::now());
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::json!({
                "schema_version": 1,
                "desired_revision": a,
                "applied_revision": null,
                "generation": s.generation,
                "applied_generation": null,
                "state": "inactive",
                "applied_at": null,
                "error_code": null,
            })
        );
    }
}

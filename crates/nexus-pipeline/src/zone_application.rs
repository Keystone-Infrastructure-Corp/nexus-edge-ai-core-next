//! SPEC-088 — which zone revision each camera's running supervisor applied.
//!
//! A revision is the SHA-256 of a camera's validated, ordered zone list. Each
//! supervisor start is a new generation ([`ZoneApplicationRegistry::start`]),
//! which acknowledges the revision of the zones it froze once its first frame
//! has gone through the zone filters and everything downstream of them
//! ([`ZoneApplicationRegistry::acknowledge`]). A read passes the revision saved
//! now ([`ZoneApplicationRegistry::snapshot`]); it is never stored here, so two
//! reads racing a save each answer against the store they read. In memory
//! only, so a process boot has applied nothing; only a floor under the next
//! boot's generations is persisted
//! ([`ZoneApplicationRegistry::load_generation_floor`]). An acknowledgement
//! says one frame was processed with the zones, not that analysis stays
//! healthy: that is the frame stats' job.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use nexus_config::ZoneConfig;
use nexus_types::CameraId;
use parking_lot::Mutex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::warn;

/// How long a generation has, from its start, to acknowledge its zones before
/// it reads `failed` with [`ZoneApplicationError::ApplicationTimeout`].
pub const APPLICATION_TIMEOUT: Duration = Duration::from_secs(30);

/// `sha256:` and the lowercase hex digest of `zones` as JSON, in saved order
/// and with every field the supervisor reads. Only the zones go in, so nothing
/// else about the camera, its credentials included, moves it or leaks into it.
// shortcut: a save that only flips a vertex between 0.0 and -0.0 hashes differently but restarts nothing (the reconciler's key compares them equal), so it reads applying until the camera's next restart; normalize the sign if a client ever sends -0.0.
#[expect(
    clippy::expect_used,
    reason = "a ZoneConfig is strings, floats, a unit enum and an Option<u32>, which always serialize to JSON"
)]
pub fn zone_revision(zones: &[ZoneConfig]) -> String {
    let json = serde_json::to_vec(zones).expect("zones serialize to JSON");
    format!("sha256:{:x}", Sha256::digest(json))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneApplicationState {
    Applying,
    Applied,
    Failed,
}

/// Why the running generation has not applied its zones. Bounded, so a reader
/// can know every value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneApplicationError {
    /// No frame finished processing within [`APPLICATION_TIMEOUT`] of the
    /// generation's start.
    ApplicationTimeout,
    /// The supervisor ended, unasked, before any frame finished processing.
    SupervisorExited,
}

/// SPEC-088's `zones_runtime`. Every key is always serialized, `null` included.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZonesRuntime {
    pub schema_version: u32,
    pub desired_revision: String,
    /// The last revision any generation of this camera acknowledged, kept
    /// while a newer one is applying so a reader can compare the two.
    pub applied_revision: Option<String>,
    pub generation: u64,
    pub state: ZoneApplicationState,
    pub applied_at: Option<DateTime<Utc>>,
    pub error_code: Option<ZoneApplicationError>,
}

struct Entry {
    generation: u64,
    /// Monotonic, so a wall-clock step cannot time a generation out early.
    started_at: Instant,
    /// Whether the current generation acknowledged `applied`.
    acknowledged: bool,
    /// The current generation began stopping ([`ZoneApplicationRegistry::stop`]),
    /// so nothing it reports counts any more.
    stopping: bool,
    failed: Option<ZoneApplicationError>,
    applied: Option<(String, DateTime<Utc>)>,
}

impl Entry {
    fn stop(&mut self) {
        self.stopping = true;
        self.acknowledged = false;
        self.failed = None;
        self.started_at = Instant::now();
    }
}

struct Inner {
    /// The last generation handed out, process-wide, so a number names one
    /// supervisor even across a camera's deletion and re-creation.
    last_generation: u64,
    /// The floor file and the highest generation it covers: none above that
    /// is handed out before a higher floor is written. `None` until
    /// [`ZoneApplicationRegistry::load_generation_floor`].
    floor: Option<(PathBuf, u64)>,
    cameras: HashMap<CameraId, Entry>,
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
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(format!("{floor}\n").as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    match path.parent().filter(|d| !d.as_os_str().is_empty()) {
        Some(dir) => std::fs::File::open(dir)?.sync_all(),
        None => Ok(()),
    }
}

/// [`write_floor`], failing open (R6): the floor only guards the next boot,
/// so this process keeps handing out rising generations either way.
fn persist_floor(path: &Path, floor: u64) {
    if let Err(e) = write_floor(path, floor) {
        warn!(path = %path.display(), error = %e,
            "could not persist the zone generation floor; a restart with the clock set back may read stale");
    }
}

pub struct ZoneApplicationRegistry {
    inner: Mutex<Inner>,
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
                cameras: HashMap::new(),
            }),
        }
    }

    /// Keep generations above every earlier boot's, whatever the wall clock
    /// did: start above the floor in `floor_file` (or the clock seed, if
    /// higher), and persist a new floor a block higher before handing out any
    /// generation. Call before the first [`Self::start`]. A missing or corrupt
    /// file falls back to the clock seed and is rewritten. Remaining
    /// assumptions: the file survives restarts, or the clock does not step
    /// back across them ([`Self::booted_at`]); and the floor stays below the
    /// JSON-safe seed cap, which takes billions of restarts or a clock past
    /// 2255.
    pub fn load_generation_floor(&self, floor_file: PathBuf) {
        let mut inner = self.inner.lock();
        match std::fs::read_to_string(&floor_file).map(|s| s.trim().parse::<u64>()) {
            // Capped like the clock seed, so a generation stays JSON-safe.
            Ok(Ok(floor)) => {
                inner.last_generation = inner.last_generation.max(floor.min(MAX_BOOT_SEED));
            }
            Ok(Err(e)) => warn!(path = %floor_file.display(), error = %e,
                "zone generation floor is corrupt; seeding from the wall clock"),
            Err(e) => warn!(path = %floor_file.display(), error = %e,
                "zone generation floor unreadable; seeding from the wall clock"),
        }
        let reserved = inner.last_generation + FLOOR_BLOCK;
        persist_floor(&floor_file, reserved);
        inner.floor = Some((floor_file, reserved));
    }

    /// Start a new supervisor generation for `camera_id` and return its
    /// number: higher than every one handed out before in this process, and
    /// in earlier ones ([`Self::load_generation_floor`], [`Self::booted_at`]).
    /// Nothing is applied until it acknowledges; the camera's last applied
    /// revision is kept for comparison.
    pub fn start(&self, camera_id: CameraId) -> u64 {
        let mut inner = self.inner.lock();
        inner.last_generation += 1;
        let generation = inner.last_generation;
        if let Some((path, reserved)) = inner.floor.as_mut().filter(|(_, r)| generation > *r) {
            *reserved += FLOOR_BLOCK;
            persist_floor(path, *reserved);
        }
        let applied = inner.cameras.remove(&camera_id).and_then(|e| e.applied);
        inner.cameras.insert(
            camera_id,
            Entry {
                generation,
                started_at: Instant::now(),
                acknowledged: false,
                stopping: false,
                failed: None,
                applied,
            },
        );
        generation
    }

    /// `generation` processed its first frame with `revision`. Ignored unless
    /// it is the camera's current generation.
    pub fn acknowledge(
        &self,
        camera_id: CameraId,
        generation: u64,
        revision: &str,
        at: DateTime<Utc>,
    ) {
        let mut inner = self.inner.lock();
        if let Some(entry) = inner
            .cameras
            .get_mut(&camera_id)
            .filter(|e| e.generation == generation && !e.stopping)
        {
            entry.acknowledged = true;
            entry.failed = None;
            entry.applied = Some((revision.to_owned(), at));
        }
    }

    /// `generation` cannot apply its zones. Ignored unless it is the camera's
    /// current generation.
    pub fn fail(&self, camera_id: CameraId, generation: u64, code: ZoneApplicationError) {
        let mut inner = self.inner.lock();
        if let Some(entry) = inner
            .cameras
            .get_mut(&camera_id)
            .filter(|e| e.generation == generation && !e.stopping)
        {
            entry.failed = Some(code);
        }
    }

    /// The camera's current generation began stopping: what it applied no
    /// longer runs, so the camera reads applying, or failed once
    /// [`APPLICATION_TIMEOUT`] has passed since now, until the next generation
    /// acknowledges. The applied revision stays for comparison.
    pub fn stop(&self, camera_id: CameraId) {
        if let Some(e) = self.inner.lock().cameras.get_mut(&camera_id) {
            e.stop();
        }
    }

    /// [`Self::stop`] for `generation`, which ended unasked after it
    /// acknowledged. Ignored unless it is the camera's current generation.
    pub fn stop_generation(&self, camera_id: CameraId, generation: u64) {
        let mut inner = self.inner.lock();
        if let Some(e) = inner
            .cameras
            .get_mut(&camera_id)
            .filter(|e| e.generation == generation)
        {
            e.stop();
        }
    }

    /// The camera was deleted or disabled: it has nothing left to report.
    pub fn remove(&self, camera_id: CameraId) {
        self.inner.lock().cameras.remove(&camera_id);
    }

    /// The camera's status against `desired`, the revision saved now, at
    /// monotonic `now`; `None` when no generation of it has started in this
    /// process. Applied only when the current generation acknowledged
    /// `desired` itself.
    pub fn snapshot(
        &self,
        camera_id: CameraId,
        desired: &str,
        now: Instant,
    ) -> Option<ZonesRuntime> {
        let inner = self.inner.lock();
        let e = inner.cameras.get(&camera_id)?;
        let applied = e.acknowledged && e.applied.as_ref().is_some_and(|(r, _)| r == desired);
        let timed_out =
            !e.acknowledged && now.saturating_duration_since(e.started_at) >= APPLICATION_TIMEOUT;
        let error_code = if applied {
            None
        } else {
            e.failed
                .or(timed_out.then_some(ZoneApplicationError::ApplicationTimeout))
        };
        Some(ZonesRuntime {
            schema_version: 1,
            desired_revision: desired.to_owned(),
            applied_revision: e.applied.as_ref().map(|(r, _)| r.clone()),
            generation: e.generation,
            state: if applied {
                ZoneApplicationState::Applied
            } else if error_code.is_some() {
                ZoneApplicationState::Failed
            } else {
                ZoneApplicationState::Applying
            },
            applied_at: e.applied.as_ref().map(|(_, at)| *at),
            error_code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use nexus_config::ZoneKind;

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
        ZoneApplicationState,
        Option<ZoneApplicationError>,
    );

    /// (desired, applied, generation, state, error) of camera 7 with
    /// `desired` saved, read at `now`.
    fn read(r: &ZoneApplicationRegistry, desired: &str, now: Instant) -> Read {
        let s = r
            .snapshot(7, desired, now)
            .expect("camera 7 has a generation");
        (
            s.desired_revision,
            s.applied_revision,
            s.generation,
            s.state,
            s.error_code,
        )
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

    /// The plan's A/B vectors: B saved while the generation running A is
    /// current, then a restart, then a stale acknowledgement from the
    /// stopped generation.
    #[test]
    fn a_newer_save_and_a_stopped_generations_ack_never_read_applied() {
        let (a, b) = (zone_revision(&zones("A")), zone_revision(&zones("B")));
        let r = ZoneApplicationRegistry::default();
        let now = Instant::now();
        let g1 = r.start(7);
        r.acknowledge(7, g1, &a, at(0));
        assert_eq!(
            read(&r, &b, now),
            (
                b.clone(),
                Some(a.clone()),
                g1,
                ZoneApplicationState::Applying,
                None
            ),
            "the old generation applied A, but B is saved"
        );
        let g2 = r.start(7);
        assert!(g2 > g1);
        r.acknowledge(7, g1, &a, at(1));
        assert_eq!(
            read(&r, &b, now),
            (
                b.clone(),
                Some(a.clone()),
                g2,
                ZoneApplicationState::Applying,
                None
            ),
            "a stopped generation's late acknowledgement is ignored"
        );
        assert_eq!(
            read(&r, &a, now).3,
            ZoneApplicationState::Applying,
            "A was applied by a generation that no longer runs"
        );
        assert_eq!(r.snapshot(7, &b, now).unwrap().applied_at, Some(at(0)));
        r.acknowledge(7, g2, &b, at(2));
        assert_eq!(
            read(&r, &b, now),
            (
                b.clone(),
                Some(b.clone()),
                g2,
                ZoneApplicationState::Applied,
                None
            )
        );
        assert_eq!(r.snapshot(7, &b, now).unwrap().applied_at, Some(at(2)));
        assert_eq!(
            read(&r, &a, now).3,
            ZoneApplicationState::Applying,
            "a save back to A is not applied by the generation running B"
        );
    }

    /// The review's sequence: A applied, B saved, the generation running A
    /// begins stopping, and A is saved again before the replacement starts.
    /// Nothing runs A any more: applying, failed after the timeout if no
    /// replacement comes, and the stopping generation's late acknowledgement
    /// cannot bring it back. The applied revision stays for comparison.
    #[test]
    fn a_stopping_generation_no_longer_counts_as_applied() {
        let (a, b) = (zone_revision(&zones("A")), zone_revision(&zones("B")));
        let r = ZoneApplicationRegistry::default();
        let g1 = r.start(7);
        r.acknowledge(7, g1, &a, at(0));
        assert_eq!(
            read(&r, &b, Instant::now()).3,
            ZoneApplicationState::Applying
        );
        r.stop(7);
        let stopped = Instant::now();
        r.acknowledge(7, g1, &a, at(1));
        r.fail(7, g1, ZoneApplicationError::SupervisorExited);
        assert_eq!(
            read(&r, &a, stopped),
            (
                a.clone(),
                Some(a.clone()),
                g1,
                ZoneApplicationState::Applying,
                None
            ),
            "a stopping generation's zones read applied"
        );
        assert_eq!(r.snapshot(7, &a, stopped).unwrap().applied_at, Some(at(0)));
        assert_eq!(
            read(&r, &a, stopped + APPLICATION_TIMEOUT).4,
            Some(ZoneApplicationError::ApplicationTimeout),
            "a stop with no replacement fails after the timeout"
        );
        let g2 = r.start(7);
        r.acknowledge(7, g2, &a, at(2));
        assert_eq!(
            read(&r, &a, Instant::now()).3,
            ZoneApplicationState::Applied
        );
    }

    #[test]
    fn a_fresh_process_has_applied_nothing() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        let now = Instant::now();
        assert_eq!(
            r.snapshot(7, &a, now),
            None,
            "no generation, nothing to report"
        );
        r.start(7);
        let s = r.snapshot(7, &a, now).unwrap();
        assert_eq!(
            (s.applied_revision, s.applied_at, s.state),
            (None, None, ZoneApplicationState::Applying),
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
            u128::from(r.start(7)) > earlier * 1000,
            "a restart's first generation is below the earlier boot's"
        );
    }

    #[test]
    fn generations_rise_within_a_boot_and_past_it_and_stay_json_safe() {
        let boot = UNIX_EPOCH + Duration::from_millis(1_795_000_000_000);
        let first = ZoneApplicationRegistry::booted_at(boot);
        let mut last = 0;
        // A thousand starts in the boot's first millisecond: a boot one
        // millisecond later still starts above them.
        for camera in 0..1000 {
            let g = first.start(camera % 3);
            assert!(g > last, "generation {g} after {last}");
            last = g;
        }
        let restarted = ZoneApplicationRegistry::booted_at(boot + Duration::from_millis(1));
        assert!(restarted.start(7) > last, "the restart went back");

        let far = ZoneApplicationRegistry::booted_at(
            UNIX_EPOCH + Duration::from_secs(40_000 * 365 * 86_400),
        );
        assert!(far.start(7) < 1 << 53, "beyond a JSON-safe integer");
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zone_generation_floor");
        let boot = UNIX_EPOCH + Duration::from_millis(1_795_000_000_000);
        let first = ZoneApplicationRegistry::booted_at(boot);
        first.load_generation_floor(path.clone());
        let mut last = 0;
        for camera in 0..1000 {
            last = first.start(camera % 3);
        }
        let floor = floor_in(&path);
        assert!(
            last <= floor,
            "generation {last} handed out above floor {floor}"
        );

        let earlier = boot - Duration::from_secs(3600);
        assert!(
            ZoneApplicationRegistry::booted_at(earlier).start(7) < last,
            "the clock alone would have gone back"
        );
        let restarted = ZoneApplicationRegistry::booted_at(earlier);
        restarted.load_generation_floor(path.clone());
        assert!(restarted.start(7) > floor, "the restart went back");
    }

    #[test]
    fn a_missing_or_corrupt_floor_falls_back_to_the_clock_and_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zone_generation_floor");
        let boot = UNIX_EPOCH + Duration::from_millis(1_795_000_000_000);
        let seeded = ZoneApplicationRegistry::booted_at(boot).start(7);
        for contents in [None, Some("not a number"), Some(""), Some("-5")] {
            if let Some(c) = contents {
                std::fs::write(&path, c).unwrap();
            }
            let r = ZoneApplicationRegistry::booted_at(boot);
            r.load_generation_floor(path.clone());
            let g = r.start(7);
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
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("state");
        std::fs::write(&not_a_dir, "").unwrap();
        let path = not_a_dir.join("zone_generation_floor");
        let r = ZoneApplicationRegistry::default();
        r.load_generation_floor(path.clone());
        let mut last = 0;
        for _ in 0..FLOOR_BLOCK + 2 {
            let g = r.start(7);
            assert!(g > last, "generation {g} after {last}");
            last = g;
        }
        assert!(!path.exists());
    }

    #[test]
    fn handing_out_a_whole_block_persists_the_next_one_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zone_generation_floor");
        let r = ZoneApplicationRegistry::default();
        r.load_generation_floor(path.clone());
        let floor = floor_in(&path);
        let mut g = 0;
        while g < floor {
            g = r.start(7);
        }
        assert_eq!(
            (g, floor_in(&path)),
            (floor, floor),
            "the block was cut short"
        );
        assert_eq!(r.start(7), floor + 1);
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
        let g1 = r.start(7);
        r.acknowledge(7, g1, &a, at(0));
        assert_eq!(read(&r, &a, now).3, ZoneApplicationState::Applied);
        r.remove(7);
        assert_eq!(r.snapshot(7, &a, now), None);
        let g2 = r.start(7);
        assert!(g2 > g1, "a generation number is never reused");
        r.acknowledge(7, g1, &a, at(1));
        assert_eq!(
            read(&r, &a, now),
            (a.clone(), None, g2, ZoneApplicationState::Applying, None),
            "the deleted camera's supervisor cannot apply the new one"
        );
        r.acknowledge(7, g2, &a, at(2));
        assert_eq!(read(&r, &a, now).3, ZoneApplicationState::Applied);
    }

    #[test]
    fn no_acknowledgement_in_30_s_fails_and_a_later_one_recovers() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        let before = Instant::now();
        let g = r.start(7);
        let after = Instant::now();
        assert_eq!(
            read(&r, &a, before + Duration::from_secs(29)).3,
            ZoneApplicationState::Applying
        );
        let late = after + APPLICATION_TIMEOUT;
        assert_eq!(
            read(&r, &a, late),
            (
                a.clone(),
                None,
                g,
                ZoneApplicationState::Failed,
                Some(ZoneApplicationError::ApplicationTimeout)
            )
        );
        let json = serde_json::to_value(r.snapshot(7, &a, late).unwrap()).unwrap();
        assert_eq!(
            (&json["state"], &json["error_code"]),
            (
                &serde_json::json!("failed"),
                &serde_json::json!("application_timeout")
            )
        );
        r.acknowledge(7, g, &a, at(0));
        assert_eq!(
            read(&r, &a, late),
            (
                a.clone(),
                Some(a.clone()),
                g,
                ZoneApplicationState::Applied,
                None
            ),
            "a valid acknowledgement after the timeout recovers"
        );
    }

    /// A generation that acknowledged and then ended unasked reads like a
    /// stopped one; a late stop of it leaves the newer generation alone.
    #[test]
    fn an_ended_generation_stops_reading_applied_and_only_itself() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        let g1 = r.start(7);
        r.acknowledge(7, g1, &a, at(0));
        r.stop_generation(7, g1);
        let ended = Instant::now();
        r.acknowledge(7, g1, &a, at(1));
        assert_eq!(
            read(&r, &a, ended),
            (
                a.clone(),
                Some(a.clone()),
                g1,
                ZoneApplicationState::Applying,
                None
            )
        );
        assert_eq!(
            read(&r, &a, ended + APPLICATION_TIMEOUT).4,
            Some(ZoneApplicationError::ApplicationTimeout)
        );
        let g2 = r.start(7);
        r.acknowledge(7, g2, &a, at(2));
        r.stop_generation(7, g1);
        assert_eq!(
            read(&r, &a, Instant::now() + APPLICATION_TIMEOUT).3,
            ZoneApplicationState::Applied,
            "the ended generation stopped its replacement"
        );
    }

    #[test]
    fn only_the_current_generation_can_fail() {
        let a = zone_revision(&zones("A"));
        let r = ZoneApplicationRegistry::default();
        let now = Instant::now();
        let g1 = r.start(7);
        let g2 = r.start(7);
        r.fail(7, g1, ZoneApplicationError::SupervisorExited);
        assert_eq!(read(&r, &a, now).3, ZoneApplicationState::Applying);
        r.fail(7, g2, ZoneApplicationError::SupervisorExited);
        assert_eq!(
            read(&r, &a, now),
            (
                a.clone(),
                None,
                g2,
                ZoneApplicationState::Failed,
                Some(ZoneApplicationError::SupervisorExited)
            )
        );
    }
}

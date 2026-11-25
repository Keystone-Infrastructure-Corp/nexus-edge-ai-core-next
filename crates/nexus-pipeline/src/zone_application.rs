//! SPEC-088 — which zone revision each camera's running supervisor applied.
//!
//! A revision is the SHA-256 of a camera's validated, ordered zone list. Each
//! supervisor start is a new generation ([`ZoneApplicationRegistry::start`]),
//! which acknowledges the revision of the zones it froze once its first frame
//! has gone through the zone filters and everything downstream of them
//! ([`ZoneApplicationRegistry::acknowledge`]). A read passes the revision saved
//! now ([`ZoneApplicationRegistry::snapshot`]); it is never stored here, so two
//! reads racing a save each answer against the store they read. In memory
//! only, so a process boot has applied nothing. An acknowledgement says one
//! frame was processed with the zones, not that analysis stays healthy: that
//! is the frame stats' job.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use nexus_config::ZoneConfig;
use nexus_types::CameraId;
use parking_lot::Mutex;
use serde::Serialize;
use sha2::{Digest, Sha256};

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
    failed: Option<ZoneApplicationError>,
    applied: Option<(String, DateTime<Utc>)>,
}

#[derive(Default)]
struct Inner {
    /// The last generation handed out, process-wide, so a number names one
    /// supervisor even across a camera's deletion and re-creation.
    last_generation: u64,
    cameras: HashMap<CameraId, Entry>,
}

#[derive(Default)]
pub struct ZoneApplicationRegistry {
    inner: Mutex<Inner>,
}

impl ZoneApplicationRegistry {
    /// Start a new supervisor generation for `camera_id` and return its
    /// number: higher than every one handed out before in this process.
    /// Nothing is applied until it acknowledges; the camera's last applied
    /// revision is kept for comparison.
    pub fn start(&self, camera_id: CameraId) -> u64 {
        let mut inner = self.inner.lock();
        inner.last_generation += 1;
        let generation = inner.last_generation;
        let applied = inner.cameras.remove(&camera_id).and_then(|e| e.applied);
        inner.cameras.insert(
            camera_id,
            Entry {
                generation,
                started_at: Instant::now(),
                acknowledged: false,
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
            .filter(|e| e.generation == generation)
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
            .filter(|e| e.generation == generation)
        {
            entry.failed = Some(code);
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
        assert_eq!(r.start(7), 1);
        let s = r.snapshot(7, &a, now).unwrap();
        assert_eq!(
            (s.applied_revision, s.applied_at, s.state),
            (None, None, ZoneApplicationState::Applying),
            "a started task is not an applied one"
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

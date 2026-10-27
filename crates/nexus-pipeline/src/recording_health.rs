//! Recording failures remain visible until a newer successful attempt recovers them.
use nexus_types::CameraId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingKind {
    Motion,
    Alert,
    Codec,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RecordingDegradation {
    pub camera_id: CameraId,
    pub kind: RecordingKind,
    pub reason: String,
}

#[cfg(feature = "gstreamer")]
pub(crate) use registry::{RecordingAttempt, RecordingHealthRegistry};

#[cfg(any(feature = "gstreamer", test))]
mod registry {
    use super::*;
    use parking_lot::Mutex;
    use std::{collections::BTreeMap, sync::Arc};

    #[derive(Default)]
    struct Status {
        revision: u64,
        reason: Option<&'static str>,
    }

    type CameraStatus = Mutex<BTreeMap<RecordingKind, Status>>;

    /// Owned by a recorder, with at most one entry per camera and recording kind.
    #[derive(Default)]
    pub struct RecordingHealthRegistry {
        cameras: Mutex<BTreeMap<CameraId, Arc<CameraStatus>>>,
    }

    impl RecordingHealthRegistry {
        pub(crate) fn begin(&self, camera_id: CameraId, kind: RecordingKind) -> RecordingAttempt {
            let camera = self.cameras.lock().entry(camera_id).or_default().clone();
            let revision = camera.lock().entry(kind).or_default().revision;
            RecordingAttempt {
                camera,
                kind,
                revision,
            }
        }

        pub fn remove(&self, camera_id: CameraId) {
            self.cameras.lock().remove(&camera_id);
        }

        pub fn degradations(&self) -> Vec<RecordingDegradation> {
            self.cameras
                .lock()
                .iter()
                .flat_map(|(&camera_id, camera)| {
                    camera
                        .lock()
                        .iter()
                        .filter_map(|(&kind, status)| {
                            status.reason.map(|reason| RecordingDegradation {
                                camera_id,
                                kind,
                                reason: reason.to_owned(),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        }
    }

    /// Capturing the camera entry prevents late jobs from reviving a removed camera.
    #[derive(Clone)]
    pub(crate) struct RecordingAttempt {
        camera: Arc<CameraStatus>,
        kind: RecordingKind,
        revision: u64,
    }

    impl RecordingAttempt {
        pub(crate) fn fail(&self, reason: &'static str) {
            let mut camera = self.camera.lock();
            let status = camera.entry(self.kind).or_default();
            status.revision = status.revision.saturating_add(1);
            status.reason = Some(reason);
        }

        pub(crate) fn guard(&self, reason: &'static str) -> RecordingFailureGuard {
            RecordingFailureGuard {
                attempt: self.clone(),
                reason,
                completed: false,
            }
        }

        pub(crate) fn succeed(&self) {
            let mut camera = self.camera.lock();
            let status = camera.entry(self.kind).or_default();
            // An older build cannot hide footage refused after that build started.
            if status.revision == self.revision {
                status.reason = None;
            }
        }
    }

    /// Errors and cancelled operations must not disappear behind an early return.
    pub(crate) struct RecordingFailureGuard {
        attempt: RecordingAttempt,
        reason: &'static str,
        completed: bool,
    }

    impl RecordingFailureGuard {
        pub(crate) fn complete(mut self) {
            self.completed = true;
        }
    }

    impl Drop for RecordingFailureGuard {
        fn drop(&mut self) {
            if !self.completed {
                self.attempt.fail(self.reason);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn success_is_scoped_to_camera_kind_and_newer_attempt() {
            let health = RecordingHealthRegistry::default();
            let old = health.begin(1, RecordingKind::Alert);
            health
                .begin(1, RecordingKind::Alert)
                .fail("capacity exhausted");
            health.begin(1, RecordingKind::Motion).fail("empty clip");
            health.begin(2, RecordingKind::Alert).fail("encode failed");
            old.succeed();
            assert_eq!(
                health.degradations().len(),
                3,
                "old success cannot hide new refusal"
            );
            health.begin(1, RecordingKind::Alert).succeed();
            assert_eq!(health.degradations().len(), 2);
            assert!(health
                .degradations()
                .iter()
                .any(|d| d.camera_id == 1 && d.kind == RecordingKind::Motion));
            assert!(health.degradations().iter().any(|d| d.camera_id == 2));
        }

        #[test]
        fn incomplete_operation_degrades_but_completed_guard_does_not_clear_failure() {
            let health = RecordingHealthRegistry::default();
            let attempt = health.begin(1, RecordingKind::Motion);
            drop(attempt.guard("open failed"));
            assert_eq!(health.degradations()[0].reason, "open failed");
            health
                .begin(1, RecordingKind::Motion)
                .guard("open failed")
                .complete();
            assert_eq!(
                health.degradations().len(),
                1,
                "opening is not proven recovery"
            );
            health.begin(1, RecordingKind::Motion).succeed();
            assert!(health.degradations().is_empty());
        }

        #[test]
        fn removed_camera_cannot_be_revived_by_late_jobs() {
            let health = RecordingHealthRegistry::default();
            let old = health.begin(1, RecordingKind::Motion);
            old.fail("empty clip");
            health.remove(1);
            health.begin(1, RecordingKind::Motion).succeed();
            old.fail("late failure");
            assert!(health.degradations().is_empty());
        }
    }
}

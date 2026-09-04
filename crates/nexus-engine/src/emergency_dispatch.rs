//! SPEC-037 — engine-side Tier-0 emergency dispatch.
//!
//! Concrete [`nexus_pipeline::EmergencyDispatch`] the per-camera
//! supervisor consults once per frame (from inside its own spawned
//! task — see `supervisor.rs`'s non-blocking contract). This is the
//! **real production caller** of `nexus_sinks::emergency`'s
//! `EmergencyPolicy::decide`, `EmergencyRateLimiter::allow`, and
//! `EmergencyDelivery::deliver`: before this module, `grep -rln
//! "Tier0Registry\|EmergencyPolicy\|EmergencySignal" crates` returned
//! only `emergency.rs` itself.
//!
//! ## The stub detector (owner ruling 4)
//!
//! SPEC-036's fire/smoke detector does not exist in this repo — building
//! one is explicitly out of this spec's scope. Per the owner's ruling
//! ("a stub is fine for now... a stub that emits a plausible signal on
//! demand is an acceptable production seam"), [`Self::stub_fire_signal`]
//! is that stub: it scans the frame's *real* tracked objects (produced
//! by the real detector → tracker → static-filter chain — nothing here
//! is scripted) for one whose `label` matches the configured
//! `stub_fire_label`. Finding that label is the ONLY thing the stub
//! decides; everything after it (persistence timing, corroboration,
//! rate-limiting, delivery) is the unmodified `nexus_sinks::emergency`
//! policy. No trained fire detector exists — this is stated plainly, not
//! papered over.
//!
//! ## Delivery rides the existing SPEC-011 sink layer
//!
//! Per SPEC-037's "no second notification transport" criterion, a Tier-0
//! alarm is not shipped over some bespoke Tier-0-only channel. It is
//! recorded as an ordinary `AlertEvent` and enqueued into
//! `alert_sink_outbox` for every *locally* deliverable sink (every
//! configured sink whose `kind()` is not `"cloud"` — LAN webhook,
//! SureView, email — none of which need the cloud tunnel to reach their
//! destination), exactly the mechanism `EngineSinkRouter` already uses
//! for ordinary rule-fires. The same M7 dispatcher (`nexus_sinks::dispatcher`)
//! drains that row later; this module never talks to a sink directly.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use nexus_config::EmergencyConfig;
use nexus_pipeline::EmergencyDispatch;
use nexus_sinks::emergency::{
    EmergencyDelivery, EmergencyOutcome, EmergencyPolicy, EmergencyRateLimiter, EmergencySignal,
    Tier0Class, Tier0Registry,
};
use nexus_sinks::SinkRegistry;
use nexus_store::Store;
use nexus_types::{AlertEvent, Artifacts, CameraId, Severity, TrackedObject};
use parking_lot::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::cloud_liveness::TunnelLiveness;

/// Per-(camera) state the stub fire signal needs to compute
/// [`EmergencySignal::persistence`] — how long the trigger label has
/// been continuously observed on that camera. Cleared the first frame
/// the label is absent, so a flicker does not accumulate stale
/// persistence across an unrelated gap.
type PersistenceMap = HashMap<CameraId, DateTime<Utc>>;

pub struct EngineEmergencyDispatch {
    cfg: EmergencyConfig,
    registry: Tier0Registry,
    policy: EmergencyPolicy,
    rate_limiter: Mutex<EmergencyRateLimiter>,
    persistence_since: Mutex<PersistenceMap>,
    liveness: Arc<TunnelLiveness>,
    started: Instant,
    store: Arc<Store>,
    sink_registry: Arc<SinkRegistry>,
}

impl EngineEmergencyDispatch {
    /// Build the dispatch. `liveness` is the SAME `Arc<TunnelLiveness>`
    /// the tunnel and go-dark watchdog update, so `cloud_reachable` is
    /// derived from the real connectivity signal — never a caller-chosen
    /// bool (BUG-163/BUG-165 defect class this programme keeps naming).
    #[must_use]
    pub fn new(cfg: EmergencyConfig, liveness: Arc<TunnelLiveness>, store: Arc<Store>, sink_registry: Arc<SinkRegistry>) -> Self {
        // Fire is the only Tier-0 class this wave gives a (stub)
        // edge-local detector. Registering it truthfully requires
        // `has_edge_local_detector = true` — the registry has no
        // "half a detector" state, and per owner ruling 4 a wired stub
        // IS the production seam, not a placeholder for one. The other
        // five Tier-0 classes are simply never registered here: an
        // unregistered class is `is_shipped() == false`, which is the
        // structurally honest state for "no signal source exists in
        // this dispatch" (violence/person-down's signed-off-exception
        // handling is SPEC-037's own registry test, out of this file).
        let mut registry = Tier0Registry::new();
        let _ = registry.register(Tier0Class::Fire, true);
        let rate_limit_window = Duration::from_secs(cfg.rate_limit_window_secs);
        Self {
            cfg,
            registry,
            policy: EmergencyPolicy::default(),
            rate_limiter: Mutex::new(EmergencyRateLimiter::new(rate_limit_window)),
            persistence_since: Mutex::new(HashMap::new()),
            liveness,
            started: Instant::now(),
            store,
            sink_registry,
        }
    }

    /// Owner ruling 4's stub: does `tracked` contain the configured
    /// trigger label for this camera, right now? Returns the persisted
    /// signal (with accumulated persistence duration) when it does, and
    /// clears this camera's persistence timer when it doesn't.
    fn stub_fire_signal(
        &self,
        camera_id: CameraId,
        tracked: &[TrackedObject],
        at: DateTime<Utc>,
    ) -> Option<EmergencySignal> {
        let seen = tracked.iter().any(|t| t.label == self.cfg.stub_fire_label);
        let mut since = self.persistence_since.lock();
        if !seen {
            since.remove(&camera_id);
            return None;
        }
        let first_seen = *since.entry(camera_id).or_insert(at);
        let persistence = (at - first_seen).to_std().unwrap_or(Duration::ZERO);
        Some(EmergencySignal {
            class: Tier0Class::Fire,
            persistence,
            brandish_confirmed: None,
        })
    }

    /// Every currently configured sink id whose kind is not `"cloud"` —
    /// the sinks that don't need the tunnel to be reachable. Resolved
    /// fresh on every dispatch so a hot-added/removed sink is honoured
    /// immediately, mirroring `EngineSinkRouter`'s live-read pattern.
    fn local_sink_ids(&self) -> Vec<String> {
        self.sink_registry
            .ids()
            .into_iter()
            .filter(|id| id.kind() != "cloud")
            .map(|id| id.to_string())
            .collect()
    }
}

#[async_trait::async_trait]
impl EmergencyDispatch for EngineEmergencyDispatch {
    async fn observe(&self, camera_id: CameraId, tracked: Arc<Vec<TrackedObject>>, at: DateTime<Utc>) {
        if !self.cfg.enabled {
            return;
        }
        let Some(signal) = self.stub_fire_signal(camera_id, &tracked, at) else {
            return;
        };
        if !self.registry.is_shipped(signal.class) {
            // Structurally unreachable given `new()` above, but the
            // registry — not this call site — is the source of truth
            // for "is this class actually Tier-0 today".
            return;
        }

        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let budget = Duration::from_secs(self.cfg.cloud_reachable_budget_secs);
        let cloud_reachable = self.liveness.is_reachable(elapsed_ms, budget);

        let outcome = self.policy.decide(&signal, cloud_reachable);
        match outcome {
            EmergencyOutcome::AwaitBrandishConfirmation => {
                // Fire never requires brandish confirmation, so this
                // arm is unreachable for the one class this dispatch
                // ships — kept exhaustive so a future class added here
                // doesn't silently fall through.
                debug!(camera_id, class = signal.class.as_str(), "emergency: awaiting confirmation (unexpected for this class)");
            }
            EmergencyOutcome::Alarm => {
                let camera_id_u64 = camera_id as u64;
                let allowed = self
                    .rate_limiter
                    .lock()
                    .allow(camera_id_u64, signal.class, at);
                if !allowed {
                    info!(
                        camera_id,
                        class = signal.class.as_str(),
                        "emergency: alarm rate-limited for this (camera, class); not persisted \
                         (no `emergency_rate_limited` outbox suppression row — this dispatch \
                         does not yet write one; see As-Built)"
                    );
                    return;
                }
                let delivery = EmergencyDelivery::deliver(camera_id_u64, signal.class, outcome, at);
                let sinks = self.local_sink_ids();
                if sinks.is_empty() {
                    warn!(
                        camera_id,
                        class = signal.class.as_str(),
                        "emergency: alarm decided but no locally-deliverable sink is configured; \
                         nothing to enqueue"
                    );
                    return;
                }
                let event = AlertEvent {
                    event_id: Uuid::now_v7(),
                    camera_id,
                    rule_id: format!("tier0:{}", signal.class.as_str()),
                    track_id: None,
                    label: signal.class.as_str().to_string(),
                    severity: Severity::Critical,
                    bbox: None,
                    frame_id: 0,
                    captured_at: delivery.delivered_at,
                    trace_id: format!("emergency-{}", delivery.delivered_at.timestamp_millis()),
                    frame_w: 0,
                    frame_h: 0,
                    artifacts: Artifacts::default(),
                    context: serde_json::Map::new(),
                };
                let sink_refs: Vec<&str> = sinks.iter().map(String::as_str).collect();
                if let Err(e) = self.store.record_event_and_enqueue(&event, &sink_refs).await {
                    warn!(camera_id, error = %e, "emergency: record_event_and_enqueue failed");
                } else {
                    info!(
                        camera_id,
                        class = signal.class.as_str(),
                        cloud_reachable,
                        sinks = sink_refs.len(),
                        "emergency: Tier-0 alarm enqueued on the local delivery path"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Integration tests wiring `EngineEmergencyDispatch` into a REAL
    //! `nexus_pipeline::spawn_camera` supervisor (VirtualSource → a
    //! test-local stub detector → tracker → static-filter), proving:
    //!
    //!   1. SPEC-037 "tunnel forced disconnected" — the tunnel never
    //!      authenticates (a real "down" `TunnelLiveness`, not a
    //!      hand-picked bool), a stub Tier-0 signal is seeded through the
    //!      real detector→tracker chain, and the alarm is delivered on a
    //!      LOCAL (non-`cloud`) sink via the same SPEC-011
    //!      `nexus_sinks::dispatcher` every ordinary alert uses — no
    //!      second transport.
    //!   2. SPEC-037 "cannot block a frame" — a deliberately slow
    //!      `EmergencyDispatch` (standing in for a slow real delivery;
    //!      owner ruling 3 permits a mock this plausible) is wired into
    //!      the same supervisor, and ordinary rule-fired alerts keep
    //!      arriving on the bus at their normal cadence while the slow
    //!      call is still in flight — proving the supervisor's own
    //!      frame loop never awaits it.

    use super::*;
    use async_trait::async_trait;
    use futures::StreamExt;
    use nexus_bus::{topic, BroadcastBus, Bus, BusExt};
    use nexus_config::{
        CameraConfig, CameraBehavior, CameraDetector, CameraIngest, ClipsConfig, RuleConfig,
        RuleDebounce, RuleGates, RulePredicate, RulesBackendKind, RulesConfig, StoreConfig,
        TrackerConfig,
    };
    use nexus_inference::{Detector, InferenceError};
    use nexus_pipeline::supervisor::spawn_camera;
    use nexus_pipeline::{ClipRecorder, NoopSinkRouter, StubClipRecorder};
    use nexus_rules::RuleEvaluator;
    use nexus_sinks::dispatcher::{self, AllowAllPolicy};
    use nexus_sinks::{AlertSink, SinkError, SinkId};
    use nexus_store::EventStore;
    use nexus_types::{BBox, Detection, Frame};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use url::Url;

    /// Test [`Detector`] that emits one fixed detection with a
    /// caller-chosen label every frame. Standing in for SPEC-036's
    /// unbuilt fire/smoke model per owner ruling 4 — this is the ONLY
    /// scripted part of these tests; everything downstream (tracker,
    /// static filter, `EngineEmergencyDispatch`'s stub-label scan,
    /// `EmergencyPolicy`, `EmergencyRateLimiter`, `EmergencyDelivery`,
    /// the store, and the M7 dispatcher) is the real, unmodified
    /// production code.
    struct FixedLabelDetector(&'static str);

    #[async_trait]
    impl Detector for FixedLabelDetector {
        async fn detect(
            &self,
            frame: &Frame,
            _prompts: &[String],
        ) -> Result<Vec<Detection>, InferenceError> {
            let w = frame.width as f32;
            let h = frame.height as f32;
            Ok(vec![Detection {
                label: self.0.to_string(),
                confidence: 0.95,
                bbox: BBox {
                    x1: w * 0.4,
                    y1: h * 0.4,
                    x2: w * 0.6,
                    y2: h * 0.9,
                },
                attributes: Default::default(),
            }])
        }

        fn name(&self) -> &'static str {
            "fixed-label-stub"
        }
    }

    /// Records every delivered [`AlertEvent`] it sees. Used both as a
    /// "local" sink (kind != "cloud") and, in the tunnel-down test, as a
    /// "cloud" sink so the test can assert the Tier-0 alarm reached ONLY
    /// the local one.
    struct RecordingSink {
        id: SinkId,
        kind: &'static str,
        delivered: Arc<Mutex<Vec<AlertEvent>>>,
    }

    #[async_trait]
    impl AlertSink for RecordingSink {
        fn kind(&self) -> &'static str {
            self.kind
        }
        fn id(&self) -> &SinkId {
            &self.id
        }
        async fn deliver(&self, event: &AlertEvent) -> Result<(), SinkError> {
            self.delivered.lock().push(event.clone());
            Ok(())
        }
    }

    async fn fresh_store() -> (Arc<Store>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let db_path = dir.path().join("nexus.db");
        let store = Store::open(&StoreConfig {
            url: format!("sqlite:{}?mode=rwc", db_path.display()),
            seed_from_config: false,
            duckdb_attach: false,
            duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
        })
        .await
        .expect("Store::open");
        (Arc::new(store), dir)
    }

    fn virtual_camera(id: i64, max_fps: u32) -> CameraConfig {
        CameraConfig {
            id,
            name: format!("emergency-test-{id}"),
            ingest: CameraIngest {
                url: Url::parse("virtual://local").unwrap(),
                enabled: true,
                max_fps,
                codec: None,
            },
            detector: CameraDetector {
                prompts: vec![],
                visual_prompts: vec![],
                model_override: None,
            },
            behavior: CameraBehavior {
                parking_lot_mode: false,
                anchor_ttl_secs: None,
                ..Default::default()
            },
            onvif: Default::default(),
            talk_down: Default::default(),
            zones: vec![],
        }
    }

    /// SPEC-037 — "Every shipped Tier-0 class fires with the tunnel
    /// forced disconnected — an integration test brings the tunnel
    /// down, seeds a detection, and asserts delivery on the local path."
    ///
    /// Fault injection: temporarily changed `local_sink_ids` to
    /// `self.sink_registry.ids().into_iter()...collect()` WITHOUT the
    /// `.filter(|id| id.kind() != "cloud")` — i.e. routed the emergency
    /// alarm to every configured sink, cloud included. This test failed
    /// on `assert_eq!(cloud_delivered.lock().len(), 0, ...)` (became 1):
    /// `assertion `left == right` failed: a Tier-0 alarm must never be
    /// routed to a cloud sink — left: 1, right: 0`. Reverted, GREEN
    /// restored, diff clean.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tunnel_down_seeds_a_detection_and_delivers_on_the_local_path() {
        // 1. A tunnel that never authenticated is a real "forced
        //    disconnected" tunnel per `TunnelLiveness::is_reachable`'s
        //    own contract (proven directly here, not assumed).
        let liveness = Arc::new(TunnelLiveness::new());
        assert!(
            !liveness.is_reachable(0, Duration::from_secs(90)),
            "test setup: a tunnel that never authenticated must be unreachable"
        );

        let (store, _dir) = fresh_store().await;
        let sink_registry = Arc::new(SinkRegistry::new());
        let local_delivered = Arc::new(Mutex::new(Vec::new()));
        let cloud_delivered = Arc::new(Mutex::new(Vec::new()));
        sink_registry.replace(vec![
            Arc::new(RecordingSink {
                id: SinkId::parse("webhook:local_test").unwrap(),
                kind: "webhook",
                delivered: local_delivered.clone(),
            }),
            Arc::new(RecordingSink {
                id: SinkId::parse("cloud:console").unwrap(),
                kind: "cloud",
                delivered: cloud_delivered.clone(),
            }),
        ]);

        let cfg = EmergencyConfig {
            enabled: true,
            stub_fire_label: "tier0_fire_stub".to_string(),
            cloud_reachable_budget_secs: 90,
            rate_limit_window_secs: 60,
        };
        let emergency: Arc<dyn EmergencyDispatch> = Arc::new(EngineEmergencyDispatch::new(
            cfg,
            liveness.clone(),
            store.clone(),
            sink_registry.clone(),
        ));

        let cam = virtual_camera(101, 5);
        store.upsert_camera(&cam).await.expect("seed camera row");
        let dir2 = tempfile::tempdir().expect("clips tmpdir");
        let recorder: Arc<dyn ClipRecorder> =
            Arc::new(StubClipRecorder::new(store.clone(), dir2.path().join("clips")));
        let evaluator = Arc::new(
            RuleEvaluator::new(
                &RulesConfig {
                    backend: RulesBackendKind::Cel,
                    ..Default::default()
                },
                &[],
            )
            .expect("compile empty ruleset"),
        );
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(16));

        let handle = spawn_camera(
            cam,
            Arc::new(FixedLabelDetector("tier0_fire_stub")),
            None,
            Arc::from(nexus_tracker::build_tracker(&TrackerConfig::default())),
            TrackerConfig::default().annotator.clone(),
            TrackerConfig::default().static_object.clone(),
            ClipsConfig::default(),
            std::env::temp_dir(),
            evaluator,
            store.clone(),
            recorder,
            bus.clone(),
            Arc::new(nexus_pipeline::LatestFrameCache::new()),
            Arc::new(nexus_pipeline::FrameStatsRegistry::new()),
            nexus_pipeline::StaticAnchorClearRegistry::new(),
            960,
            540,
            Arc::new(nexus_pipeline::NoopSightingHook),
            nexus_pipeline::supervisor::SightingSchedulerConfig::default(),
            Vec::new(),
            Arc::new(nexus_pipeline::NoopEntityLocalPersist),
            None,
            Arc::new(NoopSinkRouter),
            Arc::new(nexus_pipeline::NoopAlertClipScheduleGate),
            emergency,
        );

        // 2. Poll for the emergency path's own event row — up to 5s,
        //    generous for a 5fps virtual camera plus the spawned
        //    per-frame `observe()` task.
        let mut found_event_id: Option<String> = None;
        for _ in 0..250 {
            let recent = store.list_recent_events(50).await.expect("list_recent_events");
            if let Some(ev) = recent.iter().find(|e| e.rule_id == "tier0:fire") {
                found_event_id = Some(ev.event_id.to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        handle.task.abort();
        let event_id = found_event_id.expect(
            "a Tier-0 fire event (rule_id = 'tier0:fire') must be recorded within 5s",
        );

        // 3. Exactly one outbox row, routed to the local sink only.
        let outbox = store.outbox_for_event(&event_id).await.expect("outbox_for_event");
        assert_eq!(
            outbox.len(),
            1,
            "a Tier-0 alarm must enqueue exactly one outbox row (the local sink); cloud is excluded"
        );
        assert_eq!(outbox[0].sink_id, "webhook:local_test");
        assert_eq!(outbox[0].status, nexus_store::OutboxStatus::Pending);

        // 4. Actually drive delivery through the REAL M7 dispatcher
        //    (`nexus_sinks::dispatcher::process_row`) — the same
        //    machinery every ordinary alert uses, per the "no second
        //    transport" criterion.
        dispatcher::process_row(
            &store,
            &sink_registry,
            &AllowAllPolicy,
            None,
            None,
            outbox[0].clone(),
        )
        .await;

        assert_eq!(
            local_delivered.lock().len(),
            1,
            "the local sink must have actually received the delivered alert"
        );
        assert_eq!(local_delivered.lock()[0].rule_id, "tier0:fire");
        assert_eq!(
            cloud_delivered.lock().len(),
            0,
            "a Tier-0 alarm must never be routed to a cloud sink"
        );
        let after = store.outbox_for_event(&event_id).await.expect("outbox_for_event");
        assert_eq!(after[0].status, nexus_store::OutboxStatus::Sent);
    }

    /// A deliberately slow [`EmergencyDispatch`]. Stands in for a slow
    /// real delivery (network I/O, a lock, disk) — owner ruling 3
    /// permits a mock this plausible; the point of this test is that
    /// NOTHING about the supervisor's frame loop can be made to wait on
    /// it, regardless of how slow it is.
    struct SlowEmergencyDispatch {
        calls_started: Arc<AtomicUsize>,
        calls_finished: Arc<AtomicUsize>,
        sleep: Duration,
    }

    #[async_trait]
    impl EmergencyDispatch for SlowEmergencyDispatch {
        async fn observe(
            &self,
            _camera_id: nexus_types::CameraId,
            _tracked: Arc<Vec<TrackedObject>>,
            _at: DateTime<Utc>,
        ) {
            self.calls_started.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.sleep).await;
            self.calls_finished.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// SPEC-037 — "Detection, recording, and rule evaluation are
    /// unaffected by the emergency path — a Tier-0 fire does not stall
    /// the pipeline, and the alert emission cannot block a frame."
    ///
    /// Fault injection: temporarily changed the supervisor's emergency
    /// call site (`crates/nexus-pipeline/src/supervisor.rs`) from
    /// spawning `emergency.observe(...)` in its own task to `.await`ing
    /// it inline on the frame loop. This test failed — with a 2s sleep
    /// per `observe()` call and a 20fps camera (50ms/frame), the ordinary
    /// `any_person` rule-fire alert count observed within the 1200ms
    /// window dropped from "several" to 0/1 (`assert!(alerts.len() >= 3,
    /// ...)` failed with `left: 0`, or `1`), because every frame now
    /// blocked on the 2s sleep. Reverted, GREEN restored, diff clean.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emergency_dispatch_never_blocks_detection_recording_or_rule_evaluation() {
        let bus: Arc<dyn Bus> = Arc::new(BroadcastBus::new(64));
        let mut sub = bus
            .subscribe::<AlertEvent>(topic::ALERT_EVENT)
            .await
            .expect("subscribe alert.event");

        let rule = RuleConfig {
            id: "any_person".into(),
            name: "Any person".into(),
            predicate: RulePredicate {
                when: "object.label == 'person'".into(),
                severity: "low".into(),
            },
            gates: RuleGates {
                camera_filter: None,
                zones: None,
            },
            debounce: RuleDebounce {
                min_track_age_ms: 0,
                consecutive_frames: 1,
                cooldown_ms: 0,
            },
            enabled: true,
            sinks: Vec::new(),
            verify: false,
        };
        let evaluator = Arc::new(
            RuleEvaluator::new(
                &RulesConfig {
                    backend: RulesBackendKind::Cel,
                    ..Default::default()
                },
                &[rule],
            )
            .expect("compile cel rule"),
        );

        let (store, _dir) = fresh_store().await;
        let cam = virtual_camera(202, 20);
        store.upsert_camera(&cam).await.expect("seed camera row");
        let dir2 = tempfile::tempdir().expect("clips tmpdir");
        let recorder: Arc<dyn ClipRecorder> =
            Arc::new(StubClipRecorder::new(store.clone(), dir2.path().join("clips")));

        let calls_started = Arc::new(AtomicUsize::new(0));
        let calls_finished = Arc::new(AtomicUsize::new(0));
        let slow_emergency: Arc<dyn EmergencyDispatch> = Arc::new(SlowEmergencyDispatch {
            calls_started: calls_started.clone(),
            calls_finished: calls_finished.clone(),
            sleep: Duration::from_secs(2),
        });

        let handle = spawn_camera(
            cam,
            Arc::new(FixedLabelDetector("person")),
            None,
            Arc::from(nexus_tracker::build_tracker(&TrackerConfig::default())),
            TrackerConfig::default().annotator.clone(),
            TrackerConfig::default().static_object.clone(),
            ClipsConfig::default(),
            std::env::temp_dir(),
            evaluator,
            store.clone(),
            recorder,
            bus.clone(),
            Arc::new(nexus_pipeline::LatestFrameCache::new()),
            Arc::new(nexus_pipeline::FrameStatsRegistry::new()),
            nexus_pipeline::StaticAnchorClearRegistry::new(),
            960,
            540,
            Arc::new(nexus_pipeline::NoopSightingHook),
            nexus_pipeline::supervisor::SightingSchedulerConfig::default(),
            Vec::new(),
            Arc::new(nexus_pipeline::NoopEntityLocalPersist),
            None,
            Arc::new(NoopSinkRouter),
            Arc::new(nexus_pipeline::NoopAlertClipScheduleGate),
            slow_emergency,
        );

        // Collect ordinary rule-fired alerts for 1200ms — well under the
        // slow dispatch's 2s sleep. At 20fps that's ~24 frames' worth of
        // opportunity; a blocked frame loop would deliver 0 or 1.
        let mut alerts = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, sub.next()).await {
                Ok(Some(Ok(ev))) => alerts.push(ev),
                _ => break,
            }
        }
        handle.task.abort();

        assert!(
            calls_started.load(Ordering::SeqCst) >= 1,
            "the slow emergency dispatch must actually have been invoked"
        );
        assert_eq!(
            calls_finished.load(Ordering::SeqCst),
            0,
            "within the 700ms observation window nothing should have finished a 2s sleep — \
             if this is nonzero the test window/sleep ratio needs revisiting, not the assertion below"
        );
        assert!(
            alerts.len() >= 3,
            "rule-fired alerts must keep arriving at the camera's normal cadence while the \
             emergency dispatch is stuck mid-flight; got {} within 1200ms",
            alerts.len()
        );
    }
}

//! M4 closeout — CEL rule corpus + golden fixture suite.
//!
//! Two related goals:
//!
//! 1. **Corpus**: ship a v2-native catalog of CEL rules that exercises
//!    every attribute the [`TrackAnnotator`](nexus_tracker::annotator)
//!    stamps onto a [`TrackedObject`], plus the static-object trigger
//!    (`motion.parked_vehicle`). This proves the rule layer is wired
//!    end-to-end against the attributes that actually exist in
//!    production today, and serves as copy-paste-ready examples for
//!    customer rule editors.
//!
//! 2. **Golden fixtures**: a table-driven test that pins per-rule
//!    `(object → expected verdict)` pairs. Any accidental regression
//!    in CEL value-coercion, attribute-name renames, or json↔CEL
//!    mapping shows up here as a single failed assertion with the
//!    fixture name, not a mystery break elsewhere.
//!
//! Attributes covered by the corpus:
//!
//! | Attribute                 | Source                              | Rule IDs                                  |
//! |---------------------------|-------------------------------------|-------------------------------------------|
//! | `object.label`            | detector                            | `person_high_confidence`, `specific_camera` |
//! | `object.confidence`       | detector                            | `person_high_confidence`                  |
//! | `object.box.{w,h}`        | detector                            | `large_object`                            |
//! | `object.age_ms`           | tracker                             | `mature_track`                            |
//! | `motion.speed_class`      | annotator                           | `running_person`, `vehicle_at_speed`      |
//! | `motion.direction`        | annotator                           | `eastbound_vehicle`, `northbound_person`  |
//! | `motion.parked_vehicle`   | annotator (static-object trigger)   | `parked_vehicle_alert`                    |
//! | `motion.dwell_seconds`    | annotator                           | `loitering`                               |
//! | `motion.zone_state`       | annotator                           | `zone_breach_entering`, `zone_occupied`   |
//! | `group.size`              | annotator                           | `crowd_forming`                           |
//! | `camera.id`               | engine context                      | `specific_camera`                         |
//! | `now.hour`                | engine context                      | `after_hours` (compile-only, time-flaky)  |
//! | `now.day_of_week`         | engine context                      | `weekend_activity` (compile-only)         |
//!
//! Note on `static-object filter` interaction: when
//! `cameras[*].parking_lot_mode = true` the supervisor drops parked
//! vehicles before they reach the rule engine, so `parked_vehicle_alert`
//! only fires when parking_lot_mode is OFF (the default). The annotator
//! stamps `motion.parked_vehicle` either way — the filter is what
//! decides whether the rule layer ever sees it.

use nexus_config::{RuleConfig, RulesBackendKind, RulesConfig};
use nexus_rules::{CelEngine, RuleEngine, RuleEvaluator};
use nexus_types::{BBox, CameraId, TrackedObject};
use serde_json::json;

// ---------------------------------------------------------------------------
// Rule corpus — every rule customers can write against today's pipeline.
// ---------------------------------------------------------------------------

struct RuleSpec {
    id: &'static str,
    name: &'static str,
    when: &'static str,
    severity: &'static str,
}

const RULE_CORPUS: &[RuleSpec] = &[
    RuleSpec {
        id: "person_high_confidence",
        name: "Person seen with high confidence",
        when: "object.label == 'person' && object.confidence >= 0.7",
        severity: "medium",
    },
    RuleSpec {
        id: "running_person",
        name: "Person running",
        when: "object.label == 'person' && \
               object.attributes['motion.speed_class'] == 'running'",
        severity: "high",
    },
    RuleSpec {
        id: "vehicle_at_speed",
        name: "Vehicle at speed",
        when: "object.label.startsWith('vehicle') && \
               object.attributes['motion.speed_class'] == 'vehicle_speed'",
        severity: "high",
    },
    RuleSpec {
        id: "eastbound_vehicle",
        name: "Eastbound vehicle",
        when: "object.label.startsWith('vehicle') && \
               object.attributes['motion.direction'] == 'e'",
        severity: "low",
    },
    RuleSpec {
        id: "northbound_person",
        name: "Northbound person",
        when: "object.label == 'person' && \
               object.attributes['motion.direction'] == 'n'",
        severity: "low",
    },
    RuleSpec {
        id: "parked_vehicle_alert",
        name: "Vehicle parked (parking_lot_mode=false)",
        when: "object.label.startsWith('vehicle') && \
               object.attributes['motion.parked_vehicle'] == 'yes'",
        severity: "medium",
    },
    RuleSpec {
        id: "loitering",
        name: "Person loitering >= 60s",
        when: "object.label == 'person' && \
               object.attributes['motion.dwell_seconds'] >= 60",
        severity: "high",
    },
    RuleSpec {
        id: "zone_breach_entering",
        name: "Track entering any inclusion zone",
        when: "object.attributes['motion.zone_state'] == 'entering'",
        severity: "critical",
    },
    RuleSpec {
        id: "zone_occupied",
        name: "Track inside any inclusion zone",
        when: "object.attributes['motion.zone_state'] == 'inside'",
        severity: "low",
    },
    RuleSpec {
        // Zone-targeted rule — requires the annotator's
        // `motion.zone_ids` attribute (list of inclusion/dwell zone
        // IDs the object is currently inside). Lets a single rule
        // discriminate between zones on the same camera without
        // needing one rule per zone.
        id: "parking_zone_breach",
        name: "Person entering the parking zone",
        when: "object.label == 'person' && \
               'parking' in object.attributes['motion.zone_ids'] && \
               object.attributes['motion.zone_state'] == 'entering'",
        severity: "critical",
    },
    RuleSpec {
        id: "crowd_forming",
        name: "3+ people grouped together",
        when: "object.label == 'person' && \
               object.attributes['group.size'] >= 3",
        severity: "high",
    },
    RuleSpec {
        id: "large_object",
        name: "Detection bbox larger than 50_000 px²",
        when: "object.box.width * object.box.height > 50000.0",
        severity: "low",
    },
    RuleSpec {
        id: "mature_track",
        name: "Person seen for at least 5s",
        when: "object.label == 'person' && object.age_ms >= 5000",
        severity: "low",
    },
    RuleSpec {
        id: "specific_camera",
        name: "Front-door person",
        when: "camera.id == 1 && object.label == 'person'",
        severity: "medium",
    },
    // Time-of-day / day-of-week rules — included for compile-coverage
    // only; their golden fixtures would be time-flaky.
    RuleSpec {
        id: "after_hours",
        name: "Person seen 22:00–06:00",
        when: "object.label == 'person' && (now.hour >= 22 || now.hour < 6)",
        severity: "high",
    },
    RuleSpec {
        id: "weekend_activity",
        name: "Any track on Sat/Sun",
        when: "now.day_of_week >= 6",
        severity: "low",
    },
];

fn corpus_to_rule_configs() -> Vec<RuleConfig> {
    RULE_CORPUS
        .iter()
        .map(|r| RuleConfig {
            id: r.id.into(),
            name: r.name.into(),
            predicate: nexus_config::RulePredicate {
                when: r.when.into(),
                severity: r.severity.into(),
            },
            gates: nexus_config::RuleGates {
                camera_filter: None,
                zones: None,
            },
            debounce: nexus_config::RuleDebounce {
                min_track_age_ms: 0,
                consecutive_frames: 1,
                cooldown_ms: 0,
            },
            enabled: true,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Object builder — every fixture starts from a sensible baseline and
// overrides only the bits it cares about.
// ---------------------------------------------------------------------------

struct ObjectBuilder {
    label: String,
    confidence: f32,
    bbox: BBox,
    age_ms: u64,
    age_frames: u32,
    track_id: u64,
    attributes: serde_json::Map<String, serde_json::Value>,
}

impl ObjectBuilder {
    fn new(label: &str) -> Self {
        Self {
            label: label.into(),
            confidence: 0.85,
            bbox: BBox {
                x1: 100.0,
                y1: 100.0,
                x2: 200.0,
                y2: 250.0, // 100×150 = 15_000 px²
            },
            age_ms: 1500,
            age_frames: 30,
            track_id: 1,
            attributes: Default::default(),
        }
    }

    fn confidence(mut self, c: f32) -> Self {
        self.confidence = c;
        self
    }

    fn bbox(mut self, x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        self.bbox = BBox { x1, y1, x2, y2 };
        self
    }

    fn age_ms(mut self, ms: u64) -> Self {
        self.age_ms = ms;
        self
    }

    fn attr(mut self, key: &str, value: serde_json::Value) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }

    fn build(self) -> TrackedObject {
        TrackedObject {
            track_id: self.track_id,
            label: self.label,
            confidence: self.confidence,
            bbox: self.bbox,
            age_frames: self.age_frames,
            age_ms: self.age_ms,
            attributes: self.attributes,
        }
    }
}

// ---------------------------------------------------------------------------
// Golden fixture table — every (rule, object, expected) tuple.
// ---------------------------------------------------------------------------

struct Fixture {
    name: &'static str,
    rule_id: &'static str,
    camera_id: CameraId,
    expected: bool,
    object: fn() -> TrackedObject,
}

fn fixtures() -> Vec<Fixture> {
    vec![
        // --- person_high_confidence ----------------------------------------
        Fixture {
            name: "person_high_confidence: high-conf person matches",
            rule_id: "person_high_confidence",
            camera_id: 1,
            expected: true,
            object: || ObjectBuilder::new("person").confidence(0.9).build(),
        },
        Fixture {
            name: "person_high_confidence: low-conf person rejected",
            rule_id: "person_high_confidence",
            camera_id: 1,
            expected: false,
            object: || ObjectBuilder::new("person").confidence(0.5).build(),
        },
        Fixture {
            name: "person_high_confidence: high-conf dog rejected (label mismatch)",
            rule_id: "person_high_confidence",
            camera_id: 1,
            expected: false,
            object: || ObjectBuilder::new("dog").confidence(0.95).build(),
        },
        // --- running_person ------------------------------------------------
        Fixture {
            name: "running_person: running person matches",
            rule_id: "running_person",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.speed_class", json!("running"))
                    .build()
            },
        },
        Fixture {
            name: "running_person: walking person rejected",
            rule_id: "running_person",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.speed_class", json!("walking"))
                    .build()
            },
        },
        Fixture {
            name: "running_person: running vehicle rejected (label mismatch)",
            rule_id: "running_person",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("vehicle.car")
                    .attr("motion.speed_class", json!("running"))
                    .build()
            },
        },
        // --- vehicle_at_speed ----------------------------------------------
        Fixture {
            name: "vehicle_at_speed: vehicle.car at vehicle_speed matches",
            rule_id: "vehicle_at_speed",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("vehicle.car")
                    .attr("motion.speed_class", json!("vehicle_speed"))
                    .build()
            },
        },
        Fixture {
            name: "vehicle_at_speed: vehicle.truck at vehicle_speed matches (prefix)",
            rule_id: "vehicle_at_speed",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("vehicle.truck")
                    .attr("motion.speed_class", json!("vehicle_speed"))
                    .build()
            },
        },
        Fixture {
            name: "vehicle_at_speed: walking vehicle rejected (speed mismatch)",
            rule_id: "vehicle_at_speed",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("vehicle.car")
                    .attr("motion.speed_class", json!("walking"))
                    .build()
            },
        },
        // --- eastbound_vehicle ---------------------------------------------
        Fixture {
            name: "eastbound_vehicle: vehicle heading east matches",
            rule_id: "eastbound_vehicle",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("vehicle.car")
                    .attr("motion.direction", json!("e"))
                    .build()
            },
        },
        Fixture {
            name: "eastbound_vehicle: vehicle heading west rejected",
            rule_id: "eastbound_vehicle",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("vehicle.car")
                    .attr("motion.direction", json!("w"))
                    .build()
            },
        },
        Fixture {
            name: "eastbound_vehicle: stationary vehicle (direction=none) rejected",
            rule_id: "eastbound_vehicle",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("vehicle.car")
                    .attr("motion.direction", json!("none"))
                    .build()
            },
        },
        // --- northbound_person ---------------------------------------------
        Fixture {
            name: "northbound_person: person heading north matches",
            rule_id: "northbound_person",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.direction", json!("n"))
                    .build()
            },
        },
        Fixture {
            name: "northbound_person: person heading northeast rejected (exact match)",
            rule_id: "northbound_person",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.direction", json!("ne"))
                    .build()
            },
        },
        // --- parked_vehicle_alert ------------------------------------------
        Fixture {
            name: "parked_vehicle_alert: parked vehicle.car matches",
            rule_id: "parked_vehicle_alert",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("vehicle.car")
                    .attr("motion.parked_vehicle", json!("yes"))
                    .build()
            },
        },
        Fixture {
            name: "parked_vehicle_alert: moving vehicle rejected",
            rule_id: "parked_vehicle_alert",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("vehicle.car")
                    .attr("motion.parked_vehicle", json!("no"))
                    .build()
            },
        },
        Fixture {
            name: "parked_vehicle_alert: parked person rejected (annotator never \
                   stamps parked_vehicle on non-vehicle, but defensive coverage)",
            rule_id: "parked_vehicle_alert",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.parked_vehicle", json!("yes"))
                    .build()
            },
        },
        // --- loitering -----------------------------------------------------
        Fixture {
            name: "loitering: person dwelling 90s matches",
            rule_id: "loitering",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.dwell_seconds", json!(90))
                    .build()
            },
        },
        Fixture {
            name: "loitering: person dwelling 60s matches (boundary)",
            rule_id: "loitering",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.dwell_seconds", json!(60))
                    .build()
            },
        },
        Fixture {
            name: "loitering: person dwelling 59s rejected (boundary)",
            rule_id: "loitering",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.dwell_seconds", json!(59))
                    .build()
            },
        },
        // --- zone_breach_entering ------------------------------------------
        Fixture {
            name: "zone_breach_entering: entering matches",
            rule_id: "zone_breach_entering",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.zone_state", json!("entering"))
                    .build()
            },
        },
        Fixture {
            name: "zone_breach_entering: inside rejected (only the boundary fires)",
            rule_id: "zone_breach_entering",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.zone_state", json!("inside"))
                    .build()
            },
        },
        Fixture {
            name: "zone_breach_entering: outside rejected",
            rule_id: "zone_breach_entering",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.zone_state", json!("outside"))
                    .build()
            },
        },
        // --- zone_occupied -------------------------------------------------
        Fixture {
            name: "zone_occupied: inside matches",
            rule_id: "zone_occupied",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.zone_state", json!("inside"))
                    .build()
            },
        },
        Fixture {
            name: "zone_occupied: entering rejected (different state)",
            rule_id: "zone_occupied",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.zone_state", json!("entering"))
                    .build()
            },
        },
        // --- parking_zone_breach (zone-id targeted) ------------------------
        Fixture {
            name: "parking_zone_breach: entering parking matches",
            rule_id: "parking_zone_breach",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.zone_state", json!("entering"))
                    .attr("motion.zone_ids", json!(["parking"]))
                    .build()
            },
        },
        Fixture {
            name: "parking_zone_breach: entering loading_dock does NOT match (wrong zone)",
            rule_id: "parking_zone_breach",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.zone_state", json!("entering"))
                    .attr("motion.zone_ids", json!(["loading_dock"]))
                    .build()
            },
        },
        Fixture {
            name: "parking_zone_breach: inside parking does NOT match (wrong state)",
            rule_id: "parking_zone_breach",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.zone_state", json!("inside"))
                    .attr("motion.zone_ids", json!(["parking"]))
                    .build()
            },
        },
        Fixture {
            name: "parking_zone_breach: entering both zones matches (parking is in the list)",
            rule_id: "parking_zone_breach",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("person")
                    .attr("motion.zone_state", json!("entering"))
                    .attr("motion.zone_ids", json!(["parking", "loading_dock"]))
                    .build()
            },
        },
        // --- crowd_forming -------------------------------------------------
        Fixture {
            name: "crowd_forming: group of 3 people matches (boundary)",
            rule_id: "crowd_forming",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("person")
                    .attr("group.size", json!(3))
                    .build()
            },
        },
        Fixture {
            name: "crowd_forming: group of 5 people matches",
            rule_id: "crowd_forming",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("person")
                    .attr("group.size", json!(5))
                    .build()
            },
        },
        Fixture {
            name: "crowd_forming: group of 2 people rejected (boundary)",
            rule_id: "crowd_forming",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("person")
                    .attr("group.size", json!(2))
                    .build()
            },
        },
        Fixture {
            name: "crowd_forming: solo dog rejected (label + count)",
            rule_id: "crowd_forming",
            camera_id: 1,
            expected: false,
            object: || {
                ObjectBuilder::new("dog")
                    .attr("group.size", json!(5))
                    .build()
            },
        },
        // --- large_object --------------------------------------------------
        Fixture {
            name: "large_object: 300×200 = 60_000 px² matches",
            rule_id: "large_object",
            camera_id: 1,
            expected: true,
            object: || {
                ObjectBuilder::new("vehicle.truck")
                    .bbox(0.0, 0.0, 300.0, 200.0)
                    .build()
            },
        },
        Fixture {
            name: "large_object: 100×150 = 15_000 px² rejected (default builder)",
            rule_id: "large_object",
            camera_id: 1,
            expected: false,
            object: || ObjectBuilder::new("person").build(),
        },
        // --- mature_track --------------------------------------------------
        Fixture {
            name: "mature_track: person at 5s matches (boundary)",
            rule_id: "mature_track",
            camera_id: 1,
            expected: true,
            object: || ObjectBuilder::new("person").age_ms(5000).build(),
        },
        Fixture {
            name: "mature_track: person at 4.999s rejected (boundary)",
            rule_id: "mature_track",
            camera_id: 1,
            expected: false,
            object: || ObjectBuilder::new("person").age_ms(4999).build(),
        },
        // --- specific_camera -----------------------------------------------
        Fixture {
            name: "specific_camera: person on camera 1 matches",
            rule_id: "specific_camera",
            camera_id: 1,
            expected: true,
            object: || ObjectBuilder::new("person").build(),
        },
        Fixture {
            name: "specific_camera: person on camera 2 rejected",
            rule_id: "specific_camera",
            camera_id: 2,
            expected: false,
            object: || ObjectBuilder::new("person").build(),
        },
        Fixture {
            name: "specific_camera: dog on camera 1 rejected",
            rule_id: "specific_camera",
            camera_id: 1,
            expected: false,
            object: || ObjectBuilder::new("dog").build(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn every_corpus_rule_compiles() {
    let engine = CelEngine::new();
    let mut failures = Vec::new();
    for spec in RULE_CORPUS {
        let cfg = RuleConfig {
            id: spec.id.into(),
            name: spec.name.into(),
            predicate: nexus_config::RulePredicate {
                when: spec.when.into(),
                severity: spec.severity.into(),
            },
            gates: nexus_config::RuleGates {
                camera_filter: None,
                zones: None,
            },
            debounce: nexus_config::RuleDebounce {
                min_track_age_ms: 0,
                consecutive_frames: 1,
                cooldown_ms: 0,
            },
            enabled: true,
        };
        if let Err(e) = engine.compile(&cfg) {
            failures.push(format!("{}: {}", spec.id, e));
        }
    }
    assert!(
        failures.is_empty(),
        "rules failed to compile:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn corpus_loads_into_rule_evaluator() {
    // Round-trip: every rule the corpus exposes must also be loadable
    // by the higher-level evaluator (which is what the engine actually
    // uses). Catches drift between standalone CelEngine + RuleEvaluator
    // wrappers.
    let cfg = RulesConfig {
        backend: RulesBackendKind::Cel,
        inline: corpus_to_rule_configs(),
    };
    let evaluator =
        RuleEvaluator::new(&cfg, &cfg.inline).expect("corpus should load into RuleEvaluator");
    assert_eq!(evaluator.engine_kind(), RulesBackendKind::Cel);
}

#[test]
fn golden_fixtures_match_expected_verdicts() {
    let engine = CelEngine::new();
    let mut compiled_by_id = std::collections::HashMap::new();
    for spec in RULE_CORPUS {
        let cfg = RuleConfig {
            id: spec.id.into(),
            name: spec.name.into(),
            predicate: nexus_config::RulePredicate {
                when: spec.when.into(),
                severity: spec.severity.into(),
            },
            gates: nexus_config::RuleGates {
                camera_filter: None,
                zones: None,
            },
            debounce: nexus_config::RuleDebounce {
                min_track_age_ms: 0,
                consecutive_frames: 1,
                cooldown_ms: 0,
            },
            enabled: true,
        };
        let compiled = engine
            .compile(&cfg)
            .unwrap_or_else(|e| panic!("setup: {} failed to compile: {e}", spec.id));
        compiled_by_id.insert(spec.id, compiled);
    }

    let mut failures = Vec::new();
    for fx in fixtures() {
        let compiled = compiled_by_id.get(fx.rule_id).unwrap_or_else(|| {
            panic!("fixture {} references unknown rule {}", fx.name, fx.rule_id)
        });
        let object = (fx.object)();
        let actual = match engine.matches(compiled, &object, fx.camera_id) {
            Ok(b) => b,
            Err(e) => {
                failures.push(format!(
                    "[{}] rule {}: evaluation error: {e}",
                    fx.name, fx.rule_id
                ));
                continue;
            }
        };
        if actual != fx.expected {
            failures.push(format!(
                "[{}] rule {}: expected {}, got {}",
                fx.name, fx.rule_id, fx.expected, actual
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} fixture(s) failed:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

#[test]
fn rule_corpus_has_no_duplicate_ids() {
    let mut seen = std::collections::HashSet::new();
    for spec in RULE_CORPUS {
        assert!(
            seen.insert(spec.id),
            "duplicate rule id in corpus: {}",
            spec.id
        );
    }
}

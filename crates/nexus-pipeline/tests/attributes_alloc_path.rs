//! Allocation bounds for the rule stage of the supervisor's per-frame
//! analysis path (F1).
//!
//! The rule evaluator used to rebuild a CEL `Context` and re-convert the whole
//! object, attributes included, through a `serde_json::Value` for every
//! (rule, object) pair: ~166 allocations per pair, 52 of them the attributes.
//! It now binds each object once per frame and evaluates every rule against
//! that binding in one reused `Context`. These tests pin that shape on
//! objects produced by the real tracker + annotator chain:
//!
//! * an extra rule costs a bounded handful of allocations per object, and
//! * the attributes are converted once per object, not once per rule.
//!
//! Counting is per thread (`#[global_allocator]` + thread-locals), so the
//! numbers are deterministic and parallel tests cannot pollute each other.
//! Lives here because nexus-pipeline is the lowest crate that depends on both
//! nexus-tracker and nexus-rules; the chain mirrors `supervisor.rs`.
//!
//! `cargo test -p nexus-pipeline --test attributes_alloc_path -- --nocapture`
//! also prints a per-stage census of the whole post-inference path.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use nexus_config::{
    AnnotatorConfig, ByteTrackConfig, RuleConfig, RuleDebounce, RuleGates, RulePredicate,
    RulesConfig, ZoneConfig, ZoneKind,
};
use nexus_rules::RuleEvaluator;
use nexus_tracker::{ByteTrackTracker, MotionEventEmitter, TrackAnnotator, Tracker};
use nexus_types::{BBox, Detection, Frame, PixelFormat, TrackedObject};

struct Counting;

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    let _ = ON.try_with(|on| {
        if on.get() {
            let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note();
        System.alloc(l)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        note();
        System.alloc_zeroed(l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        note();
        System.realloc(p, l, new)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocs<R>(f: impl FnOnce() -> R) -> (R, u64) {
    ALLOCS.with(|c| c.set(0));
    ON.with(|c| c.set(true));
    let r = f();
    ON.with(|c| c.set(false));
    (r, ALLOCS.with(Cell::get))
}

const W: u32 = 1536;
const H: u32 = 864;
const PERSONS: usize = 16;
const VEHICLES: usize = 4;
const WARMUP: u64 = 30;
const FRAMES: u64 = 100;

fn frame(i: u64) -> Frame {
    Frame {
        camera_id: 1,
        frame_id: i,
        captured_at: Utc
            .timestamp_millis_opt(1_700_000_000_000 + i as i64 * 33)
            .unwrap(),
        width: W,
        height: H,
        format: PixelFormat::Rgb24,
        data: Arc::new(Vec::new()),
        trace_id: String::new(),
    }
}

/// PERSONS walking right at 1 px/frame on a grid that stays in frame,
/// VEHICLES parked: no track churn, every track matched every frame.
fn detections(i: u64) -> Vec<Detection> {
    let mut out = Vec::with_capacity(PERSONS + VEHICLES);
    for p in 0..PERSONS {
        let x = 40.0 + (p % 8) as f32 * 140.0 + i as f32;
        let y = 60.0 + (p / 8) as f32 * 250.0;
        out.push(Detection {
            label: "person".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: x,
                y1: y,
                x2: x + 50.0,
                y2: y + 140.0,
            },
            attributes: Default::default(),
        });
    }
    for v in 0..VEHICLES {
        let x = 100.0 + v as f32 * 350.0;
        out.push(Detection {
            label: "vehicle.car".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: x,
                y1: 650.0,
                x2: x + 220.0,
                y2: 770.0,
            },
            attributes: Default::default(),
        });
    }
    out
}

fn zones() -> Vec<ZoneConfig> {
    vec![
        ZoneConfig {
            id: "walkway".into(),
            name: "Walkway".into(),
            polygon: vec![(0.0, 0.0), (1.0, 0.0), (1.0, 0.5), (0.0, 0.5)],
            kind: ZoneKind::Inclusion,
            min_bbox_area_px_override: None,
        },
        ZoneConfig {
            id: "parking".into(),
            name: "Parking".into(),
            polygon: vec![(0.0, 0.6), (1.0, 0.6), (1.0, 1.0), (0.0, 1.0)],
            kind: ZoneKind::Dwell,
            min_bbox_area_px_override: None,
        },
    ]
}

fn rule(id: &str, when: &str) -> RuleConfig {
    RuleConfig {
        id: id.into(),
        name: id.into(),
        predicate: RulePredicate {
            when: when.into(),
            severity: "low".into(),
        },
        gates: RuleGates::default(),
        // ByteTrack's age_ms is wall-clock and this loop runs far faster than
        // real time; the default 500 ms would skip every object.
        debounce: RuleDebounce {
            min_track_age_ms: 0,
            ..Default::default()
        },
        enabled: true,
        sinks: Vec::new(),
        verify: false,
    }
}

/// `n` label-only rules that never match, so no alert is ever built and the
/// count is the binding + evaluation cost alone.
fn never_matching(n: usize) -> RuleEvaluator {
    let rules: Vec<RuleConfig> = (0..n)
        .map(|i| rule(&format!("r{i}"), "object.label == 'nobody'"))
        .collect();
    RuleEvaluator::new(&RulesConfig::default(), &rules).unwrap()
}

/// The real tracker + annotator output for each measured frame.
fn scenario() -> Vec<(Frame, Vec<TrackedObject>)> {
    let tracker = ByteTrackTracker::new(ByteTrackConfig::default());
    let mut annotator = TrackAnnotator::new(AnnotatorConfig::default());
    let zones = zones();
    let mut out = Vec::new();
    for i in 0..WARMUP + FRAMES {
        let f = frame(i);
        let mut tracked = tracker.update(detections(i));
        annotator.annotate(&f, &zones, &[], &mut tracked);
        if i >= WARMUP {
            assert_eq!(tracked.len(), PERSONS + VEHICLES);
            assert!(tracked.iter().all(|o| o.detection_bbox.is_some()));
            assert!(tracked.iter().all(|o| o.attributes.len() >= 10));
            out.push((f, tracked));
        }
    }
    out
}

fn stripped(objects: &[TrackedObject]) -> Vec<TrackedObject> {
    let mut v = objects.to_vec();
    for o in &mut v {
        o.attributes.clear();
    }
    v
}

/// Total rule-stage allocations over the scenario, and the number of
/// (frame, object) pairs the rules were evaluated against.
fn rule_stage(eval: &RuleEvaluator, strip: bool) -> (u64, u64) {
    let zones = zones();
    let trace_id = String::from("t");
    let mut total = 0;
    let mut objects = 0;
    for (f, tracked) in scenario() {
        let input = if strip { stripped(&tracked) } else { tracked };
        let (events, n) = allocs(|| eval.evaluate(1, f.frame_id, &trace_id, W, H, &zones, &input));
        assert!(events.is_empty());
        total += n;
        objects += input.len() as u64;
    }
    (total, objects)
}

#[test]
fn each_extra_rule_adds_at_most_eight_allocations_per_object() {
    let (one, objects) = rule_stage(&never_matching(1), false);
    let (four, _) = rule_stage(&never_matching(4), false);
    let per_extra_rule = (four - one) as f64 / (3 * objects) as f64;
    println!(
        "rules x1 {:.1}, x4 {:.1} allocs/object/frame; each extra rule adds {per_extra_rule:.2}",
        one as f64 / objects as f64,
        four as f64 / objects as f64
    );
    assert!(
        per_extra_rule <= 8.0,
        "each extra rule costs {per_extra_rule:.2} allocations per object per frame; \
         the object binding or the CEL Context is being rebuilt per rule"
    );
}

#[test]
fn attributes_are_converted_once_per_object_not_once_per_rule() {
    let attr_cost = |n| {
        let (full, objects) = rule_stage(&never_matching(n), false);
        let (bare, _) = rule_stage(&never_matching(n), true);
        (full as f64 - bare as f64) / objects as f64
    };
    let one = attr_cost(1);
    let four = attr_cost(4);
    println!("attributes cost per object per frame: x1 rule {one:.2}, x4 rules {four:.2}");
    assert!(
        one > 10.0,
        "stripping the attributes must remove their conversion ({one:.2})"
    );
    assert!(
        (four - one).abs() <= 2.0,
        "attribute conversion scales with rule count ({one:.2} -> {four:.2} per object)"
    );
}

/// Per-stage census of the whole post-inference path with two realistic
/// rules. Printed for the record; the bounds live in the tests above.
#[test]
fn whole_path_census_with_two_rules() {
    let tracker = ByteTrackTracker::new(ByteTrackConfig::default());
    let mut annotator = TrackAnnotator::new(AnnotatorConfig::default());
    let mut emitter = MotionEventEmitter::new(1.0);
    let zones = zones();
    let rules = [
        rule("person", "object.label == 'person'"),
        rule(
            "dwell",
            "object.label == 'person' && object.attributes['motion.dwell_seconds'] >= 3",
        ),
    ];
    let evaluator = RuleEvaluator::new(&RulesConfig::default(), &rules).unwrap();
    let trace_id = String::from("t");
    let mut stage = [0u64; 4];
    let mut tracks = 0u64;
    for i in 0..WARMUP + FRAMES {
        let f = frame(i);
        let dets = detections(i);
        let (mut tracked, a) = allocs(|| tracker.update(dets));
        let ((), b) = allocs(|| annotator.annotate(&f, &zones, &[], &mut tracked));
        let (decisions, c) = allocs(|| emitter.tick(1, &tracked, f.captured_at));
        let (events, d) = allocs(|| evaluator.evaluate(1, i, &trace_id, W, H, &zones, &tracked));
        drop((decisions, events));
        if i >= WARMUP {
            for (s, n) in stage.iter_mut().zip([a, b, c, d]) {
                *s += n;
            }
            tracks += tracked.len() as u64;
        }
    }
    let total: u64 = stage.iter().sum();
    println!("stage     allocs/track/frame");
    for (name, s) in ["track", "annotate", "emit", "rules(2)"].iter().zip(stage) {
        println!("{name:<9} {:>8.2}", s as f64 / tracks as f64);
    }
    println!("TOTAL     {:>8.2}", total as f64 / tracks as f64);
    assert_eq!(tracks, FRAMES * (PERSONS + VEHICLES) as u64);
}

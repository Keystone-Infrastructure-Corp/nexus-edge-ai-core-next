//! Allocation bound for the motion emitter's per-frame snapshot refresh (F1).
//!
//! The emitter keeps each live track's last attributes so a Died event can
//! carry them. It used to deep-clone the whole map for every track on every
//! frame (~18 allocations per track per frame, more than the tracker and
//! annotator spend building it). It now refreshes the snapshot in place, so
//! a steady-state frame costs well under two allocations per track.
//!
//! The annotator's per-track zone flags persist across frames the same way,
//! so a zone the object is not in costs it no allocation per frame.
//!
//! Driven by the real ByteTrack -> annotator -> static-filter chain over N
//! frames with M tracks. Counting is per thread (`#[global_allocator]` +
//! thread-locals), so the numbers are deterministic.
//! `-- --nocapture` prints the per-stage census.
//!
//! The bound counts calls into the allocator, so a `serde_json` upgrade can
//! legitimately move it. That is a re-baseline, not a flake.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use chrono::{TimeZone, Utc};
use nexus_config::{AnnotatorConfig, ByteTrackConfig, StaticObjectConfig, ZoneConfig, ZoneKind};
use nexus_tracker::{
    ByteTrackTracker, MotionEventEmitter, StaticObjectFilter, TrackAnnotator, Tracker,
};
use nexus_types::{BBox, Detection, Frame, PixelFormat};

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
const FRAMES: u64 = 300;

static MONO0: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

fn frame(i: u64) -> Frame {
    Frame {
        camera_id: 1,
        frame_id: i,
        captured_at: Utc
            .timestamp_millis_opt(1_700_000_000_000 + i as i64 * 33)
            .unwrap(),
        captured_mono: *MONO0 + std::time::Duration::from_millis(i * 33),
        width: W,
        height: H,
        format: PixelFormat::Rgb24,
        data: std::sync::Arc::new(Vec::new()),
        trace_id: String::new(),
    }
}

/// PERSONS walking right at 1 px/frame (30 px/s, `walking`) on a grid that
/// stays in frame for the whole run; VEHICLES parked. No track churn, so every
/// measured frame carries exactly PERSONS + VEHICLES confirmed tracks.
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
        let y = 650.0;
        out.push(Detection {
            label: "vehicle.car".into(),
            confidence: 0.9,
            bbox: BBox {
                x1: x,
                y1: y,
                x2: x + 220.0,
                y2: y + 120.0,
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

/// Allocations per track per frame for (track, annotate, classify, emit).
fn run(parking_lot_mode: bool, zones: &[ZoneConfig]) -> [f64; 4] {
    let tracker = ByteTrackTracker::new(ByteTrackConfig::default());
    let mut annotator = TrackAnnotator::new(AnnotatorConfig::default());
    let mut sf = parking_lot_mode.then(|| {
        StaticObjectFilter::new(
            StaticObjectConfig {
                persistence_enabled: false,
                ..Default::default()
            },
            1,
            None,
        )
    });
    let mut emitter = MotionEventEmitter::new(1.0);
    let mut stage = [0u64; 4];
    let mut tracks = 0u64;

    for i in 0..WARMUP + FRAMES {
        let f = frame(i);
        let dets = detections(i);
        let (mut tracked, a) = allocs(|| tracker.update(dets, f.captured_mono));
        let anchors: Vec<_> = sf
            .as_ref()
            .map(|s| s.anchors().to_vec())
            .unwrap_or_default();
        let ((), b) = allocs(|| annotator.annotate(&f, zones, &anchors, &mut tracked));
        let ((), c) = allocs(|| {
            if let Some(s) = sf.as_mut() {
                s.classify(&f, &mut tracked);
            }
        });
        let (decisions, d) = allocs(|| emitter.tick(1, &tracked, f.captured_at, f.captured_mono));
        drop(decisions);
        if i >= WARMUP {
            assert_eq!(tracked.len(), PERSONS + VEHICLES, "no track churn");
            for (s, n) in stage.iter_mut().zip([a, b, c, d]) {
                *s += n;
            }
            tracks += tracked.len() as u64;
        }
    }
    let per = stage.map(|s| s as f64 / tracks as f64);
    println!(
        "parking_lot_mode={parking_lot_mode} zones={}: allocs/track/frame track {:.2} \
         annotate {:.2} classify {:.2} emit {:.2}",
        zones.len(),
        per[0],
        per[1],
        per[2],
        per[3]
    );
    per
}

#[test]
fn emitter_refreshes_its_snapshot_without_cloning_every_frame() {
    for parking_lot_mode in [false, true] {
        let emit = run(parking_lot_mode, &zones())[3];
        assert!(
            emit <= 2.0,
            "motion emitter costs {emit:.2} allocations per track per frame \
             (parking_lot_mode={parking_lot_mode}); the per-frame snapshot is being cloned"
        );
    }
}

#[test]
fn a_zone_the_object_is_not_in_costs_the_annotator_no_allocation() {
    let mut more = zones();
    for i in 0..4 {
        more.push(ZoneConfig {
            id: format!("offscreen-{i}"),
            name: format!("Offscreen {i}"),
            polygon: vec![(0.0, 2.0), (1.0, 2.0), (1.0, 3.0), (0.0, 3.0)],
            kind: ZoneKind::Inclusion,
            min_bbox_area_px_override: None,
        });
    }
    let per_zone = (run(false, &more)[1] - run(false, &zones())[1]) / 4.0;
    assert!(
        per_zone <= 0.05,
        "each zone no object is in costs the annotator {per_zone:.2} allocations per track \
         per frame; its per-track flag is being re-keyed every frame"
    );
}

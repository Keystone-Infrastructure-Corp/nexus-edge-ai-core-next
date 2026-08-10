//! Source-level invariant guarding BUG-036.
//!
//! The bug was not a wrong value, it was a *call site*: a synchronous
//! `set_state(Null)` on a pipeline that owns a live network source, executed on
//! a tokio worker. Nothing about the types stops someone reintroducing it, and
//! the failure is silent at runtime — the reconciler simply stops reconciling.
//! So the guard is on the source text.

use std::path::Path;

/// Modules whose pipelines own a live network source (RTSP, WebRTC transport,
/// MoQ relay). Their NULL transitions are unbounded and must be detached.
const LIVE_SOURCE_MODULES: &[&str] = &[
    "src/preroll_ingester.rs",
    "src/source.rs",
    "src/webrtc.rs",
    "src/moq_publish.rs",
];

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn live_source_pipelines_never_null_inline() {
    let mut offenders = Vec::new();
    for module in LIVE_SOURCE_MODULES {
        for (i, line) in read(module).lines().enumerate() {
            if line.contains("set_state(gst::State::Null)") {
                offenders.push(format!("{module}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "live-source pipelines must go through `teardown::null_pipeline_detached`; \
         a synchronous NULL transition here parks the tokio worker that owns camera \
         lifecycle (BUG-036). Offending lines:\n{}",
        offenders.join("\n")
    );
}

/// Under edition 2021 an `if let` scrutinee temporary lives for the whole body,
/// so binding the map removal inline holds the `parking_lot` write guard across
/// the teardown and stalls every other camera behind it.
#[test]
fn ingester_removal_drops_its_write_guard_before_teardown() {
    let src = read("src/gst_clip_recorder.rs");
    assert!(
        !src.contains("if let Some(ing) = self.ingesters.write().remove("),
        "`remove_camera_ingester` must bind the removed ingester with a `let` \
         statement so the write guard is released before `shutdown()` runs \
         (BUG-036)"
    );
}

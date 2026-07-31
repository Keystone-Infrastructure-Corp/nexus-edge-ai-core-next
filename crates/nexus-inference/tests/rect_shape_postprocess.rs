//! Regression guard for shape-dynamic detector post-processing
//! (M_NATIVE_ASPECT Phase 2).
//!
//! The raw YOLO-family heads (`yolo26n` / `yolo_world` / `yoloe` exported
//! with `nms=False`) emit the detection tensor as `[1, features, anchors]`
//! or its transpose `[1, anchors, features]`. `nexus_inference::detectors::
//! orient_pred_rows` must orient to `[anchors, features]` for ANY anchor
//! count — this test pins that behaviour across the square→native-16:9
//! migration so a future refactor cannot silently reintroduce a hardcoded
//! `8400` (640² anchor count) or `160` (640² mask-proto side).
//!
//! Gated on `ort` because `orient_pred_rows` — like the decoders that call
//! it — lives behind that feature (ndarray is an `ort`-only dependency).
//! Runs in the ORT CI job (`cargo test -p nexus-inference --features ort`).
#![cfg(feature = "ort")]

use ndarray::{Array, ArrayD, IxDyn};
use nexus_inference::detectors::orient_pred_rows;

/// Build a `[1, d1, d2]` tensor whose every element encodes its own
/// `(i, j)` position (`i * 100000 + j`) so we can assert orientation, not
/// merely the resulting shape.
fn positional_1x(d1: usize, d2: usize) -> ArrayD<f32> {
    Array::from_shape_fn(IxDyn(&[1, d1, d2]), |ix| (ix[1] * 100_000 + ix[2]) as f32)
}

/// Feature widths = `4 + num_classes`. yolo26n NMS-free head = 6;
/// yolo_world default vocab (44) → 48; COCO (80) → 84. The helper must be
/// agnostic to all of them.
const FEATURES: &[usize] = &[6, 48, 84];

/// Anchor counts = Σ over strides {8,16,32} of `(W/s · H/s)`.
///   8400  = 640×640   (the pre-migration square count)
///   3024  = 512×288   (Standard tier)
///   12096 = 1024×576  (Long range)
///   27216 = 1536×864  (High detail)
const ANCHORS: &[usize] = &[8400, 3024, 12096, 27216];

#[test]
fn orients_features_first_export_to_anchor_rows() {
    // Orientation A: `[1, features, anchors]` — the common raw head.
    for &feat in FEATURES {
        for &anchors in ANCHORS {
            let t = positional_1x(feat, anchors);
            let pred = orient_pred_rows(t.view())
                .unwrap_or_else(|| panic!("orient failed for [1,{feat},{anchors}]"));
            assert_eq!(
                pred.dim(),
                (anchors, feat),
                "features-first [1,{feat},{anchors}] must orient to [{anchors},{feat}]"
            );
            // After transpose, pred[[a, f]] == source[0, f, a] = f*100000 + a.
            assert_eq!(pred[[0, 0]], 0.0);
            assert_eq!(pred[[1, 0]], 1.0, "row 1, col 0 == source[0,0,1]");
            if feat > 1 {
                assert_eq!(pred[[0, 1]], 100_000.0, "row 0, col 1 == source[0,1,0]");
            }
        }
    }
}

#[test]
fn keeps_anchor_first_export_as_is() {
    // Orientation B: `[1, anchors, features]` — already anchors-first.
    for &feat in FEATURES {
        for &anchors in ANCHORS {
            let t = positional_1x(anchors, feat);
            let pred = orient_pred_rows(t.view())
                .unwrap_or_else(|| panic!("orient failed for [1,{anchors},{feat}]"));
            assert_eq!(
                pred.dim(),
                (anchors, feat),
                "anchor-first [1,{anchors},{feat}] must stay [{anchors},{feat}]"
            );
            // Not transposed: pred[[a, f]] == source[0, a, f].
            assert_eq!(pred[[1, 0]], 100_000.0, "row 1, col 0 == source[0,1,0]");
        }
    }
}

#[test]
fn two_dimensional_input_passes_through() {
    let two: ArrayD<f32> = Array::from_shape_fn(IxDyn(&[3024, 48]), |ix| ix[0] as f32);
    let pred = orient_pred_rows(two.view()).expect("2-D input must pass through");
    assert_eq!(pred.dim(), (3024, 48));
}

#[test]
fn rejects_yoloe_proto_planes_regardless_of_shape() {
    // yoloe seg exports a SECOND output — the mask-prototype plane
    // `[1, 32, H/4, W/4]`: 72×128 @ 512×288 and 160×160 @ 640². The decoder
    // reads ONLY output0 and never the proto; `orient_pred_rows` rejects the
    // 4-D proto outright so it can never be mistaken for a detection tensor.
    for (h, w) in [(72usize, 128usize), (160, 160)] {
        let proto: ArrayD<f32> = Array::zeros(IxDyn(&[1, 32, h, w]));
        assert!(
            orient_pred_rows(proto.view()).is_none(),
            "yoloe proto [1,32,{h},{w}] must be rejected as a detection tensor"
        );
    }
}

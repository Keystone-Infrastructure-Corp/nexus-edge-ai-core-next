//! Hailo-8 inference backend (M_HAILO_EP).
//!
//! HailoRT is *not* an ONNX Runtime execution provider — it's a parallel
//! runtime that consumes pre-compiled `.hef` (Hailo Executable Format)
//! files. So integration in this engine happens at the `Detector` level
//! (see `crates/nexus-inference/src/hailo_yolo.rs`), not as an EP arm in
//! `execution_providers.rs`. The router inspects the resolved model
//! path: `.onnx` → `YoloOrtDetector`, `.hef` → `HailoYoloDetector`.
//!
//! ## Build matrix
//!
//! | target          | `linked` feature | behavior                              |
//! |-----------------|------------------|----------------------------------------|
//! | linux + x86_64  | on               | real FFI to `libhailort.so.4.23`       |
//! | anything else   | (any)            | stub returning `Error::NotAvailable`    |
//! | linux + linked off | n/a           | stub                                   |
//!
//! The stub keeps the public API stable so `cargo check --workspace`
//! passes on macOS dev boxes that have no HailoRT installed.
//!
//! ## Lifetimes
//!
//! Handles drop in the correct order automatically thanks to struct
//! field ordering: `InferSession { output, input, network_group, hef,
//! device }` releases vstreams first, then the network group, then the
//! HEF, then the vdevice. Reversing that order leaks driver state.

pub mod error;

pub use error::Error;

#[cfg(all(target_os = "linux", feature = "linked"))]
mod ffi;
#[cfg(all(target_os = "linux", feature = "linked"))]
mod imp;

#[cfg(not(all(target_os = "linux", feature = "linked")))]
mod stub;

#[cfg(all(target_os = "linux", feature = "linked"))]
pub use imp::{
    decode_detections, DeviceInfo, InferSession, OutputLayout, OutputStreamInfo, RawYoloScale,
};
#[cfg(not(all(target_os = "linux", feature = "linked")))]
pub use stub::{
    decode_detections, DeviceInfo, InferSession, OutputLayout, OutputStreamInfo, RawYoloScale,
};

/// One detection decoded from the on-chip NMS output.
///
/// Coordinates are normalized to `[0.0, 1.0]` against the network input
/// dimensions (the model's letterboxed canvas), with Hailo's `y_min,
/// x_min, y_max, x_max` ordering preserved. The caller scales these back
/// into pixel space; see `HailoYoloDetector::postprocess` for the
/// canonical mapping used by the engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detection {
    pub y_min: f32,
    pub x_min: f32,
    pub y_max: f32,
    pub x_max: f32,
    pub score: f32,
    pub class_id: u16,
}

/// True when this build has a real HailoRT linkage. Cheap const that
/// callers can branch on to skip device probing on unsupported builds.
pub const fn is_supported() -> bool {
    cfg!(all(target_os = "linux", feature = "linked"))
}

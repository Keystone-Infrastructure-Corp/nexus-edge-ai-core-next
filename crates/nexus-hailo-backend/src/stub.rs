//! Stub backend used when the target is not Linux or the `linked`
//! feature is off.
//!
//! Every entry point returns `Error::NotAvailable` so the rest of the
//! engine compiles cross-platform. The crate that wires this in
//! (`nexus-inference`) checks `nexus_hailo_backend::is_supported()`
//! before constructing an `InferSession`, and falls through to the
//! existing ORT path on unsupported builds.

use std::path::Path;

use crate::error::Error;
use crate::Detection;

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub board_name: String,
    pub serial: String,
    pub fw_version: (u32, u32, u32),
    pub device_id: String,
}

#[derive(Debug, Clone)]
pub struct Telemetry {
    pub devices: Vec<DeviceTelemetry>,
    pub inferences_per_sec: f32,
    pub frames_total: u64,
    pub utilization_pct: f32,
}

#[derive(Debug, Clone)]
pub struct DeviceTelemetry {
    pub board_name: String,
    pub serial: String,
    pub fw_version: (u32, u32, u32),
    pub part_number: String,
    pub product_name: String,
    pub temperature_c: Option<f32>,
    pub power_w: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputLayout {
    NmsByClass {
        num_classes: u32,
        max_bboxes_per_class: u32,
    },
    NmsByScore {
        max_bboxes_total: u32,
    },
    RawYolo26 {
        num_classes: u32,
        scales: Vec<RawYolo26Scale>,
    },
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawYolo26Scale {
    pub stride: u32,
    pub h: u32,
    pub w: u32,
    pub box_idx: usize,
    pub score_idx: usize,
}

#[derive(Debug, Clone)]
pub struct OutputStreamInfo {
    pub name: String,
    pub h: u32,
    pub w: u32,
    pub c: u32,
    pub frame_size: usize,
}

pub struct InferSession {
    _bufs: Vec<Vec<u8>>,
    _infos: Vec<OutputStreamInfo>,
    _layout: OutputLayout,
}

impl InferSession {
    pub fn open(
        _hef_path: &Path,
        _score_threshold: Option<f32>,
        _iou_threshold: Option<f32>,
    ) -> Result<Self, Error> {
        Err(Error::NotAvailable)
    }

    pub fn input_shape(&self) -> (u32, u32, u32) {
        (0, 0, 0)
    }
    pub fn input_frame_size(&self) -> usize {
        0
    }
    pub fn output_frame_size(&self) -> usize {
        0
    }
    pub fn output_infos(&self) -> &[OutputStreamInfo] {
        &self._infos
    }
    pub fn output_layout(&self) -> &OutputLayout {
        &self._layout
    }

    pub fn infer_blocking(&mut self, _input: &[u8]) -> Result<&[Vec<u8>], Error> {
        Err(Error::NotAvailable)
    }

    pub fn devices() -> Result<Vec<DeviceInfo>, Error> {
        Err(Error::NotAvailable)
    }

    pub fn telemetry(&mut self) -> Result<Telemetry, Error> {
        Err(Error::NotAvailable)
    }
}

/// Decode the per-output buffers from `InferSession::infer_blocking` into
/// a flat list. Stub is unreachable but kept for API parity.
pub fn decode_detections(
    _buffers: &[Vec<u8>],
    _layout: &OutputLayout,
    _max_detections: usize,
) -> Vec<Detection> {
    Vec::new()
}

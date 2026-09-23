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
use crate::{OutputLayout, OutputStreamInfo};

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

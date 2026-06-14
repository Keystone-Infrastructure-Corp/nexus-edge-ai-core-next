//! Real HailoRT-backed implementation. Linux + `linked` feature only.
//!
//! Owns the full lifetime of a single-model inference session:
//!   `InferSession { device, hef, network_group, input_vstream,
//!    output_vstream }` — fields are declared in reverse-construction
//!   order so `Drop` releases them in the order the C API requires.
//!
//! The session is synchronous per-frame. yolo26n at 640×640 takes
//! ~6 ms wall-clock on Hailo-8 (per Model Zoo profiler), so a single
//! session can drive ~150 fps total or ~6 fps × 24 cameras with room
//! to spare. If multi-stream throughput becomes the bottleneck we
//! upgrade to the async InferModel API; for the initial M_HAILO_EP
//! shipment, blocking write+read keeps the wire path obvious.

use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::error::Error;
use crate::ffi::{self, *};
use crate::Detection;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub board_name: String,
    pub serial: String,
    pub fw_version: (u32, u32, u32),
    pub device_id: String,
}

/// Live telemetry snapshot of every physical Hailo chip backing an
/// `InferSession`. Returned by [`InferSession::telemetry`] for the
/// System tab in the local admin UI; cheap enough (3 FFI calls per
/// device, all read-only) to call on a 1–2 s poll cadence.
#[derive(Debug, Clone)]
pub struct Telemetry {
    pub devices: Vec<DeviceTelemetry>,
    /// Inferences per second observed since the previous `telemetry()`
    /// call on this session. The first call after `open()` returns 0.0
    /// (no prior sample to delta against). Session-level because the
    /// driver does not expose a per-physical-device utilization counter
    /// in HailoRT 4.x — this is the next-best operator signal.
    pub inferences_per_sec: f32,
    /// Total inferences served by this session since `open()`. Useful
    /// for the UI to render an absolute count alongside the rate.
    pub frames_total: u64,
    /// Fraction of wall-clock time the session spent inside
    /// `infer_blocking` (the write+read FFI pair) between the previous
    /// and current `telemetry()` calls, expressed as 0–100. The real
    /// HailoRT 4.x driver exposes no per-chip utilization counter, so
    /// this busy-time ratio is the next-best operator signal — it is
    /// what the System tab renders as the big "utilization" tile to
    /// match the NPU and GPU cards. 0.0 on the first poll after open
    /// (no prior sample to delta against).
    pub utilization_pct: f32,
}

/// Per-chip telemetry: stable identity + live temperature + live power.
/// Either of the live readings may be `None` if the underlying FFI call
/// failed (e.g. a firmware that doesn't support the dvm); identity
/// fields come from `hailo_identify` and are always populated when the
/// device handle is valid.
#[derive(Debug, Clone)]
pub struct DeviceTelemetry {
    pub board_name: String,
    pub serial: String,
    pub fw_version: (u32, u32, u32),
    pub part_number: String,
    pub product_name: String,
    /// Hotter of the two on-die sensors, in degrees Celsius. `None`
    /// when `hailo_get_chip_temperature` fails (sample_count == 0
    /// before the first internal sample is taken, etc.).
    pub temperature_c: Option<f32>,
    /// Instantaneous power draw in watts, from
    /// `hailo_power_measurement(dvm=AUTO, type=AUTO)`. `None` on FFI
    /// error.
    pub power_w: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputLayout {
    /// `HAILO_FORMAT_ORDER_HAILO_NMS_BY_CLASS` (22). Single output buffer:
    ///   for each class C in 0..num_classes:
    ///     float32 bbox_count
    ///     bbox_count × { f32 y_min, x_min, y_max, x_max, score }
    /// Class id is implicit from buffer position.
    NmsByClass {
        num_classes: u32,
        max_bboxes_per_class: u32,
    },
    /// `HAILO_FORMAT_ORDER_HAILO_NMS_BY_SCORE` (23). Single output buffer:
    ///   uint16 bbox_count
    ///   bbox_count × hailo_detection_t { f32 ymin, xmin, ymax, xmax, score; u16 class_id }
    NmsByScore { max_bboxes_total: u32 },
    /// Anchor-free yolo26-style raw heads with on-chip DFL fold. Each
    /// scale pairs a 4-channel box tensor (per-cell l,t,r,b in cell units)
    /// with an N-channel class tensor (per-cell sigmoid'd probabilities).
    /// All tensors are FLOAT32 NHWC. The caller (typically
    /// `decode_detections`) runs the anchor-free decode + class-agnostic
    /// NMS on CPU. This is what the public Hailo Model Zoo yolo26n.hef
    /// emits (no on-chip NMS). Naming tracks the model id in the manifest
    /// (`yolo26n`), not the underlying head architecture family.
    RawYolo26 {
        num_classes: u32,
        scales: Vec<RawYolo26Scale>,
    },
    /// Unsupported by the YOLO detector path — caller must handle raw output.
    Other,
}

/// One anchor-free yolo26 scale (box tensor + class tensor at a given stride).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawYolo26Scale {
    pub stride: u32,
    pub h: u32,
    pub w: u32,
    /// Index into `InferSession::output_buffers()` for the box (4-channel) tensor.
    pub box_idx: usize,
    /// Index into `InferSession::output_buffers()` for the class (num_classes-channel) tensor.
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
    // Drop order matters: vstreams first, then network group, then HEF,
    // then vdevice. Rust drops fields top-to-bottom, so we list them in
    // *destruction* order (outputs+input at the top, vdevice at the bottom).
    outputs: Vec<OwnedOutputVStream>,
    input: OwnedInputVStream,
    _network_group: OwnedNetworkGroup,
    _hef: OwnedHef,
    _device: OwnedVDevice,

    input_shape: (u32, u32, u32),
    input_frame_size: usize,
    output_infos: Vec<OutputStreamInfo>,
    /// Per-output preallocated read buffers. Reused across `infer_blocking` calls.
    output_buffers: Vec<Vec<u8>>,
    output_layout: OutputLayout,

    /// Total successful `infer_blocking` calls since open.
    frames_total: u64,
    /// Cumulative wall-clock time spent inside `infer_blocking` (the
    /// write+read FFI pair) since open. `telemetry()` deltas this
    /// against the previous sample to produce a busy% utilization.
    infer_busy_total: Duration,
    /// `(wall_clock_at_last_telemetry, frames_total_at_last_telemetry,
    /// infer_busy_total_at_last_telemetry)` — used to compute
    /// inferences-per-second and busy% utilization as deltas between
    /// consecutive `telemetry()` calls.
    prev_telemetry_sample: (Instant, u64, Duration),
}

// ---------------------------------------------------------------------------
// Owned handle wrappers (each owns one HailoRT object + releases in Drop)
// ---------------------------------------------------------------------------

struct OwnedVDevice(hailo_vdevice);
impl Drop for OwnedVDevice {
    fn drop(&mut self) {
        unsafe { ffi::hailo_release_vdevice(self.0) };
    }
}
unsafe impl Send for OwnedVDevice {}

struct OwnedHef(hailo_hef);
impl Drop for OwnedHef {
    fn drop(&mut self) {
        unsafe { ffi::hailo_release_hef(self.0) };
    }
}
unsafe impl Send for OwnedHef {}

/// Network group handles do not have an individual release — they're
/// owned by the vdevice and freed when the vdevice is released. We keep
/// the handle pointer for lookup only.
struct OwnedNetworkGroup(hailo_configured_network_group);
unsafe impl Send for OwnedNetworkGroup {}

struct OwnedInputVStream(hailo_input_vstream);
impl Drop for OwnedInputVStream {
    fn drop(&mut self) {
        unsafe { ffi::hailo_release_input_vstreams(&self.0, 1) };
    }
}
unsafe impl Send for OwnedInputVStream {}

struct OwnedOutputVStream(hailo_output_vstream);
impl Drop for OwnedOutputVStream {
    fn drop(&mut self) {
        unsafe { ffi::hailo_release_output_vstreams(&self.0, 1) };
    }
}
unsafe impl Send for OwnedOutputVStream {}

// ---------------------------------------------------------------------------
// InferSession::open + getters + infer_blocking
// ---------------------------------------------------------------------------

impl InferSession {
    /// Open a single-network HEF file and create one input + one output
    /// vstream against it. Sets the on-chip NMS score / IoU thresholds
    /// if provided.
    ///
    /// Both `score_threshold` (default ~0.001 on yolo26n.hef) and
    /// `iou_threshold` (default ~0.7) are model-baked. Override sparingly
    /// — operator-facing `inference.score_threshold` is also applied
    /// post-hoc by the detector to retain wire-format compatibility with
    /// the ORT path.
    pub fn open(
        hef_path: &Path,
        score_threshold: Option<f32>,
        iou_threshold: Option<f32>,
    ) -> Result<Self, Error> {
        // dlopen libhailort.so.4 on first use. Returns NotAvailable to
        // the caller (which then falls back to ONNX) if the .deb was
        // never installed.
        ffi::ensure_loaded()?;

        // Stat first for a clean error if the file is missing — the C
        // API otherwise returns HAILO_OPEN_FILE_FAILURE which loses the path.
        let _ = fs::metadata(hef_path)?;

        let path_cstr = CString::new(hef_path.as_os_str().to_string_lossy().as_bytes())
            .map_err(|e| Error::InvalidPath(format!("{e}")))?;

        // 1) VDevice (default params = single physical Hailo-8)
        let mut device: hailo_vdevice = ptr::null_mut();
        check(
            unsafe { ffi::hailo_create_vdevice(ptr::null(), &mut device) },
            "hailo_create_vdevice",
        )?;
        let device = OwnedVDevice(device);

        // 2) HEF
        let mut hef: hailo_hef = ptr::null_mut();
        check(
            unsafe { ffi::hailo_create_hef_file(&mut hef, path_cstr.as_ptr()) },
            "hailo_create_hef_file",
        )?;
        let hef = OwnedHef(hef);

        // 3) Configure — NULL params (HailoRT picks defaults).
        //    Up to HAILO_MAX_NETWORK_GROUPS network groups; yolo26n has one.
        let mut network_groups: [hailo_configured_network_group; HAILO_MAX_NETWORK_GROUPS] =
            [ptr::null_mut(); HAILO_MAX_NETWORK_GROUPS];
        let mut ng_count: usize = HAILO_MAX_NETWORK_GROUPS;
        check(
            unsafe {
                ffi::hailo_configure_vdevice(
                    device.0,
                    hef.0,
                    ptr::null_mut(),
                    network_groups.as_mut_ptr(),
                    &mut ng_count,
                )
            },
            "hailo_configure_vdevice",
        )?;
        if ng_count == 0 {
            return Err(Error::LayoutMismatch(
                "HEF contains no network groups".into(),
            ));
        }
        if ng_count > 1 {
            warn!(
                "HEF contains {ng_count} network groups; using only the first ({})",
                "yolo backend supports single-network HEFs"
            );
        }
        let network_group = OwnedNetworkGroup(network_groups[0]);

        // 4) Make input vstream params (FLOAT32 input would force quant
        //    in the host, which is slow). We pick UINT8 to keep the input
        //    on the device's native quantization grid — the HEF embeds
        //    the qp_zp/qp_scale and the on-chip preproc handles it.
        let inputs_params = make_input_params(network_group.0, HAILO_FORMAT_TYPE_UINT8)?;
        if inputs_params.len() != 1 {
            return Err(Error::LayoutMismatch(format!(
                "expected single-input network, got {}",
                inputs_params.len()
            )));
        }
        let mut input_vs: hailo_input_vstream = ptr::null_mut();
        check(
            unsafe {
                ffi::hailo_create_input_vstreams(
                    network_group.0,
                    inputs_params.as_ptr(),
                    1,
                    &mut input_vs,
                )
            },
            "hailo_create_input_vstreams",
        )?;
        let input = OwnedInputVStream(input_vs);

        // 5) Make output vstream params (FLOAT32 — HailoRT will
        //    de-quantize for us). Force NHWC ordering on every output
        //    so the raw-YOLO decoder doesn't have to translate FCR.
        let mut outputs_params = make_output_params(network_group.0, HAILO_FORMAT_TYPE_FLOAT32)?;
        for op in outputs_params.iter_mut() {
            // Override the auto-selected order. For NMS outputs the
            // runtime keeps the NMS layout (NHWC has no meaning there);
            // for tensor outputs this forces FCR→NHWC reordering inside
            // the read pipeline.
            if op.params.user_buffer_format.order != HAILO_FORMAT_ORDER_HAILO_NMS_BY_CLASS
                && op.params.user_buffer_format.order != HAILO_FORMAT_ORDER_HAILO_NMS_BY_SCORE
            {
                op.params.user_buffer_format.order = HAILO_FORMAT_ORDER_NHWC;
            }
        }
        if outputs_params.is_empty() {
            return Err(Error::LayoutMismatch("HEF has 0 output vstreams".into()));
        }
        let num_outputs = outputs_params.len();
        // Create one vstream at a time so each gets its own owning
        // wrapper. The C API's create_output_vstreams can take an
        // array, but releasing N at once via release_output_vstreams(arr,N)
        // complicates Drop semantics — single-stream wrappers are simpler.
        let mut outputs: Vec<OwnedOutputVStream> = Vec::with_capacity(num_outputs);
        for op in &outputs_params {
            let mut handle: hailo_output_vstream = ptr::null_mut();
            check(
                unsafe { ffi::hailo_create_output_vstreams(network_group.0, op, 1, &mut handle) },
                "hailo_create_output_vstreams",
            )?;
            outputs.push(OwnedOutputVStream(handle));
        }

        // 6) Optional threshold overrides — only meaningful for the
        // single-output NMS-fused HEFs. Apply to the first output and
        // ignore errors on multi-output HEFs (the call will fail with
        // "not an NMS stream").
        if outputs.len() == 1 {
            if let Some(t) = score_threshold {
                check(
                    unsafe { ffi::hailo_vstream_set_nms_score_threshold(outputs[0].0, t) },
                    "hailo_vstream_set_nms_score_threshold",
                )?;
            }
            if let Some(t) = iou_threshold {
                check(
                    unsafe { ffi::hailo_vstream_set_nms_iou_threshold(outputs[0].0, t) },
                    "hailo_vstream_set_nms_iou_threshold",
                )?;
            }
        } else {
            if score_threshold.is_some() || iou_threshold.is_some() {
                debug!(
                    "score/iou threshold overrides ignored on multi-output HEF \
                     ({num_outputs} outputs) -- thresholds are applied in the \
                     CPU postprocessor instead"
                );
            }
        }

        // 7) Read input shape + sizes
        let in_info = get_input_info(input.0)?;
        let mut input_frame_size: usize = 0;
        check(
            unsafe { ffi::hailo_get_input_vstream_frame_size(input.0, &mut input_frame_size) },
            "hailo_get_input_vstream_frame_size",
        )?;

        // Input shape lives in the union as `hailo_3d_image_shape_t`
        // when format.order != NMS. yolo26n input is NHWC RGB (order=1).
        if in_info.format.order != HAILO_FORMAT_ORDER_NHWC {
            return Err(Error::LayoutMismatch(format!(
                "expected NHWC input (order=1), got order={}",
                in_info.format.order
            )));
        }
        let in_shape = unsafe { in_info.shape_union.shape };
        let input_shape = (in_shape.height, in_shape.width, in_shape.features);

        // 8) Per-output metadata + preallocated read buffers
        let mut output_infos: Vec<OutputStreamInfo> = Vec::with_capacity(num_outputs);
        let mut output_buffers: Vec<Vec<u8>> = Vec::with_capacity(num_outputs);
        for out in &outputs {
            let info = get_output_info(out.0)?;
            let mut frame_size: usize = 0;
            check(
                unsafe { ffi::hailo_get_output_vstream_frame_size(out.0, &mut frame_size) },
                "hailo_get_output_vstream_frame_size",
            )?;
            let order = info.format.order;
            let (h, w, c) = if order == HAILO_FORMAT_ORDER_HAILO_NMS_BY_CLASS
                || order == HAILO_FORMAT_ORDER_HAILO_NMS_BY_SCORE
                || order == HAILO_FORMAT_ORDER_HAILO_NMS_ON_CHIP
                || order == HAILO_FORMAT_ORDER_HAILO_NMS_WITH_BYTE_MASK
            {
                // NMS variants use a different shape union arm; not
                // relevant for our raw-YOLO pairing.
                (0, 0, 0)
            } else {
                // FCR, NHWC, NCHW, F8CR, NHCW, ... all populate
                // shape_union.shape with (h, w, features) of the
                // device-side tensor. We forced NHWC at vstream-open
                // time for non-NMS outputs, so the runtime data laid
                // down in the read buffer is NHWC regardless of the
                // declared HEF order reported here.
                let s = unsafe { info.shape_union.shape };
                (s.height, s.width, s.features)
            };
            let name = c_array_to_string(
                &info.name,
                info.name
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(info.name.len()),
            );
            output_buffers.push(vec![0u8; frame_size]);
            output_infos.push(OutputStreamInfo {
                name,
                h,
                w,
                c,
                frame_size,
            });
        }

        // 9) Output layout dispatch. First output's format.order picks
        // the family; multi-output HEFs are always RawYolo26 (no NMS).
        let first_order = {
            let info = get_output_info(outputs[0].0)?;
            info.format.order
        };
        let output_layout =
            if num_outputs == 1 && first_order == HAILO_FORMAT_ORDER_HAILO_NMS_BY_CLASS {
                let info = get_output_info(outputs[0].0)?;
                let nms = unsafe { info.shape_union.nms_shape };
                OutputLayout::NmsByClass {
                    num_classes: nms.number_of_classes,
                    max_bboxes_per_class: nms.max_bboxes_per_class,
                }
            } else if num_outputs == 1 && first_order == HAILO_FORMAT_ORDER_HAILO_NMS_BY_SCORE {
                let info = get_output_info(outputs[0].0)?;
                let nms = unsafe { info.shape_union.nms_shape };
                OutputLayout::NmsByScore {
                    max_bboxes_total: nms.max_bboxes_total,
                }
            } else {
                // Multi-output: pair box (c=4) and score (c=num_classes,
                // usually 80) tensors by spatial shape. Each pair becomes
                // one scale; stride = input_w / cell_w.
                match build_yolo26_scales(&output_infos, input_shape.1) {
                    Ok((num_classes, scales)) => OutputLayout::RawYolo26 {
                        num_classes,
                        scales,
                    },
                    Err(e) => {
                        warn!(
                            "Hailo HEF outputs do not match yolo26-style anchor-free \
                         layout: {e}; falling through to OutputLayout::Other"
                        );
                        OutputLayout::Other
                    }
                }
            };

        info!(
            target: "hailo",
            hef = %hef_path.display(),
            input_shape = ?input_shape,
            input_frame_size,
            num_outputs,
            output_layout = ?output_layout,
            "hailo InferSession opened",
        );

        Ok(InferSession {
            outputs,
            input,
            _network_group: network_group,
            _hef: hef,
            _device: device,
            input_shape,
            input_frame_size,
            output_infos,
            output_buffers,
            output_layout,
            frames_total: 0,
            infer_busy_total: Duration::ZERO,
            prev_telemetry_sample: (Instant::now(), 0, Duration::ZERO),
        })
    }

    pub fn input_shape(&self) -> (u32, u32, u32) {
        self.input_shape
    }
    pub fn input_frame_size(&self) -> usize {
        self.input_frame_size
    }
    pub fn output_infos(&self) -> &[OutputStreamInfo] {
        &self.output_infos
    }
    pub fn output_layout(&self) -> &OutputLayout {
        &self.output_layout
    }
    /// Total bytes of the first output stream. Provided for back-compat
    /// with single-output HEFs; on multi-output HEFs prefer
    /// `output_infos()[i].frame_size`.
    pub fn output_frame_size(&self) -> usize {
        self.output_infos.first().map(|i| i.frame_size).unwrap_or(0)
    }

    /// Blocking write input + read every output vstream. Returns slices
    /// of the internally-owned per-output buffers (one per output
    /// vstream, in HEF declaration order). The slices are valid until
    /// the next call to `infer_blocking` on the same session.
    pub fn infer_blocking(&mut self, input: &[u8]) -> Result<&[Vec<u8>], Error> {
        if input.len() != self.input_frame_size {
            return Err(Error::LayoutMismatch(format!(
                "input buffer is {} bytes; device expects {}",
                input.len(),
                self.input_frame_size
            )));
        }
        let started = Instant::now();
        check(
            unsafe {
                ffi::hailo_vstream_write_raw_buffer(
                    self.input.0,
                    input.as_ptr() as *const _,
                    input.len(),
                )
            },
            "hailo_vstream_write_raw_buffer",
        )?;
        for (i, out) in self.outputs.iter().enumerate() {
            let buf = &mut self.output_buffers[i];
            check(
                unsafe {
                    ffi::hailo_vstream_read_raw_buffer(out.0, buf.as_mut_ptr() as *mut _, buf.len())
                },
                "hailo_vstream_read_raw_buffer",
            )?;
        }
        self.frames_total = self.frames_total.saturating_add(1);
        self.infer_busy_total = self.infer_busy_total.saturating_add(started.elapsed());
        Ok(&self.output_buffers)
    }

    /// Enumerate physical devices on this host (one entry per Hailo
    /// chip). Currently allocates a transient VDevice for the
    /// enumeration — fine for the probe binary, don't call per-frame.
    pub fn devices() -> Result<Vec<DeviceInfo>, Error> {
        ffi::ensure_loaded()?;

        let mut vdev: hailo_vdevice = ptr::null_mut();
        check(
            unsafe { ffi::hailo_create_vdevice(ptr::null(), &mut vdev) },
            "hailo_create_vdevice",
        )?;
        let _guard = OwnedVDevice(vdev);

        // Up to 8 physical devices per vdevice (HailoRT internal max).
        let mut handles: [hailo_device; 8] = [ptr::null_mut(); 8];
        let mut count: usize = 8;
        check(
            unsafe { ffi::hailo_get_physical_devices(vdev, handles.as_mut_ptr(), &mut count) },
            "hailo_get_physical_devices",
        )?;

        let mut out = Vec::with_capacity(count);
        for &dev in &handles[..count] {
            let mut id: hailo_device_identity_t = unsafe { std::mem::zeroed() };
            if let Err(e) = check(
                unsafe { ffi::hailo_identify(dev, &mut id) },
                "hailo_identify",
            ) {
                debug!("hailo_identify failed: {e}");
                continue;
            }
            out.push(DeviceInfo {
                board_name: c_array_to_string(&id.board_name, id.board_name_length as usize),
                serial: c_array_to_string(&id.serial_number, id.serial_number_length as usize),
                fw_version: (
                    id.fw_version.major,
                    id.fw_version.minor,
                    id.fw_version.revision,
                ),
                device_id: c_array_to_string(&id.part_number, id.part_number_length as usize),
            });
        }
        Ok(out)
    }

    /// Read live identity + temperature + power for every physical
    /// device backing this session. Pulled by `nexus-engine`'s System
    /// tab handler at the dashboard's 1–2 s poll cadence.
    ///
    /// Re-uses the session's existing `hailo_vdevice` — does NOT open
    /// a second vdevice (a second `hailo_create_vdevice` on the same
    /// chip returns `HAILO_OUT_OF_PHYSICAL_DEVICES`, see PR #66).
    /// `hailo_get_physical_devices` just hands back the device handles
    /// the vdevice was built on, and the per-device telemetry functions
    /// are read-only from the driver's perspective.
    pub fn telemetry(&mut self) -> Result<Telemetry, Error> {
        // Up to 8 physical devices per vdevice (HailoRT internal max);
        // Hailo-8 M.2 is always exactly 1.
        let mut handles: [hailo_device; 8] = [ptr::null_mut(); 8];
        let mut count: usize = 8;
        check(
            unsafe {
                ffi::hailo_get_physical_devices(self._device.0, handles.as_mut_ptr(), &mut count)
            },
            "hailo_get_physical_devices",
        )?;

        let mut devices = Vec::with_capacity(count);
        for &dev in &handles[..count] {
            let mut id: hailo_device_identity_t = unsafe { std::mem::zeroed() };
            if let Err(e) = check(
                unsafe { ffi::hailo_identify(dev, &mut id) },
                "hailo_identify",
            ) {
                debug!("hailo_identify failed during telemetry: {e}");
                continue;
            }

            let temperature_c = {
                let mut info: hailo_chip_temperature_info_t = Default::default();
                match check(
                    unsafe { ffi::hailo_get_chip_temperature(dev, &mut info) },
                    "hailo_get_chip_temperature",
                ) {
                    Ok(()) => {
                        // Hottest of the two on-die sensors is the
                        // operator-relevant number (thermal headroom).
                        Some(info.ts0_temperature.max(info.ts1_temperature))
                    }
                    Err(e) => {
                        debug!("hailo_get_chip_temperature failed: {e}");
                        None
                    }
                }
            };

            let power_w = {
                let mut watts: f32 = 0.0;
                match check(
                    unsafe {
                        ffi::hailo_power_measurement(
                            dev,
                            HAILO_DVM_OPTIONS_AUTO,
                            HAILO_POWER_MEASUREMENT_TYPES_AUTO,
                            &mut watts,
                        )
                    },
                    "hailo_power_measurement",
                ) {
                    Ok(()) => Some(watts),
                    Err(e) => {
                        debug!("hailo_power_measurement failed: {e}");
                        None
                    }
                }
            };

            devices.push(DeviceTelemetry {
                board_name: c_array_to_string(&id.board_name, id.board_name_length as usize),
                serial: c_array_to_string(&id.serial_number, id.serial_number_length as usize),
                fw_version: (
                    id.fw_version.major,
                    id.fw_version.minor,
                    id.fw_version.revision,
                ),
                part_number: c_array_to_string(&id.part_number, id.part_number_length as usize),
                product_name: c_array_to_string(&id.product_name, id.product_name_length as usize),
                temperature_c,
                power_w,
            });
        }

        // Inferences/sec and busy% utilization over the window since
        // the last telemetry() call. First call after open returns 0
        // for both (no prior sample to delta against). Busy% is the
        // fraction of wall-clock time spent inside `infer_blocking`'s
        // FFI pair — clamped to 100 to absorb any sub-ms scheduling
        // jitter between the `Instant::now()` calls.
        let now = Instant::now();
        let (prev_at, prev_frames, prev_busy) = self.prev_telemetry_sample;
        let dt = now.duration_since(prev_at);
        let dt_secs = dt.as_secs_f32();
        let inferences_per_sec = if dt_secs > 0.001 && self.frames_total >= prev_frames {
            (self.frames_total - prev_frames) as f32 / dt_secs
        } else {
            0.0
        };
        let utilization_pct = if dt_secs > 0.001 && self.infer_busy_total >= prev_busy {
            let busy_delta = (self.infer_busy_total - prev_busy).as_secs_f32();
            (busy_delta / dt_secs * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        self.prev_telemetry_sample = (now, self.frames_total, self.infer_busy_total);

        Ok(Telemetry {
            devices,
            inferences_per_sec,
            frames_total: self.frames_total,
            utilization_pct,
        })
    }
}

// ---------------------------------------------------------------------------
// Decoder: HEF output buffers → flat Vec<Detection>
// ---------------------------------------------------------------------------

/// Decode the per-output buffers produced by `InferSession::infer_blocking`
/// into flat detections. For NMS-fused HEFs this is a near-zero-cost
/// repack of the on-chip output; for raw yolo26 HEFs it runs the
/// anchor-free decode + class-agnostic NMS on CPU.
///
/// `max_detections` caps the output (saves a malloc blow on pathological
/// frames and bounds the NMS pass cost).
pub fn decode_detections(
    buffers: &[Vec<u8>],
    layout: &OutputLayout,
    max_detections: usize,
) -> Vec<Detection> {
    match layout {
        OutputLayout::NmsByClass { num_classes, .. } => {
            if let Some(buf) = buffers.first() {
                decode_nms_by_class(buf, *num_classes as usize, max_detections)
            } else {
                Vec::new()
            }
        }
        OutputLayout::NmsByScore { .. } => buffers
            .first()
            .map(|b| decode_nms_by_score(b, max_detections))
            .unwrap_or_default(),
        OutputLayout::RawYolo26 {
            num_classes,
            scales,
        } => decode_yolo26_raw(buffers, *num_classes, scales, max_detections),
        OutputLayout::Other => Vec::new(),
    }
}

fn decode_nms_by_class(buf: &[u8], num_classes: usize, max_detections: usize) -> Vec<Detection> {
    // Layout: for each class C:
    //   f32 bbox_count
    //   bbox_count × {f32 y_min, x_min, y_max, x_max, score}  (20 bytes each)
    let mut out = Vec::new();
    let mut off = 0usize;
    for class_id in 0..num_classes {
        if off + 4 > buf.len() {
            break;
        }
        let count_f = f32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        off += 4;
        let count = count_f.round() as usize;
        for _ in 0..count {
            if out.len() >= max_detections {
                return out;
            }
            if off + 20 > buf.len() {
                return out;
            }
            let y_min = f32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
            let x_min =
                f32::from_le_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
            let y_max =
                f32::from_le_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]);
            let x_max =
                f32::from_le_bytes([buf[off + 12], buf[off + 13], buf[off + 14], buf[off + 15]]);
            let score =
                f32::from_le_bytes([buf[off + 16], buf[off + 17], buf[off + 18], buf[off + 19]]);
            off += 20;
            out.push(Detection {
                y_min,
                x_min,
                y_max,
                x_max,
                score,
                class_id: class_id as u16,
            });
        }
    }
    out
}

fn decode_nms_by_score(buf: &[u8], max_detections: usize) -> Vec<Detection> {
    // Layout: u16 count; count × hailo_detection_t (packed):
    //   {f32 y_min, x_min, y_max, x_max, score; u16 class_id}  (22 bytes each, packed)
    if buf.len() < 2 {
        return Vec::new();
    }
    let count = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    let mut out = Vec::with_capacity(count.min(max_detections));
    let mut off = 2usize;
    for _ in 0..count {
        if out.len() >= max_detections {
            break;
        }
        if off + 22 > buf.len() {
            break;
        }
        let y_min = f32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let x_min = f32::from_le_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
        let y_max = f32::from_le_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]);
        let x_max =
            f32::from_le_bytes([buf[off + 12], buf[off + 13], buf[off + 14], buf[off + 15]]);
        let score =
            f32::from_le_bytes([buf[off + 16], buf[off + 17], buf[off + 18], buf[off + 19]]);
        let class_id = u16::from_le_bytes([buf[off + 20], buf[off + 21]]);
        off += 22;
        out.push(Detection {
            y_min,
            x_min,
            y_max,
            x_max,
            score,
            class_id,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Raw yolo26 anchor-free decoder (multi-output HEFs)
// ---------------------------------------------------------------------------

/// Pair box (c=4) and class (c=num_classes) tensors by spatial shape.
/// Returns `(num_classes, scales)`.
///
/// The public Hailo Model Zoo yolo26n.hef has six outputs at three
/// strides: 80x80x{4,80} (stride 8), 40x40x{4,80} (stride 16),
/// 20x20x{4,80} (stride 32). We don't trust the HEF declaration order
/// — we pair purely by shape, so a regenerated HEF that swaps order
/// or adds a fourth scale still decodes correctly.
fn build_yolo26_scales(
    output_infos: &[OutputStreamInfo],
    input_w: u32,
) -> Result<(u32, Vec<RawYolo26Scale>), String> {
    if output_infos.len() < 2 || output_infos.len() % 2 != 0 {
        return Err(format!(
            "expected an even number of outputs (box + class pairs), got {}",
            output_infos.len()
        ));
    }
    // Find num_classes — the class (non-box) tensors all share the same c.
    let num_classes = output_infos
        .iter()
        .find(|i| i.c != 4 && i.c != 0)
        .map(|i| i.c)
        .ok_or_else(|| "no class tensor found (no output with c ≠ 4)".to_string())?;
    let mut scales: Vec<RawYolo26Scale> = Vec::new();
    let mut consumed = vec![false; output_infos.len()];
    for (i, info_i) in output_infos.iter().enumerate() {
        if consumed[i] || info_i.c != 4 {
            continue;
        }
        // Find the class tensor with matching (h, w).
        let mut pair_idx: Option<usize> = None;
        for (j, info_j) in output_infos.iter().enumerate() {
            if !consumed[j]
                && j != i
                && info_j.c == num_classes
                && info_j.h == info_i.h
                && info_j.w == info_i.w
            {
                pair_idx = Some(j);
                break;
            }
        }
        let j = pair_idx.ok_or_else(|| {
            format!(
                "box output {}x{}x4 has no matching class tensor",
                info_i.h, info_i.w
            )
        })?;
        consumed[i] = true;
        consumed[j] = true;
        if info_i.w == 0 {
            return Err("box output has zero width".into());
        }
        let stride = input_w / info_i.w;
        if stride == 0 || stride * info_i.w != input_w {
            return Err(format!(
                "box output {}x{} doesn't divide network input width {} cleanly",
                info_i.h, info_i.w, input_w
            ));
        }
        scales.push(RawYolo26Scale {
            stride,
            h: info_i.h,
            w: info_i.w,
            box_idx: i,
            score_idx: j,
        });
    }
    if scales.is_empty() {
        return Err("no valid box+class pairs found".into());
    }
    // Sort by stride so the decoder hits low→high (largest grid first).
    scales.sort_by_key(|s| s.stride);
    Ok((num_classes, scales))
}

/// Anchor-free yolo26 decoder. Inputs are FLOAT32 NHWC tensors.
/// Box tensor encodes (l, t, r, b) per cell in CELL units (already
/// DFL-projected by the chip). Class tensor encodes per-class
/// post-sigmoid probabilities.
///
/// Coords are returned in [0, 1] normalized to the *network input*
/// space, matching the convention used by NMS-fused HEFs.
fn decode_yolo26_raw(
    buffers: &[Vec<u8>],
    num_classes: u32,
    scales: &[RawYolo26Scale],
    max_detections: usize,
) -> Vec<Detection> {
    // Score floor: anything below this is discarded before NMS to keep
    // the candidate set bounded. yolo26n's default training threshold
    // is 0.001 but per-class >0.25 is the standard inference cutoff.
    const SCORE_FLOOR: f32 = 0.20;
    const IOU_THRESHOLD: f32 = 0.70;

    // Network input dims = max(stride * grid_w) across scales.
    let input_w = scales.iter().map(|s| s.stride * s.w).max().unwrap_or(640) as f32;
    let input_h = scales.iter().map(|s| s.stride * s.h).max().unwrap_or(640) as f32;
    let inv_w = 1.0 / input_w;
    let inv_h = 1.0 / input_h;
    let nc = num_classes as usize;

    let mut candidates: Vec<Detection> = Vec::new();
    for scale in scales {
        let box_buf = match buffers.get(scale.box_idx) {
            Some(b) => b,
            None => continue,
        };
        let cls_buf = match buffers.get(scale.score_idx) {
            Some(b) => b,
            None => continue,
        };
        let h = scale.h as usize;
        let w = scale.w as usize;
        let stride = scale.stride as f32;
        let need_box = h * w * 4 * 4;
        let need_cls = h * w * nc * 4;
        if box_buf.len() < need_box || cls_buf.len() < need_cls {
            // Truncated/oversized buffer — skip this scale rather than
            // panic; postproc continues on the other scales.
            continue;
        }
        for gy in 0..h {
            for gx in 0..w {
                let cls_base = (gy * w + gx) * nc * 4;
                // Best class for this cell.
                let mut best_score = 0.0f32;
                let mut best_class: u16 = 0;
                for c in 0..nc {
                    let o = cls_base + c * 4;
                    let s = f32::from_le_bytes([
                        cls_buf[o],
                        cls_buf[o + 1],
                        cls_buf[o + 2],
                        cls_buf[o + 3],
                    ]);
                    if s > best_score {
                        best_score = s;
                        best_class = c as u16;
                    }
                }
                if best_score < SCORE_FLOOR {
                    continue;
                }
                // Decode box: (l, t, r, b) in cell units → normalized xyxy.
                let box_base = (gy * w + gx) * 4 * 4;
                let l = f32::from_le_bytes([
                    box_buf[box_base],
                    box_buf[box_base + 1],
                    box_buf[box_base + 2],
                    box_buf[box_base + 3],
                ]);
                let t = f32::from_le_bytes([
                    box_buf[box_base + 4],
                    box_buf[box_base + 5],
                    box_buf[box_base + 6],
                    box_buf[box_base + 7],
                ]);
                let r = f32::from_le_bytes([
                    box_buf[box_base + 8],
                    box_buf[box_base + 9],
                    box_buf[box_base + 10],
                    box_buf[box_base + 11],
                ]);
                let b = f32::from_le_bytes([
                    box_buf[box_base + 12],
                    box_buf[box_base + 13],
                    box_buf[box_base + 14],
                    box_buf[box_base + 15],
                ]);
                let cx = gx as f32 + 0.5;
                let cy = gy as f32 + 0.5;
                let x1 = ((cx - l) * stride * inv_w).clamp(0.0, 1.0);
                let y1 = ((cy - t) * stride * inv_h).clamp(0.0, 1.0);
                let x2 = ((cx + r) * stride * inv_w).clamp(0.0, 1.0);
                let y2 = ((cy + b) * stride * inv_h).clamp(0.0, 1.0);
                if x2 <= x1 || y2 <= y1 {
                    continue;
                }
                candidates.push(Detection {
                    y_min: y1,
                    x_min: x1,
                    y_max: y2,
                    x_max: x2,
                    score: best_score,
                    class_id: best_class,
                });
            }
        }
    }

    nms_greedy(candidates, IOU_THRESHOLD, max_detections)
}

/// Class-agnostic greedy IoU NMS. Standard YOLOv8 inference uses
/// per-class NMS; the cost difference is negligible at our detection
/// volumes and class-agnostic is friendlier to the downstream tracker.
fn nms_greedy(
    mut dets: Vec<Detection>,
    iou_threshold: f32,
    max_detections: usize,
) -> Vec<Detection> {
    if dets.is_empty() {
        return dets;
    }
    dets.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut keep: Vec<Detection> = Vec::with_capacity(dets.len().min(max_detections));
    'outer: for d in dets {
        if keep.len() >= max_detections {
            break;
        }
        for k in &keep {
            if iou(&d, k) >= iou_threshold {
                continue 'outer;
            }
        }
        keep.push(d);
    }
    keep
}

fn iou(a: &Detection, b: &Detection) -> f32 {
    let ix1 = a.x_min.max(b.x_min);
    let iy1 = a.y_min.max(b.y_min);
    let ix2 = a.x_max.min(b.x_max);
    let iy2 = a.y_max.min(b.y_max);
    let iw = (ix2 - ix1).max(0.0);
    let ih = (iy2 - iy1).max(0.0);
    let inter = iw * ih;
    let area_a = (a.x_max - a.x_min) * (a.y_max - a.y_min);
    let area_b = (b.x_max - b.x_min) * (b.y_max - b.y_min);
    let union = area_a + area_b - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn check(code: i32, call: &'static str) -> Result<(), Error> {
    if code == HAILO_SUCCESS {
        Ok(())
    } else {
        Err(Error::Status {
            call,
            status: code,
            status_name: status_name(code),
        })
    }
}

fn make_input_params(
    ng: hailo_configured_network_group,
    format_type: u32,
) -> Result<Vec<hailo_input_vstream_params_by_name_t>, Error> {
    // HailoRT does NOT support NULL+count-only discovery here — the
    // header is silent but `CHECK_ARG_NOT_NULL` fires on a NULL
    // input_params even when count is 0. Preallocate the API ceiling
    // (HAILO_MAX_STREAMS_COUNT = 40), let the runtime fill in the
    // actual count, then truncate. yolo26n.hef has 1 input vstream
    // so this is ~600 KiB of stack-bound copying, no real overhead.
    let mut count: usize = HAILO_MAX_STREAMS_COUNT;
    let mut buf: Vec<hailo_input_vstream_params_by_name_t> =
        vec![unsafe { std::mem::zeroed() }; count];
    check(
        unsafe {
            ffi::hailo_make_input_vstream_params(
                ng,
                false,
                format_type,
                buf.as_mut_ptr(),
                &mut count,
            )
        },
        "hailo_make_input_vstream_params",
    )?;
    if count == 0 {
        return Err(Error::LayoutMismatch("network has 0 input vstreams".into()));
    }
    buf.truncate(count);
    Ok(buf)
}

fn make_output_params(
    ng: hailo_configured_network_group,
    format_type: u32,
) -> Result<Vec<hailo_output_vstream_params_by_name_t>, Error> {
    // Same NULL-rejection issue as make_input_params — preallocate.
    let mut count: usize = HAILO_MAX_STREAMS_COUNT;
    let mut buf: Vec<hailo_output_vstream_params_by_name_t> =
        vec![unsafe { std::mem::zeroed() }; count];
    check(
        unsafe {
            ffi::hailo_make_output_vstream_params(
                ng,
                false,
                format_type,
                buf.as_mut_ptr(),
                &mut count,
            )
        },
        "hailo_make_output_vstream_params",
    )?;
    if count == 0 {
        return Err(Error::LayoutMismatch(
            "network has 0 output vstreams".into(),
        ));
    }
    buf.truncate(count);
    Ok(buf)
}

fn get_input_info(vs: hailo_input_vstream) -> Result<hailo_vstream_info_t, Error> {
    let mut info: hailo_vstream_info_t = unsafe { std::mem::zeroed() };
    check(
        unsafe { ffi::hailo_get_input_vstream_info(vs, &mut info) },
        "hailo_get_input_vstream_info",
    )?;
    Ok(info)
}

fn get_output_info(vs: hailo_output_vstream) -> Result<hailo_vstream_info_t, Error> {
    let mut info: hailo_vstream_info_t = unsafe { std::mem::zeroed() };
    check(
        unsafe { ffi::hailo_get_output_vstream_info(vs, &mut info) },
        "hailo_get_output_vstream_info",
    )?;
    Ok(info)
}

fn c_array_to_string(buf: &[std::os::raw::c_char], len: usize) -> String {
    let bytes: Vec<u8> = buf.iter().take(len).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

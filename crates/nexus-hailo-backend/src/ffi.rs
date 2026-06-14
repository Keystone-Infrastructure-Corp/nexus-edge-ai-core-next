//! Manual FFI bindings to libhailort.so.4.
//!
//! We hand-write the ~21 functions and ~12 structs we need rather than
//! pulling in `bindgen` because:
//!
//! 1. The C ABI is documented and stable across HailoRT 4.x point
//!    releases (Hailo bumps the SONAME on breaking changes — `.so.4`
//!    spans 4.20..4.23 today).
//! 2. `bindgen` requires `libclang` at build time on every dev box, plus
//!    vendored headers in the repo — both add friction for a feature
//!    that only ships on one production target (linux + Hailo M.2).
//! 3. The struct layouts mirror exactly what's in
//!    `/usr/include/hailo/hailort.h` v4.23.0, verified against the
//!    installed headers on EQR7. If the layout changes between releases
//!    we'll see immediate test failures, not silent UB.
//!
//! Binding mechanism: the functions are resolved at **runtime** via
//! `libloading::Library::new("libhailort.so.4")` + `dlsym` on first
//! call to `ensure_loaded()`. Build-time linking is intentionally
//! avoided — see the comment on the loader section below for the
//! license + CI rationale.
//!
//! See `/tmp/hailort-headers/hailort.h` on the dev box (scp'd from
//! EQR7) for the canonical definitions.

#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(dead_code)]

use std::os::raw::{c_char, c_void};

// ---------------------------------------------------------------------------
// Opaque handles. Every HailoRT object is a `struct _foo *` pointer.
// We model them as `*mut c_void` to avoid having to declare each phantom
// struct — the C API gives them no surface beyond their pointer identity.
// ---------------------------------------------------------------------------

pub type hailo_vdevice = *mut c_void;
pub type hailo_hef = *mut c_void;
pub type hailo_configured_network_group = *mut c_void;
pub type hailo_input_vstream = *mut c_void;
pub type hailo_output_vstream = *mut c_void;
pub type hailo_device = *mut c_void;

// ---------------------------------------------------------------------------
// Constants from hailort.h
// ---------------------------------------------------------------------------

pub const HAILO_MAX_NAME_SIZE: usize = 128;
pub const HAILO_MAX_STREAM_NAME_SIZE: usize = HAILO_MAX_NAME_SIZE;
pub const HAILO_MAX_NETWORK_GROUP_NAME_SIZE: usize = HAILO_MAX_NAME_SIZE;
pub const HAILO_MAX_NETWORK_NAME_SIZE: usize =
    HAILO_MAX_NETWORK_GROUP_NAME_SIZE + 1 + HAILO_MAX_NAME_SIZE;
pub const HAILO_DEFAULT_VSTREAM_TIMEOUT_MS: u32 = 10_000;
pub const HAILO_DEFAULT_VSTREAM_QUEUE_SIZE: u32 = 2;

// ---------------------------------------------------------------------------
// hailo_status (enum) — we only name the codes we explicitly check for.
// ---------------------------------------------------------------------------

pub const HAILO_SUCCESS: i32 = 0;
pub const HAILO_INVALID_ARGUMENT: i32 = 2;
pub const HAILO_TIMEOUT: i32 = 4;
pub const HAILO_INSUFFICIENT_BUFFER: i32 = 5;
pub const HAILO_NOT_FOUND: i32 = 61;
pub const HAILO_NOT_AVAILABLE: i32 = 65;
pub const HAILO_STREAM_ABORT: i32 = 63;

/// Friendly name for diagnostic messages.
pub fn status_name(code: i32) -> &'static str {
    match code {
        0 => "HAILO_SUCCESS",
        1 => "HAILO_UNINITIALIZED",
        2 => "HAILO_INVALID_ARGUMENT",
        3 => "HAILO_OUT_OF_HOST_MEMORY",
        4 => "HAILO_TIMEOUT",
        5 => "HAILO_INSUFFICIENT_BUFFER",
        6 => "HAILO_INVALID_OPERATION",
        7 => "HAILO_NOT_IMPLEMENTED",
        8 => "HAILO_INTERNAL_FAILURE",
        13 => "HAILO_OPEN_FILE_FAILURE",
        25 => "HAILO_INVALID_FRAME",
        26 => "HAILO_INVALID_HEF",
        61 => "HAILO_NOT_FOUND",
        63 => "HAILO_STREAM_ABORT",
        64 => "HAILO_DRIVER_NOT_INSTALLED",
        65 => "HAILO_NOT_AVAILABLE",
        69 => "HAILO_NETWORK_GROUP_NOT_ACTIVATED",
        70 => "HAILO_VSTREAM_PIPELINE_NOT_ACTIVATED",
        73 => "HAILO_DEVICE_IN_USE",
        79 => "HAILO_NOT_SUPPORTED",
        91 => "HAILO_HEF_FILE_CORRUPTED",
        92 => "HAILO_HEF_NOT_SUPPORTED",
        93 => "HAILO_HEF_NOT_COMPATIBLE_WITH_DEVICE",
        _ => "HAILO_UNKNOWN",
    }
}

// ---------------------------------------------------------------------------
// hailo_format_type_t / hailo_format_order_t / hailo_format_flags_t
// ---------------------------------------------------------------------------

pub const HAILO_FORMAT_TYPE_AUTO: u32 = 0;
pub const HAILO_FORMAT_TYPE_UINT8: u32 = 1;
pub const HAILO_FORMAT_TYPE_UINT16: u32 = 2;
pub const HAILO_FORMAT_TYPE_FLOAT32: u32 = 3;

pub const HAILO_FORMAT_ORDER_AUTO: u32 = 0;
pub const HAILO_FORMAT_ORDER_NHWC: u32 = 1;
pub const HAILO_FORMAT_ORDER_NCHW: u32 = 11;
pub const HAILO_FORMAT_ORDER_HAILO_NMS_BY_CLASS: u32 = 22;
pub const HAILO_FORMAT_ORDER_HAILO_NMS_BY_SCORE: u32 = 23;
pub const HAILO_FORMAT_ORDER_HAILO_NMS_ON_CHIP: u32 = 21;
pub const HAILO_FORMAT_ORDER_HAILO_NMS_WITH_BYTE_MASK: u32 = 20;

pub const HAILO_FORMAT_FLAGS_NONE: u32 = 0;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct hailo_format_t {
    pub type_: u32, // hailo_format_type_t
    pub order: u32, // hailo_format_order_t
    pub flags: u32, // hailo_format_flags_t
}

// ---------------------------------------------------------------------------
// hailo_3d_image_shape_t / hailo_nms_shape_t / hailo_vstream_info_t
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct hailo_3d_image_shape_t {
    pub height: u32,
    pub width: u32,
    pub features: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct hailo_nms_shape_t {
    pub number_of_classes: u32,
    pub max_bboxes_per_class: u32,
    pub max_bboxes_total: u32,
    pub max_accumulated_mask_size: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct hailo_quant_info_t {
    pub qp_zp: f32,
    pub qp_scale: f32,
    pub limvals_min: f32,
    pub limvals_max: f32,
}

/// `union { hailo_3d_image_shape_t shape; hailo_nms_shape_t nms_shape; }`
/// — both are 16 bytes, so the union is 16 bytes. We use a `[u32; 4]`
/// inline representation and decode based on `format.order`.
#[repr(C)]
#[derive(Copy, Clone)]
pub union hailo_vstream_shape_union {
    pub shape: hailo_3d_image_shape_t,
    pub nms_shape: hailo_nms_shape_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct hailo_vstream_info_t {
    pub name: [c_char; HAILO_MAX_STREAM_NAME_SIZE],
    pub network_name: [c_char; HAILO_MAX_NETWORK_NAME_SIZE],
    pub direction: u32, // hailo_stream_direction_t (0 = H2D, 1 = D2H)
    pub format: hailo_format_t,
    pub shape_union: hailo_vstream_shape_union,
    pub quant_info: hailo_quant_info_t,
}

// ---------------------------------------------------------------------------
// vstream params (used to override format / timeout / queue size).
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct hailo_vstream_params_t {
    pub user_buffer_format: hailo_format_t,
    pub timeout_ms: u32,
    pub queue_size: u32,
    pub vstream_stats_flags: u32,
    pub pipeline_elements_stats_flags: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct hailo_input_vstream_params_by_name_t {
    pub name: [c_char; HAILO_MAX_STREAM_NAME_SIZE],
    pub params: hailo_vstream_params_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct hailo_output_vstream_params_by_name_t {
    pub name: [c_char; HAILO_MAX_STREAM_NAME_SIZE],
    pub params: hailo_vstream_params_t,
}

// ---------------------------------------------------------------------------
// vdevice / configure params.
//
// Both `hailo_create_vdevice(NULL, ...)` and `hailo_configure_vdevice(...,
// NULL, ...)` accept NULL params — the C side fills defaults from
// `hailo_init_vdevice_params` / `hailo_init_configure_params_by_vdevice`
// internally. Per the header at /usr/include/hailo/hailort.h line 2448:
//
//   "params: A @a hailo_configure_params_t (may be NULL). Can be
//    initialized to default values using ::hailo_init_configure_params_by_vdevice."
//
// So we DON'T mirror `hailo_configure_params_t` here — the struct is
// large (~40 KiB with HAILO_MAX_NETWORK_GROUPS=8) and the layout is
// version-prone. Passing NULL keeps us forward-compatible across HailoRT
// 4.x point releases and saves ~1000 lines of bindings.
//
// vdevice defaults that NULL gives us: 1 physical device chosen
// automatically, MULTI_PROCESS_SERVICE off, ROUND_ROBIN scheduling.
// That matches the single-tenant edge engine model.
//
// configure defaults: batch_size=0 (auto), power_mode=PERFORMANCE,
// every network group from the HEF is configured. yolo26n.hef has one
// network group ("yolo26n"), so we get one entry back.
// ---------------------------------------------------------------------------

/// Upper bound on network groups returned by `hailo_configure_vdevice`.
/// From `HAILO_MAX_NETWORK_GROUPS` in hailort.h. yolo26n.hef has one.
pub const HAILO_MAX_NETWORK_GROUPS: usize = 8;

/// From `HAILO_MAX_STREAMS_COUNT` in hailort.h. Used as the
/// up-front capacity passed to `hailo_make_input/output_vstream_params`,
/// which (unlike most other count-query APIs in HailoRT) does NOT
/// accept a NULL pointer + zero count for size discovery — it
/// requires the caller to provide a pre-allocated buffer big enough
/// to hold every stream. 40 covers anything plausible for a single
/// network group (yolo26n.hef has 1 input + 1 output).
pub const HAILO_MAX_STREAMS_COUNT: usize = 40;

// ---------------------------------------------------------------------------
// Identity / firmware info structs (for probe binary).
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone)]
pub struct hailo_device_identity_t {
    pub protocol_version: u32,
    pub fw_version: hailo_firmware_version_t,
    pub logger_version: u32,
    pub board_name_length: u8,
    pub board_name: [c_char; HAILO_MAX_NAME_SIZE],
    pub is_release: bool,
    pub extended_context_switch_buffer: bool,
    pub device_architecture: u32,
    pub serial_number_length: u8,
    pub serial_number: [c_char; HAILO_MAX_NAME_SIZE],
    pub part_number_length: u8,
    pub part_number: [c_char; HAILO_MAX_NAME_SIZE],
    pub product_name_length: u8,
    pub product_name: [c_char; HAILO_MAX_NAME_SIZE],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct hailo_firmware_version_t {
    pub major: u32,
    pub minor: u32,
    pub revision: u32,
}

// ---------------------------------------------------------------------------
// Telemetry — chip temperature + on-chip power measurement.
//
// Both symbols are core HailoRT 4.x API; the 4.23 SONAME exports them
// (`nm -D libhailort.so.4 | grep -E 'hailo_get_chip_temperature|hailo_power_measurement'`
// is non-empty). Used by the System tab in the local admin UI.
// ---------------------------------------------------------------------------

/// `hailo_chip_temperature_info_t` from `hailort.h`.
///
/// Hailo-8 carries two on-die temperature sensors; the runtime reports
/// both readings plus the count of samples that went into them.
/// `sample_count` is u16 in the header but trails 2 bytes of padding
/// to align the struct to 4 bytes, which Rust handles automatically
/// via `#[repr(C)]`.
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct hailo_chip_temperature_info_t {
    pub ts0_temperature: f32,
    pub ts1_temperature: f32,
    pub sample_count: u16,
}

/// `hailo_dvm_options_e` — which on-chip dvm (Digital Voltage Monitor)
/// to read for power measurement. `AUTO` is `INT_MAX` per the header
/// and means "let HailoRT pick the right one for this chip" (the
/// overcurrent-protection DVM on Hailo-8, per `hailortcli measure-power`
/// output). We only ever pass AUTO.
pub const HAILO_DVM_OPTIONS_AUTO: i32 = i32::MAX;

/// `hailo_power_measurement_types_e` — what to measure. `AUTO` is
/// `INT_MAX` per the header; on Hailo-8 it resolves to POWER (watts).
pub const HAILO_POWER_MEASUREMENT_TYPES_AUTO: i32 = i32::MAX;

// ---------------------------------------------------------------------------
// Runtime-loaded HailoRT bindings.
//
// We `dlopen` `libhailort.so.4` on first call to `ensure_loaded()` and
// `dlsym` each function pointer we use. Build-time linking against
// libhailort was tried first and rejected: the .deb is distributed
// under Hailo's developer-zone EULA, which would force every CI run to
// cache an EULA-gated artifact. Runtime loading keeps the CI build
// graph clean (zero Hailo artifacts on the runner) and lets the engine
// degrade gracefully on boxes where HailoRT was never installed —
// `ensure_loaded()` returns `Error::NotAvailable` and the YOLO
// dispatcher in `crates/nexus-inference/src/yolo.rs` falls through to
// the ONNX path.
//
// SONAME `libhailort.so.4` is intentional: HailoRT bumps the SONAME
// only on breaking ABI changes (4.20→4.21→…→4.23 are all so.4), so
// pinning to `.so.4` survives in-place HailoRT upgrades while
// rejecting an accidentally-installed major-bumped libhailort.so.5.
// ---------------------------------------------------------------------------

use std::sync::OnceLock;

use libloading::{Library, Symbol};

use crate::error::Error;

// --- function-pointer type aliases ---
// vdevice
pub type FnHailoCreateVdevice =
    unsafe extern "C" fn(params: *const c_void, vdevice: *mut hailo_vdevice) -> i32;
pub type FnHailoReleaseVdevice = unsafe extern "C" fn(vdevice: hailo_vdevice) -> i32;
pub type FnHailoGetPhysicalDevices = unsafe extern "C" fn(
    vdevice: hailo_vdevice,
    devices: *mut hailo_device,
    number_of_devices: *mut usize,
) -> i32;

// device identity
pub type FnHailoIdentify = unsafe extern "C" fn(
    device: hailo_device,
    device_identity: *mut hailo_device_identity_t,
) -> i32;

// device telemetry
pub type FnHailoGetChipTemperature = unsafe extern "C" fn(
    device: hailo_device,
    temp_info: *mut hailo_chip_temperature_info_t,
) -> i32;
pub type FnHailoPowerMeasurement = unsafe extern "C" fn(
    device: hailo_device,
    dvm: i32,
    measurement_type: i32,
    measurement: *mut f32,
) -> i32;

// HEF
pub type FnHailoCreateHefFile =
    unsafe extern "C" fn(hef: *mut hailo_hef, file_name: *const c_char) -> i32;
pub type FnHailoReleaseHef = unsafe extern "C" fn(hef: hailo_hef) -> i32;

// configure
pub type FnHailoConfigureVdevice = unsafe extern "C" fn(
    vdevice: hailo_vdevice,
    hef: hailo_hef,
    params: *mut c_void,
    network_groups: *mut hailo_configured_network_group,
    number_of_network_groups: *mut usize,
) -> i32;

// vstream params helpers
pub type FnHailoMakeInputVstreamParams = unsafe extern "C" fn(
    network_group: hailo_configured_network_group,
    unused: bool,
    format_type: u32,
    input_params: *mut hailo_input_vstream_params_by_name_t,
    input_params_count: *mut usize,
) -> i32;
pub type FnHailoMakeOutputVstreamParams = unsafe extern "C" fn(
    network_group: hailo_configured_network_group,
    unused: bool,
    format_type: u32,
    output_params: *mut hailo_output_vstream_params_by_name_t,
    output_params_count: *mut usize,
) -> i32;

// vstream lifecycle
pub type FnHailoCreateInputVstreams = unsafe extern "C" fn(
    network_group: hailo_configured_network_group,
    inputs_params: *const hailo_input_vstream_params_by_name_t,
    inputs_count: usize,
    input_vstreams: *mut hailo_input_vstream,
) -> i32;
pub type FnHailoCreateOutputVstreams = unsafe extern "C" fn(
    network_group: hailo_configured_network_group,
    outputs_params: *const hailo_output_vstream_params_by_name_t,
    outputs_count: usize,
    output_vstreams: *mut hailo_output_vstream,
) -> i32;
pub type FnHailoReleaseInputVstreams =
    unsafe extern "C" fn(input_vstreams: *const hailo_input_vstream, inputs_count: usize) -> i32;
pub type FnHailoReleaseOutputVstreams =
    unsafe extern "C" fn(output_vstreams: *const hailo_output_vstream, outputs_count: usize) -> i32;

// vstream info / sizes
pub type FnHailoGetInputVstreamFrameSize =
    unsafe extern "C" fn(input_vstream: hailo_input_vstream, frame_size: *mut usize) -> i32;
pub type FnHailoGetInputVstreamInfo = unsafe extern "C" fn(
    input_vstream: hailo_input_vstream,
    vstream_info: *mut hailo_vstream_info_t,
) -> i32;
pub type FnHailoGetOutputVstreamFrameSize =
    unsafe extern "C" fn(output_vstream: hailo_output_vstream, frame_size: *mut usize) -> i32;
pub type FnHailoGetOutputVstreamInfo = unsafe extern "C" fn(
    output_vstream: hailo_output_vstream,
    vstream_info: *mut hailo_vstream_info_t,
) -> i32;

// inference
pub type FnHailoVstreamWriteRawBuffer = unsafe extern "C" fn(
    input_vstream: hailo_input_vstream,
    buffer: *const c_void,
    buffer_size: usize,
) -> i32;
pub type FnHailoVstreamReadRawBuffer = unsafe extern "C" fn(
    output_vstream: hailo_output_vstream,
    buffer: *mut c_void,
    buffer_size: usize,
) -> i32;

// post-processing knobs
pub type FnHailoVstreamSetNmsScoreThreshold =
    unsafe extern "C" fn(output_vstream: hailo_output_vstream, threshold: f32) -> i32;
pub type FnHailoVstreamSetNmsIouThreshold =
    unsafe extern "C" fn(output_vstream: hailo_output_vstream, threshold: f32) -> i32;

/// Resolved HailoRT — owns the loaded `libhailort.so.4` and one
/// function pointer per symbol we use. Constructed once by
/// `ensure_loaded()` and then read-only for the rest of the process.
pub struct HailoRt {
    // `_lib` MUST outlive every function pointer below — dropping the
    // Library unmaps the .so and turns the pointers into dangling
    // references. Kept first so Drop runs it last (Rust drops fields
    // top-to-bottom).
    _lib: Library,

    pub hailo_create_vdevice: FnHailoCreateVdevice,
    pub hailo_release_vdevice: FnHailoReleaseVdevice,
    pub hailo_get_physical_devices: FnHailoGetPhysicalDevices,
    pub hailo_identify: FnHailoIdentify,
    pub hailo_get_chip_temperature: FnHailoGetChipTemperature,
    pub hailo_power_measurement: FnHailoPowerMeasurement,
    pub hailo_create_hef_file: FnHailoCreateHefFile,
    pub hailo_release_hef: FnHailoReleaseHef,
    pub hailo_configure_vdevice: FnHailoConfigureVdevice,
    pub hailo_make_input_vstream_params: FnHailoMakeInputVstreamParams,
    pub hailo_make_output_vstream_params: FnHailoMakeOutputVstreamParams,
    pub hailo_create_input_vstreams: FnHailoCreateInputVstreams,
    pub hailo_create_output_vstreams: FnHailoCreateOutputVstreams,
    pub hailo_release_input_vstreams: FnHailoReleaseInputVstreams,
    pub hailo_release_output_vstreams: FnHailoReleaseOutputVstreams,
    pub hailo_get_input_vstream_frame_size: FnHailoGetInputVstreamFrameSize,
    pub hailo_get_input_vstream_info: FnHailoGetInputVstreamInfo,
    pub hailo_get_output_vstream_frame_size: FnHailoGetOutputVstreamFrameSize,
    pub hailo_get_output_vstream_info: FnHailoGetOutputVstreamInfo,
    pub hailo_vstream_write_raw_buffer: FnHailoVstreamWriteRawBuffer,
    pub hailo_vstream_read_raw_buffer: FnHailoVstreamReadRawBuffer,
    pub hailo_vstream_set_nms_score_threshold: FnHailoVstreamSetNmsScoreThreshold,
    pub hailo_vstream_set_nms_iou_threshold: FnHailoVstreamSetNmsIouThreshold,
}

unsafe impl Send for HailoRt {}
unsafe impl Sync for HailoRt {}

/// dlopen `libhailort.so.4` and resolve every symbol we need.
///
/// SAFETY: `libloading::Library::new` runs the .so's init functions;
/// HailoRT's init is benign (no signal handlers, no thread spawn until
/// the first vdevice). Each `get` is `unsafe` because the caller
/// promises the C symbol matches the Rust signature — we hand-verify
/// every signature against `/usr/include/hailo/hailort.h` v4.23.0.
unsafe fn load_lib() -> Result<HailoRt, Error> {
    // Try the SONAME first (always present when the .deb is installed);
    // fall back to the unversioned symlink in case a future libhailort
    // installs only the linker symlink. The unversioned fallback is
    // mostly defensive — production boxes always have the SONAME.
    let lib = unsafe {
        Library::new("libhailort.so.4")
            .or_else(|_| Library::new("libhailort.so"))
            .map_err(|e| {
                Error::LibraryNotFound(format!(
                    "libhailort.so.4 (and libhailort.so fallback) not found: {e}"
                ))
            })?
    };

    // Resolve every symbol up front so a partial-install surface
    // immediately at session-open time, not on the first per-frame call.
    macro_rules! sym {
        ($lib:expr, $name:literal, $ty:ty) => {{
            let s: Symbol<$ty> = unsafe {
                $lib.get(concat!($name, "\0").as_bytes())
                    .map_err(|e| Error::SymbolNotFound {
                        symbol: $name,
                        cause: format!("{e}"),
                    })?
            };
            *s
        }};
    }

    let hailo_create_vdevice = sym!(lib, "hailo_create_vdevice", FnHailoCreateVdevice);
    let hailo_release_vdevice = sym!(lib, "hailo_release_vdevice", FnHailoReleaseVdevice);
    let hailo_get_physical_devices =
        sym!(lib, "hailo_get_physical_devices", FnHailoGetPhysicalDevices);
    let hailo_identify = sym!(lib, "hailo_identify", FnHailoIdentify);
    let hailo_get_chip_temperature =
        sym!(lib, "hailo_get_chip_temperature", FnHailoGetChipTemperature);
    let hailo_power_measurement = sym!(lib, "hailo_power_measurement", FnHailoPowerMeasurement);
    let hailo_create_hef_file = sym!(lib, "hailo_create_hef_file", FnHailoCreateHefFile);
    let hailo_release_hef = sym!(lib, "hailo_release_hef", FnHailoReleaseHef);
    let hailo_configure_vdevice = sym!(lib, "hailo_configure_vdevice", FnHailoConfigureVdevice);
    let hailo_make_input_vstream_params = sym!(
        lib,
        "hailo_make_input_vstream_params",
        FnHailoMakeInputVstreamParams
    );
    let hailo_make_output_vstream_params = sym!(
        lib,
        "hailo_make_output_vstream_params",
        FnHailoMakeOutputVstreamParams
    );
    let hailo_create_input_vstreams = sym!(
        lib,
        "hailo_create_input_vstreams",
        FnHailoCreateInputVstreams
    );
    let hailo_create_output_vstreams = sym!(
        lib,
        "hailo_create_output_vstreams",
        FnHailoCreateOutputVstreams
    );
    let hailo_release_input_vstreams = sym!(
        lib,
        "hailo_release_input_vstreams",
        FnHailoReleaseInputVstreams
    );
    let hailo_release_output_vstreams = sym!(
        lib,
        "hailo_release_output_vstreams",
        FnHailoReleaseOutputVstreams
    );
    let hailo_get_input_vstream_frame_size = sym!(
        lib,
        "hailo_get_input_vstream_frame_size",
        FnHailoGetInputVstreamFrameSize
    );
    let hailo_get_input_vstream_info = sym!(
        lib,
        "hailo_get_input_vstream_info",
        FnHailoGetInputVstreamInfo
    );
    let hailo_get_output_vstream_frame_size = sym!(
        lib,
        "hailo_get_output_vstream_frame_size",
        FnHailoGetOutputVstreamFrameSize
    );
    let hailo_get_output_vstream_info = sym!(
        lib,
        "hailo_get_output_vstream_info",
        FnHailoGetOutputVstreamInfo
    );
    let hailo_vstream_write_raw_buffer = sym!(
        lib,
        "hailo_vstream_write_raw_buffer",
        FnHailoVstreamWriteRawBuffer
    );
    let hailo_vstream_read_raw_buffer = sym!(
        lib,
        "hailo_vstream_read_raw_buffer",
        FnHailoVstreamReadRawBuffer
    );
    let hailo_vstream_set_nms_score_threshold = sym!(
        lib,
        "hailo_vstream_set_nms_score_threshold",
        FnHailoVstreamSetNmsScoreThreshold
    );
    let hailo_vstream_set_nms_iou_threshold = sym!(
        lib,
        "hailo_vstream_set_nms_iou_threshold",
        FnHailoVstreamSetNmsIouThreshold
    );

    Ok(HailoRt {
        _lib: lib,
        hailo_create_vdevice,
        hailo_release_vdevice,
        hailo_get_physical_devices,
        hailo_identify,
        hailo_get_chip_temperature,
        hailo_power_measurement,
        hailo_create_hef_file,
        hailo_release_hef,
        hailo_configure_vdevice,
        hailo_make_input_vstream_params,
        hailo_make_output_vstream_params,
        hailo_create_input_vstreams,
        hailo_create_output_vstreams,
        hailo_release_input_vstreams,
        hailo_release_output_vstreams,
        hailo_get_input_vstream_frame_size,
        hailo_get_input_vstream_info,
        hailo_get_output_vstream_frame_size,
        hailo_get_output_vstream_info,
        hailo_vstream_write_raw_buffer,
        hailo_vstream_read_raw_buffer,
        hailo_vstream_set_nms_score_threshold,
        hailo_vstream_set_nms_iou_threshold,
    })
}

static LIB: OnceLock<HailoRt> = OnceLock::new();

/// Idempotent — `dlopen`s libhailort on the first call, returns a
/// cached reference on every subsequent call. Returns
/// `Error::LibraryNotFound` / `Error::SymbolNotFound` when HailoRT is
/// not installed; the caller should surface that as
/// `Error::NotAvailable` to upstream and fall back to the ONNX path.
pub fn ensure_loaded() -> Result<&'static HailoRt, Error> {
    if let Some(lib) = LIB.get() {
        return Ok(lib);
    }
    let lib = unsafe { load_lib()? };
    // `OnceLock::set` may lose the race with another thread that also
    // called `ensure_loaded()`. That's harmless — both `HailoRt`s hold
    // independent `dlopen` handles for the same .so and either one is
    // valid. We drop the loser via `_` and read the winner back via
    // `get().expect()` (always populated at this point).
    let _ = LIB.set(lib);
    Ok(LIB.get().expect("LIB initialized above"))
}

/// Borrow the loaded HailoRt — panics if `ensure_loaded` has not yet
/// returned `Ok` in this process. Use this in inner helpers that have
/// already established the load via an `ensure_loaded()?` on entry.
#[inline]
fn lib() -> &'static HailoRt {
    LIB.get()
        .expect("nexus_hailo_backend::ffi::ensure_loaded() must succeed before using FFI wrappers")
}

// ---------------------------------------------------------------------------
// Free-function wrappers — preserve the pre-dlopen call-site shape so
// `imp.rs` only needs to add one `ensure_loaded()?` at each public
// entry point. Every wrapper assumes `ensure_loaded` already succeeded.
// ---------------------------------------------------------------------------

#[inline]
pub unsafe fn hailo_create_vdevice(params: *const c_void, vdevice: *mut hailo_vdevice) -> i32 {
    unsafe { (lib().hailo_create_vdevice)(params, vdevice) }
}
#[inline]
pub unsafe fn hailo_release_vdevice(vdevice: hailo_vdevice) -> i32 {
    unsafe { (lib().hailo_release_vdevice)(vdevice) }
}
#[inline]
pub unsafe fn hailo_get_physical_devices(
    vdevice: hailo_vdevice,
    devices: *mut hailo_device,
    number_of_devices: *mut usize,
) -> i32 {
    unsafe { (lib().hailo_get_physical_devices)(vdevice, devices, number_of_devices) }
}
#[inline]
pub unsafe fn hailo_identify(
    device: hailo_device,
    device_identity: *mut hailo_device_identity_t,
) -> i32 {
    unsafe { (lib().hailo_identify)(device, device_identity) }
}
#[inline]
pub unsafe fn hailo_get_chip_temperature(
    device: hailo_device,
    temp_info: *mut hailo_chip_temperature_info_t,
) -> i32 {
    unsafe { (lib().hailo_get_chip_temperature)(device, temp_info) }
}
#[inline]
pub unsafe fn hailo_power_measurement(
    device: hailo_device,
    dvm: i32,
    measurement_type: i32,
    measurement: *mut f32,
) -> i32 {
    unsafe { (lib().hailo_power_measurement)(device, dvm, measurement_type, measurement) }
}
#[inline]
pub unsafe fn hailo_create_hef_file(hef: *mut hailo_hef, file_name: *const c_char) -> i32 {
    unsafe { (lib().hailo_create_hef_file)(hef, file_name) }
}
#[inline]
pub unsafe fn hailo_release_hef(hef: hailo_hef) -> i32 {
    unsafe { (lib().hailo_release_hef)(hef) }
}
#[inline]
pub unsafe fn hailo_configure_vdevice(
    vdevice: hailo_vdevice,
    hef: hailo_hef,
    params: *mut c_void,
    network_groups: *mut hailo_configured_network_group,
    number_of_network_groups: *mut usize,
) -> i32 {
    unsafe {
        (lib().hailo_configure_vdevice)(
            vdevice,
            hef,
            params,
            network_groups,
            number_of_network_groups,
        )
    }
}
#[inline]
pub unsafe fn hailo_make_input_vstream_params(
    network_group: hailo_configured_network_group,
    unused: bool,
    format_type: u32,
    input_params: *mut hailo_input_vstream_params_by_name_t,
    input_params_count: *mut usize,
) -> i32 {
    unsafe {
        (lib().hailo_make_input_vstream_params)(
            network_group,
            unused,
            format_type,
            input_params,
            input_params_count,
        )
    }
}
#[inline]
pub unsafe fn hailo_make_output_vstream_params(
    network_group: hailo_configured_network_group,
    unused: bool,
    format_type: u32,
    output_params: *mut hailo_output_vstream_params_by_name_t,
    output_params_count: *mut usize,
) -> i32 {
    unsafe {
        (lib().hailo_make_output_vstream_params)(
            network_group,
            unused,
            format_type,
            output_params,
            output_params_count,
        )
    }
}
#[inline]
pub unsafe fn hailo_create_input_vstreams(
    network_group: hailo_configured_network_group,
    inputs_params: *const hailo_input_vstream_params_by_name_t,
    inputs_count: usize,
    input_vstreams: *mut hailo_input_vstream,
) -> i32 {
    unsafe {
        (lib().hailo_create_input_vstreams)(
            network_group,
            inputs_params,
            inputs_count,
            input_vstreams,
        )
    }
}
#[inline]
pub unsafe fn hailo_create_output_vstreams(
    network_group: hailo_configured_network_group,
    outputs_params: *const hailo_output_vstream_params_by_name_t,
    outputs_count: usize,
    output_vstreams: *mut hailo_output_vstream,
) -> i32 {
    unsafe {
        (lib().hailo_create_output_vstreams)(
            network_group,
            outputs_params,
            outputs_count,
            output_vstreams,
        )
    }
}
#[inline]
pub unsafe fn hailo_release_input_vstreams(
    input_vstreams: *const hailo_input_vstream,
    inputs_count: usize,
) -> i32 {
    unsafe { (lib().hailo_release_input_vstreams)(input_vstreams, inputs_count) }
}
#[inline]
pub unsafe fn hailo_release_output_vstreams(
    output_vstreams: *const hailo_output_vstream,
    outputs_count: usize,
) -> i32 {
    unsafe { (lib().hailo_release_output_vstreams)(output_vstreams, outputs_count) }
}
#[inline]
pub unsafe fn hailo_get_input_vstream_frame_size(
    input_vstream: hailo_input_vstream,
    frame_size: *mut usize,
) -> i32 {
    unsafe { (lib().hailo_get_input_vstream_frame_size)(input_vstream, frame_size) }
}
#[inline]
pub unsafe fn hailo_get_input_vstream_info(
    input_vstream: hailo_input_vstream,
    vstream_info: *mut hailo_vstream_info_t,
) -> i32 {
    unsafe { (lib().hailo_get_input_vstream_info)(input_vstream, vstream_info) }
}
#[inline]
pub unsafe fn hailo_get_output_vstream_frame_size(
    output_vstream: hailo_output_vstream,
    frame_size: *mut usize,
) -> i32 {
    unsafe { (lib().hailo_get_output_vstream_frame_size)(output_vstream, frame_size) }
}
#[inline]
pub unsafe fn hailo_get_output_vstream_info(
    output_vstream: hailo_output_vstream,
    vstream_info: *mut hailo_vstream_info_t,
) -> i32 {
    unsafe { (lib().hailo_get_output_vstream_info)(output_vstream, vstream_info) }
}
#[inline]
pub unsafe fn hailo_vstream_write_raw_buffer(
    input_vstream: hailo_input_vstream,
    buffer: *const c_void,
    buffer_size: usize,
) -> i32 {
    unsafe { (lib().hailo_vstream_write_raw_buffer)(input_vstream, buffer, buffer_size) }
}
#[inline]
pub unsafe fn hailo_vstream_read_raw_buffer(
    output_vstream: hailo_output_vstream,
    buffer: *mut c_void,
    buffer_size: usize,
) -> i32 {
    unsafe { (lib().hailo_vstream_read_raw_buffer)(output_vstream, buffer, buffer_size) }
}
#[inline]
pub unsafe fn hailo_vstream_set_nms_score_threshold(
    output_vstream: hailo_output_vstream,
    threshold: f32,
) -> i32 {
    unsafe { (lib().hailo_vstream_set_nms_score_threshold)(output_vstream, threshold) }
}
#[inline]
pub unsafe fn hailo_vstream_set_nms_iou_threshold(
    output_vstream: hailo_output_vstream,
    threshold: f32,
) -> i32 {
    unsafe { (lib().hailo_vstream_set_nms_iou_threshold)(output_vstream, threshold) }
}

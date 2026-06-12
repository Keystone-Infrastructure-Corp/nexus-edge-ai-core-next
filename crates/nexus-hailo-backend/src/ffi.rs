//! Manual FFI bindings to libhailort.so.4.23.
//!
//! We hand-write the ~25 functions and ~12 structs we need rather than
//! pulling in `bindgen` because:
//!
//! 1. The C ABI is documented and stable across HailoRT 4.x point
//!    releases (Hailo bumps the SONAME on breaking changes).
//! 2. `bindgen` requires `libclang` at build time on every dev box, plus
//!    vendored headers in the repo — both add friction for a feature
//!    that only ships on one production target (linux + Hailo M.2).
//! 3. The struct layouts mirror exactly what's in
//!    `/usr/include/hailo/hailort.h` v4.23.0, verified against the
//!    installed headers on EQR7. If the layout changes between releases
//!    we'll see immediate test failures, not silent UB.
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
// Extern declarations. Link to libhailort.so via the system loader.
//
// All functions live in `libhailort.so` (SONAME `libhailort.so.4.23` on
// the EQR7 install; the build chooses `libhailort.so` symlink so an
// in-place HailoRT upgrade picks up automatically).
// ---------------------------------------------------------------------------

#[link(name = "hailort")]
unsafe extern "C" {
    // --- vdevice ---
    pub fn hailo_create_vdevice(
        params: *const c_void, // null → defaults
        vdevice: *mut hailo_vdevice,
    ) -> i32;
    pub fn hailo_release_vdevice(vdevice: hailo_vdevice) -> i32;
    pub fn hailo_get_physical_devices(
        vdevice: hailo_vdevice,
        devices: *mut hailo_device,
        number_of_devices: *mut usize,
    ) -> i32;

    // --- device identity (for probe) ---
    pub fn hailo_identify(
        device: hailo_device,
        device_identity: *mut hailo_device_identity_t,
    ) -> i32;

    // --- HEF ---
    pub fn hailo_create_hef_file(hef: *mut hailo_hef, file_name: *const c_char) -> i32;
    pub fn hailo_release_hef(hef: hailo_hef) -> i32;

    // --- configure ---
    // `params` is documented as nullable in hailort.h — passing NULL
    // gives defaults (batch_size=auto, power_mode=PERFORMANCE, every
    // network group in the HEF gets configured). We always pass NULL.
    pub fn hailo_configure_vdevice(
        vdevice: hailo_vdevice,
        hef: hailo_hef,
        params: *mut c_void, // hailo_configure_params_t — pass NULL
        network_groups: *mut hailo_configured_network_group,
        number_of_network_groups: *mut usize,
    ) -> i32;

    // --- vstream params helpers ---
    pub fn hailo_make_input_vstream_params(
        network_group: hailo_configured_network_group,
        unused: bool,
        format_type: u32,
        input_params: *mut hailo_input_vstream_params_by_name_t,
        input_params_count: *mut usize,
    ) -> i32;
    pub fn hailo_make_output_vstream_params(
        network_group: hailo_configured_network_group,
        unused: bool,
        format_type: u32,
        output_params: *mut hailo_output_vstream_params_by_name_t,
        output_params_count: *mut usize,
    ) -> i32;

    // --- vstream lifecycle ---
    pub fn hailo_create_input_vstreams(
        network_group: hailo_configured_network_group,
        inputs_params: *const hailo_input_vstream_params_by_name_t,
        inputs_count: usize,
        input_vstreams: *mut hailo_input_vstream,
    ) -> i32;
    pub fn hailo_create_output_vstreams(
        network_group: hailo_configured_network_group,
        outputs_params: *const hailo_output_vstream_params_by_name_t,
        outputs_count: usize,
        output_vstreams: *mut hailo_output_vstream,
    ) -> i32;
    pub fn hailo_release_input_vstreams(
        input_vstreams: *const hailo_input_vstream,
        inputs_count: usize,
    ) -> i32;
    pub fn hailo_release_output_vstreams(
        output_vstreams: *const hailo_output_vstream,
        outputs_count: usize,
    ) -> i32;

    // --- vstream info / sizes ---
    pub fn hailo_get_input_vstream_frame_size(
        input_vstream: hailo_input_vstream,
        frame_size: *mut usize,
    ) -> i32;
    pub fn hailo_get_input_vstream_info(
        input_vstream: hailo_input_vstream,
        vstream_info: *mut hailo_vstream_info_t,
    ) -> i32;
    pub fn hailo_get_output_vstream_frame_size(
        output_vstream: hailo_output_vstream,
        frame_size: *mut usize,
    ) -> i32;
    pub fn hailo_get_output_vstream_info(
        output_vstream: hailo_output_vstream,
        vstream_info: *mut hailo_vstream_info_t,
    ) -> i32;

    // --- inference (synchronous, one frame at a time) ---
    pub fn hailo_vstream_write_raw_buffer(
        input_vstream: hailo_input_vstream,
        buffer: *const c_void,
        buffer_size: usize,
    ) -> i32;
    pub fn hailo_vstream_read_raw_buffer(
        output_vstream: hailo_output_vstream,
        buffer: *mut c_void,
        buffer_size: usize,
    ) -> i32;

    // --- post-processing knobs we expose ---
    pub fn hailo_vstream_set_nms_score_threshold(
        output_vstream: hailo_output_vstream,
        threshold: f32,
    ) -> i32;
    pub fn hailo_vstream_set_nms_iou_threshold(
        output_vstream: hailo_output_vstream,
        threshold: f32,
    ) -> i32;
}

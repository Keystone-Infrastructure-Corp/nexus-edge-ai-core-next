//! Error type for the Hailo backend.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// The current build cannot talk to a Hailo device: either the
    /// `linked` cargo feature is off, or the target is not Linux.
    #[error("hailo backend not available in this build (target / feature off)")]
    NotAvailable,

    /// A HailoRT C call returned a non-zero status. The numeric value
    /// matches the `hailo_status` enum in `/usr/include/hailo/hailort.h`.
    #[error("HailoRT call `{call}` failed: status {status} ({status_name})")]
    Status {
        call: &'static str,
        status: i32,
        status_name: &'static str,
    },

    /// Wrong assumption about HEF / model layout — e.g. expected
    /// `NMS_BY_CLASS` output and got `NHWC`.
    #[error("hailo HEF layout mismatch: {0}")]
    LayoutMismatch(String),

    /// Path arg couldn't be encoded as a C string (interior NUL).
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// IO error opening the HEF file (the C API takes a path, but we
    /// stat first so the error is more useful than `HAILO_OPEN_FILE_FAILURE`).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

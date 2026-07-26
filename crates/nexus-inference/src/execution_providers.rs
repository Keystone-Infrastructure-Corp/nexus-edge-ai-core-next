//! Translate [`nexus_config::InferenceConfig::ep_priority`] into the
//! list of ORT [`ExecutionProviderDispatch`] that the session builder
//! will register, in priority order.
//!
//! M5a — `ep_priority` was previously read from config and logged, but
//! the actual session was hardcoded to CPU. This module is the
//! single source of truth for the cargo-feature → EP-type mapping;
//! both `yolo` and `yolo_world` detectors call into it.
//!
//! Conventions
//! -----------
//! * Per-EP code is gated by **two** signals:
//!   1. A cargo feature (`ep-openvino`, `ep-cuda`, …) — controls
//!      whether the EP type is even compiled into the binary.
//!   2. A runtime check on `ep_priority` — controls whether ORT
//!      tries to attach it for this particular session.
//! * **CPU is always appended** at the end if not already present.
//!   The ORT shared library always ships with the CPU EP, so this
//!   fallback is total. Even if every requested accelerator EP
//!   silently fails to register (no `.so`, no device), the session
//!   will still build.
//! * **Unknown EP names** are dropped with a `warn!` log so operators
//!   can see typos in their config without the engine refusing to
//!   boot.
//! * **`"npu"` and `"gpu"` route through OpenVINO** with an
//!   explicit `device_type` set on the EP builder (`"NPU"` and
//!   `"GPU"` respectively). NPU/GPU are not first-class ORT EPs
//!   today; the OpenVINO EP is the dispatcher. If the model fails
//!   to compile for the requested device (e.g. an unsupported op
//!   on NPU), ORT silently falls through to the next EP in the
//!   priority list — log `session.providers()` after commit to
//!   see what actually attached.
//! * **`"openvino"` is the operator-tunable entry**: it honours
//!   `OV_DEVICE` (`CPU`, `GPU`, `NPU`, `AUTO`, `HETERO:NPU,GPU,CPU`,
//!   …) and defaults to `"AUTO"` when unset, which lets OpenVINO
//!   pick the best available device for the loaded model. Prefer
//!   the explicit `"npu"` / `"gpu"` entries when you specifically
//!   want that device — they're self-documenting in `nexus.toml`.
//!
//! Historical: before this commit, both `"npu"` and `"openvino"`
//! built `OpenVINOExecutionProvider::default()`, whose default
//! `device_type` is `"CPU"` (per the OpenVINO ONNX RT provider
//! summary table) — so the OpenVINO EP attached successfully BUT
//! every inference ran on the CPU plugin, and `/sys/class/accel/
//! accel0/device/npu_busy_time_us` stayed at zero. The fix is to
//! pass `device_type` explicitly.
//!
//! Logging
//! -------
//! [`selected_for_priority`] returns both the dispatch list AND the
//! human-readable names of the EPs that were actually added. Callers
//! log the names so the operator can see exactly which EPs the
//! binary chose, vs. which they asked for in config.
//!
//! Note that ORT 2.0 silently *skips* an EP that fails to attach at
//! runtime (`with_execution_providers` returns Ok even if some EPs
//! couldn't load). To see which EPs are *actually* running per
//! session, call `session.providers()` after `commit_from_file` —
//! callers do this and include the result in their own info log.

#![cfg(feature = "ort")]

use ort::execution_providers::{CPUExecutionProvider, ExecutionProviderDispatch};
use tracing::warn;

/// Returns true iff this container can plausibly drive an Intel iGPU, dGPU,
/// or NPU through the OpenVINO EP.
///
/// We detect via kernel-level device nodes:
///   * `/dev/dri/renderD12{8,9}` — i915 / xe driver render nodes (iGPU/dGPU)
///   * `/dev/accel/accel0`       — `intel_vpu` driver (NPU on Lunar Lake)
///
/// The userspace runtime libs (libonnxruntime.so + the OpenVINO provider .so
/// + libopenvino*.so + Level Zero) are bundled in the published Docker image
/// via the `onnxruntime-openvino` PyPI wheel, so device-node visibility is
/// the only check that depends on the host. When the relevant overlay
/// (`deploy/docker-compose.tNN.yml`) is in use, `/dev/dri` is bind-mounted
/// in and the container user is added to the host's `render` group — that
/// makes the device node `stat()`-able from inside the container.
///
/// Result is cached for the process lifetime (the device topology doesn't
/// change without a host reboot, and the cost of repeated `stat()`s on every
/// session create is small but non-zero).
///
/// **Override:** set `NEXUS_OPENVINO_DEVICE=force` to force-true (useful for
/// engineers iterating on the EP wiring without an Intel accelerator) or
/// `NEXUS_OPENVINO_DEVICE=skip` to force-false (useful for verifying the
/// CPU-fallback path on a box that *does* have an iGPU). Unset / any other
/// value goes through autodetection.
// rust 1.95+ clippy tightened `doc_lazy_continuation`: prose paragraphs
// after a bullet list are now flagged. The mixed bullets-plus-prose
// shape above is intentional documentation; suppress narrowly on this
// item rather than reflowing into one giant indented list.
#[allow(clippy::doc_lazy_continuation)]
pub fn openvino_runtime_available() -> bool {
    if let Ok(v) = std::env::var("NEXUS_OPENVINO_DEVICE") {
        match v.trim().to_ascii_lowercase().as_str() {
            "force" | "present" | "1" | "true" => return true,
            "skip" | "absent" | "0" | "false" => return false,
            _ => {}
        }
    }
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let igpu = std::path::Path::new("/dev/dri/renderD128").exists()
            || std::path::Path::new("/dev/dri/renderD129").exists();
        let npu = std::path::Path::new("/dev/accel/accel0").exists();
        igpu || npu
    })
}

/// Emit a single deduplicated WARN when OpenVINO was requested but no
/// matching Intel device is reachable. Without this, the ORT C++ side
/// logs an ugly
///   `[E:onnxruntime:default, provider_bridge_ort.cc:2141] Failed to
///   load library libonnxruntime_providers_openvino.so`
/// on every session create — but in fact our images ship that .so
/// since v0.1.5 and the *real* reason inference falls back to CPU is
/// that the device node isn't present. This WARN says exactly that.
#[cfg(feature = "ep-openvino")]
fn warn_openvino_unavailable_once() {
    use std::sync::OnceLock;
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        tracing::warn!(
            "ep_priority requested 'openvino' / 'npu' but no Intel iGPU \
             (/dev/dri/renderD12x) or NPU (/dev/accel/accel0) device node \
             is reachable inside this container; OpenVINO entries are being \
             skipped and inference will run on CPU. For hardware \
             acceleration, ensure the device overlay \
             (deploy/docker-compose.<device>.yml) is layered on top of the base \
             compose file — it bind-mounts /dev/dri and adds the render \
             group to the container user. Override autodetection by setting \
             NEXUS_OPENVINO_DEVICE=force in the environment."
        );
    });
}

/// CUDA device ordinal for the CUDA EP.
///
/// Defaults to 0 — every reference NVIDIA box is single-GPU. Set
/// `NEXUS_CUDA_DEVICE` to pin a specific GPU on a multi-GPU host (the
/// ordinal matches `nvidia-smi -L`). A malformed value warns once and
/// falls back to 0 rather than failing the session open, keeping this
/// consistent with the fail-soft posture of the rest of EP selection.
#[cfg(feature = "ep-cuda")]
fn cuda_device_id() -> i32 {
    match std::env::var("NEXUS_CUDA_DEVICE") {
        Err(_) => 0,
        Ok(raw) => match raw.trim().parse::<i32>() {
            Ok(id) if id >= 0 => id,
            _ => {
                warn!(
                    value = %raw,
                    "NEXUS_CUDA_DEVICE is not a non-negative integer; using CUDA device 0"
                );
                0
            }
        },
    }
}

/// Returns true iff this container can drive an AMD Radeon GPU through the ROCm EP.
///
/// We detect via kernel-level device nodes:
///   * `/dev/kfd` — AMD KFD compute driver (present on all RDNA/CDNA GPUs and some iGPUs)
///   * `/dev/dri/renderD12{8,9}` — amdgpu DRM render node
///
/// ROCm is registered only for AMD GPUs it officially supports — discrete
/// RDNA/CDNA parts (gfx1030+). The installer classifies the GPU from its PCI
/// device ID and routes unsupported parts (Phoenix/Rembrandt iGPUs,
/// gfx1035/gfx1103) to the Vulkan(WebGPU) EP instead. `HSA_OVERRIDE_GFX_VERSION`
/// is not set by default; it remains only as an unsupported manual escape
/// hatch for operators who insist on force-fitting ROCm onto such an iGPU.
///
/// The userspace runtime libs (rocm-hip-runtime, rocm-opencl-runtime, librocm_*.so,
/// and a ROCm-enabled libonnxruntime.so) are installed via apt/package manager
/// or containerized as a per-device bundle, so device-node visibility is the
/// check that depends on the host container configuration.
///
/// Unlike the OpenVINO probe, this one verifies the device nodes are actually
/// **openable**, not merely present. ORT silently skips an OpenVINO EP that
/// fails to attach, but the ROCm provider's device probe
/// (`hipGetDeviceProperties`) throws an *uncaught* C++ exception and aborts
/// the whole engine process when it cannot open `/dev/kfd` — e.g. the node
/// exists but the engine user isn't in the `render` group. There is no
/// silent EP-skip to fall back on, so we must not register the ROCm EP at
/// all in that case. Probing with a real `open()` (matching what HIP does
/// internally) means a permission failure degrades to the CPU EP instead of
/// crashing the engine.
///
/// Result is cached for the process lifetime.
///
/// **Override:** set `NEXUS_ROCM_DEVICE=force` to force-true (for testing)
/// or `NEXUS_ROCM_DEVICE=skip` to force-false (to verify CPU fallback on a
/// box that does have an AMD GPU). Unset / any other value goes through
/// autodetection.
#[cfg(feature = "ep-rocm")]
pub fn rocm_runtime_available() -> bool {
    if let Ok(v) = std::env::var("NEXUS_ROCM_DEVICE") {
        match v.trim().to_ascii_lowercase().as_str() {
            "force" | "present" | "1" | "true" => return true,
            "skip" | "absent" | "0" | "false" => return false,
            _ => {}
        }
    }
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        // Probe with an actual open() (read+write, as HIP opens the KFD).
        // The standard nodes are `crw-rw---- root render`, so a process in
        // the render group opens them and a non-member gets EACCES — which
        // is exactly the abort-vs-fallback distinction we need to make
        // before registering the EP.
        let openable = |p: &str| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(p)
                .is_ok()
        };
        let kfd = openable("/dev/kfd");
        let render = openable("/dev/dri/renderD128") || openable("/dev/dri/renderD129");
        kfd && render
    })
}

/// Emit a single deduplicated WARN when ROCm was requested but no AMD GPU
/// device nodes are reachable.
#[cfg(feature = "ep-rocm")]
fn warn_rocm_unavailable_once() {
    use std::sync::OnceLock;
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        tracing::warn!(
            "ep_priority requested 'rocm' but no AMD GPU device nodes \
             (/dev/kfd and /dev/dri/renderD12x) are reachable; ROCm entries \
             are being skipped and inference will run on CPU. For hardware \
             acceleration, ensure /dev/kfd and /dev/dri are accessible to \
             the engine process (via group membership or device permissions). \
             Override autodetection by setting NEXUS_ROCM_DEVICE=force in \
             the environment."
        );
    });
}

/// Returns true iff this host can plausibly drive a GPU through the WebGPU
/// EP on its Vulkan (Dawn) backend.
///
/// ONNX Runtime has **no native Vulkan execution provider**; on Linux the
/// WebGPU EP *is* the Vulkan path (Dawn → Vulkan). This is the default
/// accelerator for AMD GPUs that ROCm does NOT officially support — Phoenix
/// iGPUs like the Radeon 680M/780M (gfx1035/gfx1103) and other unsupported
/// gfx targets. Officially-supported discrete RDNA/CDNA GPUs go through ROCm
/// instead (see [`rocm_runtime_available`]).
///
/// We detect via two host signals:
///   * a DRM render node (`/dev/dri/renderD12{8,9}`) — the minimum for a
///     GPU-backed Vulkan device;
///   * a Vulkan ICD manifest in a standard search dir
///     (`/usr/share/vulkan/icd.d` or `/etc/vulkan/icd.d`) — without an
///     installed ICD (e.g. mesa RADV from `mesa-vulkan-drivers`) Dawn finds
///     no Vulkan driver and inference falls through to CPU.
///
/// Unlike the ROCm probe this does NOT need a permission-checking `open()`:
/// the WebGPU EP's `register()` returns a `Result` and ORT silently skips an
/// EP that fails to attach, so a permission/device failure degrades to the
/// next EP rather than aborting the process (the ROCm provider, by contrast,
/// throws an uncaught C++ exception). The probe exists mainly to suppress the
/// ORT C++ "failed to create WebGPU device" log spam on CPU-only hosts and to
/// emit a single actionable WARN.
///
/// Result is cached for the process lifetime.
///
/// **Override:** set `NEXUS_VULKAN_DEVICE=force` to force-true (testing, or to
/// override a missed ICD path) or `NEXUS_VULKAN_DEVICE=skip` to force-false
/// (verify CPU fallback / fast field kill switch). Unset / any other value
/// goes through autodetection.
#[cfg(feature = "ep-vulkan")]
#[allow(clippy::doc_lazy_continuation)]
pub fn vulkan_runtime_available() -> bool {
    if let Ok(v) = std::env::var("NEXUS_VULKAN_DEVICE") {
        match v.trim().to_ascii_lowercase().as_str() {
            "force" | "present" | "1" | "true" => return true,
            "skip" | "absent" | "0" | "false" => return false,
            _ => {}
        }
    }
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let render = std::path::Path::new("/dev/dri/renderD128").exists()
            || std::path::Path::new("/dev/dri/renderD129").exists();
        // A Vulkan ICD manifest (*.json) must be installed for Dawn to find a
        // driver. Standard loader search dirs on Debian/Ubuntu.
        let icd_present = |dir: &str| {
            std::fs::read_dir(dir)
                .map(|entries| {
                    entries.filter_map(Result::ok).any(|e| {
                        e.path()
                            .extension()
                            .is_some_and(|x| x.eq_ignore_ascii_case("json"))
                    })
                })
                .unwrap_or(false)
        };
        let icd = icd_present("/usr/share/vulkan/icd.d") || icd_present("/etc/vulkan/icd.d");
        render && icd
    })
}

/// Emit a single deduplicated WARN when Vulkan/WebGPU was requested but no
/// Vulkan-capable device + ICD is reachable.
#[cfg(feature = "ep-vulkan")]
fn warn_vulkan_unavailable_once() {
    use std::sync::OnceLock;
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        tracing::warn!(
            "ep_priority requested 'vulkan'/'webgpu' but no Vulkan-capable \
             GPU is reachable (need a DRM render node /dev/dri/renderD12x AND \
             a Vulkan ICD in /usr/share/vulkan/icd.d); Vulkan entries are \
             being skipped and inference will run on CPU. Install a Vulkan \
             driver (e.g. mesa-vulkan-drivers for AMD RADV) and ensure the \
             engine user can reach the render node. Override autodetection by \
             setting NEXUS_VULKAN_DEVICE=force in the environment."
        );
    });
}

/// Build the list of EPs to register with the ORT session, in the
/// priority order requested by `ep_priority`. Always appends CPU as
/// the final fallback if it wasn't already in the list.
///
/// Returns `(dispatchers, names)` where `names` is a human-readable
/// label for each successfully-added EP (suffixed with `"(fallback)"`
/// for the implicit CPU append).
pub fn selected_for_priority(
    ep_priority: &[String],
) -> (Vec<ExecutionProviderDispatch>, Vec<String>) {
    selected_for_priority_inner(ep_priority, openvino_runtime_available())
}

/// Pure-function core of [`selected_for_priority`]. Split out so tests can
/// drive both the "OpenVINO device present" and "OpenVINO device absent"
/// branches deterministically, regardless of the host the test runs on.
fn selected_for_priority_inner(
    ep_priority: &[String],
    #[cfg_attr(not(feature = "ep-openvino"), allow(unused_variables))] openvino_available: bool,
) -> (Vec<ExecutionProviderDispatch>, Vec<String>) {
    let mut dispatchers: Vec<ExecutionProviderDispatch> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut seen_cpu = false;

    for ep in ep_priority {
        let key = ep.trim().to_ascii_lowercase();
        match key.as_str() {
            "cpu" => {
                if !seen_cpu {
                    dispatchers.push(CPUExecutionProvider::default().build());
                    names.push("cpu".into());
                    seen_cpu = true;
                }
            }

            #[cfg(feature = "ep-openvino")]
            "openvino" => {
                if openvino_available {
                    use ort::execution_providers::OpenVINOExecutionProvider;
                    let device = std::env::var("OV_DEVICE").unwrap_or_else(|_| "AUTO".into());
                    dispatchers.push(
                        OpenVINOExecutionProvider::default()
                            .with_device_type(&device)
                            .build(),
                    );
                    names.push(format!("openvino({device})"));
                } else {
                    warn_openvino_unavailable_once();
                }
            }
            #[cfg(not(feature = "ep-openvino"))]
            "openvino" => warn!(
                "ep_priority requested 'openvino' but the binary was built without \
                 --features ep-openvino; skipping"
            ),

            #[cfg(feature = "ep-cuda")]
            "cuda" => {
                use ort::execution_providers::CUDAExecutionProvider;
                // Single-GPU boxes (every reference NVIDIA build) want
                // device 0. `NEXUS_CUDA_DEVICE` pins a specific GPU on a
                // multi-GPU host; a non-numeric value falls back to 0
                // rather than failing the session open.
                let device_id = cuda_device_id();
                dispatchers.push(
                    CUDAExecutionProvider::default()
                        .with_device_id(device_id)
                        .build(),
                );
                names.push(format!("cuda(device {device_id})"));
            }
            #[cfg(not(feature = "ep-cuda"))]
            "cuda" => warn!(
                "ep_priority requested 'cuda' but the binary was built without \
                 --features ep-cuda; skipping"
            ),

            #[cfg(feature = "ep-tensorrt")]
            "tensorrt" => {
                use ort::execution_providers::TensorRTExecutionProvider;
                dispatchers.push(TensorRTExecutionProvider::default().build());
                names.push("tensorrt".into());
            }
            #[cfg(not(feature = "ep-tensorrt"))]
            "tensorrt" => warn!(
                "ep_priority requested 'tensorrt' but the binary was built without \
                 --features ep-tensorrt; skipping"
            ),

            #[cfg(feature = "ep-coreml")]
            "coreml" => {
                use ort::execution_providers::CoreMLExecutionProvider;
                dispatchers.push(CoreMLExecutionProvider::default().build());
                names.push("coreml".into());
            }
            #[cfg(not(feature = "ep-coreml"))]
            "coreml" => warn!(
                "ep_priority requested 'coreml' but the binary was built without \
                 --features ep-coreml; skipping"
            ),

            #[cfg(feature = "ep-directml")]
            "directml" => {
                use ort::execution_providers::DirectMLExecutionProvider;
                dispatchers.push(DirectMLExecutionProvider::default().build());
                names.push("directml".into());
            }
            #[cfg(not(feature = "ep-directml"))]
            "directml" => warn!(
                "ep_priority requested 'directml' but the binary was built without \
                 --features ep-directml; skipping"
            ),

            // NPU routes through OpenVINO with device_type=NPU. ORT
            // silently falls through to the next EP if NPU compile
            // fails (e.g. unsupported op for this model), so always
            // keep `cpu` as the final fallback.
            #[cfg(feature = "ep-openvino")]
            "npu" => {
                if openvino_available {
                    use ort::execution_providers::OpenVINOExecutionProvider;
                    dispatchers.push(
                        OpenVINOExecutionProvider::default()
                            .with_device_type("NPU")
                            .build(),
                    );
                    names.push("npu(via-openvino)".into());
                } else {
                    warn_openvino_unavailable_once();
                }
            }
            #[cfg(not(feature = "ep-openvino"))]
            "npu" => warn!(
                "ep_priority requested 'npu' but the binary was built without \
                 --features ep-openvino (NPU routes through OpenVINO); skipping"
            ),

            // Intel iGPU/dGPU via OpenVINO with device_type=GPU.
            // Symmetric with the `npu` entry above — self-documenting
            // in nexus.toml. Use `openvino` + `OV_DEVICE=GPU.1` for
            // multi-GPU selection.
            #[cfg(feature = "ep-openvino")]
            "gpu" => {
                if openvino_available {
                    use ort::execution_providers::OpenVINOExecutionProvider;
                    dispatchers.push(
                        OpenVINOExecutionProvider::default()
                            .with_device_type("GPU")
                            .build(),
                    );
                    names.push("gpu(via-openvino)".into());
                } else {
                    warn_openvino_unavailable_once();
                }
            }
            #[cfg(not(feature = "ep-openvino"))]
            "gpu" => warn!(
                "ep_priority requested 'gpu' but the binary was built without \
                 --features ep-openvino (Intel GPU routes through OpenVINO); skipping"
            ),

            // AMD Radeon GPU via ROCm EP — officially-supported discrete
            // RDNA/CDNA GPUs only. Unsupported parts (Phoenix iGPUs etc.) are
            // routed to the Vulkan(WebGPU) arm by the installer, not here.
            // HSA_OVERRIDE_GFX_VERSION is an unsupported manual escape hatch,
            // never set by default.
            #[cfg(feature = "ep-rocm")]
            "rocm" => {
                let rocm_available = rocm_runtime_available();
                if rocm_available {
                    use ort::execution_providers::ROCmExecutionProvider;
                    dispatchers.push(ROCmExecutionProvider::default().build());
                    names.push("rocm".into());
                } else {
                    warn_rocm_unavailable_once();
                }
            }
            #[cfg(not(feature = "ep-rocm"))]
            "rocm" => warn!(
                "ep_priority requested 'rocm' but the binary was built without \
                 --features ep-rocm; skipping"
            ),

            // Vulkan-accelerated inference via the WebGPU EP on its Dawn→Vulkan
            // backend. ONNX Runtime has no native Vulkan EP; WebGPU IS the
            // Vulkan path on Linux. This is the default accelerator for AMD
            // GPUs that ROCm does not officially support (Phoenix iGPUs etc).
            // The operator token is `vulkan` (canonical); `webgpu` is accepted
            // as an alias for the same arm. WebGPU is flagged experimental
            // upstream, but a failed attach degrades to the next EP (no SIGABRT
            // like ROCm), so the CPU terminal fallback is a total safety net.
            #[cfg(feature = "ep-vulkan")]
            "vulkan" | "webgpu" => {
                if vulkan_runtime_available() {
                    use ort::execution_providers::webgpu::DawnBackendType;
                    use ort::execution_providers::WebGPU;
                    dispatchers.push(
                        WebGPU::default()
                            .with_dawn_backend_type(DawnBackendType::Vulkan)
                            .build(),
                    );
                    names.push("vulkan(webgpu)".into());
                } else {
                    warn_vulkan_unavailable_once();
                }
            }
            #[cfg(not(feature = "ep-vulkan"))]
            "vulkan" | "webgpu" => warn!(
                "ep_priority requested 'vulkan'/'webgpu' but the binary was built \
                 without --features ep-vulkan; skipping"
            ),

            // M_HAILO_EP — Hailo-8 is not an ORT EP. It's recognized
            // here so the unknown-EP warn doesn't fire; the actual
            // dispatch happens in `crate::yolo::build_detector_for_yolo`
            // when a `.hef` artifact is resolvable from the model pack.
            "hailo" => tracing::info!(
                "ep_priority lists 'hailo': dispatch happens at the detector layer; \
                 ORT EP list unchanged"
            ),

            other => warn!(ep = %other, "unknown EP name in ep_priority; ignoring"),
        }
    }

    if !seen_cpu {
        dispatchers.push(CPUExecutionProvider::default().build());
        names.push("cpu(fallback)".into());
    }

    (dispatchers, names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_priority_gives_cpu_fallback() {
        let (eps, names) = selected_for_priority(&[]);
        assert_eq!(eps.len(), 1);
        assert_eq!(names, vec!["cpu(fallback)"]);
    }

    #[test]
    fn explicit_cpu_not_duplicated() {
        let (eps, names) = selected_for_priority(&["cpu".into()]);
        assert_eq!(eps.len(), 1);
        assert_eq!(names, vec!["cpu"]);
    }

    #[test]
    fn explicit_cpu_in_middle_suppresses_fallback() {
        let (_, names) = selected_for_priority(&["bogus".into(), "cpu".into(), "alsobogus".into()]);
        // bogus dropped, cpu kept, alsobogus dropped, no fallback append.
        assert_eq!(names, vec!["cpu"]);
    }

    #[test]
    fn unknown_eps_dropped_cpu_still_appended() {
        let (eps, names) = selected_for_priority(&["bogus".into(), "alsobogus".into()]);
        assert_eq!(eps.len(), 1);
        assert_eq!(names, vec!["cpu(fallback)"]);
    }

    #[test]
    fn case_insensitive_match() {
        let (_, names) = selected_for_priority(&["CPU".into()]);
        assert_eq!(names, vec!["cpu"]);
    }

    #[test]
    fn whitespace_tolerated() {
        let (_, names) = selected_for_priority(&["  cpu  ".into()]);
        assert_eq!(names, vec!["cpu"]);
    }

    #[cfg(feature = "ep-coreml")]
    #[test]
    fn coreml_appended_when_feature_on() {
        let (eps, names) = selected_for_priority(&["coreml".into()]);
        assert_eq!(eps.len(), 2);
        assert_eq!(names, vec!["coreml", "cpu(fallback)"]);
    }

    #[cfg(not(feature = "ep-coreml"))]
    #[test]
    fn coreml_dropped_when_feature_off() {
        let (eps, names) = selected_for_priority(&["coreml".into()]);
        // CoreML skipped (warn-logged), CPU appended.
        assert_eq!(eps.len(), 1);
        assert_eq!(names, vec!["cpu(fallback)"]);
    }

    #[cfg(feature = "ep-openvino")]
    #[test]
    fn openvino_then_cpu_when_feature_on() {
        // Force the device-present branch so the test is host-independent.
        // `openvino` entry honours OV_DEVICE env or defaults to AUTO; the
        // displayed name reflects whichever was picked so operators can
        // see at a glance what's in effect.
        let prev = std::env::var("OV_DEVICE").ok();
        std::env::remove_var("OV_DEVICE");
        let (_, names) = selected_for_priority_inner(&["openvino".into(), "cpu".into()], true);
        if let Some(p) = prev {
            std::env::set_var("OV_DEVICE", p);
        }
        assert_eq!(names, vec!["openvino(AUTO)", "cpu"]);
    }

    #[cfg(feature = "ep-openvino")]
    #[test]
    fn openvino_honors_ov_device_env() {
        let prev = std::env::var("OV_DEVICE").ok();
        std::env::set_var("OV_DEVICE", "GPU");
        let (_, names) = selected_for_priority_inner(&["openvino".into()], true);
        match prev {
            Some(p) => std::env::set_var("OV_DEVICE", p),
            None => std::env::remove_var("OV_DEVICE"),
        }
        assert_eq!(names, vec!["openvino(GPU)", "cpu(fallback)"]);
    }

    #[cfg(feature = "ep-openvino")]
    #[test]
    fn npu_routes_through_openvino() {
        let (_, names) = selected_for_priority_inner(&["npu".into()], true);
        assert_eq!(names, vec!["npu(via-openvino)", "cpu(fallback)"]);
    }

    #[cfg(feature = "ep-openvino")]
    #[test]
    fn gpu_routes_through_openvino() {
        let (_, names) = selected_for_priority_inner(&["gpu".into()], true);
        assert_eq!(names, vec!["gpu(via-openvino)", "cpu(fallback)"]);
    }

    /// v0.1.5 regression: when no Intel iGPU/NPU device node is reachable,
    /// `openvino` entries are skipped silently (with a one-shot WARN) and
    /// CPU still gets appended. Stops the ORT C++ side from logging
    /// `Failed to load library libonnxruntime_providers_openvino.so` on
    /// every session create on CPU-only hosts.
    #[cfg(feature = "ep-openvino")]
    #[test]
    fn openvino_dropped_when_device_absent() {
        let (eps, names) = selected_for_priority_inner(&["openvino".into(), "cpu".into()], false);
        // Only CPU is registered; openvino was dropped silently.
        assert_eq!(eps.len(), 1);
        assert_eq!(names, vec!["cpu"]);
    }

    #[cfg(feature = "ep-openvino")]
    #[test]
    fn npu_dropped_when_device_absent() {
        let (eps, names) = selected_for_priority_inner(&["npu".into()], false);
        // npu → openvino dropped, CPU appended as fallback.
        assert_eq!(eps.len(), 1);
        assert_eq!(names, vec!["cpu(fallback)"]);
    }

    #[cfg(feature = "ep-openvino")]
    #[test]
    fn gpu_dropped_when_device_absent() {
        let (eps, names) = selected_for_priority_inner(&["gpu".into()], false);
        assert_eq!(eps.len(), 1);
        assert_eq!(names, vec!["cpu(fallback)"]);
    }

    /// The `NEXUS_OPENVINO_DEVICE=force` escape hatch lets engineers
    /// without an Intel iGPU verify the OpenVINO code path. The
    /// `=skip` variant lets engineers on Intel boxes verify the
    /// CPU-fallback path.
    #[test]
    fn env_override_force_returns_true() {
        // Use a unique-ish env-var dance so we don't fight other tests.
        // SAFETY: setting env vars in tests is process-global, but this
        // helper's reads-and-restores the prior value to keep parallel
        // tests well-behaved. The only state we mutate is one variable.
        let prev = std::env::var("NEXUS_OPENVINO_DEVICE").ok();
        std::env::set_var("NEXUS_OPENVINO_DEVICE", "force");
        let v = openvino_runtime_available();
        match prev {
            Some(p) => std::env::set_var("NEXUS_OPENVINO_DEVICE", p),
            None => std::env::remove_var("NEXUS_OPENVINO_DEVICE"),
        }
        assert!(v, "force override must return true");
    }

    #[test]
    fn env_override_skip_returns_false() {
        let prev = std::env::var("NEXUS_OPENVINO_DEVICE").ok();
        std::env::set_var("NEXUS_OPENVINO_DEVICE", "skip");
        let v = openvino_runtime_available();
        match prev {
            Some(p) => std::env::set_var("NEXUS_OPENVINO_DEVICE", p),
            None => std::env::remove_var("NEXUS_OPENVINO_DEVICE"),
        }
        assert!(!v, "skip override must return false");
    }

    // ── Vulkan (WebGPU-over-Vulkan) EP selection ──────────────────────────
    // The `vulkan` arm calls `vulkan_runtime_available()` directly (like the
    // `rocm` arm). Its `NEXUS_VULKAN_DEVICE=force|skip` override short-circuits
    // the probe BEFORE the cached autodetect, so the selector path is fully
    // deterministic on any host. These env-mutating tests serialize on a
    // module-local mutex so cargo's parallel runner can't interleave them.
    #[cfg(feature = "ep-vulkan")]
    static VULKAN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(feature = "ep-vulkan")]
    fn with_vulkan_env<F: FnOnce()>(value: Option<&str>, f: F) {
        let _guard = VULKAN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("NEXUS_VULKAN_DEVICE").ok();
        match value {
            Some(v) => std::env::set_var("NEXUS_VULKAN_DEVICE", v),
            None => std::env::remove_var("NEXUS_VULKAN_DEVICE"),
        }
        f();
        match prev {
            Some(p) => std::env::set_var("NEXUS_VULKAN_DEVICE", p),
            None => std::env::remove_var("NEXUS_VULKAN_DEVICE"),
        }
    }

    #[cfg(feature = "ep-vulkan")]
    #[test]
    fn vulkan_selected_when_forced() {
        with_vulkan_env(Some("force"), || {
            let (eps, names) = selected_for_priority(&["vulkan".into()]);
            assert_eq!(eps.len(), 2);
            assert_eq!(names, vec!["vulkan(webgpu)", "cpu(fallback)"]);
        });
    }

    /// `webgpu` is an accepted alias for `vulkan` and resolves to the same arm.
    #[cfg(feature = "ep-vulkan")]
    #[test]
    fn webgpu_alias_resolves_to_vulkan() {
        with_vulkan_env(Some("force"), || {
            let (_, names) = selected_for_priority(&["webgpu".into()]);
            assert_eq!(names, vec!["vulkan(webgpu)", "cpu(fallback)"]);
        });
    }

    /// Priority order is preserved: vulkan first, then an explicit cpu (no
    /// duplicate fallback append).
    #[cfg(feature = "ep-vulkan")]
    #[test]
    fn vulkan_then_cpu_preserves_order() {
        with_vulkan_env(Some("force"), || {
            let (_, names) = selected_for_priority(&["vulkan".into(), "cpu".into()]);
            assert_eq!(names, vec!["vulkan(webgpu)", "cpu"]);
        });
    }

    /// With the device forced absent, vulkan is skipped (warn-once) and CPU is
    /// appended — the kill-switch / no-Vulkan-driver path.
    #[cfg(feature = "ep-vulkan")]
    #[test]
    fn vulkan_dropped_when_device_absent() {
        with_vulkan_env(Some("skip"), || {
            let (eps, names) = selected_for_priority(&["vulkan".into(), "cpu".into()]);
            assert_eq!(eps.len(), 1);
            assert_eq!(names, vec!["cpu"]);
        });
    }

    #[cfg(feature = "ep-vulkan")]
    #[test]
    fn vulkan_env_override_force_returns_true() {
        with_vulkan_env(Some("force"), || {
            assert!(
                vulkan_runtime_available(),
                "force override must return true"
            );
        });
    }

    #[cfg(feature = "ep-vulkan")]
    #[test]
    fn vulkan_env_override_skip_returns_false() {
        with_vulkan_env(Some("skip"), || {
            assert!(
                !vulkan_runtime_available(),
                "skip override must return false"
            );
        });
    }

    /// When the binary is built WITHOUT `ep-vulkan`, both the canonical token
    /// and its alias are dropped (warn-logged) and CPU is appended.
    #[cfg(not(feature = "ep-vulkan"))]
    #[test]
    fn vulkan_dropped_when_feature_off() {
        let (eps, names) = selected_for_priority(&["vulkan".into()]);
        assert_eq!(eps.len(), 1);
        assert_eq!(names, vec!["cpu(fallback)"]);
        let (_, alias_names) = selected_for_priority(&["webgpu".into()]);
        assert_eq!(alias_names, vec!["cpu(fallback)"]);
    }
}

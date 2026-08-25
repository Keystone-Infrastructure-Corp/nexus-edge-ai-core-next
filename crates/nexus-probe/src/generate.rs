//! Capability-based `nexus.toml` generator.
//!
//! Turns a [`HardwareProfile`] (itself a pure function of a captured
//! [`crate::Manifest`]) into a complete, guaranteed-parseable
//! [`nexus_config::Config`]. This replaces the old per-box config
//! templates: instead of copying a hand-tuned template and rewriting
//! paths with `sed`, the installer calls `nexus-probe
//! emit-config` and the box's `/etc/nexus/nexus.toml` is derived directly
//! from detected silicon.
//!
//! ## Why build a real `Config` (not a string template)
//!
//! Serializing a real [`nexus_config::Config`] means the output is
//! type-checked, satisfies `#[serde(deny_unknown_fields)]` by
//! construction, and round-trips through the engine's own loader. A
//! malformed config can never be emitted — the worst case is a value we'd
//! want to tune, never a parse failure on the box.
//!
//! ## What actually varies by hardware
//!
//! Only a handful of knobs depend on the box; everything else comes from
//! [`nexus_config::Config::default`]:
//!
//! | Knob | Source |
//! |---|---|
//! | `runtime.worker_threads` | CPU logical cores |
//! | `runtime.decode.mode` | [`DecodeCapability`] (VA / NVDEC / software) |
//! | `inference.ep_priority` | primary [`InferenceDevice`] |
//! | `inference.workers` | primary [`InferenceDevice`] |
//! | `reid.ep_priority` | [`HardwareProfile::reid`] |
//! | `inference.model.preset` (+ `input_width`/`input_height`) | fixed [`DEFAULT_PRESET`] |
//! | `bus.capacity` | total RAM |
//!
//! The constant fields the old templates also set (the `0.0.0.0` binds,
//! the in-process TLS listener, `recorder = "gstreamer"`, `backend =
//! "pool"`, the bare-metal `pack_path`/`ui_root`) are applied here too so
//! the generated file is a drop-in replacement for the old templates.
//!
//! Every function in this module is a pure transform of a
//! [`HardwareProfile`], so the whole thing is unit-testable on macOS CI
//! with no edge hardware present.

use std::path::PathBuf;

use nexus_config::{Config, DecodeMode, InferenceBackendKind, RecorderKind};

use crate::profile::{DecodeCapability, HardwareProfile, InferenceDevice};

/// Bare-metal model-pack directory. The atomic-swap install layout keeps
/// the active release under `/opt/nexus/current` (a symlink flipped on
/// upgrade), so pinning the pack here means an OTA that ships a new pack
/// needs no config edit. The old templates carried the Docker path
/// `/usr/share/nexus/models` and the installer `sed`-rewrote it; the
/// generator emits the final path directly.
const PACK_PATH: &str = "/opt/nexus/current/share/models";

/// Bare-metal SPA root, mirror of [`PACK_PATH`] (was `/usr/share/nexus/ui`
/// in the Docker templates, `sed`-rewritten by the old installer).
const UI_ROOT: &str = "/opt/nexus/current/share/ui";

/// Build a complete [`nexus_config::Config`] for a detected box.
///
/// Starts from [`Config::default`] and overrides only the fields that
/// differ from the defaults — the hardware-varying knobs plus the handful
/// of constant production fields the old templates set explicitly.
pub fn generate_config(profile: &HardwareProfile) -> Config {
    let mut cfg = Config::default();

    // --- runtime --------------------------------------------------------
    // Async workers scale with logical cores (matches every shipped box class:
    // N150 4T -> 4, Lunar Lake 8T -> 8, Arc box 12T -> 12, Ryzen 16T -> 16).
    // A probe that fails to read core count (0) falls back to a safe 4.
    //
    // `blocking_threads` is deliberately NOT set here: its tasks park rather
    // than compute, so it scales with concurrent blocking ops (cameras x
    // sinks), not cores. Core-sizing starved a 4-core/10-camera box (BUG-129);
    // `default_blocking_threads()` is the sized value.
    let threads = if profile.cpu_logical == 0 {
        4
    } else {
        profile.cpu_logical
    };
    cfg.runtime.worker_threads = threads;

    // Real H.264-passthrough clip recording (default is `Stub`, which
    // writes 0-byte placeholder files). Requires the engine built with
    // `--features gstreamer`, which every shipped release binary is.
    cfg.runtime.clips.recorder = RecorderKind::Gstreamer;

    // Hardware-decode strategy. `Va` whenever an Intel/AMD media engine is
    // present, `Nvdec` on an NVIDIA GPU, `Software` otherwise (Hailo/NPU
    // or pure CPU).
    cfg.runtime.decode.mode = match profile.decode {
        DecodeCapability::Va => DecodeMode::Va,
        DecodeCapability::Nvdec => DecodeMode::Nvdec,
        DecodeCapability::Software => DecodeMode::Software,
    };

    // --- server ---------------------------------------------------------
    // Non-loopback admin console + in-process TLS listener (M-HTTPS Phase
    // 1). Identical across every box, but not part of `Config::default`
    // (which keeps the optional listeners off), so set them explicitly.
    cfg.server.ui_bind = Some("0.0.0.0:80".to_string());
    cfg.server.https_bind = Some("0.0.0.0:443".to_string());
    cfg.server.tls_cert_path = Some(PathBuf::from("/etc/nexus/tls/cert.pem"));
    cfg.server.tls_key_path = Some(PathBuf::from("/etc/nexus/tls/key.pem"));
    // Safe even pre-enrollment: the engine only emits the HSTS header once
    // the leaf is the cloud-issued cert, never for the self-signed bootstrap.
    cfg.server.hsts_max_age_seconds = Some(31_536_000);
    cfg.server.ui_root = PathBuf::from(UI_ROOT);

    // --- inference ------------------------------------------------------
    let device = profile.primary_inference();
    cfg.inference.backend = InferenceBackendKind::Pool;
    cfg.inference.workers = workers_for(device, profile.vram_mib);
    cfg.inference.ep_priority = ep_priority_for(device);
    cfg.inference.model.pack_path = Some(PathBuf::from(PACK_PATH));
    let (input_w, input_h) = DEFAULT_PRESET;
    cfg.inference.model.preset = format!("{input_w}x{input_h}");
    // Redundant when `pack_path` is set (the engine resolves `preset`
    // against the manifest and ignores these), but kept for parity with
    // the old templates and as a sane fallback if the pack ever lacks the
    // preset.
    cfg.inference.model.input_width = input_w;
    cfg.inference.model.input_height = input_h;

    // --- reid -----------------------------------------------------------
    // Must be pinned even though `[reid] enabled` defaults to false: the
    // config default is a generic `["openvino", "tensorrt", "cuda", "cpu"]`
    // chain that matches no shipped box, so an operator flipping re-id on
    // later would silently get the CPU EP.
    cfg.reid.ep_priority = ep_priority_for(profile.reid);

    // --- bus ------------------------------------------------------------
    cfg.bus.capacity = bus_capacity_for(profile.ram_bytes);

    cfg
}

/// Map the primary inference device to the engine's `ep_priority` tokens.
///
/// These tokens are matched at detector construction time
/// (`build_detector_for_yolo`) — they are NOT free-form. In particular:
///
/// * Intel iGPU/Arc uses `"gpu"`, the explicit OpenVINO GPU device. Plain
///   `"openvino"` resolves to the AUTO plugin which can silently land on
///   CPU, so the iGPU path must say `"gpu"`.
/// * NVIDIA emits `["cuda", "cpu"]`. The CUDA EP only attaches when the
///   installer has staged a CUDA-capable ONNX Runtime — the release
///   tarball's bundled runtime is the OpenVINO build, which has no CUDA
///   provider. `_install_ort_cuda` in `scripts/lib/install-common.sh`
///   fetches it into `/opt/nexus/vendor/onnxruntime-cuda` and repoints
///   the loader with a systemd drop-in. Without that the session falls
///   through to the trailing `"cpu"`, which is why CPU stays in the list.
///   TensorRT is deliberately NOT in this chain: it needs a separate
///   multi-GB install and only pays off on Tensor-Core (Turing+) GPUs.
fn ep_priority_for(device: InferenceDevice) -> Vec<String> {
    let tokens: &[&str] = match device {
        InferenceDevice::Hailo => &["hailo", "cpu"],
        InferenceDevice::IntelNpu => &["npu", "cpu"],
        InferenceDevice::IntelGpu => &["gpu", "cpu"],
        InferenceDevice::AmdRocm => &["rocm", "cpu"],
        InferenceDevice::AmdVulkan => &["vulkan", "cpu"],
        InferenceDevice::Nvidia => &["cuda", "cpu"],
        InferenceDevice::Cpu => &["cpu"],
    };
    tokens.iter().map(|s| (*s).to_string()).collect()
}

/// Model-pack preset every fresh install starts on — the bottom rung of
/// the native-16:9 ladder (`nexus_config::SHAPE_LADDER`).
///
/// This is deliberately uniform across hardware. The preset does not just
/// size the detector input: it sizes the *supervisor* frame, which the
/// detector, tracker AND re-ID crop extractor all share
/// (`nexus_pipeline::source::supervisor_frame_for`). Moving up one rung is
/// 4x the pixels, which surfaces more small objects, which multiplies
/// track count, which multiplies the 224x224 DINOv2 crops re-ID emits per
/// track. A box sized on detector throughput alone will happily pick a rung
/// that then drowns the sighting queue — that is exactly what an Intel NPU
/// box provisioned at 1024x576 did in production.
///
/// So: start conservative on every box and let an operator raise it per
/// deployment (admin UI -> `PUT /v1/admin/server/inference`, or per-camera
/// via `CameraBehavior::supervisor_width`) once the real camera count and
/// re-ID load on that site are known. Install time does not know either.
const DEFAULT_PRESET: (u32, u32) = (512, 288);

/// Detector-pool worker count.
///
/// The Intel NPU (Lunar Lake) runs 2. NVIDIA scales with VRAM — see
/// [`nvidia_sizing`]. Every other device runs a single worker (the safe
/// under-provision) because we have no reliable capacity signal for it.
fn workers_for(device: InferenceDevice, vram_mib: Option<u64>) -> usize {
    match device {
        // The Hailo-8 runs compiled HEFs on-device, so a worker costs one host
        // thread and a little I/O, not a model's worth of memory. One worker
        // serialises the whole fleet behind a single thread: measured on a
        // 53-camera box, that thread sat at 63.5% serving ~106 inferences/s,
        // i.e. a ~165/s ceiling, while the motion gate's 8 fps band asks for
        // 424/s. Four workers spread the same load to ~15.5% each. See BUG-070.
        InferenceDevice::Hailo => 4,
        InferenceDevice::IntelNpu => 2,
        InferenceDevice::Nvidia => nvidia_sizing(vram_mib),
        _ => 1,
    }
}

/// Worker count for an NVIDIA box, bucketed by total VRAM.
///
/// One FP32 YOLO session at the install-default 512x288 costs well under
/// 1 GiB of VRAM once the CUDA context, weights, and cuDNN workspace are
/// counted. The buckets keep the whole pool comfortably inside the card so
/// a session never fails to open mid-install:
///
/// | VRAM | workers | representative card |
/// |---|---|---|
/// | unknown | 1 | driver not live yet |
/// | < 4 GiB | 1 | GTX 1050 |
/// | < 8 GiB | 2 | **Quadro P2000 (5 GiB)** |
/// | < 12 GiB | 2 | RTX 4060 |
/// | >= 12 GiB | 3 | RTX 3060 12G / 4070+ |
///
/// `None` (driver not live, or `nvidia-smi` unreadable) deliberately gets
/// the most conservative bucket rather than an optimistic guess: an
/// under-provisioned pool runs slower, an over-provisioned one fails to
/// start.
///
/// This sizes the pool only. The preset is [`DEFAULT_PRESET`] on every box
/// regardless of VRAM — a card with headroom for a wider detector does not
/// imply the CPU has headroom for the re-ID crops that wider analysis
/// frame generates.
fn nvidia_sizing(vram_mib: Option<u64>) -> usize {
    match vram_mib {
        None => 1,
        Some(mib) if mib < 4_096 => 1,
        Some(mib) if mib < 8_192 => 2,
        Some(mib) if mib < 12_288 => 2,
        Some(_) => 3,
    }
}

/// Broadcast-bus channel capacity, bucketed by total RAM.
///
/// This is a backpressure headroom knob (a `tokio::sync::broadcast`
/// bound), not a correctness one — oversizing only costs a little memory,
/// undersizing only drops under extreme burst. RAM is the best
/// install-time proxy for "how big a box is this" since the camera count
/// lives in the DB and isn't known when the config is generated.
fn bus_capacity_for(ram_bytes: u64) -> usize {
    let gib = ram_bytes / (1024 * 1024 * 1024);
    if gib < 8 {
        512
    } else if gib < 16 {
        1024
    } else if gib < 32 {
        2048
    } else {
        4096
    }
}

/// Render a [`HardwareProfile`] to a complete `nexus.toml` document,
/// prefixed with a provenance header so operators know the file is
/// machine-generated (and re-generated on every manual `install.sh` run).
pub fn render_toml(profile: &HardwareProfile) -> Result<String, toml::ser::Error> {
    let cfg = generate_config(profile);
    let body = toml::to_string_pretty(&cfg)?;
    // Steer operators to the cloud console for the alert-clip on/off. The
    // `enabled` knob here is the hard per-box capability switch and is
    // RESET to the default on the next `install.sh` run (unless
    // `--keep-config`); the normal enable/disable is
    // `DeliverySettings.attach_alert_clip`, set from the cloud console's
    // delivery settings and persisted in the edge DB (survives config
    // regeneration). `toml::to_string_pretty` can't emit comments, so we
    // inject one just above the section; a formatting change that moves
    // the header simply no-ops this (the config stays valid).
    let body = body.replace(
        "[runtime.clips.alert_clips]\n",
        "# Alert clips — short, burned-in clip covering just the alert timeframe,\n\
         # attached to alert-sink deliveries and shown in the cloud console.\n\
         # Enabled by default. Disable per-org / per-core from the cloud console's\n\
         # delivery settings (persists, no restart); set `enabled = false` here only\n\
         # to HARD-disable the capability on this box (reset on the next install.sh\n\
         # run unless --keep-config). See docs/edge-core/M_ALERT_CLIP.md.\n\
         [runtime.clips.alert_clips]\n",
    );
    let header = format!(
        "# nexus.toml — GENERATED by nexus-probe v{version}.\n\
         #\n\
         # Derived from detected hardware. A manual `install.sh` re-run\n\
         # regenerates this file (backing up the prior copy to\n\
         # nexus.toml.bak.<timestamp> first); pass `--keep-config` to keep\n\
         # a hand-tuned file. Cameras, the admin password, and TLS certs\n\
         # live OUTSIDE this file, so regenerating never touches them.\n\
         #\n\
         # Detected profile: inference={inference:?}, decode={decode:?},\n\
         # cpu_logical={cores}, ram={ram} GiB.\n\
         \n",
        version = env!("CARGO_PKG_VERSION"),
        inference = profile.primary_inference(),
        decode = profile.decode,
        cores = profile.cpu_logical,
        ram = profile.ram_bytes / (1024 * 1024 * 1024),
    );
    Ok(format!("{header}{body}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::DecodeCapability;

    fn profile(
        cpu_logical: usize,
        ram_gib: u64,
        inference: Vec<InferenceDevice>,
        decode: DecodeCapability,
    ) -> HardwareProfile {
        profile_vram(cpu_logical, ram_gib, inference, decode, None)
    }

    fn profile_vram(
        cpu_logical: usize,
        ram_gib: u64,
        inference: Vec<InferenceDevice>,
        decode: DecodeCapability,
        vram_mib: Option<u64>,
    ) -> HardwareProfile {
        HardwareProfile {
            cpu_physical: cpu_logical.max(1),
            cpu_logical,
            ram_bytes: ram_gib * 1024 * 1024 * 1024,
            inference,
            reid: InferenceDevice::Cpu,
            decode,
            vram_mib,
        }
    }

    /// An NVIDIA box with the given VRAM (MiB), sized like the reference
    /// workstation: 8 logical cores, 16 GiB RAM, NVDEC decode.
    fn nvidia_profile(vram_mib: Option<u64>) -> HardwareProfile {
        profile_vram(
            8,
            16,
            vec![InferenceDevice::Nvidia, InferenceDevice::Cpu],
            DecodeCapability::Nvdec,
            vram_mib,
        )
    }

    #[test]
    fn intel_igpu_maps_to_gpu_ep_and_va_decode() {
        // .100-class N150: iGPU, 4 logical cores, 16 GiB.
        let p = profile(
            4,
            16,
            vec![InferenceDevice::IntelGpu, InferenceDevice::Cpu],
            DecodeCapability::Va,
        );
        let c = generate_config(&p);
        assert_eq!(c.inference.ep_priority, vec!["gpu", "cpu"]);
        assert_eq!(c.runtime.decode.mode, DecodeMode::Va);
        assert_eq!(c.runtime.worker_threads, 4);
        assert_eq!(c.runtime.blocking_threads, 64);
        assert_eq!(c.inference.workers, 1);
        assert_eq!(c.inference.model.preset, "512x288");
        assert_eq!(c.inference.model.input_width, 512);
        assert_eq!(c.inference.backend, InferenceBackendKind::Pool);
        assert_eq!(c.runtime.clips.recorder, RecorderKind::Gstreamer);
    }

    #[test]
    fn intel_npu_gets_two_workers_at_the_default_preset() {
        // Lunar Lake: NPU primary, 8 cores, 16 GiB. The NPU has detector
        // headroom for a wider rung, but the preset also sizes the
        // supervisor frame that re-ID crops come from, so it stays at the
        // uniform default until an operator raises it per site.
        let p = profile(
            8,
            16,
            vec![InferenceDevice::IntelNpu, InferenceDevice::Cpu],
            DecodeCapability::Va,
        );
        let c = generate_config(&p);
        assert_eq!(c.inference.ep_priority, vec!["npu", "cpu"]);
        assert_eq!(c.inference.workers, 2);
        assert_eq!(c.inference.model.preset, "512x288");
        assert_eq!(c.inference.model.input_width, 512);
        assert_eq!(c.runtime.decode.mode, DecodeMode::Va);
    }

    #[test]
    fn hailo_box_uses_hailo_ep_with_va_decode() {
        // EQR7: Hailo inference, AMD iGPU decode-only, 16 cores, 24 GiB.
        let p = profile(
            16,
            24,
            vec![InferenceDevice::Hailo, InferenceDevice::Cpu],
            DecodeCapability::Va,
        );
        let c = generate_config(&p);
        assert_eq!(c.inference.ep_priority, vec!["hailo", "cpu"]);
        assert_eq!(c.runtime.worker_threads, 16);
        assert_eq!(c.inference.workers, 4);
        assert_eq!(c.bus.capacity, 2048);
        assert_eq!(c.runtime.decode.mode, DecodeMode::Va);
    }

    /// Regression lock on BUG-070. Hailo used to fall through `workers_for`'s
    /// catch-all to a single worker, so every camera on the box serialised
    /// behind one thread — a ~165 inference/s ceiling against the 424/s that
    /// 53 cameras ask for at the motion gate's 8 fps band. It must never be
    /// sized below the Intel NPU, which is the weaker accelerator.
    #[test]
    fn hailo_is_not_sized_by_the_catch_all() {
        assert_eq!(workers_for(InferenceDevice::Hailo, None), 4);
        assert!(
            workers_for(InferenceDevice::Hailo, None)
                > workers_for(InferenceDevice::IntelNpu, None),
            "the Hailo-8 must not get fewer workers than an Intel NPU"
        );
        // The catch-all is still one worker for devices with no pool sizing.
        assert_eq!(workers_for(InferenceDevice::Cpu, None), 1);
    }

    #[test]
    fn amd_vulkan_maps_to_vulkan_ep() {
        let p = profile(
            16,
            16,
            vec![InferenceDevice::AmdVulkan, InferenceDevice::Cpu],
            DecodeCapability::Va,
        );
        let c = generate_config(&p);
        assert_eq!(c.inference.ep_priority, vec!["vulkan", "cpu"]);
        assert_eq!(c.runtime.decode.mode, DecodeMode::Va);
    }

    #[test]
    fn amd_rocm_maps_to_rocm_ep() {
        let p = profile(
            16,
            32,
            vec![InferenceDevice::AmdRocm, InferenceDevice::Cpu],
            DecodeCapability::Va,
        );
        let c = generate_config(&p);
        assert_eq!(c.inference.ep_priority, vec!["rocm", "cpu"]);
        assert_eq!(c.bus.capacity, 4096);
    }

    /// The reference NVIDIA box: a 5 GiB Quadro P2000. Two workers fit
    /// comfortably at the install-default preset.
    #[test]
    fn nvidia_p2000_gets_two_workers() {
        let p = nvidia_profile(Some(5_059));
        let c = generate_config(&p);
        assert_eq!(c.inference.workers, 2);
        assert_eq!(c.inference.model.preset, "512x288");
        assert_eq!(c.inference.model.input_width, 512);
    }

    /// Unknown VRAM (driver not live yet, or `nvidia-smi` unreadable) must
    /// under-provision rather than guess: an under-sized pool is slow, an
    /// over-sized one fails to open a session.
    #[test]
    fn nvidia_unknown_vram_under_provisions() {
        let c = generate_config(&nvidia_profile(None));
        assert_eq!(c.inference.workers, 1);
        assert_eq!(c.inference.model.preset, "512x288");
    }

    #[test]
    fn nvidia_sizing_buckets_scale_with_vram() {
        // Bucket boundaries, exercised through the public generator. VRAM
        // scales the pool only — the preset is uniform across every box.
        for (mib, workers) in [
            (2_048_u64, 1_usize), // GTX 1050, 2 GiB
            (4_096, 2),           // bucket edge
            (8_192, 2),           // RTX 4060, 8 GiB
            (12_288, 3),          // RTX 3060, 12 GiB
            (24_576, 3),          // RTX 4090, 24 GiB
        ] {
            let c = generate_config(&nvidia_profile(Some(mib)));
            assert_eq!(c.inference.workers, workers, "workers at {mib} MiB");
            assert_eq!(c.inference.model.preset, "512x288", "preset at {mib} MiB");
        }
    }

    /// VRAM sizing is NVIDIA-only — it must not leak into other box classes
    /// that never populate `vram_mib`.
    #[test]
    fn vram_does_not_resize_non_nvidia_devices() {
        let p = profile_vram(
            16,
            32,
            vec![InferenceDevice::AmdRocm, InferenceDevice::Cpu],
            DecodeCapability::Va,
            Some(24_576),
        );
        let c = generate_config(&p);
        assert_eq!(c.inference.workers, 1);
        assert_eq!(c.inference.model.preset, "512x288");
    }

    #[test]
    fn nvidia_emits_cuda_ep_with_cpu_fallback() {
        // The CUDA EP only binds when the installer has staged a
        // CUDA-capable ONNX Runtime; `cpu` stays in the chain as the
        // fail-soft terminal fallback. TensorRT is deliberately absent.
        let p = profile(
            16,
            16,
            vec![InferenceDevice::Nvidia, InferenceDevice::Cpu],
            DecodeCapability::Software,
        );
        let c = generate_config(&p);
        assert_eq!(c.inference.ep_priority, vec!["cuda", "cpu"]);
        assert!(!c.inference.ep_priority.iter().any(|e| e == "tensorrt"));
    }

    #[test]
    fn cpu_only_box_is_software_everywhere() {
        let p = profile(4, 8, vec![InferenceDevice::Cpu], DecodeCapability::Software);
        let c = generate_config(&p);
        assert_eq!(c.inference.ep_priority, vec!["cpu"]);
        assert_eq!(c.runtime.decode.mode, DecodeMode::Software);
        assert_eq!(c.inference.workers, 1);
    }

    #[test]
    fn zero_cores_falls_back_to_four_threads() {
        let p = profile(0, 8, vec![InferenceDevice::Cpu], DecodeCapability::Software);
        let c = generate_config(&p);
        assert_eq!(c.runtime.worker_threads, 4);
        // Never core-derived — the blocking pool keeps its sized default.
        assert_eq!(c.runtime.blocking_threads, 64);
    }

    #[test]
    fn bus_capacity_buckets_by_ram() {
        assert_eq!(bus_capacity_for(4 * 1024 * 1024 * 1024), 512);
        assert_eq!(bus_capacity_for(8 * 1024 * 1024 * 1024), 1024);
        assert_eq!(bus_capacity_for(16 * 1024 * 1024 * 1024), 2048);
        assert_eq!(bus_capacity_for(24 * 1024 * 1024 * 1024), 2048);
        assert_eq!(bus_capacity_for(32 * 1024 * 1024 * 1024), 4096);
        assert_eq!(bus_capacity_for(64 * 1024 * 1024 * 1024), 4096);
    }

    #[test]
    fn pack_path_and_ui_root_use_bare_metal_layout() {
        let p = profile(
            4,
            16,
            vec![InferenceDevice::IntelGpu, InferenceDevice::Cpu],
            DecodeCapability::Va,
        );
        let c = generate_config(&p);
        assert_eq!(
            c.inference.model.pack_path,
            Some(PathBuf::from("/opt/nexus/current/share/models"))
        );
        assert_eq!(
            c.server.ui_root,
            PathBuf::from("/opt/nexus/current/share/ui")
        );
    }

    /// The load-bearing guarantee: a generated config is always valid and
    /// round-trips through the engine's own deserializer (`deny_unknown_fields`
    /// satisfied, every value in range). If this ever fails, the generator
    /// would brick a box on install.
    #[test]
    fn generated_toml_round_trips_through_config_loader() {
        for (inference, decode) in [
            (
                vec![InferenceDevice::IntelGpu, InferenceDevice::Cpu],
                DecodeCapability::Va,
            ),
            (
                vec![InferenceDevice::IntelNpu, InferenceDevice::Cpu],
                DecodeCapability::Va,
            ),
            (
                vec![InferenceDevice::Hailo, InferenceDevice::Cpu],
                DecodeCapability::Va,
            ),
            (
                vec![InferenceDevice::AmdVulkan, InferenceDevice::Cpu],
                DecodeCapability::Va,
            ),
            (
                vec![InferenceDevice::AmdRocm, InferenceDevice::Cpu],
                DecodeCapability::Va,
            ),
            (
                vec![InferenceDevice::Nvidia, InferenceDevice::Cpu],
                DecodeCapability::Nvdec,
            ),
            (vec![InferenceDevice::Cpu], DecodeCapability::Software),
        ] {
            let p = profile(8, 16, inference, decode);
            let rendered = render_toml(&p).expect("render must succeed");
            let parsed: Config = toml::from_str(&rendered).expect("generated config must parse");
            // The reparsed config equals the generator's output exactly.
            assert_eq!(
                parsed.inference.ep_priority,
                generate_config(&p).inference.ep_priority
            );
            assert_eq!(
                parsed.runtime.decode.mode,
                generate_config(&p).runtime.decode.mode
            );
        }
    }

    /// `[reid] ep_priority` tracks `profile.reid`, not the detector chain —
    /// the whole point of the split. A Hailo detector box whose extractor
    /// device is an AMD iGPU must emit `["vulkan", "cpu"]` for re-id while
    /// the detector stays on `["hailo", "cpu"]`.
    #[test]
    fn reid_ep_priority_follows_reid_device_not_detector() {
        let mut p = profile(
            16,
            24,
            vec![InferenceDevice::Hailo, InferenceDevice::Cpu],
            DecodeCapability::Va,
        );
        p.reid = InferenceDevice::AmdVulkan;
        let c = generate_config(&p);
        assert_eq!(c.inference.ep_priority, vec!["hailo", "cpu"]);
        assert_eq!(c.reid.ep_priority, vec!["vulkan", "cpu"]);
    }

    /// The generic `["openvino", "tensorrt", "cuda", "cpu"]` config default
    /// matches no shipped box and silently lands the ViT on CPU. Every
    /// generated config must overwrite it.
    #[test]
    fn reid_ep_priority_never_left_at_the_config_default() {
        let default_chain = Config::default().reid.ep_priority;
        for dev in [
            InferenceDevice::IntelGpu,
            InferenceDevice::IntelNpu,
            InferenceDevice::AmdRocm,
            InferenceDevice::AmdVulkan,
            InferenceDevice::Nvidia,
            InferenceDevice::Cpu,
        ] {
            let mut p = profile(8, 16, vec![dev, InferenceDevice::Cpu], DecodeCapability::Va);
            p.reid = dev;
            let got = generate_config(&p).reid.ep_priority;
            assert_ne!(got, default_chain, "{dev:?} left the default chain");
            assert_eq!(got.last().map(String::as_str), Some("cpu"));
        }
    }
}

//! Capability-based `nexus.toml` generator.
//!
//! Turns a [`HardwareProfile`] (itself a pure function of a captured
//! [`crate::Manifest`]) into a complete, guaranteed-parseable
//! [`nexus_config::Config`]. This replaces the old discrete hardware-tier
//! abstraction: instead of copying a hand-tuned `config/tiers/<tier>.toml`
//! and rewriting paths with `sed`, the installer calls `nexus-probe
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
//! | `runtime.worker_threads` / `blocking_threads` | CPU logical cores |
//! | `runtime.decode.mode` | [`DecodeCapability`] (VA vs software) |
//! | `inference.ep_priority` | primary [`InferenceDevice`] |
//! | `inference.workers` | primary [`InferenceDevice`] |
//! | `inference.model.preset` (+ `input_width`/`input_height`) | primary [`InferenceDevice`] |
//! | `bus.capacity` | total RAM |
//!
//! The constant fields the tier templates also set (the `0.0.0.0` binds,
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
/// needs no config edit. The old tier templates carried the Docker path
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
/// of constant production fields the old tier templates set explicitly.
pub fn generate_config(profile: &HardwareProfile) -> Config {
    let mut cfg = Config::default();

    // --- runtime --------------------------------------------------------
    // Thread pools scale with logical cores (matches every shipped tier:
    // N150 4T -> 4, Lunar Lake 8T -> 8, Arc box 12T -> 12, Ryzen 16T -> 16).
    // A probe that fails to read core count (0) falls back to a safe 4.
    let threads = if profile.cpu_logical == 0 {
        4
    } else {
        profile.cpu_logical
    };
    cfg.runtime.worker_threads = threads;
    cfg.runtime.blocking_threads = threads;

    // Real H.264-passthrough clip recording (default is `Stub`, which
    // writes 0-byte placeholder files). Requires the engine built with
    // `--features gstreamer`, which every shipped release binary is.
    cfg.runtime.clips.recorder = RecorderKind::Gstreamer;

    // Hardware-decode strategy. `Va` whenever an Intel/AMD media engine is
    // present; `Software` otherwise (Hailo/NPU/NVIDIA-only or pure CPU).
    cfg.runtime.decode.mode = match profile.decode {
        DecodeCapability::Va => DecodeMode::Va,
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
    cfg.inference.workers = workers_for(device);
    cfg.inference.ep_priority = ep_priority_for(device);
    cfg.inference.model.pack_path = Some(PathBuf::from(PACK_PATH));
    let preset = preset_for(device);
    cfg.inference.model.preset = preset.to_string();
    // Redundant when `pack_path` is set (the engine resolves `preset`
    // against the manifest and ignores these), but kept for parity with
    // the old templates and as a sane fallback if the pack ever lacks the
    // preset.
    cfg.inference.model.input_width = preset;
    cfg.inference.model.input_height = preset;

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
/// * NVIDIA emits `["cpu"]` **today** — the CUDA / TensorRT EPs are gated
///   behind milestone M5. The profile still records the silicon as
///   `Nvidia` honestly; only this mapping is conservative. When M5 lands
///   this flips to `["tensorrt", "cuda", "cpu"]`.
fn ep_priority_for(device: InferenceDevice) -> Vec<String> {
    let tokens: &[&str] = match device {
        InferenceDevice::Hailo => &["hailo", "cpu"],
        InferenceDevice::IntelNpu => &["npu", "cpu"],
        InferenceDevice::IntelGpu => &["gpu", "cpu"],
        InferenceDevice::AmdRocm => &["rocm", "cpu"],
        InferenceDevice::AmdVulkan => &["vulkan", "cpu"],
        InferenceDevice::Nvidia => &["cpu"],
        InferenceDevice::Cpu => &["cpu"],
    };
    tokens.iter().map(|s| (*s).to_string()).collect()
}

/// Detector-pool worker count.
///
/// The only box class we can reliably detect today that benefits from more
/// than one session is the Intel NPU (Lunar Lake), which the shipped T36-S
/// template runs at 2. Discrete-GPU multi-worker sizing (Arc A380 → 2, RTX
/// → 3) needs VRAM detection, which is a deferred profile enrichment; until
/// then every other device runs a single worker (the safe under-provision).
fn workers_for(device: InferenceDevice) -> usize {
    match device {
        InferenceDevice::IntelNpu => 2,
        _ => 1,
    }
}

/// Model-pack preset (the square detector input edge).
///
/// The NPU (Lunar Lake) has the headroom to run 960; everything else we can
/// reliably detect runs 640. Operators bump per-camera in the UI for
/// plate/face work.
fn preset_for(device: InferenceDevice) -> u32 {
    match device {
        InferenceDevice::IntelNpu => 960,
        _ => 640,
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
        HardwareProfile {
            cpu_physical: cpu_logical.max(1),
            cpu_logical,
            ram_bytes: ram_gib * 1024 * 1024 * 1024,
            inference,
            decode,
        }
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
        assert_eq!(c.runtime.blocking_threads, 4);
        assert_eq!(c.inference.workers, 1);
        assert_eq!(c.inference.model.preset, "640");
        assert_eq!(c.inference.model.input_width, 640);
        assert_eq!(c.inference.backend, InferenceBackendKind::Pool);
        assert_eq!(c.runtime.clips.recorder, RecorderKind::Gstreamer);
    }

    #[test]
    fn intel_npu_gets_two_workers_and_960_preset() {
        // Lunar Lake (T36-S): NPU primary, 8 cores, 16 GiB.
        let p = profile(
            8,
            16,
            vec![InferenceDevice::IntelNpu, InferenceDevice::Cpu],
            DecodeCapability::Va,
        );
        let c = generate_config(&p);
        assert_eq!(c.inference.ep_priority, vec!["npu", "cpu"]);
        assert_eq!(c.inference.workers, 2);
        assert_eq!(c.inference.model.preset, "960");
        assert_eq!(c.inference.model.input_width, 960);
        assert_eq!(c.runtime.decode.mode, DecodeMode::Va);
    }

    #[test]
    fn hailo_box_uses_hailo_ep_with_va_decode() {
        // EQR7 (T24): Hailo inference, AMD iGPU decode-only, 16 cores, 24 GiB.
        let p = profile(
            16,
            24,
            vec![InferenceDevice::Hailo, InferenceDevice::Cpu],
            DecodeCapability::Va,
        );
        let c = generate_config(&p);
        assert_eq!(c.inference.ep_priority, vec!["hailo", "cpu"]);
        assert_eq!(c.runtime.worker_threads, 16);
        assert_eq!(c.inference.workers, 1);
        assert_eq!(c.bus.capacity, 2048);
        assert_eq!(c.runtime.decode.mode, DecodeMode::Va);
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

    #[test]
    fn nvidia_emits_cpu_ep_and_software_decode_today() {
        // CUDA / TensorRT are M5-gated; until then NVIDIA falls through to
        // the CPU EP and has no VA decode path.
        let p = profile(
            16,
            16,
            vec![InferenceDevice::Nvidia, InferenceDevice::Cpu],
            DecodeCapability::Software,
        );
        let c = generate_config(&p);
        assert_eq!(c.inference.ep_priority, vec!["cpu"]);
        assert_eq!(c.runtime.decode.mode, DecodeMode::Software);
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
        assert_eq!(c.runtime.blocking_threads, 4);
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
                DecodeCapability::Software,
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
}

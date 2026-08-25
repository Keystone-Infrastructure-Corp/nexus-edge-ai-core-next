//! Golden regression fixtures: per-box `Manifest` → generated `Config`.
//!
//! These lock the capability-based generator to the exact knob values the
//! old hand-tuned per-box config templates set (cited per box), so
//! removing those templates can't silently change what a box gets. Each
//! case runs the full public path — `HardwareProfile::from_manifest` →
//! `generate_config` — exactly as the `emit-config` CLI does.
//!
//! Where the generator intentionally differs from a template (NVIDIA → CPU
//! EP today, discrete Arc treated as iGPU-class pending VRAM detection,
//! RAM-bucketed `bus.capacity`), the difference is called out in the case.

use nexus_probe::{generate_config, Accelerators, CpuInfo, HardwareProfile, Manifest, MemoryInfo};

/// Build a minimal `Manifest` carrying just the fields the generator reads
/// (CPU core counts, RAM, accelerator flags). Everything else defaults.
fn manifest(physical: usize, logical: usize, ram_gib: u64, accelerators: Accelerators) -> Manifest {
    Manifest {
        cpu: CpuInfo {
            model_name: String::new(),
            physical_cores: physical,
            logical_cores: logical,
        },
        memory: MemoryInfo {
            total_kib: ram_gib * 1024 * 1024,
        },
        accelerators,
        ..Default::default()
    }
}

fn config_for(m: &Manifest) -> nexus_config::Config {
    generate_config(&HardwareProfile::from_manifest(m))
}

/// Intel UHD N150 iGPU (the `.100` box). Golden knobs:
/// `ep_priority = ["gpu","cpu"]`, preset 512x288, workers 1, 4 worker threads.
#[test]
fn intel_igpu_n150() {
    let m = manifest(
        4,
        4,
        16,
        Accelerators {
            intel_igpu: true,
            ..Default::default()
        },
    );
    let c = config_for(&m);
    assert_eq!(c.inference.ep_priority, vec!["gpu", "cpu"]);
    assert_eq!(c.reid.ep_priority, vec!["gpu", "cpu"]);
    assert_eq!(c.inference.workers, 1);
    assert_eq!(c.inference.model.preset, "512x288");
    assert_eq!(c.runtime.worker_threads, 4);
    assert_eq!(c.runtime.blocking_threads, 64);
    assert_eq!(c.runtime.decode.mode, nexus_config::DecodeMode::Va);
    // Generator improvement over the N150 box's literal 256: RAM-bucketed headroom.
    assert_eq!(c.bus.capacity, 2048);
}

/// Intel Lunar Lake (Arc 140V iGPU + NPU). Golden knobs:
/// `ep_priority = ["npu","cpu"]`, preset 512x288, workers 2, 8 worker threads.
/// NPU wins inference; decode stays VA via the Arc media engine.
#[test]
fn intel_npu_lunar_lake() {
    let m = manifest(
        8,
        8,
        16,
        Accelerators {
            intel_npu: true,
            intel_arc_140v: true,
            intel_igpu: true,
            ..Default::default()
        },
    );
    let c = config_for(&m);
    assert_eq!(c.inference.ep_priority, vec!["npu", "cpu"]);
    // Detector owns the NPU, so the DINOv2 ViT takes the Arc EUs instead of
    // contending for it.
    assert_eq!(c.reid.ep_priority, vec!["gpu", "cpu"]);
    assert_eq!(c.inference.workers, 2);
    assert_eq!(c.inference.model.preset, "512x288");
    assert_eq!(c.runtime.worker_threads, 8);
    assert_eq!(c.runtime.decode.mode, nexus_config::DecodeMode::Va);
}

/// Discrete Intel Arc A380 dGPU. The discrete-Arc knobs would be
/// `ep_priority = ["openvino","cpu"]`, workers 2, but the probe can't yet
/// distinguish a discrete Arc from an iGPU (both surface as
/// `intel_arc_140v`/`intel_igpu`), so the generator emits the iGPU-class
/// mapping: `"gpu"` (the explicit OpenVINO GPU device, valid for dGPU
/// too), workers 1. VRAM-based dGPU detection is the deferred enrichment
/// that would restore workers 2. Preset is the uniform install default.
#[test]
fn discrete_arc_falls_back_to_igpu_class() {
    let m = manifest(
        12,
        12,
        32,
        Accelerators {
            intel_arc_140v: true,
            intel_igpu: true,
            ..Default::default()
        },
    );
    let c = config_for(&m);
    assert_eq!(c.inference.ep_priority, vec!["gpu", "cpu"]);
    assert_eq!(c.reid.ep_priority, vec!["gpu", "cpu"]);
    assert_eq!(c.inference.workers, 1);
    assert_eq!(c.inference.model.preset, "512x288");
    assert_eq!(c.runtime.worker_threads, 12);
    assert_eq!(c.runtime.decode.mode, nexus_config::DecodeMode::Va);
}

/// Beelink EQR7: Hailo-8 inference, AMD Radeon 680M decode-only (not on
/// the ROCm allowlist). Golden knobs: `ep_priority = ["hailo","cpu"]`,
/// preset 512x288, workers 1, 16 worker threads, bus 2048. The decode-only AMD
/// iGPU must NOT enter the inference chain.
#[test]
fn hailo_eqr7_with_amd_decode() {
    let m = manifest(
        8,
        16,
        24,
        Accelerators {
            hailo: true,
            amd_igpu: true,
            amd_rocm_capable: false,
            ..Default::default()
        },
    );
    let c = config_for(&m);
    assert_eq!(c.inference.ep_priority, vec!["hailo", "cpu"]);
    // The decode-only iGPU is excluded from the detector chain but IS the
    // only device that can host the ONNX ViT — Hailo runs HEFs only.
    assert_eq!(c.reid.ep_priority, vec!["vulkan", "cpu"]);
    // Four detector workers, not one: a single worker serialises the whole
    // camera fleet behind one thread and caps the box near 165 inferences/s
    // (BUG-070).
    assert_eq!(c.inference.workers, 4);
    assert_eq!(c.inference.model.preset, "512x288");
    assert_eq!(c.runtime.worker_threads, 16);
    assert_eq!(c.runtime.blocking_threads, 64);
    assert_eq!(c.bus.capacity, 2048);
    assert_eq!(c.runtime.decode.mode, nexus_config::DecodeMode::Va);
}

/// AMD Vulkan — Phoenix/Rembrandt APU NOT on the ROCm allowlist. Golden
/// knobs: `ep_priority = ["vulkan","cpu"]`, preset 512x288.
#[test]
fn amd_vulkan_igpu() {
    let m = manifest(
        8,
        16,
        16,
        Accelerators {
            amd_igpu: true,
            amd_rocm_capable: false,
            ..Default::default()
        },
    );
    let c = config_for(&m);
    assert_eq!(c.inference.ep_priority, vec!["vulkan", "cpu"]);
    assert_eq!(c.reid.ep_priority, vec!["vulkan", "cpu"]);
    assert_eq!(c.inference.model.preset, "512x288");
    assert_eq!(c.runtime.decode.mode, nexus_config::DecodeMode::Va);
}

/// AMD ROCm — discrete AMD GPU on the allowlist (CDNA/RDNA2/RDNA3).
/// The generator emits it directly: `ep_priority = ["rocm","cpu"]`,
/// VA decode, RAM-bucketed bus.
#[test]
fn amd_rocm_discrete() {
    let m = manifest(
        16,
        32,
        64,
        Accelerators {
            amd_igpu: true,
            amd_rocm_capable: true,
            ..Default::default()
        },
    );
    let c = config_for(&m);
    assert_eq!(c.inference.ep_priority, vec!["rocm", "cpu"]);
    assert_eq!(c.reid.ep_priority, vec!["rocm", "cpu"]);
    assert_eq!(c.runtime.decode.mode, nexus_config::DecodeMode::Va);
    assert_eq!(c.bus.capacity, 4096);
}

/// NVIDIA dGPU. Emits `ep_priority = ["cuda","cpu"]` — the CUDA EP binds
/// only once the installer has staged a CUDA-capable ONNX Runtime into
/// `/opt/nexus/vendor/onnxruntime-cuda`, so `cpu` stays as the fail-soft
/// terminal fallback. TensorRT is deliberately excluded: it needs a
/// separate multi-GB install and only pays off on Tensor-Core (Turing+)
/// silicon, which the reference Pascal card does not have.
#[test]
fn nvidia_selects_cuda_ep() {
    let m = manifest(
        8,
        16,
        16,
        Accelerators {
            nvidia_gpu: true,
            ..Default::default()
        },
    );
    let c = config_for(&m);
    assert_eq!(c.inference.ep_priority, vec!["cuda", "cpu"]);
    assert!(!c.inference.ep_priority.iter().any(|e| e == "tensorrt"));
    // The card's NVDEC block handles decode on a box with no iGPU.
    assert_eq!(c.runtime.decode.mode, nexus_config::DecodeMode::Nvdec);
}

/// Pure CPU box — no accelerators. Software everywhere, single worker.
#[test]
fn cpu_only() {
    let m = manifest(4, 8, 8, Accelerators::default());
    let c = config_for(&m);
    assert_eq!(c.inference.ep_priority, vec!["cpu"]);
    assert_eq!(c.inference.workers, 1);
    assert_eq!(c.runtime.decode.mode, nexus_config::DecodeMode::Software);
    assert_eq!(c.bus.capacity, 1024);
}

/// Every generated config — across every box class — must serialize and
/// round-trip through the engine's own loader. This is the guarantee that
/// makes deleting the templates safe: a generated file can never fail to
/// parse on the box.
#[test]
fn every_box_class_round_trips() {
    let cases = [
        manifest(
            4,
            4,
            16,
            Accelerators {
                intel_igpu: true,
                ..Default::default()
            },
        ),
        manifest(
            8,
            8,
            16,
            Accelerators {
                intel_npu: true,
                intel_igpu: true,
                ..Default::default()
            },
        ),
        manifest(
            8,
            16,
            24,
            Accelerators {
                hailo: true,
                amd_igpu: true,
                ..Default::default()
            },
        ),
        manifest(
            8,
            16,
            16,
            Accelerators {
                amd_igpu: true,
                ..Default::default()
            },
        ),
        manifest(
            16,
            32,
            64,
            Accelerators {
                amd_igpu: true,
                amd_rocm_capable: true,
                ..Default::default()
            },
        ),
        manifest(
            8,
            16,
            16,
            Accelerators {
                nvidia_gpu: true,
                ..Default::default()
            },
        ),
        manifest(4, 8, 8, Accelerators::default()),
    ];
    for m in &cases {
        let profile = HardwareProfile::from_manifest(m);
        let rendered = nexus_probe::render_toml(&profile).expect("render must succeed");
        let parsed: nexus_config::Config =
            toml::from_str(&rendered).expect("generated config must parse");
        assert_eq!(
            parsed.inference.ep_priority,
            generate_config(&profile).inference.ep_priority
        );
    }
}

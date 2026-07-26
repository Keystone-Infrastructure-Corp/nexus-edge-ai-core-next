//! Typed hardware-capability model derived purely from a [`Manifest`].
//!
//! [`HardwareProfile`] is the bridge between raw host enumeration
//! ([`crate::build_manifest`]) and the forthcoming capability-based config
//! generator: it collapses the loose boolean accelerator flags into a
//! single ranked inference chain plus a decode-capability verdict that the
//! generator maps to `[inference] ep_priority` and `[runtime.decode] mode`.
//!
//! Everything here is a pure function of an already-captured [`Manifest`]
//! (no I/O, no syscalls), so it is fully unit-testable on any host —
//! including macOS CI where no edge hardware exists.
//!
//! Design notes worth keeping straight:
//!
//! * **Inference and decode are orthogonal.** The EQR7 box runs inference
//!   on a Hailo-8 while decoding on its AMD Radeon iGPU; an Intel Lunar
//!   Lake box runs inference on the NPU while decoding on the Arc media
//!   engine. So [`HardwareProfile::decode`] is computed from GPU presence
//!   independently of [`HardwareProfile::inference`].
//! * **The inference chain is `[best, Cpu]`** (or just `[Cpu]`). It is a
//!   fallback chain, not a list of every backend present — a decode-only
//!   AMD iGPU on a Hailo box is intentionally absent from the chain.

use serde::{Deserialize, Serialize};

use crate::{Accelerators, Manifest};

/// A single inference backend the host can drive. Ordering of the variants
/// reflects the global preference used to pick the primary backend:
/// `Hailo` > `IntelNpu` > `IntelGpu` > `AmdRocm` > `AmdVulkan` > `Nvidia`
/// > `Cpu`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceDevice {
    /// Hailo-8 / Hailo-8L M.2 accelerator (inference only — it has no
    /// media block, so it never contributes to [`DecodeCapability`]).
    Hailo,
    /// Intel NPU (Lunar Lake NPU 4 and later). Inference only.
    IntelNpu,
    /// Intel iGPU / discrete Arc, driven via the OpenVINO GPU EP.
    IntelGpu,
    /// AMD GPU on the ROCm allowlist (discrete CDNA / RDNA2 / RDNA3).
    AmdRocm,
    /// AMD GPU NOT on the ROCm allowlist (Phoenix / Rembrandt iGPUs and
    /// unvetted parts) — driven via the Vulkan/WebGPU EP.
    AmdVulkan,
    /// NVIDIA GPU. The generator maps this to a CPU EP today (CUDA /
    /// TensorRT are gated behind milestone M5); the profile still records
    /// the silicon honestly.
    Nvidia,
    /// CPU EP — the universal fallback, always last in the chain.
    Cpu,
}

/// Hardware-accelerated video-decode availability for this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeCapability {
    /// VA-API decode is available (an Intel or AMD GPU with a media engine
    /// is present). Maps to `[runtime.decode] mode = "va"`.
    Va,
    /// NVDEC decode is available (an NVIDIA GPU is present). Maps to
    /// `[runtime.decode] mode = "nvdec"`.
    Nvdec,
    /// No hardware decoder detected — decode stays in software. Maps to
    /// `[runtime.decode] mode = "software"`.
    Software,
}

/// Operator override for the inference backend, supplied via the installer
/// (`--force-profile <name>`). It pins the primary inference device while
/// leaving CPU/RAM-derived knobs — and the physically-determined
/// [`DecodeCapability`] — scaled to the actual box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedProfile {
    /// Intel iGPU / Arc via the OpenVINO GPU EP.
    IntelIgpu,
    /// Intel NPU EP.
    IntelNpu,
    /// AMD Vulkan/WebGPU EP.
    AmdVulkan,
    /// AMD ROCm EP.
    AmdRocm,
    /// Hailo EP.
    Hailo,
    /// NVIDIA (CPU EP today; CUDA/TensorRT at M5).
    Nvidia,
    /// Force the CPU EP regardless of detected accelerators.
    Cpu,
}

impl ForcedProfile {
    /// The single inference device this forced profile pins. `Cpu` returns
    /// `None` because the CPU EP is not an accelerator entry in the chain —
    /// it is the universal fallback the chain always ends with.
    fn inference_device(self) -> Option<InferenceDevice> {
        match self {
            ForcedProfile::IntelIgpu => Some(InferenceDevice::IntelGpu),
            ForcedProfile::IntelNpu => Some(InferenceDevice::IntelNpu),
            ForcedProfile::AmdVulkan => Some(InferenceDevice::AmdVulkan),
            ForcedProfile::AmdRocm => Some(InferenceDevice::AmdRocm),
            ForcedProfile::Hailo => Some(InferenceDevice::Hailo),
            ForcedProfile::Nvidia => Some(InferenceDevice::Nvidia),
            ForcedProfile::Cpu => None,
        }
    }
}

impl std::str::FromStr for ForcedProfile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "intel-igpu" | "intel_igpu" | "igpu" => Ok(ForcedProfile::IntelIgpu),
            "intel-npu" | "intel_npu" | "npu" => Ok(ForcedProfile::IntelNpu),
            "amd-vulkan" | "amd_vulkan" | "vulkan" => Ok(ForcedProfile::AmdVulkan),
            "amd-rocm" | "amd_rocm" | "rocm" => Ok(ForcedProfile::AmdRocm),
            "hailo" => Ok(ForcedProfile::Hailo),
            "nvidia" | "cuda" => Ok(ForcedProfile::Nvidia),
            "cpu" => Ok(ForcedProfile::Cpu),
            other => Err(format!("unknown forced profile: {other}")),
        }
    }
}

/// A typed hardware-capability profile derived purely from a [`Manifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareProfile {
    /// Physical CPU core count (for thread sizing).
    pub cpu_physical: usize,
    /// Logical CPU core count (for thread sizing).
    pub cpu_logical: usize,
    /// Total system RAM in bytes (for bus-capacity / worker sizing).
    pub ram_bytes: u64,
    /// Ranked inference fallback chain, best first, always ending in
    /// [`InferenceDevice::Cpu`].
    pub inference: Vec<InferenceDevice>,
    /// Whether hardware-accelerated decode is available.
    pub decode: DecodeCapability,
}

impl HardwareProfile {
    /// Derive the profile straight from a captured [`Manifest`].
    pub fn from_manifest(m: &Manifest) -> Self {
        Self::from_manifest_forced(m, None)
    }

    /// Derive the profile, optionally overriding the inference backend with
    /// an operator-supplied [`ForcedProfile`]. The override pins only the
    /// inference chain; `cpu_*`, `ram_bytes`, and [`DecodeCapability`] are
    /// still read from the box, because decode capability is a physical
    /// fact of the hardware (you cannot force a media engine into existence
    /// by choosing an inference EP).
    pub fn from_manifest_forced(m: &Manifest, forced: Option<ForcedProfile>) -> Self {
        let inference = match forced {
            Some(f) => match f.inference_device() {
                Some(dev) => vec![dev, InferenceDevice::Cpu],
                None => vec![InferenceDevice::Cpu],
            },
            None => rank_inference(&m.accelerators),
        };
        HardwareProfile {
            cpu_physical: m.cpu.physical_cores,
            cpu_logical: m.cpu.logical_cores,
            ram_bytes: m.memory.total_kib.saturating_mul(1024),
            inference,
            decode: decode_capability(&m.accelerators),
        }
    }

    /// The preferred inference backend — the first entry in the ranked
    /// chain. Always defined (the chain is never empty).
    pub fn primary_inference(&self) -> InferenceDevice {
        self.inference
            .first()
            .copied()
            .unwrap_or(InferenceDevice::Cpu)
    }
}

/// Pick the single best inference backend present and return the fallback
/// chain `[best, Cpu]` (or `[Cpu]` when nothing accelerates).
///
/// The priority order is fixed: Hailo > IntelNpu > IntelGpu > AMD (ROCm if
/// allowlisted, else Vulkan) > NVIDIA > CPU. A decode-only AMD iGPU on a
/// Hailo or NVIDIA box is deliberately excluded — it is not an inference
/// fallback, so the chain there is `[Hailo, Cpu]` / `[Nvidia, Cpu]`.
fn rank_inference(acc: &Accelerators) -> Vec<InferenceDevice> {
    let best = if acc.hailo {
        Some(InferenceDevice::Hailo)
    } else if acc.intel_npu {
        Some(InferenceDevice::IntelNpu)
    } else if acc.intel_igpu || acc.intel_arc_140v {
        Some(InferenceDevice::IntelGpu)
    } else if acc.amd_igpu {
        Some(if acc.amd_rocm_capable {
            InferenceDevice::AmdRocm
        } else {
            InferenceDevice::AmdVulkan
        })
    } else if acc.nvidia_gpu {
        Some(InferenceDevice::Nvidia)
    } else {
        None
    };
    match best {
        Some(dev) => vec![dev, InferenceDevice::Cpu],
        None => vec![InferenceDevice::Cpu],
    }
}

/// Pick the hardware decode path this host can drive.
///
/// VA-API whenever an Intel or AMD GPU with a media engine is present;
/// NVDEC on an NVIDIA GPU (every card the engine targets carries an
/// NVDEC block — 4th-gen on the reference Pascal, covering H.264 and
/// 8-bit HEVC). VA wins when both are present, because the integrated
/// media engine leaves the discrete GPU free for inference.
///
/// Hailo and the Intel NPU are inference-only and contribute no decode
/// path of their own.
fn decode_capability(acc: &Accelerators) -> DecodeCapability {
    if acc.intel_igpu || acc.intel_arc_140v || acc.amd_igpu {
        DecodeCapability::Va
    } else if acc.nvidia_gpu {
        DecodeCapability::Nvdec
    } else {
        DecodeCapability::Software
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CpuInfo, MemoryInfo};

    fn manifest_with(cpu: CpuInfo, memory: MemoryInfo, accelerators: Accelerators) -> Manifest {
        Manifest {
            cpu,
            memory,
            accelerators,
            ..Default::default()
        }
    }

    #[test]
    fn intel_igpu_box_uses_gpu_inference_and_va_decode() {
        // .100-class N150 UHD: iGPU only.
        let m = manifest_with(
            CpuInfo {
                model_name: "Intel(R) N150".into(),
                physical_cores: 4,
                logical_cores: 4,
            },
            MemoryInfo {
                total_kib: 8 * 1024 * 1024,
            },
            Accelerators {
                intel_igpu: true,
                ..Default::default()
            },
        );
        let p = HardwareProfile::from_manifest(&m);
        assert_eq!(
            p.inference,
            vec![InferenceDevice::IntelGpu, InferenceDevice::Cpu]
        );
        assert_eq!(p.primary_inference(), InferenceDevice::IntelGpu);
        assert_eq!(p.decode, DecodeCapability::Va);
        assert_eq!(p.cpu_logical, 4);
        assert_eq!(p.ram_bytes, 8 * 1024 * 1024 * 1024);
    }

    #[test]
    fn intel_npu_box_prefers_npu_over_igpu() {
        // Lunar Lake: NPU + Arc 140V iGPU. NPU wins for inference, decode
        // still VA via the Arc media engine.
        let m = manifest_with(
            CpuInfo {
                model_name: "Intel(R) Core(TM) Ultra 7 256V".into(),
                physical_cores: 8,
                logical_cores: 8,
            },
            MemoryInfo::default(),
            Accelerators {
                intel_npu: true,
                intel_arc_140v: true,
                intel_igpu: true,
                ..Default::default()
            },
        );
        let p = HardwareProfile::from_manifest(&m);
        assert_eq!(
            p.inference,
            vec![InferenceDevice::IntelNpu, InferenceDevice::Cpu]
        );
        assert_eq!(p.decode, DecodeCapability::Va);
    }

    #[test]
    fn eqr7_hailo_box_uses_hailo_inference_with_amd_va_decode() {
        // EQR7: Hailo-8 inference, AMD Radeon 680M iGPU decode-only.
        // The decode-only AMD iGPU must NOT appear in the inference chain.
        let m = manifest_with(
            CpuInfo {
                model_name: "AMD Ryzen 7 7735HS with Radeon Graphics".into(),
                physical_cores: 8,
                logical_cores: 16,
            },
            MemoryInfo::default(),
            Accelerators {
                hailo: true,
                amd_igpu: true,
                amd_rocm_capable: false,
                ..Default::default()
            },
        );
        let p = HardwareProfile::from_manifest(&m);
        assert_eq!(
            p.inference,
            vec![InferenceDevice::Hailo, InferenceDevice::Cpu]
        );
        assert_eq!(p.decode, DecodeCapability::Va);
    }

    #[test]
    fn amd_igpu_without_rocm_uses_vulkan() {
        // Bare AMD APU, not on the ROCm allowlist -> Vulkan EP, VA decode.
        let m = manifest_with(
            CpuInfo::default(),
            MemoryInfo::default(),
            Accelerators {
                amd_igpu: true,
                amd_rocm_capable: false,
                ..Default::default()
            },
        );
        let p = HardwareProfile::from_manifest(&m);
        assert_eq!(
            p.inference,
            vec![InferenceDevice::AmdVulkan, InferenceDevice::Cpu]
        );
        assert_eq!(p.decode, DecodeCapability::Va);
    }

    #[test]
    fn amd_rocm_allowlisted_uses_rocm() {
        // Discrete AMD on the allowlist -> ROCm EP.
        let m = manifest_with(
            CpuInfo::default(),
            MemoryInfo::default(),
            Accelerators {
                amd_igpu: true,
                amd_rocm_capable: true,
                ..Default::default()
            },
        );
        let p = HardwareProfile::from_manifest(&m);
        assert_eq!(
            p.inference,
            vec![InferenceDevice::AmdRocm, InferenceDevice::Cpu]
        );
        assert_eq!(p.decode, DecodeCapability::Va);
    }

    #[test]
    fn nvidia_takes_the_inference_slot_and_nvdec_decode() {
        // NVIDIA dGPU with no integrated media engine: Nvidia leads the
        // inference chain and the card's NVDEC block handles decode.
        let m = manifest_with(
            CpuInfo::default(),
            MemoryInfo::default(),
            Accelerators {
                nvidia_gpu: true,
                ..Default::default()
            },
        );
        let p = HardwareProfile::from_manifest(&m);
        assert_eq!(
            p.inference,
            vec![InferenceDevice::Nvidia, InferenceDevice::Cpu]
        );
        assert_eq!(p.decode, DecodeCapability::Nvdec);
    }

    #[test]
    fn nvidia_beside_an_intel_igpu_keeps_va_decode() {
        // Workstation shape (iGPU + discrete NVIDIA): inference goes to the
        // NVIDIA card while decode stays on the Intel media engine, so the
        // dGPU's whole budget is left to inference.
        let m = manifest_with(
            CpuInfo::default(),
            MemoryInfo::default(),
            Accelerators {
                nvidia_gpu: true,
                intel_igpu: true,
                ..Default::default()
            },
        );
        let p = HardwareProfile::from_manifest(&m);
        assert_eq!(p.decode, DecodeCapability::Va);
    }

    #[test]
    fn cpu_only_box_is_software_everywhere() {
        let m = manifest_with(
            CpuInfo::default(),
            MemoryInfo::default(),
            Accelerators::default(),
        );
        let p = HardwareProfile::from_manifest(&m);
        assert_eq!(p.inference, vec![InferenceDevice::Cpu]);
        assert_eq!(p.primary_inference(), InferenceDevice::Cpu);
        assert_eq!(p.decode, DecodeCapability::Software);
    }

    #[test]
    fn hailo_outranks_amd_rocm() {
        // If a box ever pairs Hailo with an allowlisted AMD dGPU, Hailo
        // still wins the inference slot; AMD remains decode-only.
        let m = manifest_with(
            CpuInfo::default(),
            MemoryInfo::default(),
            Accelerators {
                hailo: true,
                amd_igpu: true,
                amd_rocm_capable: true,
                ..Default::default()
            },
        );
        let p = HardwareProfile::from_manifest(&m);
        assert_eq!(
            p.inference,
            vec![InferenceDevice::Hailo, InferenceDevice::Cpu]
        );
    }

    #[test]
    fn forced_profile_pins_inference_but_keeps_physical_decode() {
        // Operator forces Vulkan on the EQR7 (Hailo + AMD VA) box: the
        // inference chain is pinned to AMD Vulkan, but decode stays VA
        // (a physical fact) and CPU/RAM still scale to the box.
        let m = manifest_with(
            CpuInfo {
                model_name: "AMD Ryzen 7 7735HS with Radeon Graphics".into(),
                physical_cores: 8,
                logical_cores: 16,
            },
            MemoryInfo {
                total_kib: 32 * 1024 * 1024,
            },
            Accelerators {
                hailo: true,
                amd_igpu: true,
                ..Default::default()
            },
        );
        let p = HardwareProfile::from_manifest_forced(&m, Some(ForcedProfile::AmdVulkan));
        assert_eq!(
            p.inference,
            vec![InferenceDevice::AmdVulkan, InferenceDevice::Cpu]
        );
        assert_eq!(p.decode, DecodeCapability::Va);
        assert_eq!(p.cpu_logical, 16);
        assert_eq!(p.ram_bytes, 32 * 1024 * 1024 * 1024);
    }

    #[test]
    fn forced_cpu_collapses_chain_to_cpu_only() {
        let m = manifest_with(
            CpuInfo::default(),
            MemoryInfo::default(),
            Accelerators {
                intel_igpu: true,
                ..Default::default()
            },
        );
        let p = HardwareProfile::from_manifest_forced(&m, Some(ForcedProfile::Cpu));
        assert_eq!(p.inference, vec![InferenceDevice::Cpu]);
        // Decode capability is physical, so the iGPU still yields VA.
        assert_eq!(p.decode, DecodeCapability::Va);
    }

    #[test]
    fn forced_profile_parses_aliases() {
        use std::str::FromStr;
        assert_eq!(
            ForcedProfile::from_str("igpu").unwrap(),
            ForcedProfile::IntelIgpu
        );
        assert_eq!(
            ForcedProfile::from_str("INTEL-NPU").unwrap(),
            ForcedProfile::IntelNpu
        );
        assert_eq!(
            ForcedProfile::from_str(" vulkan ").unwrap(),
            ForcedProfile::AmdVulkan
        );
        assert_eq!(
            ForcedProfile::from_str("amd_rocm").unwrap(),
            ForcedProfile::AmdRocm
        );
        assert_eq!(
            ForcedProfile::from_str("hailo").unwrap(),
            ForcedProfile::Hailo
        );
        assert_eq!(
            ForcedProfile::from_str("cuda").unwrap(),
            ForcedProfile::Nvidia
        );
        assert_eq!(ForcedProfile::from_str("cpu").unwrap(), ForcedProfile::Cpu);
        assert!(ForcedProfile::from_str("quantum").is_err());
    }
}

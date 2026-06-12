//! `nexus-probe` library — host enumeration + tier recommendation.
//!
//! The companion CLI (`crates/nexus-probe/src/main.rs`) is a thin wrapper
//! that builds a [`Manifest`] and serializes it to `device-manifest.json`.
//!
//! The same logic is exposed as a library so `nexus-engine` can call
//! [`recommend_tier_config_path`] in-process to implement
//! `--tier auto` (M-Install Checkpoint 1).
//!
//! Wire shape is stable: changing serde field names / order is a
//! breaking change for `device-manifest.json` consumers.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use chrono::Utc;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Wire types — exposed so external callers (engine + tests) can consume them.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Manifest {
    pub schema: u32,
    pub captured_at: String,
    pub host: HostInfo,
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub accelerators: Accelerators,
    pub runtimes: Runtimes,
    /// Recommended hardware tier (T10/T24/T36/T36-S/T64/dev) derived from
    /// the detected CPU + accelerators. See docs/HARDWARE_TIERS.md for the
    /// exact mapping. Advisory only — operator can override in nexus.toml.
    pub recommended_tier: String,
    /// Filename in `config/tiers/` that matches `recommended_tier`.
    pub recommended_tier_config: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub kernel: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct CpuInfo {
    pub model_name: String,
    pub physical_cores: usize,
    pub logical_cores: usize,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct MemoryInfo {
    pub total_kib: u64,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Accelerators {
    pub intel_igpu: bool,
    pub intel_arc_140v: bool,
    pub intel_npu: bool,
    pub nvidia_gpu: bool,
    pub apple_silicon: bool,
    /// AMD Radeon iGPU (Phoenix-class Ryzen 7000/8000 APUs) or any
    /// AMD dGPU. Used for hardware H.264/HEVC decode via VA-API
    /// (Mesa Gallium). Inference EP for AMD is not wired today —
    /// ROCm on Phoenix gfx1035 is an experimental spike.
    pub amd_igpu: bool,
    /// Hailo-8 / Hailo-8L M.2 accelerator (PCI vendor 0x1e60).
    /// T24 tier signal: a Hailo present recommends `t24.toml`.
    /// Until `M_HAILO_EP` lands the engine still falls back to the
    /// CPU EP at runtime — the detection is what unlocks the
    /// auto-tier mapping ahead of the EP work.
    pub hailo: bool,
    pub render_nodes: Vec<String>,
    pub accel_nodes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Runtimes {
    pub gstreamer: Option<String>,
    pub onnxruntime: Option<String>,
    pub docker: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Probe the host and assemble a fresh [`Manifest`]. This is the single
/// entry point used by both the CLI binary and the engine's
/// `--tier auto` resolver.
pub fn build_manifest() -> Manifest {
    let host = probe_host();
    let cpu = probe_cpu();
    let memory = probe_memory();
    let accelerators = probe_accelerators();
    let runtimes = probe_runtimes();
    let (tier, tier_cfg) = recommend_tier(&cpu, &accelerators);
    Manifest {
        schema: 1,
        captured_at: Utc::now().to_rfc3339(),
        host,
        cpu,
        memory,
        accelerators,
        runtimes,
        recommended_tier: tier.into(),
        recommended_tier_config: tier_cfg.into(),
    }
}

/// Resolve the path of the recommended tier config file for this host.
///
/// Returned path is relative (e.g. `config/tiers/t24.toml`). Resolution
/// is left to the caller — the engine `Config::load` accepts any path
/// the OS can `open()`. See M-Install Checkpoint 1 in
/// `docs/ROADMAP.md` for why this is intentionally minimal.
pub fn recommend_tier_config_path() -> PathBuf {
    let cpu = probe_cpu();
    let accelerators = probe_accelerators();
    let (_, tier_cfg) = recommend_tier(&cpu, &accelerators);
    PathBuf::from(tier_cfg)
}

// ---------------------------------------------------------------------------
// Probe helpers
// ---------------------------------------------------------------------------

fn probe_host() -> HostInfo {
    HostInfo {
        hostname: hostname_or("unknown"),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        kernel: shell_out("uname", &["-r"]).unwrap_or_default(),
    }
}

fn hostname_or(default: &str) -> String {
    shell_out("hostname", &[]).unwrap_or_else(|| default.into())
}

fn probe_cpu() -> CpuInfo {
    let model = read_first_field("/proc/cpuinfo", "model name").unwrap_or_default();
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    // Physical: count unique "physical id"s on Linux; on other OS just use logical.
    let physical = if cfg!(target_os = "linux") {
        count_unique_field("/proc/cpuinfo", "physical id").unwrap_or(logical)
    } else {
        logical
    };
    CpuInfo {
        model_name: model,
        physical_cores: physical,
        logical_cores: logical,
    }
}

fn probe_memory() -> MemoryInfo {
    let total = read_first_field("/proc/meminfo", "MemTotal")
        .and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|x| x.parse::<u64>().ok())
        })
        .unwrap_or(0);
    MemoryInfo { total_kib: total }
}

fn probe_accelerators() -> Accelerators {
    let lspci = shell_out("lspci", &[]).unwrap_or_default().to_lowercase();
    let render_nodes = list_dir_starts_with("/dev/dri", "renderD");
    let accel_nodes = list_dir_starts_with("/dev/accel", "");
    Accelerators {
        intel_igpu: lspci.contains("intel") && lspci.contains("graphics"),
        intel_arc_140v: lspci.contains("arc")
            || lspci.contains("battlemage")
            || lspci.contains("lunar lake"),
        intel_npu: !accel_nodes.is_empty() || lspci.contains("npu"),
        nvidia_gpu: lspci.contains("nvidia"),
        apple_silicon: cfg!(all(target_os = "macos", target_arch = "aarch64")),
        // AMD: vendor string in lspci is "Advanced Micro Devices, Inc.
        // [AMD/ATI]" — match the bracketed alias because the long form
        // also appears under non-graphics PCI devices on AMD chipsets.
        // "radeon" catches the marketing name across iGPU/dGPU SKUs.
        amd_igpu: lspci.contains("amd/ati") || lspci.contains("radeon"),
        // Hailo: only ships AI accelerators under the "Hailo
        // Technologies" PCI vendor string. Substring match is enough.
        hailo: lspci.contains("hailo"),
        render_nodes,
        accel_nodes,
    }
}

fn probe_runtimes() -> Runtimes {
    Runtimes {
        gstreamer: shell_out("gst-launch-1.0", &["--version"])
            .and_then(|s| s.lines().next().map(|l| l.to_string())),
        onnxruntime: detect_onnxruntime(),
        docker: shell_out("docker", &["--version"]),
    }
}

fn detect_onnxruntime() -> Option<String> {
    for p in [
        "/usr/local/lib/libonnxruntime.so",
        "/opt/onnxruntime/lib/libonnxruntime.so",
    ] {
        if std::path::Path::new(p).exists() {
            return Some(p.into());
        }
    }
    None
}

/// Map detected hardware to one of the documented tiers.
/// Mirrors the table in docs/HARDWARE_TIERS.md — keep them in sync.
fn recommend_tier(cpu: &CpuInfo, acc: &Accelerators) -> (&'static str, &'static str) {
    let model = cpu.model_name.to_lowercase();
    if acc.nvidia_gpu {
        return ("T64", "config/tiers/t64.toml");
    }
    // EQR7 + Hailo-8 SKU. Detected by Hailo presence alone — the box
    // is "T24" regardless of which AMD/Intel host CPU drives it.
    // Engine still falls back to CPU EP at runtime until M_HAILO_EP
    // ships; the recommendation here is what unlocks the auto-tier
    // selector ahead of EP work.
    if acc.hailo {
        return ("T24", "config/tiers/t24.toml");
    }
    // Lunar Lake = Core Ultra 7 256V (Arc 140V iGPU + NPU 4).
    if acc.intel_arc_140v
        || acc.intel_npu
        || model.contains("core ultra 7 256v")
        || model.contains("core ultra 9 288v")
        || model.contains("lunar lake")
    {
        return ("T36-S", "config/tiers/t36s.toml");
    }
    // Discrete Arc (A310/A380/A580/A750/A770) — distinct from the iGPU "Arc 140V" above.
    if model.contains("arc a3") || model.contains("arc a5") || model.contains("arc a7") {
        return ("T36", "config/tiers/t36.toml");
    }
    if model.contains("i7-12700h") || model.contains("iris xe") || model.contains("96 eu") {
        return ("T24", "config/tiers/t24.toml");
    }
    if model.contains("n100")
        || model.contains("n150")
        || model.contains("n200")
        || model.contains("n305")
    {
        return ("T10", "config/tiers/t10.toml");
    }
    if acc.apple_silicon {
        return ("dev", "config/single-camera.toml");
    }
    // Fallback: assume the smallest tier so the box doesn't oversubscribe itself.
    // AMD-only (Radeon iGPU/dGPU without Hailo) lands here intentionally — there's
    // no dedicated AMD inference tier today (ROCm spike pending). The Mesa VA-API
    // decode path still works at T10 limits.
    ("T10", "config/tiers/t10.toml")
}

// ---------------------------------------------------------------------------
// Generic helpers
// ---------------------------------------------------------------------------

fn shell_out(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn read_first_field(path: &str, key: &str) -> Option<String> {
    let txt = std::fs::read_to_string(path).ok()?;
    for line in txt.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim() == key {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn count_unique_field(path: &str, key: &str) -> Option<usize> {
    let txt = std::fs::read_to_string(path).ok()?;
    let mut seen = HashSet::new();
    for line in txt.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim() == key {
                seen.insert(v.trim().to_string());
            }
        }
    }
    Some(seen.len().max(1))
}

fn list_dir_starts_with(path: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(path) else {
        return out;
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with(prefix) {
            out.push(format!("{}/{}", path, name));
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_wins_over_everything() {
        let cpu = CpuInfo {
            model_name: "Intel(R) Core(TM) i7-12700H".into(),
            physical_cores: 6,
            logical_cores: 12,
        };
        let acc = Accelerators {
            nvidia_gpu: true,
            intel_igpu: true,
            ..Default::default()
        };
        assert_eq!(recommend_tier(&cpu, &acc), ("T64", "config/tiers/t64.toml"));
    }

    #[test]
    fn lunar_lake_via_npu_node() {
        let cpu = CpuInfo {
            model_name: "Intel(R) Core(TM) Ultra 7 256V".into(),
            ..Default::default()
        };
        let acc = Accelerators {
            intel_npu: true,
            accel_nodes: vec!["/dev/accel/accel0".into()],
            ..Default::default()
        };
        assert_eq!(
            recommend_tier(&cpu, &acc),
            ("T36-S", "config/tiers/t36s.toml")
        );
    }

    #[test]
    fn discrete_arc_is_t36() {
        // The branch matches a literal "arc a3" / "arc a5" / "arc a7"
        // substring (with the space). Trademark markers like
        // "Arc(TM) A380" disqualify the model string; this is a
        // pre-existing probe limitation worth pinning so it doesn't
        // silently regress when someone refactors `recommend_tier`.
        let cpu = CpuInfo {
            model_name: "Intel Arc A380 Graphics".into(),
            ..Default::default()
        };
        let acc = Accelerators::default();
        assert_eq!(recommend_tier(&cpu, &acc), ("T36", "config/tiers/t36.toml"));
    }

    #[test]
    fn iris_xe_is_t24() {
        let cpu = CpuInfo {
            model_name: "12th Gen Intel(R) Core(TM) i7-12700H".into(),
            ..Default::default()
        };
        let acc = Accelerators::default();
        assert_eq!(recommend_tier(&cpu, &acc), ("T24", "config/tiers/t24.toml"));
    }

    #[test]
    fn hailo_present_is_t24() {
        // EQR7 + Hailo-8 SKU: Hailo flag set on a Ryzen host. T24
        // should win regardless of the AMD iGPU also being detected.
        let cpu = CpuInfo {
            model_name: "AMD Ryzen 7 7735HS with Radeon Graphics".into(),
            ..Default::default()
        };
        let acc = Accelerators {
            hailo: true,
            amd_igpu: true,
            ..Default::default()
        };
        assert_eq!(recommend_tier(&cpu, &acc), ("T24", "config/tiers/t24.toml"));
    }

    #[test]
    fn amd_only_falls_back_to_t10() {
        // Bare AMD APU with no Hailo: no dedicated tier today, so
        // fallback is T10 (CPU-only soak target). Decode path still
        // works via Mesa VA-API; engine just won't oversubscribe.
        let cpu = CpuInfo {
            model_name: "AMD Ryzen 7 7735HS with Radeon Graphics".into(),
            ..Default::default()
        };
        let acc = Accelerators {
            amd_igpu: true,
            ..Default::default()
        };
        assert_eq!(recommend_tier(&cpu, &acc), ("T10", "config/tiers/t10.toml"));
    }

    #[test]
    fn nvidia_wins_over_hailo() {
        // Defensive: if a future SKU ever pairs an NVIDIA dGPU with
        // a Hailo M.2 (unlikely but not impossible) we want T64 to
        // win — that's where the CUDA EP actually attaches.
        let cpu = CpuInfo {
            model_name: "AMD Ryzen 9".into(),
            ..Default::default()
        };
        let acc = Accelerators {
            nvidia_gpu: true,
            hailo: true,
            ..Default::default()
        };
        assert_eq!(recommend_tier(&cpu, &acc), ("T64", "config/tiers/t64.toml"));
    }

    #[test]
    fn n_series_is_t10() {
        let cpu = CpuInfo {
            model_name: "Intel(R) N150".into(),
            ..Default::default()
        };
        let acc = Accelerators::default();
        assert_eq!(recommend_tier(&cpu, &acc), ("T10", "config/tiers/t10.toml"));
    }

    #[test]
    fn apple_silicon_fallback_is_dev() {
        let cpu = CpuInfo {
            model_name: "Apple M2 Pro".into(),
            ..Default::default()
        };
        let acc = Accelerators {
            apple_silicon: true,
            ..Default::default()
        };
        assert_eq!(
            recommend_tier(&cpu, &acc),
            ("dev", "config/single-camera.toml")
        );
    }

    #[test]
    fn unknown_falls_back_to_smallest_tier() {
        let cpu = CpuInfo {
            model_name: "Some Future Chip 9000".into(),
            ..Default::default()
        };
        let acc = Accelerators::default();
        assert_eq!(recommend_tier(&cpu, &acc), ("T10", "config/tiers/t10.toml"));
    }

    #[test]
    fn recommend_tier_config_path_returns_pathbuf() {
        // Just exercise the public entry point — exact value depends on
        // host. All we want is "doesn't panic, path is non-empty".
        let p = recommend_tier_config_path();
        assert!(!p.as_os_str().is_empty());
    }

    #[test]
    fn manifest_serializes_with_stable_field_names() {
        // Wire-compat regression: device-manifest.json consumers depend
        // on these top-level keys staying as-is.
        let m = Manifest::default();
        let v = serde_json::to_value(&m).expect("serialize");
        for key in [
            "schema",
            "captured_at",
            "host",
            "cpu",
            "memory",
            "accelerators",
            "runtimes",
            "recommended_tier",
            "recommended_tier_config",
        ] {
            assert!(v.get(key).is_some(), "missing top-level key {key}");
        }
    }
}

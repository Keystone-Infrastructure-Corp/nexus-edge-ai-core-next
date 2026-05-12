//! `nexus-probe` — one-shot host enumeration.
//!
//! Writes a JSON `device-manifest.json` describing the box. Used by deploy
//! tooling to pick the right Dockerfile target / EP feature flags.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
#[command(name = "nexus-probe", version, about = "Enumerate this host's hardware + runtimes")]
struct Cli {
    /// Where to write the manifest. `-` writes to stdout.
    #[arg(long, default_value = "data/device-manifest.json")]
    out: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Manifest {
    schema: u32,
    captured_at: String,
    host: HostInfo,
    cpu: CpuInfo,
    memory: MemoryInfo,
    accelerators: Accelerators,
    runtimes: Runtimes,
    /// Recommended hardware tier (T10/T24/T36/T36-S/T64/dev) derived from
    /// the detected CPU + accelerators. See docs/HARDWARE_TIERS.md for the
    /// exact mapping. Advisory only — operator can override in nexus.toml.
    recommended_tier: String,
    /// Filename in `config/tiers/` that matches `recommended_tier`.
    recommended_tier_config: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct HostInfo {
    hostname: String,
    os: String,
    arch: String,
    kernel: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct CpuInfo {
    model_name: String,
    physical_cores: usize,
    logical_cores: usize,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct MemoryInfo {
    total_kib: u64,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Accelerators {
    intel_igpu: bool,
    intel_arc_140v: bool,
    intel_npu: bool,
    nvidia_gpu: bool,
    apple_silicon: bool,
    render_nodes: Vec<String>,
    accel_nodes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Runtimes {
    gstreamer: Option<String>,
    onnxruntime: Option<String>,
    docker: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let host = probe_host();
    let cpu = probe_cpu();
    let memory = probe_memory();
    let accelerators = probe_accelerators();
    let runtimes = probe_runtimes();
    let (tier, tier_cfg) = recommend_tier(&cpu, &accelerators);
    let m = Manifest {
        schema: 1,
        captured_at: Utc::now().to_rfc3339(),
        host,
        cpu,
        memory,
        accelerators,
        runtimes,
        recommended_tier: tier.into(),
        recommended_tier_config: tier_cfg.into(),
    };
    let json = serde_json::to_string_pretty(&m)?;
    if cli.out == "-" {
        println!("{json}");
    } else {
        let path = PathBuf::from(&cli.out);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&path, json)?;
        eprintln!("wrote {}", path.display());
    }
    Ok(())
}

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
        .and_then(|s| s.split_whitespace().next().and_then(|x| x.parse::<u64>().ok()))
        .unwrap_or(0);
    MemoryInfo { total_kib: total }
}

fn probe_accelerators() -> Accelerators {
    let lspci = shell_out("lspci", &[]).unwrap_or_default().to_lowercase();
    let render_nodes = list_dir_starts_with("/dev/dri", "renderD");
    let accel_nodes = list_dir_starts_with("/dev/accel", "");
    Accelerators {
        intel_igpu: lspci.contains("intel") && lspci.contains("graphics"),
        intel_arc_140v: lspci.contains("arc") || lspci.contains("battlemage") || lspci.contains("lunar lake"),
        intel_npu: !accel_nodes.is_empty() || lspci.contains("npu"),
        nvidia_gpu: lspci.contains("nvidia"),
        apple_silicon: cfg!(all(target_os = "macos", target_arch = "aarch64")),
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
    for p in ["/usr/local/lib/libonnxruntime.so", "/opt/onnxruntime/lib/libonnxruntime.so"] {
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
    // Lunar Lake = Core Ultra 7 256V (Arc 140V iGPU + NPU 4).
    if acc.intel_arc_140v
        || acc.intel_npu
        || model.contains("core ultra 7 256v")
        || model.contains("core ultra 9 288v")
        || model.contains("lunar lake")
    {
        return ("T36-S", "config/tiers/t36s.toml");
    }
    // Discrete Arc (A310/A380/A580/A750/A770) — distinct from the iGPU “Arc 140V” above.
    if model.contains("arc a3") || model.contains("arc a5") || model.contains("arc a7") {
        return ("T36", "config/tiers/t36.toml");
    }
    if model.contains("i7-12700h") || model.contains("iris xe") || model.contains("96 eu") {
        return ("T24", "config/tiers/t24.toml");
    }
    if model.contains("n100") || model.contains("n150") || model.contains("n200") || model.contains("n305") {
        return ("T10", "config/tiers/t10.toml");
    }
    if acc.apple_silicon {
        return ("dev", "config/single-camera.toml");
    }
    // Fallback: assume the smallest tier so the box doesn't oversubscribe itself.
    ("T10", "config/tiers/t10.toml")
}

// ---------------------------------------------------------------------------
// helpers
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
    let mut seen = std::collections::HashSet::new();
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

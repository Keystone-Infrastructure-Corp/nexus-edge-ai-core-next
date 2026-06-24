//! `nexus-probe` CLI.
//!
//! Two jobs, both one-shot:
//!
//! * **default / no subcommand** — enumerate the host and write a JSON
//!   `device-manifest.json` describing the box (diagnostics; consumed by
//!   deploy tooling and the installer's hardware detection). Unchanged
//!   legacy behaviour, so `nexus-probe --out -` still emits the manifest.
//! * **`emit-config`** — detect the hardware, derive a [`HardwareProfile`],
//!   and write a complete, guaranteed-parseable `nexus.toml`. This replaces
//!   the old "copy a `config/tiers/<tier>.toml` template and `sed`-rewrite
//!   the paths" install step.
//! * **`accel-tags`** — print the host's accelerator tags (the same
//!   vocabulary the installer's driver selection consumes) plus the
//!   ROCm-vs-Vulkan verdict. The installer shells out to this instead of
//!   re-implementing the `lspci` scan and the ROCm device-ID allowlist in
//!   bash, so hardware detection lives in exactly one place.
//!
//! All real logic lives in [`nexus_probe`] (the library); this binary is a
//! thin wrapper.
//!
//! [`HardwareProfile`]: nexus_probe::HardwareProfile

use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nexus_probe::{ForcedProfile, HardwareProfile};

#[derive(Debug, Parser)]
#[command(
    name = "nexus-probe",
    version,
    about = "Enumerate this host's hardware + emit a capability-based nexus.toml"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Where to write the manifest in the default (no-subcommand) mode.
    /// `-` writes to stdout.
    #[arg(long, default_value = "data/device-manifest.json")]
    out: String,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a complete `nexus.toml` from the detected hardware.
    EmitConfig {
        /// Where to write the config. `-` writes to stdout (the default,
        /// so the installer can capture it). Typically
        /// `/etc/nexus/nexus.toml`.
        #[arg(long, default_value = "-")]
        out: String,

        /// Override the auto-detected inference backend. Accepts
        /// `intel-igpu`, `intel-npu`, `amd-vulkan`, `amd-rocm`, `hailo`,
        /// `nvidia`, or `cpu` (plus short aliases like `igpu` / `npu` /
        /// `vulkan` / `rocm` / `cuda`). CPU/RAM-derived knobs and the
        /// physically-determined decode capability still scale to the box.
        #[arg(long, value_name = "PROFILE")]
        force_profile: Option<String>,
    },

    /// Print the host's accelerator tags (the installer's driver-selection
    /// vocabulary): `intel-igpu`, `intel-arc-dgpu`, `intel-npu`,
    /// `nvidia-gpu`, `amd-igpu`, `hailo-m2`. The default output is one tag
    /// per line, plus `amd-rocm-capable` when a ROCm-allowlisted AMD GPU is
    /// present, so the bash installer can consume it without `jq`. `--json`
    /// emits `{"tags":[...],"amd_rocm_capable":<bool>}` for programmatic
    /// consumers.
    AccelTags {
        /// Emit JSON instead of newline-delimited tags.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => write_manifest(&cli.out),
        Some(Command::EmitConfig { out, force_profile }) => {
            emit_config(&out, force_profile.as_deref())
        }
        Some(Command::AccelTags { json }) => emit_accel_tags(json),
    }
}

/// Legacy manifest mode: enumerate the host and serialize to JSON.
fn write_manifest(out: &str) -> Result<()> {
    let m = nexus_probe::build_manifest();
    let json = serde_json::to_string_pretty(&m)?;
    write_out(out, &json)
}

/// Detect hardware, derive the profile (honouring an optional
/// `--force-profile`), and render the complete `nexus.toml`.
fn emit_config(out: &str, force_profile: Option<&str>) -> Result<()> {
    let forced = match force_profile {
        Some(s) => Some(
            ForcedProfile::from_str(s)
                .map_err(|e| anyhow::anyhow!("{e}"))
                .context("invalid --force-profile value")?,
        ),
        None => None,
    };
    let manifest = nexus_probe::build_manifest();
    let profile = HardwareProfile::from_manifest_forced(&manifest, forced);
    let toml = nexus_probe::render_toml(&profile).context("rendering nexus.toml")?;
    write_out(out, &toml)
}

/// Print the host's accelerator tags for the installer. Default output is
/// one tag per line (plus `amd-rocm-capable` when present) so bash can
/// consume it without `jq`; `--json` emits the structured form.
fn emit_accel_tags(json: bool) -> Result<()> {
    let tags = nexus_probe::accel_tags();
    if json {
        println!("{}", serde_json::to_string(&tags)?);
    } else {
        for tag in &tags.tags {
            println!("{tag}");
        }
        if tags.amd_rocm_capable {
            println!("amd-rocm-capable");
        }
    }
    Ok(())
}

/// Write `contents` to `out` (`-` = stdout), creating parent dirs as needed.
fn write_out(out: &str, contents: &str) -> Result<()> {
    if out == "-" {
        print!("{contents}");
        return Ok(());
    }
    let path = PathBuf::from(out);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir for {}", path.display()))?;
    }
    fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

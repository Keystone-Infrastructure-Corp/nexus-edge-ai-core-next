# Hardware capability matrix

The engine is **capability-based**: there is no fixed tier ladder to pick
from. At install time `nexus-probe` enumerates the host (CPU, RAM,
accelerators, render nodes, NPU/Hailo presence) and **generates** a
complete `/etc/nexus/nexus.toml` sized for whatever silicon it finds
(`nexus-probe emit-config`, run automatically by `scripts/install.sh`).
The hand-tuned `config/tiers/<tier>.toml` templates that this document
used to enumerate have been retired — the generator is now the single
source of truth for `ep_priority`, decode mode, worker/thread sizing,
model preset, and bus capacity.

This page documents the **vendor × capability** matrix the generator maps
against, plus the per-vendor caveats an operator still needs to know.

## Capability profiles

Each row is a distinct inference/decode capability the generator
recognises. The `--force-profile <name>` install flag pins inference to
one of these explicitly (it still scales the CPU/RAM-derived knobs to the
actual box); without it, `nexus-probe` auto-selects by detected hardware.

| Profile (`--force-profile`) | Representative box                                 | Inference accelerator          | EP order (`ep_priority`)     | Decode block            | Preset | Cams (1080p @ 15 fps) |
| --------------------------- | -------------------------------------------------- | ------------------------------ | ---------------------------- | ----------------------- | ------ | --------------------- |
| `intel-igpu`                | Beelink Mini S13 (N150) · GMKtec M3 (Iris Xe)      | Intel iGPU (OpenVINO)          | `gpu, cpu`                   | Intel iGPU MFX (VA)     | 640    | 1–6                   |
| `intel-igpu`                | Lenovo P3 Tiny / HP Z2 Mini + Arc A380             | Intel Arc dGPU (OpenVINO)      | `gpu, cpu`                   | Arc MFX (VA)            | 960    | 8–12                  |
| `intel-npu`                 | GMKtec K13 AI / EVO-X1 (Ultra 7 256V Lunar Lake)   | Intel NPU 4 (OpenVINO NPU)     | `npu, cpu`                   | Arc 140V Xe2 (VA)       | 640    | 6–8                   |
| `hailo`                     | Beelink EQR7 (Ryzen 7 7735HS) + Hailo-8 M.2        | Hailo-8 (26 TOPS)              | `hailo, cpu`                 | host AMD Radeon (VA)    | 640    | up to ~24             |
| `amd-vulkan`                | Beelink EQR7 (Ryzen 7 7735HS), Radeon 680M/780M    | AMD iGPU (ORT Vulkan/WebGPU)   | `vulkan, cpu`                | AMD VCN (radeonsi VA)   | 640    | 4–6                   |
| `amd-rocm`                  | discrete RDNA/CDNA GPU on the ROCm allowlist       | AMD dGPU (ORT ROCm)            | `rocm, cpu`                  | AMD VCN (radeonsi VA)   | 960    | hardware-dependent    |
| `nvidia`                    | Lenovo P3 Tower / HP Z2 G9 + RTX 4060              | NVIDIA (CUDA/TensorRT — M5)    | `cpu` **today**              | NVDEC (M5)              | 640    | CPU-only until M5     |
| `cpu`                       | any host with no usable accelerator                | CPU                            | `cpu`                        | software                | 640    | 1–2                   |

Notes on the matrix:

- **Decode is orthogonal to inference.** Every Intel/AMD GPU — including a
  decode-only iGPU sitting next to a Hailo-8 or NPU — gives hardware
  H.264/HEVC decode through the unified `va` GStreamer path
  (`vah26Xdec` + `vapostproc` over libva). Hailo-8 and the Intel NPU are
  **inference-only** and never decode; the host iGPU does. See
  [INSTALL.md](INSTALL.md) for the decode plumbing.
- **NVIDIA emits `ep_priority = ["cpu"]` today.** CUDA/TensorRT EPs and
  NVDEC land in M5; until then an NVIDIA box falls through to CPU and is
  not a meaningful production target.
- **AMD ROCm vs Vulkan** is decided from the PCI device ID against a
  default-deny allowlist (CDNA MI100/200/300, RDNA2 RX6000, RDNA3 RX7000).
  Phoenix/Rembrandt iGPUs (gfx1035/1103) are **not** on the allowlist and
  resolve to `amd-vulkan`, never ROCm. The same allowlist is enforced in
  both `nexus-probe` and the bash installer until the single-detector
  cutover unifies them.

## Camera baseline (every profile)

- 1080p H.264 (or H.265 with hardware decode) over RTSP.
- 15 fps capture, motion-gated to the detector.
- One `nexus-engine` process per host. Internal fan-out via
  `[inference].workers`; **do not** stack engines on one box.

If your cameras don't fit this profile (4K, sub-stream only, sub-1 fps,
JPEG snapshot mode) document it in the per-camera config and don't
multiply the soak ceiling by anything optimistic.

## What the generator decides

`nexus-probe emit-config` computes only the knobs that actually vary with
hardware; everything else comes from `nexus_config::Config` defaults, so
the output always parses and satisfies `deny_unknown_fields` by
construction:

- `[inference].ep_priority` — from the inference ranking
  (Hailo > Intel NPU > Intel GPU > AMD ROCm > AMD Vulkan > NVIDIA→CPU > CPU).
- `[runtime.decode].mode` — `va` when a VA-capable Intel/AMD GPU is
  present, else `software`.
- `[inference].workers` — 2–3 for a discrete GPU with ≥6 GB VRAM, else 1.
- `[inference.model].preset` (+ derived input width/height) — `960`/`1280`
  for big discrete GPUs, `640` for iGPU/Hailo/NPU/low-power N-series.
- `[runtime].worker_threads` / `blocking_threads` — from the CPU logical
  core count.
- `[bus].capacity` — bucketed by system RAM.

The generated file carries a `# GENERATED by nexus-probe` provenance
header. To hand-tune a box and keep your edits, re-run the installer with
`--keep-config` (the default behaviour **regenerates** and backs the old
file up to `nexus.toml.bak.<ts>` first).

## AMD GPU inference (`amd-vulkan`)

The EQR7's Radeon 680M (Phoenix gfx1035) runs ONNX-Runtime inference
through the **Vulkan(WebGPU) execution provider** (`ep-vulkan` Cargo
feature, ORT WebGPU on its Dawn→Vulkan backend, bundled ONNX Runtime
1.27), pinned by `ep_priority = ["vulkan", "cpu"]`. It is **opt-in**:
`nexus-probe` never auto-selects Vulkan inference, because routing
inference onto the iGPU is a deliberate choice and the same EQR7 chassis
defaults to the Hailo-8 when one is fitted (and to CPU otherwise). To use
it, install with `--force-profile amd-vulkan`. No
`HSA_OVERRIDE_GFX_VERSION` force-fit: ROCm is reserved for the discrete
RDNA/CDNA GPUs it officially supports (classified from the PCI device ID).
Either way the Mesa VA-API path gives hardware H.264/HEVC decode on the
Radeon. Setup details:
[INSTALL.md §5.5d](INSTALL.md#55d-amd-gpu-inference-vulkan-default-rocm-for-supported-discrete-gpus).

## Lunar Lake / NPU caveat (`intel-npu`)

The Lunar Lake iGPU + NPU 4 stack is the highest-throughput profile
(~115 TOPS combined) but requires a kernel ≥ 6.10, OpenVINO ≥ 2024.4, and
the Intel NPU driver trio installed out-of-band — see
[INSTALL.md §5.3](INSTALL.md#53-tier-t36-s-lunar-lake--add-igpu--npu) and
[nexus-edge-deploy OS_INSTALL.md §6.3](../../nexus-edge-deploy/docs/OS_INSTALL.md).
The generator lists `npu` first with `cpu` as the fallback; if the NPU
driver isn't present yet the engine falls through automatically — that's
the whole point of EP priority lists. (Decode still runs on the Arc 140V
Xe2 media engine via `va`, fully separate from the NPU.)

> **2025-Q3 Intel package rename — heads-up for stale install scripts.**
> The historical `repositories.intel.com/gpu/ubuntu noble unified` apt
> recipe is now **data-center-only** (Flex/Max) and hard-fails on
> client Lunar Lake / Arc / Battlemage / Panther Lake silicon with
> `intel-level-zero-gpu : Depends: libigc1 ... but it is not
> installable`. The new path is `ppa:kobuk-team/intel-graphics`, and
> two packages were renamed in the cutover:
> `intel-level-zero-gpu` → `libze-intel-gpu1`,
> `level-zero` → `libze1`. INSTALL.md §5.3 is current; any third-party
> install transcript citing the old repo or old package names is wrong
> for any Lunar Lake box delivered after 2025-Q3.

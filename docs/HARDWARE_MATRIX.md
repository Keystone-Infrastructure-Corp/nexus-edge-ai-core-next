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

| Profile (`--force-profile`) | Representative box                                 | Inference accelerator          | EP order (`ep_priority`)     | Decode block            | Preset |
| --------------------------- | -------------------------------------------------- | ------------------------------ | ---------------------------- | ----------------------- | ------ |
| `intel-igpu`                | Beelink Mini S13 (N150) · GMKtec M3 (Iris Xe)      | Intel iGPU (OpenVINO)          | `gpu, cpu`                   | Intel iGPU MFX (VA)     | 640    |
| `intel-igpu`                | Lenovo P3 Tiny / HP Z2 Mini + Arc A380             | Intel Arc dGPU (OpenVINO)      | `gpu, cpu`                   | Arc MFX (VA)            | 960    |
| `intel-npu`                 | GMKtec K13 AI / EVO-X1 (Ultra 7 256V Lunar Lake)   | Intel NPU 4 (OpenVINO NPU)     | `npu, cpu`                   | Arc 140V Xe2 (VA)       | 640    |
| `hailo`                     | Beelink EQR7 (Ryzen 7 7735HS) + Hailo-8 M.2        | Hailo-8 (26 TOPS)              | `hailo, cpu`                 | host AMD Radeon (VA)    | 640    |
| `amd-vulkan`                | Beelink EQR7 (Ryzen 7 7735HS), Radeon 680M/780M    | AMD iGPU (ORT Vulkan/WebGPU)   | `vulkan, cpu`                | AMD VCN (radeonsi VA)   | 640    |
| `amd-rocm`                  | discrete RDNA/CDNA GPU on the ROCm allowlist       | AMD dGPU (ORT ROCm)            | `rocm, cpu`                  | AMD VCN (radeonsi VA)   | 960    |
| `nvidia`                    | Lenovo P3 Tower / HP Z2 G9 + Quadro P2000 or RTX    | NVIDIA dGPU (ORT CUDA)         | `cuda, cpu`                  | NVDEC (nvcodec)         | 640 / 960 |
| `cpu`                       | any host with no usable accelerator                | CPU                            | `cpu`                        | software                | 640    |

Notes on the matrix:

- **Decode is orthogonal to inference.** Every Intel/AMD GPU — including a
  decode-only iGPU sitting next to a Hailo-8 or NPU — gives hardware
  H.264/HEVC decode through the unified `va` GStreamer path
  (`vah26Xdec` + `vapostproc` over libva). NVIDIA decodes on its own NVDEC
  block (`nvh26Xdec`, from the `nvcodec` plugin). Hailo-8 and the Intel NPU
  are **inference-only** and never decode; the host iGPU does. See
  [INSTALL.md](INSTALL.md) for the decode plumbing.
- **A box with both an iGPU and a discrete NVIDIA card decodes on VA, not
  NVDEC.** Inference still goes to the NVIDIA card; keeping decode on the
  integrated media engine leaves the dGPU's whole budget for inference.
- **NVIDIA needs the installer to stage a CUDA runtime.** The release
  tarball bundles the OpenVINO-flavoured ONNX Runtime, which has no CUDA
  provider. `scripts/install.sh` installs the proprietary driver plus the
  CUDA runtime and fetches a CUDA-capable ONNX Runtime into
  `/opt/nexus/vendor/onnxruntime-cuda`, then repoints the loader with a
  systemd drop-in. Without that stage the session falls through to the
  trailing `cpu` entry. TensorRT is deliberately **not** in the chain: it
  needs a separate multi-GB install and only pays off on Tensor-Core
  (Turing+) silicon.
- **NVIDIA preset and worker count scale with VRAM** (read from
  `nvidia-smi`): <4 GiB → 1×640, <8 GiB → 2×640 (Quadro P2000),
  <12 GiB → 2×960, ≥12 GiB → 3×960. Unknown VRAM under-provisions to
  1×640 rather than guessing.
- **AMD ROCm vs Vulkan** is decided from the PCI device ID against a
  default-deny allowlist (CDNA MI100/200/300, RDNA2 RX6000, RDNA3 RX7000).
  Phoenix/Rembrandt iGPUs (gfx1035/1103) are **not** on the allowlist and
  resolve to `amd-vulkan`, never ROCm. The allowlist lives solely in
  `nexus-probe`; the bash installer queries it via `nexus-probe
  accel-tags` (the duplicate bash array was removed in the
  single-detector cutover).

## Camera baseline (every profile)

- 1080p H.264 (or H.265 with hardware decode) over RTSP.
- 15 fps capture, motion-gated to the detector.
- One `nexus-engine` process per host. Internal fan-out via
  `[inference].workers`; **do not** stack engines on one box.

**Camera capacity is not a fixed per-profile number** and the matrix
above deliberately omits one. Sustained stream count depends on
resolution, codec, frame rate, motion duty-cycle, model preset, and how
many streams are concurrently active — size it empirically per box
rather than reading a count off a table. As a real-world anchor, an
`intel-npu` Lunar Lake box with 16 GB RAM comfortably sustains **29
cameras** at this baseline (measured 2026-08). If your cameras don't fit
this profile (4K, sub-stream only, sub-1 fps, JPEG snapshot mode),
document it in the per-camera config and validate with a short pilot.

Two budgets move independently and should be sized separately:

- **Decode is continuous** — every stream, every frame, regardless of
  activity. It scales purely with camera count and is the harder ceiling.
- **Inference is motion-gated.** `MotionGate` sits between the source and
  the detector pool and drops the bulk of frames, so inference load is
  camera count × motion duty cycle. Size it for the **concurrent-motion
  burst** (dawn, weather, shift change), not the average — the burst can
  be an order of magnitude above steady state.

Keeping decode and inference on separate silicon is what makes high
camera counts work. Hailo-8 and the Intel NPU never decode; the host iGPU
does. On `amd-vulkan` the same Radeon does both, which is precisely the
gap a Hailo-8 fills on that chassis.

## What the generator decides

`nexus-probe emit-config` computes only the knobs that actually vary with
hardware; everything else comes from `nexus_config::Config` defaults, so
the output always parses and satisfies `deny_unknown_fields` by
construction:

- `[inference].ep_priority` — from the inference ranking
  (Hailo > Intel NPU > Intel GPU > AMD ROCm > AMD Vulkan > NVIDIA > CPU).
- `[runtime.decode].mode` — `va` when a VA-capable Intel/AMD GPU is
  present, `nvdec` on an NVIDIA GPU, else `software`.
- `[inference].workers` — 2 for the Intel NPU, 1–3 for an NVIDIA GPU by
  VRAM, else 1.
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
[INSTALL.md §5.3](INSTALL.md#53-lunar-lake--arc-140v-igpu--npu-4) and
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

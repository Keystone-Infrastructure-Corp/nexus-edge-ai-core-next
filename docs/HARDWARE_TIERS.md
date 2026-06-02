# Hardware tiers

The engine is sized against the same five-tier pyramid as
`nexus-edge-ai-core` v1. Boxes that have already been ordered or are on
the desk are flagged ✅. The ranges are the *soak target* per tier; they
assume the camera baseline below.

## Tier table

| Tier        | Box                                              | Accelerator                   | EP order                     | Workers | Preset | Cams (1080p @ 15 fps) | $       | Status |
| ----------- | ------------------------------------------------ | ----------------------------- | ---------------------------- | ------- | ------ | --------------------- | ------- | ------ |
| **T10**     | Beelink Mini S13 (N150, 16 GB)                   | UHD 24EU iGPU                 | `openvino, cpu`              | 1       | 320    | 1–2                   | ~$300   | ✅ ordered |
| **T24**     | Beelink EQR7 (Ryzen 7 7735HS, 24 GB) + Hailo-8 M.2 | Hailo-8 (26 TOPS, M.2 2280) | `hailo, cpu`                 | 1       | 640    | 24 (target)¹          | ~$1,005 | ✅ ordered |
| **T36**     | Lenovo P3 Tiny / HP Z2 Mini + Arc A380           | Intel Arc A380 6 GB (dGPU)    | `openvino, cpu`              | 2       | 640    | 8–12                  | ~$1100  | not yet sourced |
| **T36-S**   | GMKtec K13 AI / EVO-X1 (Ultra 7 256V Lunar Lake) | Arc 140V Xe2 + NPU 4 (47 TOPS)| `openvino, npu, cpu`         | 2       | 640    | 6–8                   | ~$800   | ✅ ordered |
| **T64**     | Lenovo P3 Tower / HP Z2 G9 + RTX 4060            | NVIDIA RTX 4060 8 GB          | `tensorrt, cuda, cpu`        | 3       | 640    | 12–20                 | ~$1300  | post-beta |

T64 stays opt-in until M5 wires the CUDA/TensorRT EPs. T24 is similarly
gated on **`M_HAILO_EP`** — the Hailo execution provider + `.hef`
model-pack tooling are not yet in the engine, so the EQR7+Hailo-8
appliance currently falls through to the CPU EP (Ryzen 7 7735HS soak
~6–10 cams) until that milestone ships. The hardware is the right
shape for 24 cams; the engine is the blocker. The rest of the tiers
are first-class M1 targets.

¹ T24 soak is the **design target** for the EQR7 + Hailo-8 SKU after
`M_HAILO_EP` lands. The legacy GMKtec M3 Ultra (Iris Xe 96 EU) T24
soak of 4–6 cams is no longer the canonical T24 hardware; existing
GMKtec deployments continue to work with whatever per-deployment
`nexus.toml` they currently have.

## Camera baseline (every tier)

- 1080p H.264 (or H.265 with hardware decode) over RTSP.
- 15 fps capture, motion-gated to the detector.
- One `nexus-engine` process per host. Internal fan-out via
  `[inference].workers`; **do not** stack engines on one box.

If your cameras don't fit this profile (4K, sub-stream only, GB1 fps,
JPEG snapshot mode) document it in the per-camera config and don't
multiply the tier soak ceiling by anything optimistic.

## Per-tier configs

Reference TOML lives in `config/tiers/`. Pick the one that matches the
box, copy it to `/etc/nexus/nexus.toml`, edit camera URLs.

| File                                       | Use it on                                |
| ------------------------------------------ | ---------------------------------------- |
| [`config/tiers/t10.toml`](../config/tiers/t10.toml)     | T10 — Beelink Mini S13                  |
| [`config/tiers/t24.toml`](../config/tiers/t24.toml)     | T24 — Beelink EQR7 + Hailo-8            |
| [`config/tiers/t36.toml`](../config/tiers/t36.toml)     | T36 — Arc A380 SFF                      |
| [`config/tiers/t36s.toml`](../config/tiers/t36s.toml)   | T36-S — GMKtec K13 / EVO-X1 Lunar Lake  |
| [`config/tiers/t64.toml`](../config/tiers/t64.toml)     | T64 — RTX 4060 desktop                  |

The differences are just the four scale knobs:

- `[inference].workers`
- `[inference].ep_priority`
- `[inference.model].preset` (320 / 640 / 1280 from the model pack)
- `[bus].capacity` (proportional to per-tier camera count)

Everything else is identical, which is the point of the trait + pool
pattern: tier choice is a config edit, not a code change.

## How `nexus-probe` recommends a tier

`nexus-probe --out data/device-manifest.json` enumerates the host (CPU
SKU, accelerators, render nodes, NPU presence, OpenVINO / ORT / Docker
versions) and adds a `recommended_tier` field. The mapping:

```
NVIDIA dGPU present                           → T64
Hailo-8 M.2 present (PCI vendor 0x1e60)²      → T24
Intel Arc 140V iGPU OR /dev/accel/* present   → T36-S
Intel Arc dGPU (A310/A380/A580) present       → T36
Intel Iris Xe 96 EU iGPU                      → T24  (legacy GMKtec mapping; still resolves to t24.toml — see note)
Intel UHD 24EU iGPU (N100/N150 class)         → T10
Apple Silicon                                 → dev-only (no soak tier)
fallback                                      → T10
```

² Hailo M.2 detection is **not yet implemented** in `nexus-probe`
(M_HAILO_EP scope). Until then, fresh installs on a Beelink EQR7
box must select `config/tiers/t24.toml` manually — `nexus-probe`
will currently fall through to the T10 fallback for AMD CPUs.

The probe is advisory — operator can always override with an explicit
config — but the recommendation lines up with the boxes actually on the
desk so a clean install picks sensible defaults out of the box.

## Lunar Lake / NPU caveat (T36-S)

The Lunar Lake iGPU + NPU 4 stack is the prize tier (~115 TOPS combined)
but requires a kernel ≥ 6.10, OpenVINO ≥ 2024.4, and the Intel NPU
driver trio installed out-of-band — see
[INSTALL.md §5.3](INSTALL.md#53-tier-t36-s-lunar-lake--add-igpu--npu) and
[nexus-edge-deploy OS_INSTALL.md §6.3](../../nexus-edge-deploy/docs/OS_INSTALL.md).
The tier config `t36s.toml` lists `npu` second in `ep_priority`; if the
NPU driver isn't present yet the engine falls through to `openvino`
(iGPU) automatically — that's the whole point of EP priority lists.

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
> for any T36-S box delivered after 2025-Q3.

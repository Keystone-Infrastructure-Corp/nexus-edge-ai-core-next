# nexus-edge-ai-core-next

> **Status:** v2 architectural rewrite, beta. **M0–M4, M2.1, M2.2,
> M-Install Ckpts 1–3a.1, M-Admin (all 6 phases), M3.1–M3.3, M6
> (identity + audit), and M7 Phase 1 (webhook sinks + cascading
> delivery policy + UI + e2e) all shipped on `main`.** Suitable for
> dogfooding on the reference hardware profiles; production deployment
> blocked on M7 Phase 2 (SureView), and M8
> (bare-metal install + first customer trial) per
> [`../nexus-cloud-console/docs/product/ROADMAP.md`](../nexus-cloud-console/docs/product/ROADMAP.md). Full operator CRUD UI shipped:
> cameras (ONVIF + CIDR discovery), rules (visual + raw CEL),
> polygon zones, storage backends, delivery policy, users, audit log.
> The shell is at `http://<engine-host>:8089/` — see
> [`docs/INSTALL.md`](docs/INSTALL.md) §10.0. New here? Read
> [`../nexus-cloud-console/docs/edge-core/ARCHITECTURE.md`](../nexus-cloud-console/docs/edge-core/ARCHITECTURE.md) for the L0–L7 model,
> then [`../nexus-cloud-console/docs/edge-core/PIPELINE.md`](../nexus-cloud-console/docs/edge-core/PIPELINE.md) for the full
> end-to-end engine walk-through with mermaid diagrams.
>
> **Strategic docs moved (2026).** Architecture, pipeline, milestones, roadmap, and
> business plan now live in the cloud-console repo at
> [`../nexus-cloud-console/docs/edge-core/`](../nexus-cloud-console/docs/edge-core/) and
> [`../nexus-cloud-console/docs/product/`](../nexus-cloud-console/docs/product/). This
> repo retains only install + dev docs ([`docs/INSTALL.md`](docs/INSTALL.md),
> [`docs/DEV_NOTES.md`](docs/DEV_NOTES.md),
> [`docs/HARDWARE_MATRIX.md`](docs/HARDWARE_MATRIX.md)). The repo boundary + license
> asymmetry (engine AGPL-3.0-or-later, cloud Proprietary) is documented in
> [`AGENTS.md`](AGENTS.md) and
> [`../nexus-cloud-console/docs/REPO_BOUNDARY.md`](../nexus-cloud-console/docs/REPO_BOUNDARY.md).

A streaming-DAG edge-AI pipeline for surveillance video.

The first attempt at this rewrite (now wiped) tried to be elegant. This one
tries to be **right**: every layer that scales horizontally is a *trait + pool
of backends*, every layer that's pluggable is a *trait + multiple
implementations*, and the substrate decisions are committed at the architecture
level instead of being defaults nobody can change.

## The single architectural commitment

```text
                       trait + N backends + scale-factor knob
                       ────────────────────────────────────────
   FrameSource           rtsp / file / virtual                  pool: per-camera
   Detector              in-process / thread-isolated /          pool: N workers
                         worker-process / open-vocab / ensemble
   Tracker               iou-naive / bytetrack                   per-camera
   RuleEngine            cel                                     single
   EventStore            sqlite                                  single
   Bus                   broadcast / nats                        capacity knob
```

Every backend has the same operational surface: `slot()`, `state()`,
`generation()`, `push_camera_config()`. That makes pool routing, fail-soft
fallback, hot-reload fan-out, and OPS observability **the same code** at every
layer that needs it.

See [`../nexus-cloud-console/docs/edge-core/ARCHITECTURE.md`](../nexus-cloud-console/docs/edge-core/ARCHITECTURE.md)
for the data flow and the explicit list of side-channels (`LatestFrameCache` etc.) that
live alongside the main bus.

## Hardware support

The engine is **capability-based**: at install time `nexus-probe` detects the
host's silicon and **generates** `/etc/nexus/nexus.toml` sized for it
(`emit-config`) — there are no per-box templates to pick from. Boxes already
on the desk are flagged ✅.

| Profile (`--force-profile`) | Box                                       | Accelerator         | Status |
| --------------------------- | ----------------------------------------- | ------------------- | ------ |
| `intel-igpu`                | Beelink Mini S13 (N150)                   | UHD 24EU iGPU       | ✅ ordered |
| `hailo`                     | Beelink EQR7 (Ryzen 7 7735HS) + Hailo-8   | Hailo-8 26 TOPS M.2 | ✅ ordered |
| `amd-vulkan`                | Beelink EQR7 (Ryzen 7 7735HS)             | Radeon 680M (Vulkan)| ✅ shipping |
| `intel-igpu`                | Lenovo P3 Tiny + Arc A380                 | Intel Arc A380 dGPU | not yet sourced |
| `intel-npu`                 | GMKtec K13 / EVO-X1 (Lunar Lake 256V)     | Arc 140V + NPU 4    | ✅ ordered |
| `nvidia`                    | Lenovo P3 Tower + Quadro P2000 / RTX      | NVIDIA dGPU (CUDA)  | ✅ shipping |

Sustained camera count is **sized empirically per box** — it depends on
resolution, codec, frame rate, motion duty-cycle, and model preset, so
it isn't a fixed number you can read off a table. As a real-world
anchor, an `intel-npu` Lunar Lake box comfortably runs ~21 cameras at
1080p/15fps.

The Hailo EP shipped in v0.1.79: HailoRT 4.23 + `.hef` model pack
load through `nexus-hailo-backend`, and the System tab surfaces live
chip temp, power, utilization%, inferences/sec, firmware, serial, and
part number. The AMD profile ships the Radeon 680M (Phoenix gfx1035)
running inference through the Vulkan(WebGPU) execution provider
(`ep-vulkan` + bundled ONNX Runtime 1.27 on its Dawn→Vulkan backend),
opt-in via `--force-profile amd-vulkan`; the System tab shows the
Radeon's VRAM, temperature, and utilization%. ROCm is reserved for the
discrete RDNA/CDNA GPUs it officially supports (classified from the PCI
device ID at install time) — never force-fit onto an unsupported iGPU.
NVIDIA runs inference on the CUDA execution provider and decodes on the
card's NVDEC block; worker count and detector preset scale with VRAM.
Because the release tarball bundles the OpenVINO-flavoured ONNX Runtime
(which has no CUDA provider), `install.sh` installs the proprietary
driver plus CUDA runtime and stages a CUDA-capable ONNX Runtime into
`/opt/nexus/vendor/`, repointing the loader with a systemd drop-in.
TensorRT is intentionally not used — it needs a separate multi-GB
install and only pays off on Tensor-Core (Turing+) silicon.

`nexus-probe emit-config` generates the right `ep_priority`, decode mode,
worker/thread sizing, and model preset for the detected box automatically
on a clean install. Full matrix + Lunar Lake driver caveats:
[`docs/HARDWARE_MATRIX.md`](docs/HARDWARE_MATRIX.md).

## Workspace layout

```text
crates/
├── nexus-types/        Wire types — Frame, Detection, TrackedObject, AlertEvent
├── nexus-config/       TOML schema + validation. Scale knobs per layer.
├── nexus-bus/          Bus trait + BroadcastBus + NatsBus (feature)
├── nexus-telemetry/    OTEL init; the `frame.*` span family lives here
├── nexus-store/        SQLite via sqlx + DuckDB attach for analytics
├── nexus-rules/        RuleEngine trait + CelEngine
├── nexus-tracker/      Tracker trait + ByteTrack + IouNaive
├── nexus-inference/    Detector + DetectorBackend + DetectorPool
│                         ├── InProcessBackend (synchronous)
│                         ├── ThreadIsolatedBackend (panic recovery)
│                         └── WorkerProcessBackend (OS-level isolation)
├── nexus-pipeline/     FrameSource + Source pool + LatestFrameCache
│                         (cache documented as L7 in ARCHITECTURE.md)
├── nexus-engine/       Binary. Wires pipeline + serves /api + serves /ui
└── nexus-probe/        One-shot host probe → device-manifest.json

ui/                     TypeScript SPA (Vite). Types come from Rust via ts-rs.
                        Built into engine container under /usr/share/nexus/ui.
deploy/                 ONE Dockerfile (multi-stage rust+node→runtime) + compose
config/                 Example TOML configs
docs/                   ARCHITECTURE, ROADMAP, COMPARISON
tools/                  youtube-rtsp-bridge (dev), eval-labeler (prompt QA)
```

## Build

```bash
# Native (requires GStreamer + ONNX Runtime + node 20)
cargo build --release
(cd ui && npm install && npm run build)

# Container (recommended — no system dep hunt)
docker compose -f deploy/docker-compose.yml build
docker compose -f deploy/docker-compose.yml up
```

## Run

```bash
./target/release/nexus-probe --out data/device-manifest.json
# Or generate a full config from detected hardware: nexus-probe emit-config --out -
./target/release/nexus-engine --config config/single-camera.toml
# Engine listens on :8089 — the SPA at / hosts Viewer (live), Timeline (clips),
# Events (alerts) plus an admin console (Cameras CRUD + ONVIF/CIDR discovery,
# Rules CRUD + visual CEL builder, polygon Zones, Storage backends, Backends
# pool, Health). REST API is mounted under /api/*.
```

Single binary, single port, single container. Admin and viewer are routes in
the same SPA. Python sidecars are gone.

## Status

Beta. Cores **M0–M4** + **M-Install Checkpoints 1–2** + **M-Admin Phases 0–6**
all shipped. Full workspace compile + tests pass on macOS (`brew install
gstreamer onnxruntime node@22`) and on the Docker reference image. Production
deployment is still blocked on **M7** (alert delivery — webhook + SureView)
and **M8** (first customer trial). See
[`../nexus-cloud-console/docs/product/ROADMAP.md`](../nexus-cloud-console/docs/product/ROADMAP.md)
for the milestone tracker.

## License

AGPL-3.0-or-later.

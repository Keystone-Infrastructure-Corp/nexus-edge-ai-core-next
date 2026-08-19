# Nexus Edge AI Core

**Nexus Edge AI Core is the on-premises video-analytics engine that runs on a
customer's own hardware, next to their cameras.** It is a single Rust binary
(`nexus-engine`) that pulls live RTSP streams in, decodes and analyzes every
frame, detects and tracks objects, evaluates operator-defined rules, records
motion clips and alert events, and serves a local web UI/API for viewing and
administering the box — all without requiring a connection to the internet.
It is the edge half of **Nexus Edge AI**; the companion cloud control plane
([nexus-cloud-console](../nexus-cloud-console)) is optional and adds
multi-site fleet management, remote viewing, and long-term storage, but the
engine keeps detecting, recording, and enforcing rules on its own even when
the cloud is unreachable — see "Local-first, cloud-optional" below.

The admin UI and live/clip viewer are served from the engine itself at
`http://<engine-host>:8089/` — see [`docs/INSTALL.md`](docs/INSTALL.md) for
installation and first-boot setup.

## What it does

- **Ingests** RTSP (and file / virtual, for dev) camera streams per-camera,
  decoding on hardware video acceleration where the box supports it.
- **Analyzes** every frame with an ONNX object detector (closed-vocabulary
  YOLO or open-vocabulary YOLO-World) running on whatever accelerator the
  host has — Intel iGPU/NPU, Hailo M.2, AMD iGPU (Vulkan), or NVIDIA GPU.
- **Tracks** objects across frames (ByteTrack or a naive IOU tracker) so
  rules reason about persistent objects, not single-frame detections.
- **Evaluates rules** (CEL expressions, authored visually or raw, scoped to
  polygon zones) against tracked objects to decide what's alert-worthy.
- **Records** motion-triggered video clips and a structured event/audit log
  to local storage, with configurable storage backends and retention.
- **Serves** a local single-page UI and REST API on one port (`:8089`) for
  live viewing, clip playback, and full admin CRUD (cameras, rules, zones,
  storage, users, delivery policy) — no separate admin tool, no Python
  sidecars.
- **Delivers alerts** outward via webhook sinks (and, optionally, over a
  tunnel to the cloud control plane) once a rule fires.

## Local-first, cloud-optional

The engine is fully functional with zero cloud connectivity: detection,
tracking, rule evaluation, clip recording, and the local admin UI all keep
working if the box is offline. The cloud control plane in
[nexus-cloud-console](../nexus-cloud-console) is a separate, optional service
that a fleet operator points many edge boxes at for centralized camera/rule
management, remote live view, and off-box storage — reached over one mTLS
tunnel and a versioned wire protocol, never a shared dependency. See
[`AGENTS.md`](AGENTS.md) and
[`../nexus-cloud-console/docs/REPO_BOUNDARY.md`](../nexus-cloud-console/docs/REPO_BOUNDARY.md)
for the hard boundary between the two repos.

## Architecture in one line

Every layer that scales horizontally is a *trait + pool of backends*; every
layer that's pluggable is a *trait + multiple implementations*:

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
(`emit-config`) — there are no per-box templates to pick from.

| Profile (`--force-profile`) | Example box                                | Accelerator          |
| --------------------------- | ------------------------------------------- | -------------------- |
| `intel-igpu`                | Beelink Mini S13 (N150)                     | UHD 24EU iGPU         |
| `hailo`                     | Beelink EQR7 (Ryzen 7 7735HS) + Hailo-8     | Hailo-8 26 TOPS M.2   |
| `amd-vulkan`                | Beelink EQR7 (Ryzen 7 7735HS)               | Radeon 680M (Vulkan)  |
| `intel-igpu`                | Lenovo P3 Tiny + Arc A380                   | Intel Arc A380 dGPU   |
| `intel-npu`                 | GMKtec K13 / EVO-X1 (Lunar Lake 256V)       | Arc 140V + NPU 4      |
| `nvidia`                    | Lenovo P3 Tower + Quadro P2000 / RTX        | NVIDIA dGPU (CUDA)    |

Sustained camera count is **sized empirically per box** — it depends on
resolution, codec, frame rate, motion duty-cycle, and model preset, so
it isn't a fixed number you can read off a table. As a real-world
anchor, an `intel-npu` Lunar Lake box comfortably runs ~21 cameras at
1080p/15fps.

The Hailo profile runs HailoRT + a `.hef` model pack through
`nexus-hailo-backend`, and the System tab surfaces live chip temp, power,
utilization%, inferences/sec, firmware, serial, and part number. The AMD
profile runs inference on the Radeon 680M/780M iGPU through the
Vulkan(WebGPU) execution provider
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
                        Built into the release tarball under share/ui/
                        (served from /opt/nexus/current/share/ui on install).
deploy/                 Bare-metal install surface: systemd unit, apt deps,
                        udev rules, sudoers.d, OTA appliers (no Dockerfile —
                        Docker/GHCR publishing was removed, see docs/INSTALL.md)
config/                 Example TOML configs
docs/                   ARCHITECTURE, ROADMAP, COMPARISON
tools/                  youtube-rtsp-bridge (dev), eval-labeler (prompt QA)
```

## Build

```bash
# Native (requires GStreamer + ONNX Runtime + node 20)
cargo build --release
(cd ui && npm install && npm run build)

# Bare-metal release tarball (recommended — no system dep hunt)
# Download a release from GitHub Releases, then:
./install.sh
# See docs/INSTALL.md for the full guide.
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

## Documentation

- [`docs/INSTALL.md`](docs/INSTALL.md) — installation guide, per-hardware driver setup
- [`docs/DEV_NOTES.md`](docs/DEV_NOTES.md) — developer workflow, local toolchain
- [`docs/HARDWARE_MATRIX.md`](docs/HARDWARE_MATRIX.md) — full hardware/EP support matrix
- [`../nexus-cloud-console/docs/edge-core/ARCHITECTURE.md`](../nexus-cloud-console/docs/edge-core/ARCHITECTURE.md) — the L0–L7 architecture model
- [`../nexus-cloud-console/docs/edge-core/PIPELINE.md`](../nexus-cloud-console/docs/edge-core/PIPELINE.md) — end-to-end pipeline walk-through
- [`../nexus-cloud-console/docs/REPO_BOUNDARY.md`](../nexus-cloud-console/docs/REPO_BOUNDARY.md) — the boundary between this repo and the cloud control plane

## License

AGPL-3.0-or-later.

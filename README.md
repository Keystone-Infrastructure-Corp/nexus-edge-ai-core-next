# nexus-edge-ai-core-next

> **Status:** v2 architectural rewrite, beta. **M0–M4, M2.1, M2.2,
> M-Install Ckpts 1–3a.1, M-Admin (all 6 phases), M3.1–M3.3, M6
> (identity + audit), and M7 Phase 1 (webhook sinks + cascading
> delivery policy + UI + e2e) all shipped on `main`.** Suitable for
> dogfooding on the reference hardware tiers; production deployment
> blocked on M5 (CUDA/TensorRT EPs), M7 Phase 2 (SureView), and M8
> (bare-metal install + first customer trial) per
> [`docs/ROADMAP.md`](docs/ROADMAP.md). Full operator CRUD UI shipped:
> cameras (ONVIF + CIDR discovery), rules (visual + raw CEL),
> polygon zones, storage backends, delivery policy, users, audit log.
> The shell is at `http://<engine-host>:8089/` — see
> [`docs/INSTALL.md`](docs/INSTALL.md) §10.0. New here? Read
> [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the L0–L7 model,
> then [`docs/PIPELINE.md`](docs/PIPELINE.md) for the full
> end-to-end engine walk-through with mermaid diagrams.

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

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the data flow and the
explicit list of side-channels (`LatestFrameCache` etc.) that live alongside the
main bus.

## Hardware tiers

Sized against the same five-tier pyramid as v1. Boxes already on the desk
are flagged ✅. Per-tier reference configs live in
[`config/tiers/`](config/tiers/). Pick the file that matches the box.

| Tier        | Box                                       | Accelerator         | Cams (1080p/15fps) | Status |
| ----------- | ----------------------------------------- | ------------------- | ------------------ | ------ |
| **T10**     | Beelink Mini S13 (N150)                   | UHD 24EU iGPU       | 1–2               | ✅ ordered |
| **T24**     | GMKtec M3 Ultra (i7-12700H)               | Iris Xe 96 EU       | 4–6               | ✅ ordered |
| **T36**     | Lenovo P3 Tiny + Arc A380                 | Intel Arc A380 dGPU | 8–12              | not yet sourced |
| **T36-S**   | GMKtec K13 / EVO-X1 (Lunar Lake 256V)     | Arc 140V + NPU 4    | 6–8               | ✅ ordered |
| **T64**     | Lenovo P3 Tower + RTX 4060                | NVIDIA RTX 4060     | 12–20             | post-beta |

`nexus-probe` writes `recommended_tier` into the device manifest so a
clean install picks the right `config/tiers/*.toml` automatically. Full
table + Lunar Lake driver caveats: [`docs/HARDWARE_TIERS.md`](docs/HARDWARE_TIERS.md).

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
# Probe writes recommended_tier; pick the matching config/tiers/*.toml.
./target/release/nexus-engine --config config/tiers/t24.toml
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
and **M8** (first customer trial). See [`docs/ROADMAP.md`](docs/ROADMAP.md)
for the milestone tracker.

## License

AGPL-3.0-or-later.

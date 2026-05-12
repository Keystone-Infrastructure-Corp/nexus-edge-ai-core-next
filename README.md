# nexus-edge-ai-core-next

> **Status:** v2 architectural rewrite. Pre-alpha. Not yet at parity with
> `nexus-edge-ai-core`; do not deploy.

A streaming-DAG edge-AI pipeline for surveillance video.

The first attempt at this rewrite (now wiped) tried to be elegant. This one
tries to be **right**: every layer that scales horizontally is a *trait + pool
of backends*, every layer that's pluggable is a *trait + multiple
implementations*, and the substrate decisions are committed at the architecture
level instead of being defaults nobody can change.

## The single architectural commitment

```
                       trait + N backends + scale-factor knob
                       ────────────────────────────────────────
   FrameSource           rtsp / file / virtual                  pool: per-camera
   Detector              in-process / thread-isolated /          pool: N workers
                         worker-process / open-vocab / ensemble
   Tracker               iou-naive / bytetrack                   per-camera
   RuleEngine            cel / legacy-json                       single
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

```
crates/
├── nexus-types/        Wire types — Frame, Detection, TrackedObject, AlertEvent
├── nexus-config/       TOML schema + validation. Scale knobs per layer.
├── nexus-bus/          Bus trait + BroadcastBus + NatsBus (feature)
├── nexus-telemetry/    OTEL init; the `frame.*` span family lives here
├── nexus-store/        SQLite via sqlx + DuckDB attach for analytics
├── nexus-rules/        RuleEngine trait + CelEngine + LegacyJsonEngine
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
# Engine listens on :8089 — UI is at /, API is at /api/*
```

Single binary, single port, single container. Admin and viewer are routes in
the same SPA. Python sidecars are gone.

## Status

Pre-alpha. Crates compile in isolation; full workspace compile requires
`gstreamer-1.0` and `onnxruntime` system libs. No real frame has flowed through
end-to-end yet — that's M1.

## License

AGPL-3.0-or-later.

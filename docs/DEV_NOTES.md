# Dev notes

Short, sharp lessons from working on this repo. Intended for humans and
future automated agents who need to skip the same potholes we already hit.

## Local toolchain

- All 6 CI gates can be mirrored locally on macOS (Apple Silicon). Rough
  setup, one time:

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # rust
  brew install gstreamer node@22 onnxruntime
  brew install --cask docker-desktop && open -a Docker
  ```

  After install, source `. "$HOME/.cargo/env"` per shell (zsh doesn't pick
  it up from `~/.zshrc` by default).

- `command -v rustup` returning empty does NOT mean rustup is missing —
  it usually means the current shell hasn't sourced `~/.cargo/env`. Check
  `[ -d ~/.rustup ] || ls ~/.cargo/bin/rustup` first.

- Per-change loop, in priority order. Run only the smallest one that
  exercises the file you touched:

  ```bash
  cargo fmt --all -- --check                          # ~1 s
  cargo check  --locked --workspace --all-targets     # ~3 s incremental
  cargo clippy --locked --workspace --all-targets -- -D warnings   # ~5 s
  cargo test   --locked --workspace --no-fail-fast    # ~4 s
  cargo test   -p nexus-types --features ts           # regenerates ui/src/api/types/
  cargo check  --locked -p nexus-pipeline --features gstreamer
  cargo check  --locked -p nexus-inference --features ort,ep-cpu \
       ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib
  cargo check  --locked -p nexus-inference --features ort,ep-cuda   # compile-only
  cd ui && npm install && npm run typecheck && npm run build
  ```

  Burning a CI cycle for any of these is wasted iteration.

- ORT brew formula ≥ 1.22.x works at runtime against the `ort = "=2.0.0-rc.12"`
  crate (api-24 feature) via `load-dynamic`. The crate compiles against the
  1.24.x C API but the runtime ABI is forward-compatible — brew's current
  1.22.x dylib still loads fine on macOS for dev work. The Linux production
  path pins the matched 1.24.0 tarball; see `.github/workflows/ci.yml` and
  `docs/INSTALL.md §7.4`. Set `ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib`.

- `--features ort,ep-cuda` **compiles anywhere**, including macOS and every
  CI runner, because the CUDA provider is `dlopen`'d when a session opens,
  not linked at build time. That makes `cargo check -p nexus-inference
  --features ort,ep-cuda` a genuine gate on the `ep-cuda` code path
  (the `CUDAExecutionProvider` builder API) with no GPU present — it runs in
  CI for exactly this reason. What it cannot prove is *attachment*: ORT
  silently skips a provider it cannot load, so a CUDA-less
  `libonnxruntime.so` yields a CPU session with no error. Verifying the EP
  actually binds requires the reference NVIDIA box.

- The engine's own `ORT_DYLIB_PATH` on an NVIDIA deployment is **not** the
  one inside the release tree. The release tarball bundles the
  OpenVINO-flavoured runtime; `install.sh` stages a CUDA-capable build into
  `/opt/nexus/vendor/onnxruntime-cuda/` and repoints the loader with a
  systemd drop-in (`10-ort-cuda.conf`). The vendor directory lives outside
  the release tree so it survives OTA flips of the `/opt/nexus/current`
  symlink. To reproduce locally, download
  `onnxruntime-linux-x64-gpu-<ver>.tgz` (the plain `-gpu-` asset — the
  `_cuda13` variant drops Pascal) and point `ORT_DYLIB_PATH` and
  `LD_LIBRARY_PATH` at its `lib/`.

## Cargo / Rust

- After removing or renaming a source file referenced from `Cargo.toml`'s
  `[[bin]]` / `[[test]]` tables, run `cargo check` on the touched crate
  before assuming the build is sane — stale build graphs occasionally
  resurrect deleted files.

- Never combine `#[derive(Default)]` AND `#[serde(default = "fn")]` on the
  same struct. The derive zeroes fields; serde's `default = "fn"` only fires
  for missing keys during deserialise. Operators reading a partial config
  and library code constructing `T::default()` then disagree on what
  "default" means. Hand-write `impl Default` whenever any field uses a
  custom serde default. Canonical example: `nexus-config::TrackerConfig`.

## CI behaviour

- `system-libs` job (gstreamer + ort) used to run for 10–20 min before
  caching landed (commit `312941f`). Apt mirror at Azure was slow. The
  cache now key-pins `pkg-config libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev`
  and the ORT tarball at `/opt/onnxruntime`. After cache warmup the job
  is ~30 s. If you bump the package list, also bump `version: N` on the
  `awalsh128/cache-apt-pkgs-action` step or you'll pull a stale cache.

- `cargo fmt --check` is required (not `continue-on-error`). A drift-free
  workspace is the contract.

- `ts-rs` exports per-type `.ts` files into `ui/src/api/types/`.
  `cargo test -p nexus-types --features ts` regenerates them; CI then
  runs `git diff --exit-code -- ui/src/api/types/`. Re-commit the
  generated files when you change a `#[derive(TS)]` type.

## Working with long-running processes

- Background-mode terminals get killed when the chat tool cleans up,
  taking child processes with them. For HTTP smoke tests against a
  long-running engine, prefer:

  ```bash
  ( engine & echo $! > pid ) && sleep 2 && curl … ; kill "$(cat pid)" ; wait
  ```

  in a single sync command instead of starting an async terminal.

## YOLO model + smoke test

The real ORT detector lives in
[`crates/nexus-inference/src/yolo.rs`](../crates/nexus-inference/src/yolo.rs)
and is gated by the `ort` cargo feature. The smoke test
`yolo_smoke_runs_on_synthetic_frame` only runs when both:

- the binary is built with `--features ort,ep-cpu` (so the `ort` symbols
  link), and
- the env var `NEXUS_TEST_YOLO_MODEL` points at an existing
  `yolo26n_640.onnx` (or one of the other shipped sizes) on disk.

The `models/` directory is in `.gitignore`. Stage it locally:

```bash
mkdir -p models
# Generate via tools/models/gen_yolo26n.py --all-static (needs yolo26n.pt
# in the modelgen venv), OR download from a release:
gh release download v0.1.19 \
  --pattern 'yolo26n_640.onnx' \
  --pattern 'models-manifest.json' \
  --dir models/
ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib \
NEXUS_TEST_YOLO_MODEL=$PWD/models/yolo26n_640.onnx \
  cargo test --locked -p nexus-inference --features ort,ep-cpu \
  yolo_smoke -- --nocapture
```

The worker binary picks up the same model with
`NEXUS_WORKER_MODEL_KIND=yolo` + `NEXUS_WORKER_MODEL_PATH=$PWD/models/yolo26n_640.onnx`.

## YOLO-World (open-vocab) model + smoke test

M3 ships an open-vocab detector — `YoloWorldDetector` in
[`crates/nexus-inference/src/yolo_world.rs`](../crates/nexus-inference/src/yolo_world.rs).
The model is regenerated from a tracked prompt vocabulary, not copied
from v1.

```bash
# One-time: model-gen venv (Python 3.11 — torch wheels exist for it).
/opt/homebrew/opt/python@3.11/bin/python3.11 -m venv .venv-modelgen
source .venv-modelgen/bin/activate
pip install -r tools/models/requirements.txt

# Pre-download the YOLO-World checkpoint to a known path
# (ultralytics' built-in downloader sometimes fails behind picky network
#  stacks; curl is reliable):
mkdir -p models/.cache
curl -sL --fail \
  -o models/.cache/yolov8s-worldv2.pt \
  https://github.com/ultralytics/assets/releases/download/v8.4.0/yolov8s-worldv2.pt

# Generate the static-size variants
# (`models/yolo_world_v2_s_640.onnx` ~50 MB, the 640 default,
#  `models/yolo_world_v2_s_960.onnx` ~50 MB, the 960 default) with the
# default vocabulary baked in. `--all-static` is the release path; it
# runs both sizes in one ultralytics session to avoid the import +
# checkpoint-load overhead twice. The script also upserts the entry
# into models/models-manifest.json with sha256 + prompts[] for each
# size:
python tools/models/gen_yolo_world.py \
  --base-model models/.cache/yolov8s-worldv2.pt \
  --all-static
```

Then the smoke test (point the env-var at whichever size you want to
exercise — the smoke test boots one ORT session per file, so 640 is
faster):

```bash
ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib \
NEXUS_TEST_YOLO_WORLD_MODEL=$PWD/models/yolo_world_v2_s_640.onnx \
NEXUS_TEST_YOLO_WORLD_MANIFEST=$PWD/models/models-manifest.json \
  cargo test --locked -p nexus-inference --features ort,ep-cpu \
  yolo_world_smoke -- --nocapture
```

To change the prompt vocabulary, edit
[`tools/models/yolo_world_default_prompts.txt`](../tools/models/yolo_world_default_prompts.txt)
and re-run `gen_yolo_world.py --all-static`. The manifest sha256 will
refresh for every per-size artifact and the engine's loader will catch
the diff.

The worker binary picks up the open-vocab model with
`NEXUS_WORKER_MODEL_KIND=yolo_world` +
`NEXUS_WORKER_MODEL_PATH=$PWD/models/yolo_world_v2_s_640.onnx` +
`NEXUS_WORKER_MODEL_PACK=$PWD/models` (so it can find the manifest +
load the baked vocab). Substitute `_960` for the 960-input variant if
running on a tier whose default is 960.

## Per-camera model selection (`InferenceRouter`)

The global `inference.model` block in `core-config.toml` is the
*default* model — every camera uses it unless it sets a per-camera
override:

```toml
[[cameras]]
id = 7
name = "loading-dock-east"
url = "rtsp://…"
enabled = true

# This camera runs YOLO-World instead of the default closed-vocab
# YOLOv26-nano. The override only swaps the model substruct — backend,
# pool worker kind, worker count, and EP priority are inherited from
# the global `inference` block.
[cameras.model_override]
kind = "yolo_world"
preset = "vga"
input_width = 640
input_height = 640
score_threshold = 0.25
```

At boot, `InferenceRouter::build` walks the camera list, dedups
overrides by `kind`, and builds one `InferenceLayer` per *kind
referenced by any camera* (default + each unique override). Each
camera spawn calls `router.detector_for_camera(&cam)` to get its
`Arc<dyn Detector>`. The default kind's pool is what `/api/v1/backends`
shows; per-kind pool visibility is a future expansion.

Two cameras that pick the same `kind` but different thresholds today
share one detector — per-camera score thresholds are honored at the
rule layer, not in the detector. If we ever need per-camera
*detector instances* (separate ORT sessions for separate models of the
same kind), the router's layer-key shape can be reved without changing
callers.

## YOLOE visual-prompt mode (M3.1)

`kind = "yoloe_visual"` plugs an open-vocab detector that matches
against operator-uploaded reference crops (not text labels). The
admin uploads a JPEG/PNG/WEBP via
`POST /api/v1/admin/visual-prompts` (multipart `name`,
`description?`, `image`); the engine SHA256s the bytes, persists
the file under `runtime.visual_prompts_dir`
(default `/var/lib/nexus/visual_prompts`), runs the encoder ONNX
once, and writes the resulting embedding to the
`visual_prompts` table. Attaching a prompt to a camera
(`POST /api/v1/admin/cameras/:cid/visual-prompts/:vpid`) inserts a
join row and fires a `CameraConfigUpdate` so the visual-mode
detector picks up the new embeddings without restart.

The image encoder is `inference.model.pack_path / yoloe26_s_image_encoder.onnx`
— same pack directory that ships the per-frame detector. With no
pack path configured the upload endpoint returns 503
`encoder_not_configured`; nothing else 503s.

**Worker mode quirk:** when the inference backend is
`spawned_process`, the visual-mode worker needs read access to the
visual-prompts table. The binary expects `NEXUS_WORKER_DB_URL`
(SQLite URL pointing at the engine's DB) in its env; falls back to
the mock detector if missing or unreadable, so a misconfigured
worker degrades to "no detections" rather than panic. The worker
IPC channel does **not** yet have a `PushConfig` RPC — embedding
changes only land after a worker restart in spawned mode. The
default `in_process` backend has no such limitation.

Embedding-dimension override is `NEXUS_WORKER_EMBEDDING_DIM` (default
512). Must match the encoder's output dim or `push_camera_config`
drops the binding with a warn.

Detections emitted by visual mode carry the operator-supplied label
verbatim (no vocab-index lookup), so CEL rules read e.g.
`object.label == "amazon_van"`.

## OTA update handler (Phase 9)

The cloud-orchestrated updater is **in-process** — it lives in
[`crates/nexus-engine/src/cloud_update.rs`](../crates/nexus-engine/src/cloud_update.rs),
not a separate binary. It reacts to `update_assignment` /
`update_rollback` frames off the cloud tunnel. Flow: Ed25519-verify
the release manifest → preflight the privileged surface (`sudo -n -l`
the applier) → download + SHA-256 → stage the tarball and hand the
whole privileged sequence to the single root-owned applier
`/usr/local/sbin/nexus-apply-release apply <version>`
(extract → deps/journald → flip → restart) → post-restart health
check → post-health prune. Each step is
reported as an `update_progress` phase (`verifying_signature`,
`fetching_artifact`, `draining`, `restarting`, `verifying_health`,
`success`, `failed:<code>`). `UpdateState` is persisted in the
engine settings store (key `update.state`) so a restart can resume
and finalise.

Dev/test notes:

- The extract/flip/restart path is `cfg(target_os = "linux")`; on
  macOS the handler compiles but fails closed with
  `unsupported_platform`, so unit tests exercise the verify/download
  logic only.
- The pinned signing key `NEXUS_RELEASE_SIGNING_PUBKEY_V1` is empty
  in un-provisioned builds → every assignment fails
  `signature_invalid`. To exercise the full path locally, override
  with a test key via `NEXUS_RELEASE_SIGNING_PUBKEY_PEM`.
- Crash-loop auto-rollback fires after the **3rd** consecutive
  post-restart version mismatch (`crash_count >= 3`), self-issuing a
  local `handle_rollback` to `previous_good` with `actor_token: None`
  (never crosses the tunnel, so no token to verify).
- The privileged surface is a SINGLE frozen sudoers grant on
  `/usr/local/sbin/nexus-apply-release`
  ([`deploy/sudoers.d/nexus-update`](../deploy/sudoers.d/nexus-update)).
  Because the grant is an argv-independent wildcard, the engine's
  `Command` argv can change without any sudoers edit — the old
  per-command byte-for-byte coupling (which could brick an OTA on
  drift) is gone. A drifted/old sudoers file is caught by the
  `sudo -n -l` preflight, failing fast with `failed:privsurface_drift`
  instead of a half-applied update. Adding a NEW kind of privileged
  action is a deliberate edit to the applier
  ([`deploy/nexus-apply-release`](../deploy/nexus-apply-release)),
  adopted via one install.sh run.
- The applier **restarts the engine out of its own cgroup**
  (`systemd-run --collect` transient unit, `--no-block` fallback).
  It has to: the engine spawns the applier, so the applier runs
  inside `nexus-engine.service`; a direct `systemctl restart` there
  gets the applier SIGTERM'd by the unit stop job before it can
  exit 0. The engine correspondingly treats "applier died by signal
  **and** `/opt/nexus/current` already names the target" as a
  successful apply — without that, a perfectly good update reported
  `failed:apply_failed` to the cloud and rolled `current_version`
  back to `previous_good`. Both halves must stay in sync; see
  `current_release_is` in
  [`cloud_update.rs`](../crates/nexus-engine/src/cloud_update.rs).
- `apply` **refreshes both wrappers** (`nexus-apply-release`,
  `nexus-apply-deps`) from `<release>/scripts/` before flipping, via
  an atomic rename (never an in-place write — the applier is the
  running script). Without this, an applier fix could only reach a
  box through a manual `install.sh` re-run, so applier bugs were
  effectively unshippable over the air. The trust boundary is
  unchanged: the engine has already Ed25519-verified the tarball and
  the applier was about to execute that same tree as root.

## See also

- [`ARCHITECTURE.md`](../../nexus-cloud-console/docs/edge-core/ARCHITECTURE.md) — trait + pool + fail-soft pattern,
  L7 cache, frame-lifecycle spans, sampling.
- [`ROADMAP.md`](../../nexus-cloud-console/docs/product/ROADMAP.md) — milestones M0 → M8.
- [`HARDWARE_MATRIX.md`](HARDWARE_MATRIX.md) — vendor/capability matrix (Intel iGPU / NPU / AMD Vulkan / ROCm / Hailo / NVIDIA / CPU).

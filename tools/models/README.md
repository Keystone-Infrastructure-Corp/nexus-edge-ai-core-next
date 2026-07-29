# `tools/models/` — model generators (M3+)

Reproducible exports for every ONNX artifact the engine loads. The
artifacts themselves are gitignored (they're large binary blobs); the
scripts here are the source of truth and run inside the dedicated
`.venv-modelgen` virtualenv.

## Setup (one-time)

The model-gen toolchain is heavy (torch, ultralytics, transformers
sometimes). It lives in its own Python venv so it never collides with
the runtime Python (or with the OS Homebrew Python and PEP 668).

```bash
# Python 3.11 — torch + ultralytics ship full wheels for it; 3.14 is too new.
/opt/homebrew/opt/python@3.11/bin/python3.11 -m venv .venv-modelgen
source .venv-modelgen/bin/activate
pip install -r tools/models/requirements.txt
```

If you keep a single venv at the workspace root, that's fine — every
script in this directory is `cd`-independent and writes into the
repo's `models/` directory by absolute path.

## Generators

| Script | Output | Used by |
|---|---|---|
| `gen_yolo26n.py` | `models/yolo26n_{512x288,1024x576,1536x864,2048x1152}.onnx` (~10 MB each) | M1 closed-vocab detector (`YoloOrtDetector`); ships the native 16:9 ladder (M_NATIVE_ASPECT, exact 16:9 &cap; stride-32). `--all-static` is the release path. |
| `gen_yolo_world.py` | `models/yolo_world_v2_s_{512x288,1024x576,1536x864,2048x1152}.onnx` (~50&nbsp;MB each) | M3 open-vocab detector (`YoloWorldDetector`). Embeds the text encoder into the graph and bakes the operator-supplied prompt vocabulary as fixed text inputs. Ships the full native 16:9 ladder; `--all-static` is the release path. |
| `gen_yoloe.py` | `models/yoloe26_s_{512x288,1024x576,1536x864,2048x1152}.onnx` (~42&nbsp;MB each) | M3.1 text-mode YOLOE detector (`YoloeDetector`). Mirrors `gen_yolo_world.py` against the upstream `ultralytics.YOLOE` checkpoint; native 16:9 ladder, `--all-static`. |
| `gen_yoloe_visual.py` | `models/yoloe26_s_image_encoder.onnx` (~15–20 MB) | M3.1 visual-prompt encoder for `YoloeVisualDetector`. Run AFTER `gen_yoloe.py`; produces the standalone image-embedding ONNX the engine's admin upload path uses to encode reference crops. |
| `gen_yolo26n_hailo.py` | `models/yolo26n_{512x288,1024x576,1536x864,2048x1152}_hailo.hef` (~6&ndash;20&nbsp;MB each) | M_HAILO_EP + M_NATIVE_ASPECT — Hailo-8 quantized build of yolo26n. No rectangular shape has a public Hailo Model Zoo prebuild, so **every** shape is a local DFC compile from the per-shape ONNX. |
| `gen_yolo_world_hailo.py` | `models/yolo_world_v2_s_{512x288,1024x576,1536x864,2048x1152}_hailo.hef` | M_HAILO_EP — Hailo-8 quantized open-vocab. Prompts must already be baked into the ONNX (see `gen_yolo_world.py`); changing the vocab requires re-running both generators. |
| `gen_yoloe_hailo.py` | `models/yoloe26_s_{512x288,1024x576,1536x864,2048x1152}_hailo.hef` | M_HAILO_EP — Hailo-8 quantized YOLOE. Same prompt-baking caveat as YOLO-World. |
| `gen_dinov2_hailo.py` | `models/dinov2_s_224_hailo.hef` (~25&nbsp;MB) | M_HAILO_EP \u2014 Hailo-8 quantized appearance backbone. Calibrated on ImageNet-val. Re-tune the cloud linker's `COSINE_MAX` if switching the appearance pipeline to this HEF in production (int8 quant may shift cosine distances by <1%). |

Run them from the repo root with the venv active:

```bash
python tools/models/gen_yolo26n.py
python tools/models/gen_yolo_world.py --prompts models/yolo_world_default_prompts.txt
python tools/models/gen_yoloe.py --prompts tools/models/yoloe_default_prompts.txt
python tools/models/gen_yoloe_visual.py
```

Both scripts:

* are idempotent — re-running with the same args produces the same
  artifact (modulo any non-deterministic export choice in ultralytics
  itself, which we slim away with `onnxslim`),
* refresh `models/models-manifest.json` with the new sha256 so the
  engine's manifest loader (W-DETECT D5) catches a stale download,
* exit non-zero on any error so CI can wire them up later (NOT in M3
  scope — the artifacts stay gitignored and operator-built, but the
  exit-code contract is in place).

## Why a separate venv?

* The runtime engine doesn't ship Python.
* The model-gen deps (torch ~2 GB, ultralytics, onnxslim) are
  developer-only and would dominate any prod image.
* Pinning Python 3.11 is the only way to get torch CPU wheels on
  macOS Apple Silicon today; Homebrew defaults to Python 3.14 which
  has no torch wheels.

## Hailo HEF compilation (M_HAILO_EP)

The `gen_*_hailo.py` generators produce Hailo-8 HEF (Hailo Executable
Format) artifacts the engine's Hailo backend (`nexus-hailo-backend`)
loads via the per-size dispatcher in
`crates/nexus-inference/src/yolo.rs::resolve_hailo_hef`. HEFs are
**operator-built artifacts** &mdash; the engine ships without them, and
hosts that don't have a Hailo-8 chip silently fall back to the ONNX
path.

### Toolchain prerequisites

| Tool | Purpose | Where to get it |
|---|---|---|
| Hailo Dataflow Compiler (DFC) | Quantize + compile ONNX &rarr; HEF. Linux x86_64 only (Ubuntu 22.04 / 24.04 LTS verified). ~5 GB install. | Free [Hailo Dev Zone account](https://hailo.ai/developer-zone/) &rarr; "Hailo AI Software Suite" / Dataflow Compiler wheel (v3.34.0 verified for the native pack; installs on Python 3.12). The `hailo_sdk_client` Python module is the entry point our scripts call. |
| `hailomz` CLI (optional) | Hailo Model Zoo wrapper with prebuilt recipes for yolo26n. | Installed alongside DFC. |
| Calibration data | int8 PTQ needs ~1024 representative samples. | COCO val2017 auto-downloaded by `gen_hailo_common.py::ensure_calibration_set`; ImageNet-val operator-provisioned at `$NEXUS_IMAGENET_VAL_DIR` (or `~/datasets/imagenet/val/`). |

**The Hailo DFC is NOT redistributable** &mdash; it requires accepting
Hailo's EULA per developer. Treat its install the same way we treat
`yolo26n.pt` (the upstream ultralytics weight): operator-only.

### Per-model compile commands

Activate the same venv used for ONNX generation, then:

```bash
# Native 16:9 ladder — no public Model Zoo prebuild at any rectangular shape,
# so every shape DFC-compiles from its per-shape ONNX. --all does 4 shapes.
python tools/models/gen_yolo26n_hailo.py --all

# yolo_world: all shapes DFC-compile from the pre-baked ONNX. Re-run
# gen_yolo_world.py first if the vocab changed.
python tools/models/gen_yolo_world_hailo.py --all

# yoloe: all shapes DFC-compile. Same prompt-baking constraint as yolo_world.
python tools/models/gen_yoloe_hailo.py --all

# dinov2: 224 ViT. Uses ImageNet-val calibration.
python tools/models/gen_dinov2_hailo.py
```

Each script:

* refreshes `models/models-manifest.json` in-place: replaces the
  `PENDING_COMPILE` sentinel sha256 with the real one and drops the
  `requires_compile: true` flag,
* leaves the `_compile` metadata block intact so the recipe is
  reproducible,
* exits non-zero on any DFC error so a partial pack never gets
  uploaded.

### Per-pack release flow

After all required HEFs are built:

```bash
# Upload to the matching models-vN GitHub release.
gh release upload models-v5 \
  models/*_hailo.hef \
  models/models-manifest.json \
  --clobber
```

The engine's `release.yml` workflow tolerates missing HEFs &mdash; the
required-files check covers ONNX only &mdash; so a partial roll-out
(say, only yolo26n HEFs uploaded while the rest stay PENDING) ships a
working pack for Hailo hosts running detector-only workloads.

### Accuracy / throughput notes

* **`compiler_optimization_level`**: the shipped `.alls` files pin
  `compiler_optimization_level=max`. That runs an exhaustive multi-context
  placement search that can STALL indefinitely on a modest (e.g. 4-core)
  compile host — if "Finding the best partition to contexts..." hangs for
  many minutes, lower it to `1` (or `0`); the allocation then finishes in
  seconds. Per M_NATIVE_ASPECT D2 fps does not gate the pack, so a lower
  level is acceptable. Also stop any local `nexus-engine` first (`sudo
  systemctl stop nexus-engine`) — it can hog ~4 GB and starve the DFC into
  swap-thrash. The v5 pack was compiled at `optimization_level=2` (QAT) +
  `compiler_optimization_level=1`.
* **native 16:9 ladder** (512x288 / 1024x576 / 1536x864 / 2048x1152): all
  local DFC builds — no public Model Zoo prebuild exists at any rectangular
  shape. Most shapes exceed one Hailo-8 context and compile Multi-Context
  ("Single context flow failed: Recoverable" is normal). Per-shape fps on a
  real Hailo-8 board is TBD — fill in `M_NATIVE_ASPECT.md` "Phase 0 results"
  after `hailortcli run` on the card box.
* **yolo_world / yoloe**: open-vocab heads with ~44-class baked
  output. Throughput tracks yolo26n at the same shape.
* **dinov2-s 224 ViT**: partial Hailo-8 op coverage on transformers
  means some attention layers may fall back to CPU inside the chip
  preroll. Validate end-to-end with
  `nexus-hailo-probe --hef dinov2_s_224_hailo.hef` after compile.

### Why this is operator-only

* DFC EULA prevents redistribution. We can't bake it into CI.
* Compile is CPU-bound and slow (~5 min for the smallest shape to ~2 h
  for the biggest at `optimization_level=2` QAT); not
  GitHub-Actions-friendly even if the binary were redistributable.
* HEFs are tied to a specific DFC version; bumping requires a
  re-compile pass anyway. Pinning to operator-run keeps the release
  tooling simple.

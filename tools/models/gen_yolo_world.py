#!/usr/bin/env python3
"""
Generate the open-vocab YOLO-World ONNX used by M3's `YoloWorldDetector`.

This is the M3 counterpart of `gen_yolo26n.py`. Two differences from the
closed-vocab head:

* **Prompts are wired into the graph at export time.** YOLO-World takes
  a list of class prompts and bakes them in as fixed text embeddings,
  producing a YOLOv8-style detector with one class per prompt.
  Per-camera config picks a *subset* of those prompts at runtime; the
  Rust detector filters detections to that subset before emitting them.
* **Multi-size static exports (as of v0.1.22).** Mirrors `gen_yolo26n.py`
  — ships per-size static-shape ONNXs `yolo_world_v2_s_640.onnx`
  (640 default) and `yolo_world_v2_s_960.onnx` (960 default).
  Static shapes are mandatory for the Intel NPU plugin (which silently
  falls back to CPU on dynamic-shape models, observed on Lunar Lake k13
  under v0.1.18–v0.1.20) and let the OpenVINO blob cache hit on
  subsequent boots. The legacy unsuffixed `yolo_world_v2_s.onnx` (640
  only) is no longer produced; the engine resolver keeps one cycle of
  back-compat for pre-v0.1.22 packs already staged on disk.

  1280 is intentionally NOT shipped — open-vocab YOLO-World is used
  for coarse semantic queries ("forklift", "person wearing hardhat")
  where 1280 ~7×s the inference cost without commensurate accuracy.
  The 1280 use case (plate / face) is on the YOLO26n closed-vocab
  head, not here.

Run from the workspace root with the model-gen venv active:

    source .venv-modelgen/bin/activate
    # Generate all static-size variants in one ultralytics session
    # (saves the import + checkpoint-load overhead vs. N invocations):
    python tools/models/gen_yolo_world.py --all-static
    # …or one at a time:
    python tools/models/gen_yolo_world.py --imgsz 640
    python tools/models/gen_yolo_world.py --imgsz 960

Output:
    models/yolo_world_v2_s_640.onnx   (~50–80 MB)
    models/yolo_world_v2_s_960.onnx   (~50–80 MB)

Weights file size is constant w.r.t. input shape — only the activation
tensors grow at runtime — so the 960 variant adds ~50–80 MB to the
release tarball, not 9× that.

The prompt file lives under `tools/models/` (tracked) so the prompt
vocabulary is reproducible; the ONNX itself stays under `models/`
(gitignored) per the same policy as `yolo26n_{640,960,1280}.onnx`.

Note: this script does NOT ship size variants of `yoloe26_s.onnx`. The
upstream YOLOE export pathway is being reworked in M3.4 (visual prompts
via prompt-sets, static-export rework) and will regenerate the YOLOE
ONNX per prompt-set anyway. Adding a parallel multi-size loop here
would be thrown away in M3.4 Phase B — size variance moves into
`build_prompt_set.py` instead.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
from pathlib import Path
from typing import List

REPO_ROOT = Path(__file__).resolve().parents[2]
MODELS_DIR = REPO_ROOT / "models"
STATIC_SIZES = (640, 960)
LEGACY_OUTPUT = MODELS_DIR / "yolo_world_v2_s.onnx"  # pre-v0.1.22, no longer produced by default
DEFAULT_PROMPTS = Path(__file__).resolve().parent / "yolo_world_default_prompts.txt"
DEFAULT_BASE_MODEL = "yolov8s-worldv2.pt"


def static_output_for(imgsz: int) -> Path:
    """Where the static-mode export writes the per-size ONNX."""

    return MODELS_DIR / f"yolo_world_v2_s_{imgsz}.onnx"


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def read_prompts(path: Path) -> List[str]:
    """Read a one-prompt-per-line text file, skipping blanks + `#` comments."""

    out: List[str] = []
    seen: set[str] = set()
    for raw in path.read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line in seen:
            continue
        seen.add(line)
        out.append(line)
    if not out:
        raise ValueError(f"prompt file {path} produced no usable entries")
    return out


def upsert_manifest_entry(
    *,
    model_id: str,
    sized_artifacts: List[dict],
    prompts: List[str],
    default_preset: str,
) -> None:
    """Insert/update a YOLO-World entry in `models/models-manifest.json`.

    The manifest format is the v2 schema documented under
    nexus-edge-ai-core/models/MODEL_PACK_V2.md. As of v0.1.22 we mirror
    the yolo26n pattern of one entry per model id with multiple
    `artifacts[]` (one per preset/size) and matching `presets[]`. The
    engine resolver picks a file by `(model_id, input_width)` rather
    than reading this manifest — so the artifact list here is
    informational / forward-compatible with a future Rust
    ModelRegistry port.

    `sized_artifacts` is a list of `{"imgsz": int, "path": str, "sha": str}`.
    """

    manifest_path = MODELS_DIR / "models-manifest.json"
    if manifest_path.exists():
        manifest = json.loads(manifest_path.read_text())
    else:
        manifest = {"version": "v2", "models": []}
    if manifest.get("version") != "v2":
        raise SystemExit(
            f"manifest at {manifest_path} is not v2 — refuse to clobber"
        )
    models = manifest.setdefault("models", [])

    # Sort by imgsz ascending so the manifest reads naturally and diffs
    # cleanly across runs.
    sized_artifacts = sorted(sized_artifacts, key=lambda a: a["imgsz"])
    default_imgsz = next(
        (a["imgsz"] for a in sized_artifacts if str(a["imgsz"]) == default_preset),
        sized_artifacts[0]["imgsz"],
    )

    entry = {
        "id": model_id,
        "task": "detect_open_vocab_text",
        "_comment": (
            "YOLO-World v2 (small) export. As of v0.1.22 the engine ships "
            "per-size STATIC-shape ONNXs — yolo_world_v2_s_640.onnx "
            "(640 default) and yolo_world_v2_s_960.onnx (960 "
            "default). Static shapes are mandatory for the Intel NPU plugin "
            "(silent CPU fallback on dynamic shapes, observed on Lunar Lake "
            "k13 under v0.1.18–0.1.20) and let the OpenVINO blob cache hit "
            "on subsequent boots. Prompts are baked into the graph at export "
            "time; per-camera config picks a subset at runtime. Regenerate "
            "via `python tools/models/gen_yolo_world.py --all-static` "
            "whenever the prompt vocabulary changes — the manifest sha256 "
            "values below will refresh and the engine's loader will catch "
            "the diff. 1280 is intentionally NOT shipped (overkill for the "
            "coarse semantic queries open-vocab is used for); the YOLOE "
            "multi-size export is deferred to M3.4 (prompt-set rework)."
        ),
        "input": {
            "width": default_imgsz,
            "height": default_imgsz,
            "channels": 3,
            "format": "RGB",
        },
        "default_thresholds": {
            "confidence": 0.10,  # YOLO-World logits run lower than v8/yolo26.
            "nms": 0.50,
        },
        "artifacts": [
            {
                "backend": "onnx",
                "path": a["path"],
                "preset": str(a["imgsz"]),
                "sha256": a["sha"],
            }
            for a in sized_artifacts
        ],
        "presets": [
            {
                "name": str(a["imgsz"]),
                "inputWidth": a["imgsz"],
                "inputHeight": a["imgsz"],
                "artifact": a["path"],
            }
            for a in sized_artifacts
        ],
        "default_preset": default_preset,
        # Forward-compatible: this block is YOLO-World specific. The v1 v2
        # loader ignores unknown top-level fields; the new Rust loader will
        # parse this block to seed `OpenVocabConfig.prompts`.
        "prompts": prompts,
    }

    for i, m in enumerate(models):
        if m.get("id") == model_id:
            models[i] = entry
            break
    else:
        models.append(entry)

    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    sizes = ", ".join(str(a["imgsz"]) for a in sized_artifacts)
    print(
        f"[gen_yolo_world] manifest upserted: {model_id} "
        f"({len(prompts)} prompts, sizes [{sizes}], default {default_preset})"
    )


def export_yolo_world(
    base_model: str,
    prompts: List[str],
    imgsz: int,
    opset: int,
    output: Path,
) -> None:
    """Run the ultralytics export with the prompt vocab baked in."""

    from ultralytics import YOLO  # type: ignore

    print(f"[gen_yolo_world] loading base model: {base_model}")
    model = YOLO(base_model)
    print(f"[gen_yolo_world] setting {len(prompts)} prompt classes")
    # `set_classes` is the ultralytics-supported way to bake an open-vocab
    # vocabulary into a YOLO-World checkpoint before export. After this
    # call the model behaves like a closed-vocab YOLO-v8 with C = len(prompts).
    model.set_classes(prompts)

    print(f"[gen_yolo_world] exporting ONNX (imgsz={imgsz}, opset={opset})")
    model.export(
        format="onnx",
        dynamic=False,  # Static for predictability; per-camera always
        # uses the same input dims for the open-vocab head.
        opset=opset,
        imgsz=imgsz,
        simplify=True,
        nms=False,  # keep raw YOLOv8 head — Rust postprocess does NMS.
    )

    # ultralytics writes `<base_stem>.onnx` next to the input checkpoint
    # (yes — *next to the .pt*, not in cwd). Sweep both spots.
    base_stem = Path(base_model).stem
    base_dir = Path(base_model).resolve().parent
    candidates = [
        base_dir / f"{base_stem}.onnx",
        Path.cwd() / f"{base_stem}.onnx",
        REPO_ROOT / f"{base_stem}.onnx",
        MODELS_DIR / f"{base_stem}.onnx",
    ]
    src = next((p for p in candidates if p.is_file()), None)
    if src is None:
        raise SystemExit(
            f"export succeeded but ONNX not found in any of: {candidates}"
        )
    output.parent.mkdir(parents=True, exist_ok=True)
    if src.resolve() != output.resolve():
        shutil.copy2(src, output)
        try:
            os.unlink(src)
        except OSError:
            pass
        print(f"[gen_yolo_world] copied {src} → {output}")


def smoke_check(onnx_path: Path, input_w: int, input_h: int) -> None:
    """Load the exported ONNX in onnxruntime and run a single zero tensor.

    Catches export-time bugs (missing op, shape mismatch) inside the
    generator so a bad artifact never lands on disk silently.
    """

    import numpy as np  # noqa: WPS433
    import onnxruntime as ort  # noqa: WPS433

    print(f"[gen_yolo_world] smoke loading {onnx_path}")
    sess = ort.InferenceSession(
        str(onnx_path), providers=["CPUExecutionProvider"]
    )
    in_name = sess.get_inputs()[0].name
    in_shape = sess.get_inputs()[0].shape
    print(f"[gen_yolo_world] input '{in_name}' shape={in_shape}")
    dummy = np.zeros((1, 3, input_h, input_w), dtype=np.float32)
    outputs = sess.run(None, {in_name: dummy})
    print(
        f"[gen_yolo_world] smoke ok — got {len(outputs)} output(s); "
        f"first shape={outputs[0].shape}"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description="Export YOLO-World ONNX")
    parser.add_argument(
        "--base-model",
        type=str,
        default=DEFAULT_BASE_MODEL,
        help=(
            "Ultralytics YOLO-World checkpoint to start from. "
            "Defaults to yolov8s-worldv2.pt (small model — ~50 MB ONNX). "
            "Use yolov8m-worldv2.pt or larger for higher accuracy."
        ),
    )
    parser.add_argument(
        "--prompts",
        type=Path,
        default=DEFAULT_PROMPTS,
        help=f"Prompt file (default: {DEFAULT_PROMPTS.relative_to(REPO_ROOT)})",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help=(
            "Override the output ONNX path. Defaults: per-size mode → "
            "models/yolo_world_v2_s_<imgsz>.onnx. Ignored under --all-static."
        ),
    )
    parser.add_argument("--imgsz", type=int, default=640)
    parser.add_argument(
        "--opset",
        type=int,
        default=17,
        help=(
            "ONNX opset. 17 = YOLO-World requires the gather + matmul "
            "ops the text-embedding fusion uses; 12 (the closed-vocab "
            "default) won't export."
        ),
    )
    parser.add_argument(
        "--manifest-id",
        type=str,
        default="yolo_world_v2_s",
        help="ID written into models-manifest.json",
    )
    parser.add_argument(
        "--skip-smoke",
        action="store_true",
        help="Skip the onnxruntime smoke load (use only when debugging exports).",
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--all-static",
        action="store_true",
        help=(
            f"Generate all static-shape ONNXs in one ultralytics session: "
            f"{', '.join(str(s) for s in STATIC_SIZES)}. Saves the import + "
            f"checkpoint load + set_classes overhead vs. N separate "
            f"invocations. This is the release-pipeline path."
        ),
    )
    parser.add_argument(
        "--default-preset",
        type=str,
        default="640",
        help=(
            "Which preset name to write as `default_preset` in the manifest. "
            "Defaults to 640 (the iGPU/Hailo default)."
        ),
    )
    args = parser.parse_args()

    # Default mode if neither --all-static nor --imgsz with --output was
    # explicitly mode-selected: --all-static. Matches what the release
    # workflow expects to find uploaded against a tag.
    explicit_single = (
        any(arg.startswith("--imgsz") for arg in sys.argv[1:])
        or args.output is not None
    )
    if not args.all_static and not explicit_single:
        args.all_static = True

    try:
        prompts = read_prompts(args.prompts)
    except (FileNotFoundError, ValueError) as ex:
        print(f"[gen_yolo_world] ERROR reading prompts: {ex}")
        return 1

    print(f"[gen_yolo_world] prompts ({len(prompts)}): {prompts}")

    if args.all_static:
        sizes = list(STATIC_SIZES)
    else:
        sizes = [args.imgsz]

    sized_artifacts: List[dict] = []
    for sz in sizes:
        output = (
            args.output
            if (args.output is not None and not args.all_static)
            else static_output_for(sz)
        )
        try:
            export_yolo_world(
                base_model=args.base_model,
                prompts=prompts,
                imgsz=sz,
                opset=args.opset,
                output=output,
            )
        except Exception as ex:  # noqa: BLE001
            print(f"[gen_yolo_world] ERROR exporting imgsz={sz}: {ex}")
            return 1

        if not args.skip_smoke:
            try:
                smoke_check(output, sz, sz)
            except Exception as ex:  # noqa: BLE001
                print(f"[gen_yolo_world] ERROR smoke-loading {output}: {ex}")
                return 1

        sha = sha256_file(output)
        size_mb = output.stat().st_size / (1024 * 1024)
        print(f"[gen_yolo_world] imgsz={sz} sha256 {sha}  size {size_mb:.2f} MB")
        sized_artifacts.append({"imgsz": sz, "path": output.name, "sha": sha})

    upsert_manifest_entry(
        model_id=args.manifest_id,
        sized_artifacts=sized_artifacts,
        prompts=prompts,
        default_preset=args.default_preset,
    )
    for a in sized_artifacts:
        print(f"[gen_yolo_world] success: models/{a['path']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

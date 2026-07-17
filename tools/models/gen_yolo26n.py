#!/usr/bin/env python3
"""
Generate the closed-vocab YOLOv26-nano ONNX detectors that power M1's
`YoloOrtDetector`. As of v0.1.19 the engine ships three STATIC-shape
exports — `yolo26n_640.onnx`, `yolo26n_960.onnx`, `yolo26n_1280.onnx` —
one per supported input size (matched to per-profile defaults and the
per-camera size override). Static shapes are mandatory for the Intel
NPU plugin (which silently falls back to CPU on dynamic-shape models)
and let the OpenVINO blob cache hit on every subsequent boot. The
older `yolo26n_dynamic.onnx` is deprecated; the dynamic export mode
is preserved here only for niche dev workflows.

Run from the workspace root with the model-gen venv active:

    source .venv-modelgen/bin/activate
    # Generate all three static models in one ultralytics session
    # (saves ~30s of import + checkpoint-load overhead vs. 3 invocations):
    python tools/models/gen_yolo26n.py --all-static
    # …or one at a time:
    python tools/models/gen_yolo26n.py --static --imgsz 640
    python tools/models/gen_yolo26n.py --static --imgsz 960
    python tools/models/gen_yolo26n.py --static --imgsz 1280

Each invocation patches the matching `artifacts[].sha256` entry in
`models/models-manifest.json` so the engine's load-time checksum
verification (when wired) sees fresh values.

Outputs:
    models/yolo26n_640.onnx   (1×3×640×640, 1ch image input → output0 [1,300,6])
    models/yolo26n_960.onnx
    models/yolo26n_1280.onnx
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
MODELS_DIR = REPO_ROOT / "models"
DYNAMIC_OUTPUT = MODELS_DIR / "yolo26n_dynamic.onnx"
STATIC_SIZES = (640, 960, 1280)


def static_output_for(imgsz: int) -> Path:
    """Where the static-mode export writes the per-size ONNX."""

    return MODELS_DIR / f"yolo26n_{imgsz}.onnx"


def static_artifact_path(imgsz: int) -> str:
    """The `path` field in `models-manifest.json` for this size."""

    return f"yolo26n_{imgsz}.onnx"


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def update_manifest_sha(model_id: str, artifact_path: str, new_sha: str) -> None:
    """Patch the on-disk sha256 for one artifact in `models-manifest.json`.

    Idempotent — leaves the file untouched (and the file's mtime intact)
    if the sha already matches what we just computed.
    """

    manifest_path = MODELS_DIR / "models-manifest.json"
    if not manifest_path.exists():
        print(f"[gen_yolo26n] no manifest at {manifest_path}, skipping sha update")
        return
    manifest = json.loads(manifest_path.read_text())
    for model in manifest.get("models", []):
        if model.get("id") != model_id:
            continue
        for art in model.get("artifacts", []):
            if art.get("path") != artifact_path:
                continue
            if art.get("sha256") == new_sha:
                print(f"[gen_yolo26n] manifest sha already current ({new_sha[:12]}…)")
                return
            art["sha256"] = new_sha
            manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
            print(f"[gen_yolo26n] manifest sha updated → {new_sha[:12]}…")
            return
    print(f"[gen_yolo26n] no manifest entry matched (id={model_id} path={artifact_path})")


def main() -> int:
    parser = argparse.ArgumentParser(description="Export yolo26n ONNX (static or dynamic)")
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help=(
            "Override the output ONNX path. Defaults: static → "
            "models/yolo26n_<imgsz>.onnx; dynamic → models/yolo26n_dynamic.onnx."
        ),
    )
    parser.add_argument(
        "--imgsz",
        type=int,
        default=640,
        help="Input size in pixels (default 640). Static mode pins to this; "
        "dynamic mode uses it only as the anchor for the dynamic axes.",
    )
    parser.add_argument(
        "--opset",
        type=int,
        default=12,
        help="ONNX opset version (default 12, matches the v1 ORT 1.18 pin).",
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--static",
        action="store_true",
        help="Export a static-shape ONNX (1x3x<imgsz>x<imgsz>). Required for "
        "the Intel NPU plugin and for OpenVINO blob caching to hit.",
    )
    mode.add_argument(
        "--all-static",
        action="store_true",
        help=f"Generate all three static-shape ONNXs in one ultralytics session: "
        f"{', '.join(str(s) for s in STATIC_SIZES)}. Saves the import + checkpoint "
        f"load overhead vs. three separate invocations.",
    )
    mode.add_argument(
        "--dynamic",
        action="store_true",
        help="Export the legacy dynamic-axis ONNX (yolo26n_dynamic.onnx). "
        "Kept for niche dev workflows — production releases ship the three static models.",
    )
    args = parser.parse_args()

    # Default mode if none requested: --all-static. Matches what the release
    # workflow expects to find uploaded against a tag.
    if not (args.static or args.all_static or args.dynamic):
        args.all_static = True

    try:
        from ultralytics import YOLO  # type: ignore
    except ImportError:
        print("[gen_yolo26n] ultralytics not installed.")
        print("[gen_yolo26n]   pip install -r tools/models/requirements.txt")
        return 1

    print("[gen_yolo26n] loading YOLO26N checkpoint")
    try:
        model = YOLO("yolo26n.pt")
    except Exception as ex:  # noqa: BLE001
        print(f"[gen_yolo26n] ERROR: checkpoint load failed: {ex}")
        return 1

    sizes: list[int]
    if args.all_static:
        sizes = list(STATIC_SIZES)
    elif args.static:
        sizes = [args.imgsz]
    else:
        sizes = []  # dynamic path below

    for sz in sizes:
        output = args.output if (args.output and not args.all_static) else static_output_for(sz)
        rc = export_one(
            model,
            imgsz=sz,
            opset=args.opset,
            dynamic=False,
            output=output,
            manifest_artifact=static_artifact_path(sz),
        )
        if rc != 0:
            return rc

    if args.dynamic:
        output = args.output or DYNAMIC_OUTPUT
        rc = export_one(
            model,
            imgsz=args.imgsz,
            opset=args.opset,
            dynamic=True,
            output=output,
            manifest_artifact="yolo26n_dynamic.onnx",
        )
        if rc != 0:
            return rc

    return 0


def export_one(
    model,
    *,
    imgsz: int,
    opset: int,
    dynamic: bool,
    output: Path,
    manifest_artifact: str,
) -> int:
    """Run one ultralytics export → copy to `output` → patch manifest sha."""

    output.parent.mkdir(parents=True, exist_ok=True)
    shape = f"1x3x{imgsz}x{imgsz}" if not dynamic else f"Bx3xHxW (anchor {imgsz})"
    print(f"[gen_yolo26n] exporting ({shape}, opset={opset}) → {output}")

    try:
        model.export(
            format="onnx",
            dynamic=dynamic,
            opset=opset,
            imgsz=imgsz,
        )
    except Exception as ex:  # noqa: BLE001
        print(f"[gen_yolo26n] ERROR: export failed: {ex}")
        return 1

    # ultralytics writes `yolo26n.onnx` to the current working directory
    # (or next to the source .pt). Sweep both spots.
    candidates = [
        Path.cwd() / "yolo26n.onnx",
        REPO_ROOT / "yolo26n.onnx",
        MODELS_DIR / "yolo26n.onnx",
    ]
    src = next((p for p in candidates if p.is_file()), None)
    if src is None:
        print(f"[gen_yolo26n] ERROR: exported file not found in {candidates}")
        return 1
    if src.resolve() != output.resolve():
        shutil.copy2(src, output)
        try:
            os.unlink(src)
        except OSError:
            pass
        print(f"[gen_yolo26n] copied {src} → {output}")

    sha = sha256_file(output)
    print(f"[gen_yolo26n] sha256 {sha}")
    print(f"[gen_yolo26n] size   {output.stat().st_size / (1024 * 1024):.2f} MB")
    update_manifest_sha("yolo26n", manifest_artifact, sha)
    print(f"[gen_yolo26n] success: {output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

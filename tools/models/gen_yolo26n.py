#!/usr/bin/env python3
"""
Generate the closed-vocab YOLOv26-nano ONNX detectors that power M1's
`YoloOrtDetector`. The engine ships STATIC-shape exports on the native
16:9 ladder — `yolo26n_512x288.onnx`, `yolo26n_1024x576.onnx`,
`yolo26n_1536x864.onnx`, `yolo26n_2048x1152.onnx` — one per supported
input shape (matched to per-profile defaults and the per-camera shape
override). Every shape is exact 16:9 with both dimensions multiples of
32 (W=512k, H=288k), so the detector no longer stretches the 16:9
supervisor frame into a square tensor. See
`docs/edge-core/M_NATIVE_ASPECT.md` in the cloud-console repo.

Static shapes are mandatory for the Intel NPU plugin (which silently
falls back to CPU on dynamic-shape models) and let the OpenVINO blob
cache hit on every subsequent boot. The legacy `yolo26n_dynamic.onnx`
dynamic export is retired.

Run from the workspace root with the model-gen venv active:

    source .venv-modelgen/bin/activate
    # Generate all four static models in one ultralytics session
    # (saves ~30s of import + checkpoint-load overhead vs. 4 invocations):
    python tools/models/gen_yolo26n.py --all-static
    # …or one at a time:
    python tools/models/gen_yolo26n.py --static --shape 512x288
    python tools/models/gen_yolo26n.py --static --shape 1024x576
    python tools/models/gen_yolo26n.py --static --shape 2048x1152

Each invocation patches the matching `artifacts[].sha256` entry in
`models/models-manifest.json` so the engine's load-time checksum
verification (when wired) sees fresh values.

Outputs:
    models/yolo26n_512x288.onnx    (1×3×288×512, image input → output0 [1,300,6])
    models/yolo26n_1024x576.onnx
    models/yolo26n_1536x864.onnx
    models/yolo26n_2048x1152.onnx
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
# Native 16:9 ladder: exact 16:9 ∩ stride-32 (W=512k, H=288k). The only
# family that satisfies both YOLO's multiple-of-32 rule and exact 16:9.
STATIC_SHAPES = ((512, 288), (1024, 576), (1536, 864), (2048, 1152))


def parse_shape(text: str) -> tuple[int, int]:
    """Parse a `WxH` shape string (e.g. "512x288") into `(w, h)`."""

    try:
        w_str, h_str = text.lower().split("x", 1)
        return int(w_str), int(h_str)
    except (ValueError, AttributeError):
        raise argparse.ArgumentTypeError(
            f"invalid --shape {text!r}; expected WxH like 512x288"
        )


def static_output_for(w: int, h: int) -> Path:
    """Where the static-mode export writes the per-shape ONNX."""

    return MODELS_DIR / f"yolo26n_{w}x{h}.onnx"


def static_artifact_path(w: int, h: int) -> str:
    """The `path` field in `models-manifest.json` for this shape."""

    return f"yolo26n_{w}x{h}.onnx"


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
    parser = argparse.ArgumentParser(description="Export yolo26n ONNX (static shapes)")
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help=(
            "Override the output ONNX path. Default: "
            "models/yolo26n_<W>x<H>.onnx."
        ),
    )
    parser.add_argument(
        "--shape",
        type=parse_shape,
        default=(512, 288),
        help="Input shape as WxH (default 512x288). Static mode pins to this. "
        "Must be a native-16:9 ladder rung: 512x288 | 1024x576 | 1536x864 | 2048x1152.",
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
        help="Export a single static-shape ONNX (1x3xHxW). Required for "
        "the Intel NPU plugin and for OpenVINO blob caching to hit.",
    )
    mode.add_argument(
        "--all-static",
        action="store_true",
        help="Generate all four static-shape ONNXs in one ultralytics session: "
        + ", ".join(f"{w}x{h}" for w, h in STATIC_SHAPES)
        + ". Saves the import + checkpoint load overhead vs. four separate invocations.",
    )
    args = parser.parse_args()

    # Default mode if none requested: --all-static. Matches what the release
    # workflow expects to find uploaded against a tag.
    if not (args.static or args.all_static):
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

    shapes: list[tuple[int, int]]
    if args.all_static:
        shapes = list(STATIC_SHAPES)
    else:  # args.static
        shapes = [args.shape]

    for w, h in shapes:
        output = args.output if (args.output and not args.all_static) else static_output_for(w, h)
        rc = export_one(
            model,
            w=w,
            h=h,
            opset=args.opset,
            output=output,
            manifest_artifact=static_artifact_path(w, h),
        )
        if rc != 0:
            return rc

    return 0


def export_one(
    model,
    *,
    w: int,
    h: int,
    opset: int,
    output: Path,
    manifest_artifact: str,
) -> int:
    """Run one ultralytics export → copy to `output` → patch manifest sha."""

    output.parent.mkdir(parents=True, exist_ok=True)
    shape = f"1x3x{h}x{w}"
    print(f"[gen_yolo26n] exporting ({shape}, opset={opset}) → {output}")

    try:
        # ultralytics takes imgsz as [height, width] — height first.
        model.export(
            format="onnx",
            dynamic=False,
            opset=opset,
            imgsz=[h, w],
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

#!/usr/bin/env python3
"""Generate Hailo-8 HEF artifacts for the YOLO-World v2 open-vocab detector.

Two sizes ship: 640 and 960. Source-of-truth ONNX files at each size
MUST already exist under `models/` \u2014 generate them first with:

    python tools/models/gen_yolo_world.py --all-static

YOLO-World bakes the CLIP text encoder's output (one embedding per
prompt) into the ONNX graph as fixed constants at export time. The
runtime ONNX is image-only \u2014 no host-side text encoder needed at
inference. The HEF inherits this property: changing the vocab
requires re-running gen_yolo_world.py (regenerates ONNX) AND
re-running this script (regenerates HEF).

NO public Hailo Model Zoo build exists for YOLO-World; both sizes
require local DFC compilation.

Outputs:
    models/yolo_world_v2_s_640_hailo.hef     (~20 MB)
    models/yolo_world_v2_s_960_hailo.hef     (~40 MB)
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from gen_hailo_common import (
    MODELS_DIR,
    clear_pending_compile,
    compile_with_dfc,
    ensure_calibration_set,
    require_hailo_sdk,
    sha256_file,
)

MODEL_ID = "yolo_world_v2_s"
STATIC_SIZES = (640, 960)


def hef_path_for(size: int) -> Path:
    return MODELS_DIR / f"yolo_world_v2_s_{size}_hailo.hef"


def onnx_path_for(size: int) -> Path:
    return MODELS_DIR / f"yolo_world_v2_s_{size}.onnx"


def build_one(size: int) -> int:
    onnx = onnx_path_for(size)
    if not onnx.exists():
        print(
            f"[gen_yolo_world_hailo] FATAL: {onnx} missing. Run "
            f"`python tools/models/gen_yolo_world.py --all-static` first to "
            f"regenerate the baked-prompt ONNXs."
        )
        return 1
    require_hailo_sdk()

    hef_out = hef_path_for(size)
    calib = ensure_calibration_set("coco-val2017-1024")
    rc = compile_with_dfc(
        onnx,
        hef_out,
        calibration_dir=calib,
        # YOLO-World v2 has a YOLOv8-style anchor-free head with one
        # class per baked prompt (44 classes for the default vocab).
        # We let DFC discover the output nodes automatically; if it
        # picks up the post-NMS path on a future ultralytics version,
        # add explicit end-node names that target the raw box/cls
        # tensors here. The chip emits the RawYolo26 layout
        # nexus-hailo-backend already decodes.
        end_node_names=None,
    )
    if rc != 0:
        return rc

    sha = sha256_file(hef_out)
    print(f"[gen_yolo_world_hailo] sha256 {sha}  ({hef_out.stat().st_size / (1024 * 1024):.2f} MB)")
    clear_pending_compile(MODEL_ID, hef_out.name, sha)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Compile yolo_world_v2_s HEFs for Hailo-8")
    parser.add_argument(
        "--size",
        type=int,
        choices=STATIC_SIZES,
        help="Compile a single size. Omit for --all.",
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help=f"Compile both sizes: {', '.join(str(s) for s in STATIC_SIZES)}",
    )
    args = parser.parse_args()
    if not (args.size or args.all):
        args.all = True

    sizes = [args.size] if args.size else list(STATIC_SIZES)
    for sz in sizes:
        rc = build_one(sz)
        if rc != 0:
            return rc
    return 0


if __name__ == "__main__":
    sys.exit(main())

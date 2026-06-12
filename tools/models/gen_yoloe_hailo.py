#!/usr/bin/env python3
"""Generate Hailo-8 HEF artifact for the YOLOE text-mode detector.

One size ships: 640. Source-of-truth ONNX MUST exist at
`models/yoloe26_s.onnx` \u2014 generate it first with:

    python tools/models/gen_yoloe.py \\
        --prompts tools/models/yoloe_default_prompts.txt

Like YOLO-World, YOLOE bakes the text encoder output into the graph
at export time, so the runtime ONNX is image-only. Re-baking the
vocab requires regenerating both the ONNX and this HEF.

NO public Hailo Model Zoo build exists for YOLOE; local DFC compile
only.

Output:
    models/yoloe26_s_640_hailo.hef     (~18 MB)
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

MODEL_ID = "yoloe26_s"
STATIC_SIZES = (640,)


def hef_path_for(size: int) -> Path:
    return MODELS_DIR / f"yoloe26_s_{size}_hailo.hef"


def build_one(size: int) -> int:
    # Source ONNX is unsized (no per-size variants in gen_yoloe.py); the
    # 640 manifest entry covers the only operating point.
    onnx = MODELS_DIR / "yoloe26_s.onnx"
    if not onnx.exists():
        print(
            f"[gen_yoloe_hailo] FATAL: {onnx} missing. Run "
            f"`python tools/models/gen_yoloe.py --prompts "
            f"tools/models/yoloe_default_prompts.txt` first."
        )
        return 1
    require_hailo_sdk()

    hef_out = hef_path_for(size)
    calib = ensure_calibration_set("coco-val2017-1024")
    rc = compile_with_dfc(
        onnx,
        hef_out,
        calibration_dir=calib,
        # Mirror gen_yolo_world_hailo.py: let DFC auto-discover the
        # raw-head output nodes. nexus-hailo-backend's RawYolo26 decoder
        # handles the multi-output anchor-free layout.
        end_node_names=None,
    )
    if rc != 0:
        return rc

    sha = sha256_file(hef_out)
    print(f"[gen_yoloe_hailo] sha256 {sha}  ({hef_out.stat().st_size / (1024 * 1024):.2f} MB)")
    clear_pending_compile(MODEL_ID, hef_out.name, sha)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Compile yoloe26_s HEF for Hailo-8")
    parser.add_argument(
        "--size",
        type=int,
        choices=STATIC_SIZES,
        default=640,
        help="Operating size (only 640 ships).",
    )
    args = parser.parse_args()
    return build_one(args.size)


if __name__ == "__main__":
    sys.exit(main())

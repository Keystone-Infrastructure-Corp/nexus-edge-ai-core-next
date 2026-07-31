#!/usr/bin/env python3
"""Generate Hailo-8 HEF artifacts for the YOLO-World v2 open-vocab detector.

Three native-16:9 shapes ship: 512x288, 1024x576, 1536x864.
Source-of-truth ONNX files at each shape
MUST already exist under `models/` \u2014 generate them first with:

    python tools/models/gen_yolo_world.py --all-static

YOLO-World bakes the CLIP text encoder's output (one embedding per
prompt) into the ONNX graph as fixed constants at export time. The
runtime ONNX is image-only \u2014 no host-side text encoder needed at
inference. The HEF inherits this property: changing the vocab
requires re-running gen_yolo_world.py (regenerates ONNX) AND
re-running this script (regenerates HEF).

NO public Hailo Model Zoo build exists for YOLO-World; every shape
requires local DFC compilation.

Outputs:
    models/yolo_world_v2_s_512x288_hailo.hef
    models/yolo_world_v2_s_1024x576_hailo.hef
    models/yolo_world_v2_s_1536x864_hailo.hef
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
# Native 16:9 ladder: exact 16:9 ∩ stride-32 (W=512k, H=288k).
STATIC_SHAPES = ((512, 288), (1024, 576), (1536, 864))
ALLS_DIR = Path(__file__).resolve().parent / "alls"


def parse_shape(text: str) -> tuple[int, int]:
    """Parse a `WxH` shape string (e.g. "512x288") into `(w, h)`."""

    try:
        w_str, h_str = text.lower().split("x", 1)
        return int(w_str), int(h_str)
    except (ValueError, AttributeError):
        raise argparse.ArgumentTypeError(
            f"invalid --shape {text!r}; expected WxH like 512x288"
        )


def hef_path_for(w: int, h: int) -> Path:
    return MODELS_DIR / f"yolo_world_v2_s_{w}x{h}_hailo.hef"


def onnx_path_for(w: int, h: int) -> Path:
    return MODELS_DIR / f"yolo_world_v2_s_{w}x{h}.onnx"


def build_one(w: int, h: int) -> int:
    onnx = onnx_path_for(w, h)
    if not onnx.exists():
        print(
            f"[gen_yolo_world_hailo] FATAL: {onnx} missing. Run "
            f"`python tools/models/gen_yolo_world.py --all-static` first to "
            f"regenerate the baked-prompt ONNXs."
        )
        return 1
    require_hailo_sdk()

    hef_out = hef_path_for(w, h)
    calib = ensure_calibration_set("coco-val2017-1024")
    rc = compile_with_dfc(
        onnx,
        hef_out,
        calibration_dir=calib,
        alls_script=ALLS_DIR / "yolo_world_v2_s.alls",
        # YOLO-World v2 head: cv2.X.2/Conv is box pre-DFL (c=64),
        # cv3.X.2/Conv is COCO 80-class pre-sigmoid (cv4/Add Einsum unsupported on DFC; cv3 fallback)
        # baked prompt — 44 for the default vocab). cv3 is the
        # vestigial yolov8 COCO class head; we skip it. Six end
        # nodes → RawYolo26-compatible 6-output layout.
        end_node_names=[
            "/model.22/cv2.0/cv2.0.2/Conv",
            "/model.22/cv2.1/cv2.1.2/Conv",
            "/model.22/cv2.2/cv2.2.2/Conv",
            "/model.22/cv3.0/cv3.0.2/Conv",
            "/model.22/cv3.1/cv3.1.2/Conv",
            "/model.22/cv3.2/cv3.2.2/Conv",
        ],
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
        "--shape",
        type=parse_shape,
        help="Compile a single shape (WxH, e.g. 512x288). Omit for --all.",
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="Compile all four shapes: "
        + ", ".join(f"{w}x{h}" for w, h in STATIC_SHAPES),
    )
    args = parser.parse_args()
    if not (args.shape or args.all):
        args.all = True

    shapes = [args.shape] if args.shape else list(STATIC_SHAPES)
    for w, h in shapes:
        rc = build_one(w, h)
        if rc != 0:
            return rc
    return 0


if __name__ == "__main__":
    sys.exit(main())

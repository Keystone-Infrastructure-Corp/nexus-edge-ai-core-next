#!/usr/bin/env python3
"""Generate Hailo-8 HEF artifact for the YOLOE text-mode detector.

Three native-16:9 shapes ship: 512x288, 1024x576, 1536x864.
Source-of-truth ONNX files MUST exist under `models/` \u2014 generate it first with:

    python tools/models/gen_yoloe.py \\
        --prompts tools/models/yoloe_default_prompts.txt

Like YOLO-World, YOLOE bakes the text encoder output into the graph
at export time, so the runtime ONNX is image-only. Re-baking the
vocab requires regenerating both the ONNX and this HEF.

NO public Hailo Model Zoo build exists for YOLOE; local DFC compile
only.

Output:
    models/yoloe26_s_512x288_hailo.hef
    models/yoloe26_s_1024x576_hailo.hef
    models/yoloe26_s_1536x864_hailo.hef
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
    return MODELS_DIR / f"yoloe26_s_{w}x{h}_hailo.hef"


def onnx_path_for(w: int, h: int) -> Path:
    return MODELS_DIR / f"yoloe26_s_{w}x{h}.onnx"


def build_one(w: int, h: int) -> int:
    onnx = onnx_path_for(w, h)
    if not onnx.exists():
        print(
            f"[gen_yoloe_hailo] FATAL: {onnx} missing. Run "
            f"`python tools/models/gen_yoloe.py --all-static` first to "
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
        alls_script=ALLS_DIR / "yoloe26_s.alls",
        # YOLOE has a YOLOv10-style one2one anchor-free head + segmentation.
        # Cut at the cv2.X.2/Conv (box pre-DFL, c=64) and cv3.X.2/Conv
        # (class pre-sigmoid) leaf convs of each FPN scale so the chip
        # emits the 6-output RawYolo26-compatible layout (skipping the
        # post-NMS TopK / GatherElements ops that DFC can't lower).
        end_node_names=[
            "/model.23/one2one_cv2.0/one2one_cv2.0.2/Conv",
            "/model.23/one2one_cv2.1/one2one_cv2.1.2/Conv",
            "/model.23/one2one_cv2.2/one2one_cv2.2.2/Conv",
            "/model.23/one2one_cv3.0/one2one_cv3.0.2/Conv",
            "/model.23/one2one_cv3.1/one2one_cv3.1.2/Conv",
            "/model.23/one2one_cv3.2/one2one_cv3.2.2/Conv",
        ],
    )
    if rc != 0:
        return rc

    sha = sha256_file(hef_out)
    print(f"[gen_yoloe_hailo] sha256 {sha}  ({hef_out.stat().st_size / (1024 * 1024):.2f} MB)")
    clear_pending_compile(MODEL_ID, hef_out.name, sha)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Compile yoloe26_s HEFs for Hailo-8")
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

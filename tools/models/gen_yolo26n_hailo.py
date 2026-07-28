#!/usr/bin/env python3
"""Generate Hailo-8 HEF artifacts for the closed-vocab yolo26n detector.

Four native-16:9 shapes ship: 512x288, 1024x576, 1536x864, 2048x1152
(exact 16:9 ∩ stride-32, W=512k / H=288k — see
`docs/edge-core/M_NATIVE_ASPECT.md`). Source-of-truth ONNX files at
each shape MUST already exist under `models/` \u2014 generate them first with:

    python tools/models/gen_yolo26n.py --all-static

None of the rectangular shapes have a public Hailo Model Zoo prebuilt
(the Zoo only ships square 640), so every shape is compiled locally
via the Hailo Dataflow Compiler. See `gen_hailo_common.py` for the DFC
bring-up runbook and `tools/models/README.md` \u00a7"Hailo
HEF compilation" for the operator-facing workflow.

Outputs:
    models/yolo26n_512x288_hailo.hef
    models/yolo26n_1024x576_hailo.hef
    models/yolo26n_1536x864_hailo.hef
    models/yolo26n_2048x1152_hailo.hef

Each invocation patches the matching `artifacts[].sha256` entry in
`models/models-manifest.json` and removes the `requires_compile`
flag once the HEF is on disk.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from gen_hailo_common import (
    MODELS_DIR,
    clear_pending_compile,
    compile_with_dfc,
    compile_with_hailomz,
    ensure_calibration_set,
    require_hailo_sdk,
    sha256_file,
)

MODEL_ID = "yolo26n"
# Native 16:9 ladder: exact 16:9 ∩ stride-32 (W=512k, H=288k).
STATIC_SHAPES = ((512, 288), (1024, 576), (1536, 864), (2048, 1152))
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
    return MODELS_DIR / f"yolo26n_{w}x{h}_hailo.hef"


def onnx_path_for(w: int, h: int) -> Path:
    return MODELS_DIR / f"yolo26n_{w}x{h}.onnx"


def build_one(w: int, h: int) -> int:
    hef_out = hef_path_for(w, h)

    # No rectangular shape has a public Model Zoo prebuilt — compile every
    # shape from the local ONNX via the DFC.
    onnx = onnx_path_for(w, h)
    if not onnx.exists():
        print(
            f"[gen_yolo26n_hailo] FATAL: {onnx} missing. Run "
            f"`python tools/models/gen_yolo26n.py --static --shape {w}x{h}` first."
        )
        return 1
    require_hailo_sdk()

    # Prefer `hailomz compile yolo26n` if the upstream Model Zoo recipe
    # is installed. It carries the .alls quant config + the correct
    # end-node names for the multi-output (no on-chip NMS) head that
    # nexus-hailo-backend's RawYolo26 decoder expects. The upstream recipe
    # is square-640; we pass `--resolution WxH` if hailomz supports it,
    # otherwise fall through to the DFC path below.
    recipe_rc = compile_with_hailomz(
        "yolo26n",
        hef_out,
        extra_args=[
            "--ckpt",
            str(onnx),
            "--resolution",
            f"{w}x{h}",
            "--output-name",
            hef_out.name,
        ],
    )
    if recipe_rc == 0:
        print(f"[gen_yolo26n_hailo] hailomz compile succeeded for {w}x{h}")
    else:
        if recipe_rc == 127:
            print(
                "[gen_yolo26n_hailo] hailomz CLI not on PATH; falling back to DFC SDK"
            )
        else:
            print(
                f"[gen_yolo26n_hailo] hailomz exited {recipe_rc}; falling back to DFC SDK"
            )
        calib = ensure_calibration_set("coco-val2017-1024")
        rc = compile_with_dfc(
            onnx,
            hef_out,
            calibration_dir=calib,
            alls_script=ALLS_DIR / "yolo26n.alls",
            # Six end nodes mirror hailo_model_zoo's official yolo26.yaml
            # parser config: cv2.X.2/Conv (box pre-DFL, c=64) and
            # cv3.X.2/Conv (class pre-sigmoid, c=80) leaf convs at each
            # FPN scale. Chip post-processes DFL into c=4 boxes and emits
            # 6 raw tensors matching the public yolo26n_640.hef layout
            # that nexus-hailo-backend's RawYolo26 decoder consumes.
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

    if not hef_out.exists():
        print(f"[gen_yolo26n_hailo] FATAL: {hef_out} not produced")
        return 1
    sha = sha256_file(hef_out)
    print(f"[gen_yolo26n_hailo] sha256 {sha}  ({hef_out.stat().st_size / (1024 * 1024):.2f} MB)")
    clear_pending_compile(MODEL_ID, hef_out.name, sha)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Compile yolo26n HEFs for Hailo-8")
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

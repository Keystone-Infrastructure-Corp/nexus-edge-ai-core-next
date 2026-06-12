#!/usr/bin/env python3
"""Generate Hailo-8 HEF artifacts for the closed-vocab yolo26n detector.

Three sizes ship: 640, 960, 1280. Source-of-truth ONNX files at each
size MUST already exist under `models/` \u2014 generate them first with:

    python tools/models/gen_yolo26n.py --all-static

The 640 path resolves to a public Hailo Model Zoo prebuilt (v2.18.0
snapshot of the yolo26n COCO weights, MIT + Apache-2.0 + CC-BY-4.0).
The 960 and 1280 paths have no public build and must be compiled
locally via the Hailo Dataflow Compiler. See `gen_hailo_common.py`
for the DFC bring-up runbook and `tools/models/README.md` \u00a7"Hailo
HEF compilation" for the operator-facing workflow.

Outputs:
    models/yolo26n_640_hailo.hef     (public prebuilt, ~7 MB)
    models/yolo26n_960_hailo.hef     (DFC compile, ~14 MB)
    models/yolo26n_1280_hailo.hef    (DFC compile, ~22 MB)

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
    download_public_hef,
    ensure_calibration_set,
    require_hailo_sdk,
    sha256_file,
)

MODEL_ID = "yolo26n"
STATIC_SIZES = (640, 960, 1280)
ALLS_DIR = Path(__file__).resolve().parent / "alls"


def hef_path_for(size: int) -> Path:
    return MODELS_DIR / f"yolo26n_{size}_hailo.hef"


def onnx_path_for(size: int) -> Path:
    return MODELS_DIR / f"yolo26n_{size}.onnx"


def build_one(size: int) -> int:
    hef_out = hef_path_for(size)

    if size == 640:
        # The public Model Zoo file is the bytes we already validated against
        # `nexus-hailo-backend`. Don't re-compile it locally even if DFC is
        # available \u2014 a fresh compile would diverge from the sha already
        # pinned in the engine release builds.
        rc = download_public_hef("yolo26n.hef", hef_out)
        if rc != 0:
            return rc
    else:
        # 960 / 1280: no public build. Compile from the local ONNX.
        onnx = onnx_path_for(size)
        if not onnx.exists():
            print(
                f"[gen_yolo26n_hailo] FATAL: {onnx} missing. Run "
                f"`python tools/models/gen_yolo26n.py --static --imgsz {size}` first."
            )
            return 1
        require_hailo_sdk()

        # Prefer `hailomz compile yolo26n` if the upstream Model Zoo recipe
        # is installed. It carries the .alls quant config + the correct
        # end-node names for the multi-output (no on-chip NMS) head that
        # nexus-hailo-backend's RawYolo26 decoder expects. Note: the
        # upstream recipe is for 640; for 960/1280 we still wrap it but
        # pass `--resolution <size>` if hailomz supports it; otherwise we
        # fall through to the DFC path below.
        recipe_rc = compile_with_hailomz(
            "yolo26n",
            hef_out,
            extra_args=[
                "--ckpt",
                str(onnx),
                "--resolution",
                f"{size}x{size}",
                "--output-name",
                hef_out.name,
            ],
        )
        if recipe_rc == 0:
            print(f"[gen_yolo26n_hailo] hailomz compile succeeded for size={size}")
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
        "--size",
        type=int,
        choices=STATIC_SIZES,
        help="Compile a single size. Omit for --all.",
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help=f"Compile all three sizes: {', '.join(str(s) for s in STATIC_SIZES)}",
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

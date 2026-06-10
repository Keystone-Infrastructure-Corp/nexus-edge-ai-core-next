#!/usr/bin/env python3
"""Generate the Hailo-8 HEF for the DINOv2-S appearance-embedding backbone.

One size ships: 224x224 (the only operating point \u2014 DINOv2 ViTs are
trained at 224 and don't benefit from larger crops for a per-track
appearance fingerprint).

Source-of-truth ONNX MUST exist at `models/dinov2_s_224.onnx` \u2014
generate it first with:

    python tools/models/gen_dinov2.py

The HEF carries the same CLS-token output as the ONNX (384 floats
per crop). Note the cloud-side cross-camera linker
(`identity-graph` service in nexus-cloud-console) currently uses
`COSINE_MAX = 0.40` tuned against the FP32 ONNX path; the int8
Hailo quant may shift cosine distances by <1% and could require
re-tuning that threshold if the appearance pipeline is switched to
the HEF backend in production.

Hailo-8 ViT support is partial \u2014 some ops may fall back to CPU
inside the chip preroll, slowing total throughput vs. a CNN of
equivalent param count. Run `nexus-hailo-probe --hef
dinov2_s_224_hailo.hef` after compile to confirm the model lands
fully on chip.

NO public Hailo Model Zoo build exists for DINOv2; local DFC
compile only.

Output:
    models/dinov2_s_224_hailo.hef     (~25 MB)
"""

from __future__ import annotations

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

MODEL_ID = "dinov2-s-v1"


def main() -> int:
    onnx = MODELS_DIR / "dinov2_s_224.onnx"
    if not onnx.exists():
        print(
            f"[gen_dinov2_hailo] FATAL: {onnx} missing. Run "
            f"`python tools/models/gen_dinov2.py` first."
        )
        return 1
    require_hailo_sdk()

    hef_out = MODELS_DIR / "dinov2_s_224_hailo.hef"
    calib = ensure_calibration_set("imagenet-val-1024")
    rc = compile_with_dfc(
        onnx,
        hef_out,
        calibration_dir=calib,
        # ViT: let DFC walk the full transformer graph and emit the
        # CLS-token (single 384-dim tensor) as the only output. No NMS
        # postproc on the chip side; the host just consumes the
        # embedding directly.
        end_node_names=None,
    )
    if rc != 0:
        return rc

    sha = sha256_file(hef_out)
    print(f"[gen_dinov2_hailo] sha256 {sha}  ({hef_out.stat().st_size / (1024 * 1024):.2f} MB)")
    clear_pending_compile(MODEL_ID, hef_out.name, sha)
    return 0


if __name__ == "__main__":
    sys.exit(main())

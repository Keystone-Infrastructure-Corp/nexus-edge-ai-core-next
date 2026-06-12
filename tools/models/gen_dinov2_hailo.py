#!/usr/bin/env python3
"""Generate the Hailo-8 HEF for the DINOv2-S appearance-embedding backbone.

One size ships: 224x224 (the only operating point \u2014 DINOv2 ViTs are
trained at 224 and don't benefit from larger crops for a per-track
appearance fingerprint).

The shipped ORT-path ONNX at `models/dinov2_s_224.onnx` is the
operator-facing artifact (its sha is pinned in models-manifest.json
and immutable across releases). The Hailo path needs a *different*
intermediate ONNX because the DFC 5.x ViT parser misclassifies
DINOv2's per-layer `LayerScale` Mul as a Swin-style
window-partition candidate and crashes at parse with
`ValueError: height is not in list`
(`onnx_translator/onnx_graph.py: get_input_to_windows_info`). The
fix is to *absorb* each `LayerScale.lambda1` (per-channel learnable
scale) into the preceding `Linear` weight + bias \u2014
mathematically equivalent (cosine 1.000000, max abs diff <1e-5 vs.
the unfolded model), but it removes the per-block Mul node so the
parser no longer trips. We then export the folded model to a
build-only intermediate `models/dinov2_s_224_hailo_src.onnx` and
feed THAT to DFC. The intermediate is .gitignored \u2014 only the
HEF ships.

The HEF carries the same CLS-token output as the ONNX (384 floats
per crop). Cosine-similarity space is preserved by the fold, but
int8 quantisation may still shift cosine distances <1%, so the
cloud-side linker's `COSINE_MAX = 0.40` (currently tuned against
the FP32 ORT path) should be re-validated against the HEF path
before switching `identity-graph` to it in production.

NO public Hailo Model Zoo build exists for DINOv2; local DFC
compile only.

Output:
    models/dinov2_s_224_hailo.hef     (~25 MB, ships)
    models/dinov2_s_224_hailo_src.onnx (~84 MB, build artifact,
                                        regenerated each compile)
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
HF_CHECKPOINT = "facebook/dinov2-small"
INPUT_SIZE = 224
OUTPUT_DIM = 384
ALLS_DIR = Path(__file__).resolve().parent / "alls"
HAILO_SRC_ONNX = MODELS_DIR / "dinov2_s_224_hailo_src.onnx"


def _export_hailo_compat_onnx(out_path: Path) -> None:
    """Re-export HF DINOv2-S with `LayerScale` folded into preceding
    `Linear` weight + bias so DFC's ViT parser accepts the graph.

    For each transformer block:
        y = (W_attn_out @ x + b) * gamma1   ==   (W_attn_out * gamma1) @ x + b * gamma1
        y = (W_mlp_fc2 @ x + b) * gamma2    ==   (W_mlp_fc2 * gamma2) @ x + b * gamma2
    After folding, the LayerScale modules are replaced with `nn.Identity`,
    eliminating the per-block Mul nodes that confuse the parser.
    """
    import torch
    import torch.nn as nn
    from transformers import AutoModel

    print(f"[gen_dinov2_hailo] loading {HF_CHECKPOINT} for LayerScale fold")
    backbone = AutoModel.from_pretrained(HF_CHECKPOINT).eval()

    n_folded = 0
    for layer in backbone.encoder.layer:
        gamma1 = layer.layer_scale1.lambda1.detach()  # shape (384,)
        with torch.no_grad():
            layer.attention.output.dense.weight.mul_(gamma1.unsqueeze(1))
            layer.attention.output.dense.bias.mul_(gamma1)
        layer.layer_scale1 = nn.Identity()
        gamma2 = layer.layer_scale2.lambda1.detach()
        with torch.no_grad():
            layer.mlp.fc2.weight.mul_(gamma2.unsqueeze(1))
            layer.mlp.fc2.bias.mul_(gamma2)
        layer.layer_scale2 = nn.Identity()
        n_folded += 2
    print(f"[gen_dinov2_hailo] folded {n_folded} LayerScale modules")

    class _CLSWrapper(nn.Module):
        def __init__(self, inner: nn.Module) -> None:
            super().__init__()
            self.inner = inner

        def forward(self, pixel_values: "torch.Tensor") -> "torch.Tensor":  # noqa: F821
            out = self.inner(pixel_values=pixel_values, return_dict=True)
            return out.last_hidden_state[:, 0, :]

    wrapper = _CLSWrapper(backbone).eval()
    dummy = torch.zeros(1, 3, INPUT_SIZE, INPUT_SIZE, dtype=torch.float32)

    out_path.parent.mkdir(parents=True, exist_ok=True)
    print(f"[gen_dinov2_hailo] exporting folded ONNX (opset=17) \u2192 {out_path.name}")
    torch.onnx.export(
        wrapper,
        (dummy,),
        str(out_path),
        input_names=["pixel_values"],
        output_names=["embedding"],
        dynamic_axes=None,
        opset_version=17,
        do_constant_folding=True,
        export_params=True,
    )
    size_mib = out_path.stat().st_size / (1 << 20)
    print(f"[gen_dinov2_hailo] folded ONNX: {size_mib:.1f} MiB")


def main() -> int:
    # Sanity-link to the canonical ORT-path ONNX: forces operators
    # to keep `gen_dinov2.py` in sync, since the shipped ONNX sha
    # is what the manifest pins for the ORT execution path.
    canonical = MODELS_DIR / "dinov2_s_224.onnx"
    if not canonical.exists():
        print(
            f"[gen_dinov2_hailo] FATAL: {canonical} missing. Run "
            f"`python tools/models/gen_dinov2.py` first so the ORT-path "
            f"ONNX exists alongside the HEF this script produces."
        )
        return 1
    require_hailo_sdk()

    _export_hailo_compat_onnx(HAILO_SRC_ONNX)

    hef_out = MODELS_DIR / "dinov2_s_224_hailo.hef"
    calib = ensure_calibration_set("imagenet-val-1024")
    rc = compile_with_dfc(
        HAILO_SRC_ONNX,
        hef_out,
        calibration_dir=calib,
        alls_script=ALLS_DIR / "dinov2_s_224.alls",
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

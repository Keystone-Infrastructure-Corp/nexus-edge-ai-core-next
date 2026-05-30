#!/usr/bin/env python3
"""
Generate the DINOv2-S 224x224 appearance-embedding ONNX that powers
nexus-edge-ai-core-next's Phase 5.6 wedge identity graph.

The engine's `nexus-reid::DinoV2Extractor` takes a single normalised
1x3x224x224 RGB tensor and reads the 384-dim CLS token from the
backbone output. We export the HuggingFace `facebook/dinov2-small`
backbone in eval mode with a tiny `nn.Module` wrapper that returns
exactly that token, then optionally onnxslim-pass it to drop the
~5-10% of unused init nodes.

Why DINOv2-S?
* Released under Apache-2.0 by Meta (paper + weights both).
* Trained on LVD-142M (curated public web images, no PII filtering
  contract — Meta's own filtering, not human-faces-as-targets).
* 384-dim CLS token is the right size for a per-track appearance
  fingerprint: matches OSNet's 512-dim coarseness within 10%, indexes
  into pgvector cheaply.
* Backbone is 21M params -- runs ~6 ms on CoreML, ~10 ms on a Lunar
  Lake NPU, ~20 ms on Iris Xe iGPU at fp32. Quantising to int8
  later is a tier-3 perf win if needed.

Run from the workspace root with the model-gen venv active:

    source .venv-modelgen/bin/activate
    pip install transformers safetensors onnxslim     # one-time, if missing
    python tools/models/gen_dinov2.py

Output:
    models/dinov2_s_224.onnx        (~82 MiB, FP32)
    models/dinov2_s_224.onnx.sha256 (hex digest + filename, releaseable)

The manifest entry in models/models-manifest.json is patched in place
with the new sha256 the same way every other gen_*.py script does it.
The .onnx itself is .gitignore'd (binary artifact, lives in the
models-vN GitHub release).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
MODELS_DIR = REPO_ROOT / "models"
DEFAULT_OUTPUT = MODELS_DIR / "dinov2_s_224.onnx"
MODEL_ID = "dinov2-s-v1"
ARTIFACT_PATH = "dinov2_s_224.onnx"
HF_CHECKPOINT = "facebook/dinov2-small"
INPUT_SIZE = 224
OUTPUT_DIM = 384


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def update_manifest_sha(new_sha: str) -> None:
    """Patch the on-disk sha256 for the dinov2-s-v1 artifact in
    `models-manifest.json`. Idempotent.
    """
    manifest_path = MODELS_DIR / "models-manifest.json"
    if not manifest_path.exists():
        print(f"[gen_dinov2] no manifest at {manifest_path}, skipping sha update")
        return
    manifest = json.loads(manifest_path.read_text())
    for model in manifest.get("models", []):
        if model.get("id") != MODEL_ID:
            continue
        for art in model.get("artifacts", []):
            if art.get("path") != ARTIFACT_PATH:
                continue
            if art.get("sha256") == new_sha:
                print(f"[gen_dinov2] manifest sha already current ({new_sha[:12]}\u2026)")
                return
            art["sha256"] = new_sha
            manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
            print(f"[gen_dinov2] manifest sha updated \u2192 {new_sha[:12]}\u2026")
            return
    print(
        f"[gen_dinov2] no manifest entry matched (id={MODEL_ID} path={ARTIFACT_PATH}); "
        "add it before re-running"
    )


def write_sha256_sidecar(model_path: Path, sha: str) -> None:
    """Write `<model>.sha256` in the GNU `sha256sum` format so an
    operator can `sha256sum -c models/<file>.sha256` after pulling
    the asset from the GitHub Release.
    """
    sidecar = model_path.with_suffix(model_path.suffix + ".sha256")
    sidecar.write_text(f"{sha}  {model_path.name}\n")
    print(f"[gen_dinov2] wrote {sidecar.name}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Export DINOv2-S 224 ONNX for nexus-reid")
    parser.add_argument(
        "--out",
        type=Path,
        default=DEFAULT_OUTPUT,
        help=f"Output ONNX path (default: {DEFAULT_OUTPUT.relative_to(REPO_ROOT)})",
    )
    parser.add_argument(
        "--opset",
        type=int,
        default=17,
        help="ONNX opset (default 17 -- needed for the vision-transformer ops Meta ships).",
    )
    parser.add_argument(
        "--no-slim",
        action="store_true",
        help="Skip the onnxslim pass. Useful for diffing the raw HF export.",
    )
    parser.add_argument(
        "--smoke",
        action="store_true",
        help="After export, run onnxruntime against a single dummy input and assert the "
        "output is 1x384 and L2-finite. Off by default so the script doesn't import ORT "
        "on every invocation.",
    )
    args = parser.parse_args()

    try:
        import torch
        import torch.nn as nn
        from transformers import AutoModel
    except ImportError as e:
        print(f"[gen_dinov2] missing dep ({e}). Install with:")
        print("[gen_dinov2]   pip install torch transformers safetensors")
        return 1

    print(f"[gen_dinov2] loading HF checkpoint: {HF_CHECKPOINT}")
    try:
        backbone = AutoModel.from_pretrained(HF_CHECKPOINT)
    except Exception as ex:  # noqa: BLE001
        print(f"[gen_dinov2] ERROR loading {HF_CHECKPOINT}: {ex}")
        print("[gen_dinov2] (HF cache is at ~/.cache/huggingface; rm -rf if corrupted)")
        return 1
    backbone.eval()

    # Wrap the backbone so the ONNX graph has a single, named output
    # ('embedding') equal to the CLS token (a.k.a. last_hidden_state[:, 0, :]).
    # That matches what nexus-reid::DinoV2Extractor reads.
    class DinoBackbone(nn.Module):
        def __init__(self, inner: nn.Module) -> None:
            super().__init__()
            self.inner = inner

        def forward(self, pixel_values: "torch.Tensor") -> "torch.Tensor":  # noqa: F821
            out = self.inner(pixel_values=pixel_values, return_dict=True)
            # last_hidden_state is [B, N+1, 384]; element [:, 0, :] is the CLS token.
            return out.last_hidden_state[:, 0, :]

    wrapper = DinoBackbone(backbone).eval()

    dummy = torch.zeros(1, 3, INPUT_SIZE, INPUT_SIZE, dtype=torch.float32)
    with torch.no_grad():
        ref = wrapper(dummy)
    if tuple(ref.shape) != (1, OUTPUT_DIM):
        print(
            f"[gen_dinov2] ERROR: wrapper output shape {tuple(ref.shape)} != (1, {OUTPUT_DIM}). "
            "HF checkpoint shape changed?"
        )
        return 1

    args.out.parent.mkdir(parents=True, exist_ok=True)
    print(f"[gen_dinov2] exporting ONNX (opset={args.opset}) \u2192 {args.out}")
    torch.onnx.export(
        wrapper,
        (dummy,),
        str(args.out),
        input_names=["pixel_values"],
        output_names=["embedding"],
        # Keep batch axis static at 1: nexus-reid only ever runs a
        # single 224x224 crop per submission (per-track per-cadence).
        # Static batch makes the Intel NPU plugin happy + lets the
        # OpenVINO blob cache hit on every reboot.
        dynamic_axes=None,
        opset_version=args.opset,
        do_constant_folding=True,
        export_params=True,
    )

    if not args.no_slim:
        try:
            import onnxslim  # type: ignore
        except ImportError:
            print("[gen_dinov2] onnxslim not installed, skipping slim pass")
        else:
            print("[gen_dinov2] running onnxslim pass")
            try:
                onnxslim.slim(str(args.out), str(args.out))
            except Exception as ex:  # noqa: BLE001
                print(f"[gen_dinov2] onnxslim failed ({ex}); keeping raw export")

    if args.smoke:
        try:
            import numpy as np
            import onnxruntime as ort  # type: ignore
        except ImportError:
            print("[gen_dinov2] onnxruntime/numpy not installed, skipping smoke test")
        else:
            print("[gen_dinov2] smoke: onnxruntime forward pass")
            sess = ort.InferenceSession(str(args.out), providers=["CPUExecutionProvider"])
            arr = np.zeros((1, 3, INPUT_SIZE, INPUT_SIZE), dtype=np.float32)
            (out,) = sess.run(["embedding"], {"pixel_values": arr})
            if out.shape != (1, OUTPUT_DIM):
                print(f"[gen_dinov2] ERROR smoke: ORT output shape {out.shape} != (1, {OUTPUT_DIM})")
                return 1
            if not np.isfinite(out).all():
                print("[gen_dinov2] ERROR smoke: ORT output has non-finite values")
                return 1
            print(f"[gen_dinov2] smoke OK \u2014 mean={out.mean():.3e} std={out.std():.3e}")

    sha = sha256_file(args.out)
    size_mib = args.out.stat().st_size / (1 << 20)
    print(f"[gen_dinov2] {args.out.name}: {size_mib:.2f} MiB, sha256={sha[:16]}\u2026")
    write_sha256_sidecar(args.out, sha)
    update_manifest_sha(sha)
    return 0


if __name__ == "__main__":
    sys.exit(main())

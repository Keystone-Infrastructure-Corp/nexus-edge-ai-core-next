#!/usr/bin/env python3
"""
Generate the text-mode YOLOE ONNX used by M3's `YoloeDetector`.

YOLOE is the open-vocabulary detector line that supersedes YOLO-World
in the ultralytics ecosystem. This script is the M3.1 counterpart of
`gen_yolo_world.py`: same vocab-baking flow, different upstream
checkpoint. The Rust detector (`crates/nexus-inference/src/yoloe.rs`)
treats the resulting ONNX as a closed-vocab YOLOv8-style head where
each class index 0..N-1 maps to a prompt in `prompts[]`.

Run from the workspace root with the model-gen venv active:

    source .venv-modelgen/bin/activate
    # all three shapes in one session:
    python tools/models/gen_yoloe.py --all-static \\
        --prompts tools/models/yoloe_default_prompts.txt
    # …or one shape at a time:
    python tools/models/gen_yoloe.py --shape 512x288

Ships per-shape static ONNXs on the native 16:9 ∩ stride-32 ladder
(W=512k, H=288k) — matching gen_yolo26n.py / gen_yolo_world.py. See
`docs/edge-core/M_NATIVE_ASPECT.md` in the cloud-console repo.

Output:
    models/yoloe26_s_512x288.onnx    (~25–35 MB; smaller than YOLO-World v2 s)
    models/yoloe26_s_1024x576.onnx
    models/yoloe26_s_1536x864.onnx

The prompt file lives under `tools/models/` (tracked) so the prompt
vocabulary is reproducible; the ONNX itself stays under `models/`
(gitignored) per the same policy as `yolo26n_<W>x<H>.onnx`.

NOTE on upstream availability: as of M3.1 the ultralytics PyPI release
shipping the `YOLOE` symbol is moving rapidly. If `from ultralytics
import YOLOE` fails, install from the main branch:

    pip install -U "git+https://github.com/ultralytics/ultralytics@main"

The export incantation (`model.set_classes(...)` then `model.export(
format="onnx", ...)`) mirrors `gen_yolo_world.py` exactly — the
ultralytics team kept the public surface stable across the YOLOE
rename.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
from pathlib import Path
from typing import List

REPO_ROOT = Path(__file__).resolve().parents[2]
MODELS_DIR = REPO_ROOT / "models"
# Native 16:9 ladder: exact 16:9 ∩ stride-32 (W=512k, H=288k).
STATIC_SHAPES = ((512, 288), (1024, 576), (1536, 864))
DEFAULT_PROMPTS = Path(__file__).resolve().parent / "yoloe_default_prompts.txt"
# Ultralytics 8.4.x consolidated the YOLOE release assets on the segmentation
# checkpoints (`yoloe-26{n,s,l,x}-seg.pt` and `yoloe-v8{s,m,l}-seg.pt`); the
# bare `yoloe-s.pt` originally referenced here no longer exists upstream.
# We pick the 26-arch small variant (smallest with the new backbone) and
# ultralytics auto-strips the segmentation head when we export with the
# detection task. The Rust loader only consumes the detection output anyway.
DEFAULT_BASE_MODEL = "yoloe-26s-seg.pt"


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

    return MODELS_DIR / f"yoloe26_s_{w}x{h}.onnx"


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def read_prompts(path: Path) -> List[str]:
    """Read a one-prompt-per-line text file, skipping blanks + `#` comments."""

    out: List[str] = []
    seen: set[str] = set()
    for raw in path.read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line in seen:
            continue
        seen.add(line)
        out.append(line)
    if not out:
        raise ValueError(f"prompt file {path} produced no usable entries")
    return out


def upsert_manifest_entry(
    *,
    model_id: str,
    sized_artifacts: List[dict],
    prompts: List[str],
    default_preset: str,
) -> None:
    """Insert/update a YOLOE entry in `models/models-manifest.json`.

    Re-uses the v2 manifest extension `prompts[]` block that
    YOLO-World introduced — the Rust loader keys off `task` to pick
    the detector backend, and YOLOE shares the open-vocab task tag
    `detect_open_vocab_text` (alias `detect_open_vocab` for legacy
    YOLO-World configs).
    """

    manifest_path = MODELS_DIR / "models-manifest.json"
    if manifest_path.exists():
        manifest = json.loads(manifest_path.read_text())
    else:
        manifest = {"version": "v2", "models": []}
    if manifest.get("version") != "v2":
        raise SystemExit(
            f"manifest at {manifest_path} is not v2 — refuse to clobber"
        )
    models = manifest.setdefault("models", [])

    # Sort by width ascending so the manifest reads naturally and diffs
    # cleanly across runs.
    sized_artifacts = sorted(sized_artifacts, key=lambda a: a["w"])
    default_art = next(
        (a for a in sized_artifacts if f"{a['w']}x{a['h']}" == default_preset),
        sized_artifacts[0],
    )

    entry = {
        "id": model_id,
        "task": "detect_open_vocab_text",
        "_comment": (
            "YOLOE (small) text-mode export on the native 16:9 ladder "
            "(512x288 / 1024x576 / 1536x864). Prompts are baked "
            "into the graph at export time; per-camera config picks a subset "
            "at runtime. Regenerate via tools/models/gen_yoloe.py whenever "
            "the prompt vocabulary changes — the manifest sha256 values below "
            "will refresh and the engine's loader will catch the diff."
        ),
        "input": {
            "width": default_art["w"],
            "height": default_art["h"],
            "channels": 3,
            "format": "RGB",
        },
        "default_thresholds": {
            # YOLOE logits run in the same range as YOLO-World — keep the
            # same conservative confidence floor; operators tune per-camera.
            "confidence": 0.10,
            "nms": 0.50,
        },
        "artifacts": [
            {
                "backend": "onnx",
                "path": a["path"],
                "preset": f"{a['w']}x{a['h']}",
                "sha256": a["sha"],
            }
            for a in sized_artifacts
        ],
        "presets": [
            {
                "name": f"{a['w']}x{a['h']}",
                "inputWidth": a["w"],
                "inputHeight": a["h"],
                "artifact": a["path"],
            }
            for a in sized_artifacts
        ],
        "default_preset": default_preset,
        "prompts": prompts,
    }

    for i, m in enumerate(models):
        if m.get("id") == model_id:
            # This upsert owns only the ONNX exports. Preserve any
            # operator-authored non-ONNX artifacts (Hailo HEF entries) and
            # their `-hef` presets so regenerating ONNX never clobbers the
            # hand-authored HEF metadata that `gen_*_hailo.py` patches.
            preserved_artifacts = [
                a for a in m.get("artifacts", []) if a.get("backend") != "onnx"
            ]
            preserved_presets = [
                p for p in m.get("presets", []) if p.get("name", "").endswith("-hef")
            ]
            entry["artifacts"].extend(preserved_artifacts)
            entry["presets"].extend(preserved_presets)
            models[i] = entry
            break
    else:
        models.append(entry)

    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    sizes = ", ".join(f"{a['w']}x{a['h']}" for a in sized_artifacts)
    print(
        f"[gen_yoloe] manifest upserted: {model_id} "
        f"({len(prompts)} prompts, sizes [{sizes}], default {default_preset})"
    )


def export_yoloe(
    base_model: str,
    prompts: List[str],
    w: int,
    h: int,
    opset: int,
    output: Path,
) -> None:
    """Run the ultralytics export with the prompt vocab baked in."""

    try:
        from ultralytics import YOLOE  # type: ignore
    except ImportError as ex:
        raise SystemExit(
            "ultralytics.YOLOE is unavailable in this venv. Upgrade with:\n"
            "    pip install -U 'git+https://github.com/ultralytics/ultralytics@main'\n"
            f"underlying error: {ex}"
        )

    print(f"[gen_yoloe] loading base model: {base_model}")
    model = YOLOE(base_model)
    print(f"[gen_yoloe] setting {len(prompts)} prompt classes")
    # Mirrors gen_yolo_world.py: `set_classes` bakes the open-vocab
    # vocabulary into the checkpoint as fixed text embeddings before
    # export. Post-call the model behaves like a closed-vocab YOLOv8
    # detector with C = len(prompts).
    model.set_classes(prompts)

    print(f"[gen_yoloe] exporting ONNX (1x3x{h}x{w}, opset={opset})")
    # ultralytics takes imgsz as [height, width] — height first.
    model.export(
        format="onnx",
        dynamic=False,  # Static for predictability — open-vocab head
        # always runs at the configured input size.
        opset=opset,
        imgsz=[h, w],
        simplify=True,
        nms=False,  # keep raw YOLOv8 head — Rust postprocess does NMS.
    )

    base_stem = Path(base_model).stem
    base_dir = Path(base_model).resolve().parent
    candidates = [
        base_dir / f"{base_stem}.onnx",
        Path.cwd() / f"{base_stem}.onnx",
        REPO_ROOT / f"{base_stem}.onnx",
        MODELS_DIR / f"{base_stem}.onnx",
    ]
    src = next((p for p in candidates if p.is_file()), None)
    if src is None:
        raise SystemExit(
            f"export succeeded but ONNX not found in any of: {candidates}"
        )
    output.parent.mkdir(parents=True, exist_ok=True)
    if src.resolve() != output.resolve():
        shutil.copy2(src, output)
        try:
            os.unlink(src)
        except OSError:
            pass
        print(f"[gen_yoloe] copied {src} → {output}")


def smoke_check(onnx_path: Path, input_w: int, input_h: int) -> None:
    """Load the exported ONNX in onnxruntime and run a single zero tensor."""

    import numpy as np  # noqa: WPS433
    import onnxruntime as ort  # noqa: WPS433

    print(f"[gen_yoloe] smoke loading {onnx_path}")
    sess = ort.InferenceSession(
        str(onnx_path), providers=["CPUExecutionProvider"]
    )
    in_name = sess.get_inputs()[0].name
    in_shape = sess.get_inputs()[0].shape
    print(f"[gen_yoloe] input '{in_name}' shape={in_shape}")
    dummy = np.zeros((1, 3, input_h, input_w), dtype=np.float32)
    outputs = sess.run(None, {in_name: dummy})
    print(
        f"[gen_yoloe] smoke ok — got {len(outputs)} output(s); "
        f"first shape={outputs[0].shape}"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description="Export YOLOE text-mode ONNX")
    parser.add_argument(
        "--base-model",
        type=str,
        default=DEFAULT_BASE_MODEL,
        help=(
            "Ultralytics YOLOE checkpoint to start from. "
            "Defaults to yoloe-s.pt (small model — ~25 MB ONNX). "
            "Use yoloe-m.pt or larger for higher accuracy at the cost of fps."
        ),
    )
    parser.add_argument(
        "--prompts",
        type=Path,
        default=DEFAULT_PROMPTS,
        help=f"Prompt file (default: {DEFAULT_PROMPTS.relative_to(REPO_ROOT)})",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Override the output ONNX path. Default: per-shape mode → "
        "models/yoloe26_s_<W>x<H>.onnx. Ignored under --all-static.",
    )
    parser.add_argument(
        "--shape",
        type=parse_shape,
        default=(512, 288),
        help="Input shape as WxH (default 512x288). Ladder rung: "
        "512x288 | 1024x576 | 1536x864.",
    )
    parser.add_argument(
        "--opset",
        type=int,
        default=17,
        help=(
            "ONNX opset. 17 = YOLOE requires the gather + matmul ops the "
            "text-embedding fusion uses; 12 (the closed-vocab default) "
            "won't export."
        ),
    )
    parser.add_argument(
        "--manifest-id",
        type=str,
        default="yoloe26_s",
        help="ID written into models-manifest.json",
    )
    parser.add_argument(
        "--skip-smoke",
        action="store_true",
        help="Skip the onnxruntime smoke load (use only when debugging exports).",
    )
    parser.add_argument(
        "--all-static",
        action="store_true",
        help="Generate all static-shape ONNXs in one ultralytics session: "
        + ", ".join(f"{w}x{h}" for w, h in STATIC_SHAPES)
        + ". This is the release-pipeline path.",
    )
    parser.add_argument(
        "--default-preset",
        type=str,
        default="512x288",
        help="Which preset name to write as `default_preset` in the manifest "
        "(default 512x288, the Standard tier).",
    )
    args = parser.parse_args()

    explicit_single = (
        any(arg.startswith("--shape") for arg in sys.argv[1:])
        or args.output is not None
    )
    if not args.all_static and not explicit_single:
        args.all_static = True

    try:
        prompts = read_prompts(args.prompts)
    except (FileNotFoundError, ValueError) as ex:
        print(f"[gen_yoloe] ERROR reading prompts: {ex}")
        return 1

    print(f"[gen_yoloe] prompts: {prompts}")

    if args.all_static:
        shapes = list(STATIC_SHAPES)
    else:
        shapes = [args.shape]

    sized_artifacts: List[dict] = []
    for w, h in shapes:
        output = (
            args.output
            if (args.output is not None and not args.all_static)
            else static_output_for(w, h)
        )
        try:
            export_yoloe(
                base_model=args.base_model,
                prompts=prompts,
                w=w,
                h=h,
                opset=args.opset,
                output=output,
            )
        except Exception as ex:  # noqa: BLE001
            print(f"[gen_yoloe] ERROR exporting {w}x{h}: {ex}")
            return 1

        if not args.skip_smoke:
            try:
                smoke_check(output, w, h)
            except Exception as ex:  # noqa: BLE001
                print(f"[gen_yoloe] ERROR smoke-loading {output}: {ex}")
                return 1

        sha = sha256_file(output)
        size_mb = output.stat().st_size / (1024 * 1024)
        print(f"[gen_yoloe] {w}x{h} sha256 {sha}  size {size_mb:.2f} MB")
        sized_artifacts.append({"w": w, "h": h, "path": output.name, "sha": sha})

    upsert_manifest_entry(
        model_id=args.manifest_id,
        sized_artifacts=sized_artifacts,
        prompts=prompts,
        default_preset=args.default_preset,
    )
    for a in sized_artifacts:
        print(f"[gen_yoloe] success: models/{a['path']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

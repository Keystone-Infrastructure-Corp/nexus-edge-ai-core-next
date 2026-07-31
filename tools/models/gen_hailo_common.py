"""Shared helpers for the `gen_*_hailo.py` HEF generators.

The Hailo Dataflow Compiler (DFC) is a Linux-x86_64-only proprietary
toolchain gated behind a free Hailo Developer Zone account. None of
the modules below are importable on macOS or without DFC installed,
so every script that uses these helpers MUST call
`require_hailo_sdk()` first and surface the install runbook to the
operator on failure.

Convention enforced here:

* Per-model gen scripts share the manifest-patching contract:
  on success they OVERWRITE `sha256` with the real digest and DROP
  the `requires_compile` field. The engine's runtime dispatcher
  doesn't read `requires_compile` (it just checks `path.exists()`),
  so leaving it in place after a successful compile is purely a
  manifest-hygiene bug.
* Calibration sets live under `tools/models/calibration/` and are
  gitignored (~GB-scale). Each script declares its expected set;
  this helper provides `ensure_calibration_set()` which fetches +
  caches on first use.
* The compile API surface is wrapped in `compile_onnx_to_hef()`.
  It tries `hailomz` CLI first (Hailo Model Zoo wrapper — has known
  recipes for popular models including yolo26n) and falls back to
  the DFC Python SDK when no recipe matches.

See `/memories/repo/nexus-models-manifest-pending-compile.md` for
the cross-session contract this file implements.
"""

from __future__ import annotations

import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path
from typing import Iterable, Optional

REPO_ROOT = Path(__file__).resolve().parents[2]
MODELS_DIR = REPO_ROOT / "models"
MANIFEST_PATH = MODELS_DIR / "models-manifest.json"
CALIBRATION_DIR = REPO_ROOT / "tools" / "models" / "calibration"

# Public Hailo Model Zoo prebuilt HEF mirror. Pinned to v2.18.0
# because that's the snapshot we validated `nexus-hailo-backend`
# against; later snapshots may change output-tensor names + break the
# pair-by-shape decode in `build_yolo26_scales`.
HAILO_MODEL_ZOO_URL_BASE = (
    "https://hailo-model-zoo.s3.eu-west-2.amazonaws.com/ModelZoo/Compiled/v2.18.0/hailo8"
)


# --------------------------------------------------------------------------- #
# Environment / SDK checks
# --------------------------------------------------------------------------- #

def require_hailo_sdk() -> None:
    """Fail-loud if the Hailo SDK is not importable.

    The error message is the operator runbook for getting DFC
    installed; we do NOT try to install it automatically because it
    requires acceptance of Hailo's EULA + a Dev Zone login.
    """

    try:
        import hailo_sdk_client  # noqa: F401
    except ImportError:
        sys.stderr.write(
            "[gen_hailo_common] FATAL: hailo_sdk_client is not importable.\n"
            "\n"
            "The Hailo Dataflow Compiler (DFC) is required to compile HEFs.\n"
            "It is gated behind a free Hailo Developer Zone account:\n"
            "\n"
            "  1. Register: https://hailo.ai/developer-zone/\n"
            "  2. Download: 'Hailo AI Software Suite' (Linux x86_64, ~5 GB)\n"
            "  3. Install:  follow the suite installer; activates inside its\n"
            "               own venv at ~/hailo_ai_sw_suite/\n"
            "  4. Activate: `source ~/hailo_ai_sw_suite/hailo_venv/bin/activate`\n"
            "               BEFORE running this script.\n"
            "\n"
            "DFC is Linux-x86_64-only; macOS is not supported.\n"
        )
        raise SystemExit(2)


def require_hailomz_cli() -> Optional[str]:
    """Return the path to `hailomz` if installed, else None.

    `hailomz` (Hailo Model Zoo CLI) wraps DFC with known per-model
    recipes (parser configs + .alls quantization scripts +
    calibration loaders). It's the highest-leverage compile path
    for models the upstream Model Zoo already supports.
    """

    return shutil.which("hailomz")


# --------------------------------------------------------------------------- #
# Hashing + manifest patching
# --------------------------------------------------------------------------- #

def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def clear_pending_compile(
    model_id: str,
    artifact_path: str,
    new_sha: str,
) -> None:
    """Patch the manifest after a successful HEF compile.

    Overwrites `sha256` and removes `requires_compile`. Idempotent —
    no-op if the sha already matches and the flag is already gone.
    """

    if not MANIFEST_PATH.exists():
        print(f"[gen_hailo_common] no manifest at {MANIFEST_PATH}, skipping sha update")
        return
    manifest = json.loads(MANIFEST_PATH.read_text())
    changed = False
    for model in manifest.get("models", []):
        if model.get("id") != model_id:
            continue
        for art in model.get("artifacts", []):
            if art.get("path") != artifact_path:
                continue
            if art.get("sha256") != new_sha:
                art["sha256"] = new_sha
                changed = True
            if "requires_compile" in art:
                del art["requires_compile"]
                changed = True
    if changed:
        MANIFEST_PATH.write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"[gen_hailo_common] manifest patched: {artifact_path} sha={new_sha[:12]}\u2026")
    else:
        print(f"[gen_hailo_common] manifest already current: {artifact_path}")


# --------------------------------------------------------------------------- #
# Calibration sets
# --------------------------------------------------------------------------- #

def ensure_calibration_set(name: str) -> Path:
    """Return a directory of calibration images for the given set name.

    Supported sets:
      * `coco-val2017-1024` — 1024 random samples from COCO val2017
        (CC-BY-4.0). 5 GB raw download; samples kept locally.
      * `imagenet-val-1024` — 1024 random samples from ImageNet
        validation set. Requires the operator to provision the
        upstream archive separately (ImageNet has a research-only
        access agreement); the script will fail-loud if the source
        isn't present at `$NEXUS_IMAGENET_VAL_DIR` or the default
        `~/datasets/imagenet/val/`.

    Returns a `Path` to a directory of JPEG/PNG files. Calibration
    loaders in each gen script enumerate it.
    """

    dest = CALIBRATION_DIR / name
    if dest.exists() and any(dest.iterdir()):
        return dest
    CALIBRATION_DIR.mkdir(parents=True, exist_ok=True)
    if name == "coco-val2017-1024":
        return _fetch_coco_val_sample(dest, 1024)
    if name == "imagenet-val-1024":
        return _link_imagenet_val_sample(dest, 1024)
    raise ValueError(f"unknown calibration set: {name}")


def _fetch_coco_val_sample(dest: Path, count: int) -> Path:
    """Fetch a random subset of COCO val2017 into `dest`.

    Pulls the official val2017.zip (~1 GB), unpacks, takes a fixed
    deterministic seeded sample of `count` images, deletes the
    archive + extraction. Skips entirely if `dest/` already has
    `>= count` JPEGs.
    """

    import random
    import zipfile

    existing = list(dest.glob("*.jpg"))
    if len(existing) >= count:
        print(f"[gen_hailo_common] using cached {len(existing)} COCO calibration imgs at {dest}")
        return dest

    archive_url = "http://images.cocodataset.org/zips/val2017.zip"
    archive_path = CALIBRATION_DIR / "val2017.zip"
    extract_dir = CALIBRATION_DIR / "val2017_full"
    if not archive_path.exists():
        print(f"[gen_hailo_common] downloading {archive_url} (~1 GB) \u2014 cached locally")
        urllib.request.urlretrieve(archive_url, archive_path)
    if not extract_dir.exists():
        print(f"[gen_hailo_common] extracting val2017.zip \u2014 takes ~1 min")
        with zipfile.ZipFile(archive_path) as zf:
            zf.extractall(CALIBRATION_DIR)
        # zip extracts to CALIBRATION_DIR/val2017/
        (CALIBRATION_DIR / "val2017").rename(extract_dir)
    images = sorted(extract_dir.glob("*.jpg"))
    rng = random.Random(0xC0CA)  # fixed seed for reproducible calibration
    sample = rng.sample(images, min(count, len(images)))
    dest.mkdir(parents=True, exist_ok=True)
    for src in sample:
        shutil.copy2(src, dest / src.name)
    print(f"[gen_hailo_common] sampled {len(sample)} COCO val2017 imgs \u2192 {dest}")
    return dest


def _link_imagenet_val_sample(dest: Path, count: int) -> Path:
    """Sample `count` images from an operator-provisioned ImageNet val dir."""

    import random

    src_dir_env = os.environ.get("NEXUS_IMAGENET_VAL_DIR")
    src_dir = Path(src_dir_env) if src_dir_env else Path.home() / "datasets" / "imagenet" / "val"
    if not src_dir.exists():
        raise SystemExit(
            f"[gen_hailo_common] FATAL: ImageNet val set not found at {src_dir}.\n"
            f"  ImageNet is research-only and cannot be auto-fetched. Either:\n"
            f"    * set NEXUS_IMAGENET_VAL_DIR=/path/to/imagenet/val, or\n"
            f"    * symlink ~/datasets/imagenet/val to your local copy.\n"
            f"  The dir should contain class-name subdirectories with JPEGs."
        )
    candidates: list[Path] = []
    for p in src_dir.rglob("*.JPEG"):
        candidates.append(p)
        if len(candidates) > count * 4:
            # cap exploration so we don't enumerate the full 50k set
            break
    if len(candidates) < count:
        raise SystemExit(
            f"[gen_hailo_common] FATAL: only {len(candidates)} JPEGs under {src_dir}; "
            f"need {count}. Check the dir structure."
        )
    rng = random.Random(0x1A9E)
    sample = rng.sample(candidates, count)
    dest.mkdir(parents=True, exist_ok=True)
    for src in sample:
        shutil.copy2(src, dest / f"{src.parent.name}__{src.name}")
    print(f"[gen_hailo_common] sampled {count} ImageNet val imgs \u2192 {dest}")
    return dest


# --------------------------------------------------------------------------- #
# Compile drivers
# --------------------------------------------------------------------------- #

def compile_with_hailomz(
    model_name: str,
    hef_out: Path,
    *,
    extra_args: Optional[list[str]] = None,
    hw_arch: str = "hailo8",
) -> int:
    """Shell out to `hailomz compile` for upstream-supported models.

    Returns the process exit code. Captures stdout/stderr to a
    sidecar log at `hef_out.with_suffix('.compile.log')` so DFC
    diagnostics survive past the script run.
    """

    cli = require_hailomz_cli()
    if cli is None:
        return 127
    cmd = [cli, "compile", model_name, "--hw-arch", hw_arch]
    if extra_args:
        cmd.extend(extra_args)
    log_path = hef_out.with_suffix(".compile.log")
    print(f"[gen_hailo_common] hailomz: {shlex.join(cmd)}")
    print(f"[gen_hailo_common]   log \u2192 {log_path}")
    with log_path.open("w") as log:
        rc = subprocess.call(cmd, stdout=log, stderr=subprocess.STDOUT)
    return rc


def compile_with_dfc(
    onnx_path: Path,
    hef_out: Path,
    *,
    calibration_dir: Path,
    alls_script: Optional[Path] = None,
    end_node_names: Optional[Iterable[str]] = None,
    start_node_names: Optional[Iterable[str]] = None,
) -> int:
    """Drive the DFC Python SDK directly when no `hailomz` recipe applies.

    This is the fallback for the open-vocab + DINOv2 models, which
    are not upstream-supported. The script materializes a minimal
    ClientRunner pipeline:

        1. parse: ONNX -> Hailo HAR (hardware-agnostic intermediate)
        2. optimize: int8 quantization using `calibration_dir` samples
        3. compile: HAR -> HEF

    `alls_script` is an optional `.alls` quantization-config file
    that pins per-layer precision and post-processing (e.g.
    embedding the on-chip NMS metadata for YOLO-family models).
    When omitted, DFC infers defaults from the ONNX graph.

    Returns 0 on success, non-zero on any stage failure (logged to
    `<hef>.compile.log`).
    """

    require_hailo_sdk()
    from hailo_sdk_client import ClientRunner  # type: ignore

    log_path = hef_out.with_suffix(".compile.log")
    print(f"[gen_hailo_common] DFC: {onnx_path.name} \u2192 {hef_out.name}")
    print(f"[gen_hailo_common]   log \u2192 {log_path}")

    runner = ClientRunner(hw_arch="hailo8")
    try:
        parse_kwargs = {}
        if start_node_names:
            parse_kwargs["start_node_names"] = list(start_node_names)
        if end_node_names:
            parse_kwargs["end_node_names"] = list(end_node_names)
        runner.translate_onnx_model(str(onnx_path), str(onnx_path.stem), **parse_kwargs)

        # Load + apply the .alls quantization config if provided. Otherwise
        # DFC builds a default quant schema from the ONNX op set.
        if alls_script is not None and alls_script.exists():
            runner.load_model_script(str(alls_script))

        # Calibration: DFC expects a tensor-loader callable. We feed it
        # JPEG-decoded RGB crops resized to the model's input dim, in
        # [0,1] FP32. The model's input dim is auto-discovered from the
        # parsed HAR (DFC exposes this via runner.input_shape).
        calib_array = _calibration_loader(calibration_dir, runner)
        runner.optimize(calib_array)

        compiled_hef = runner.compile()
        hef_out.parent.mkdir(parents=True, exist_ok=True)
        hef_out.write_bytes(compiled_hef)
    except Exception as ex:  # noqa: BLE001
        with log_path.open("w") as log:
            log.write(f"DFC compile failed: {type(ex).__name__}: {ex}\n")
            import traceback
            traceback.print_exc(file=log)
        print(f"[gen_hailo_common] ERROR: {ex}")
        return 1
    return 0


def _calibration_loader(calibration_dir: Path, runner):
    """Yield FP32 RGB tensors from `calibration_dir` for DFC optimize().

    Reads the model's input shape off the runner (set at parse time)
    and bilinear-resizes each JPEG to (1, H, W, 3) in [0, 1].
    """

    try:
        import numpy as np  # type: ignore
        from PIL import Image  # type: ignore
    except ImportError as ex:
        raise SystemExit(
            "[gen_hailo_common] FATAL: numpy + pillow are required for calibration. "
            "Install: pip install numpy pillow"
        ) from ex

    # Best-effort introspection of the parsed model input shape. DFC's
    # API exposes this as `runner.get_hn_dict()['layers'][input_layer]['input_shapes']`,
    # but the public surface for it varies across DFC versions, so we
    # fall back to 224x224x3 if we can't detect it.
    h, w = 224, 224
    try:
        hn = runner.get_hn_dict()
        for layer in hn.get("layers", {}).values():
            if layer.get("type") == "input_layer":
                # input_shape is [N, H, W, C] in DFC's canonical NHWC ordering
                shape = layer.get("input_shapes", [[1, 224, 224, 3]])[0]
                h, w = int(shape[1]), int(shape[2])
                break
    except Exception:  # noqa: BLE001
        pass

    images = sorted(calibration_dir.glob("*.jpg")) + sorted(calibration_dir.glob("*.JPEG")) + sorted(calibration_dir.glob("*.png"))
    # Cap at CALIB_LIMIT to keep DFC's optimize() FP32 array in RAM on
    # smaller hosts (1024 x 1280 x 1280 x 3 x 4 = 20 GB; 256 is plenty
    # for INT8 PTQ per Hailo's optimization guide).
    import os as _os
    _limit = int(_os.environ.get("CALIB_LIMIT", "256"))
    if len(images) > _limit:
        images = images[:_limit]
    if not images:
        raise SystemExit(
            f"[gen_hailo_common] FATAL: no calibration images found under {calibration_dir}"
        )
    print(f"[gen_hailo_common]   calibration: {len(images)} imgs @ {h}x{w}")
    # DFC's runner.optimize() expects a single (N, H, W, C) FP32 array
    # in pre-normalization range, NOT a generator. The .alls
    # normalization op handles the [0,255] -> per-model scaling.
    batch = np.empty((len(images), h, w, 3), dtype=np.float32)
    for i, img_path in enumerate(images):
        with Image.open(img_path).convert("RGB") as im:
            im = im.resize((w, h), Image.Resampling.BILINEAR)
            batch[i] = np.asarray(im, dtype=np.float32)
    return batch


# --------------------------------------------------------------------------- #
# Public Model Zoo download (for the one prebuilt HEF we ship)
# --------------------------------------------------------------------------- #

def download_public_hef(name: str, dest: Path) -> int:
    """Download a prebuilt HEF from the Hailo Model Zoo S3 mirror.

    Currently unused: every shipped shape is on the native 16:9 ladder
    (512x288 / 1024x576 / 1536x864) and has no public
    prebuilt, so all HEFs are compiled locally via the DFC. Retained for
    the square public-prebuilt path in case a Zoo-supported square shape
    is ever reintroduced.
    """

    url = f"{HAILO_MODEL_ZOO_URL_BASE}/{name}"
    print(f"[gen_hailo_common] downloading {url}")
    try:
        dest.parent.mkdir(parents=True, exist_ok=True)
        urllib.request.urlretrieve(url, dest)
    except Exception as ex:  # noqa: BLE001
        print(f"[gen_hailo_common] ERROR: download failed: {ex}")
        return 1
    print(f"[gen_hailo_common]   wrote {dest} ({dest.stat().st_size / (1024 * 1024):.2f} MB)")
    return 0

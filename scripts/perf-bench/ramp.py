#!/usr/bin/env python3
"""Ramp driver for the Nexus camera-capacity bench.

Invoked by ``nexus-perf-bench.sh``; not meant to be run directly.

Walks the box up a camera ladder, one dedicated RTSP publisher per camera,
and samples the engine at each rung. Emits ``ramp.csv`` (one row per sample)
plus a per-step verdict so the knee is obvious without post-processing.

**One publisher per camera** is deliberate. An earlier version of this bench
fanned 30 cameras out of 6 mediamtx paths; the collapse it "found" at 30
cameras could not be separated from ~2 TCP sessions per camera piling into
one server process, which made the whole result unusable. A publisher per
camera costs a stream copy (no encode) and keeps the load generator off the
suspect list.
"""

from __future__ import annotations

import argparse
import base64
import csv
import hashlib
import hmac
import json
import os
import re
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

WORKDIR = os.environ.get("NEXUS_PERF_DIR", "/tmp/nexus-perf-bench")
API = os.environ.get("NEXUS_PERF_API", "http://127.0.0.1:8089/api/v1")
SECRET_PATH = os.environ.get("NEXUS_PERF_SECRET", "/var/lib/nexus/state/admin-secret")
RTSP_PORT = int(os.environ.get("NEXUS_PERF_RTSP_PORT", "9554"))
CAM_PREFIX = os.environ.get("NEXUS_PERF_CAM_PREFIX", "perfbench-")

# Last `frames_emitted` seen per camera id. Health is judged on the delta:
# the engine keys frame stats by camera id, never purges them on delete, and
# hands out small reusable integer ids — so a freshly created bench camera
# inherits the counters of whatever camera last held its id. Trusting
# `last_frame_age_ms` therefore reports a working camera as stale (or, worse,
# a dead one as healthy) depending on what the id used to be.
_prev_frames: dict[int, int] = {}


# --------------------------------------------------------------------------
# engine admin API
# --------------------------------------------------------------------------


def _b64(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def mint_token(ttl: int = 300) -> str:
    """HS256 bearer for the engine's admin API.

    The secret file ends in a newline. The engine trims it before keying the
    HMAC, so anything that signs over the raw bytes gets a 401 that looks
    exactly like a wrong secret.
    """
    with open(SECRET_PATH, "rb") as fh:
        key = fh.read().strip()
    now = int(time.time())
    header = _b64(json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(",", ":")).encode())
    claims = _b64(
        json.dumps(
            {"sub": "perf-bench", "iat": now, "exp": now + ttl}, separators=(",", ":")
        ).encode()
    )
    signing_input = f"{header}.{claims}".encode()
    sig = _b64(hmac.new(key, signing_input, hashlib.sha256).digest())
    return f"{header}.{claims}.{sig}"


def api(path: str, method: str = "GET", body: dict | None = None, timeout: int = 30):
    req = urllib.request.Request(
        API + path,
        method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={
            "Authorization": "Bearer " + mint_token(),
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        payload = resp.read()
    return json.loads(payload) if payload else None


def cameras() -> list[dict]:
    data = api("/admin/cameras")
    return data if isinstance(data, list) else (data or {}).get("cameras", [])


def bench_cameras() -> list[dict]:
    return [c for c in cameras() if str(c.get("name", "")).startswith(CAM_PREFIX)]


def create_camera(name: str, path: str) -> None:
    # Do NOT send "onvif": null — the engine rejects it with
    # `invalid type: null, expected struct CameraOnvif`. Omit the key.
    api(
        "/admin/cameras",
        "POST",
        {
            "name": name,
            "url": f"rtsp://127.0.0.1:{RTSP_PORT}/{path}",
            "enabled": True,
            "max_fps": 15,
            "codec": "h264",
            "prompts": [],
            "visual_prompts": [],
            "parking_lot_mode": False,
            "zones": [],
        },
    )


# --------------------------------------------------------------------------
# publishers
# --------------------------------------------------------------------------


def publisher_pids() -> list[int]:
    out = subprocess.run(
        ["pgrep", "-f", f"rtsp://127.0.0.1:{RTSP_PORT}/{CAM_PREFIX}"],
        capture_output=True,
        text=True,
    ).stdout
    return [int(p) for p in out.split()]


def start_publisher(path: str, offset: int) -> None:
    """One ffmpeg per camera, stream-copying the prebuilt clip in a loop.

    ``-ss`` staggers each camera into a different part of the clip so the box
    is not decoding 60 identical frame sequences in lockstep.
    """
    log = open(os.path.join(WORKDIR, f"pub-{path}.log"), "ab")
    subprocess.Popen(
        [
            "ffmpeg", "-hide_banner", "-loglevel", "warning",
            "-re", "-stream_loop", "-1", "-ss", str(offset),
            "-i", os.path.join(WORKDIR, "source.mp4"),
            "-c", "copy", "-f", "rtsp",
            "-rtsp_transport", "tcp",
            f"rtsp://127.0.0.1:{RTSP_PORT}/{path}",
        ],
        stdout=log,
        stderr=log,
        stdin=subprocess.DEVNULL,
        start_new_session=True,
    )


# --------------------------------------------------------------------------
# sampling
# --------------------------------------------------------------------------


def psi(resource: str) -> float:
    try:
        with open(f"/proc/pressure/{resource}") as fh:
            return float(fh.readline().split("avg10=")[1].split()[0])
    except Exception:
        return -1.0


def engine_rss_kb_and_threads() -> tuple[int, int]:
    try:
        pid = subprocess.run(
            ["pgrep", "-f", "nexus-engine"], capture_output=True, text=True
        ).stdout.split()[0]
        with open(f"/proc/{pid}/status") as fh:
            text = fh.read()
        rss = int(re.search(r"VmRSS:\s+(\d+)", text).group(1))
        threads = int(re.search(r"Threads:\s+(\d+)", text).group(1))
        return rss, threads
    except Exception:
        return -1, -1


def source_errors(window: str = "-1 min") -> tuple[int, int, int, int]:
    """Count the failures that mean the bench is lying to itself, plus the
    alert-clip encode load.

    A repeating-frame-cycle warning means the source clip lost its per-frame
    entropy; a data-stream error means the engine tore the source down. Either
    invalidates the rung.

    Alert-clip encodes are counted separately because they are a CPU libx264
    re-encode with burned-in boxes, running on the same cores as everything
    else. A box that saturates while its accelerator sits idle is usually
    clip-encode bound, not detection bound, and that distinction changes the
    remedy entirely.
    """
    try:
        out = subprocess.run(
            ["journalctl", "-u", "nexus-engine", "--since", window, "--no-pager", "-o", "cat"],
            capture_output=True,
            text=True,
            timeout=20,
        ).stdout
        return (
            out.count("repeating on a fixed cycle"),
            out.count("Internal data stream error"),
            out.count("alert-clip encode"),
            out.count("exceeded 45s deadline"),
        )
    except Exception:
        return -1, -1, -1, -1


def camera_health() -> tuple[int, int, int]:
    """(fresh, stalled, nodata) — fresh means the frame counter moved."""
    fresh = stalled = nodata = 0
    for cam in bench_cameras():
        cid = cam["id"]
        try:
            stats = api(f"/cameras/{cid}/stats", timeout=10)
        except Exception:
            stats = None
        if not stats:
            nodata += 1
            _prev_frames.pop(cid, None)
            continue
        emitted = int(stats.get("frames_emitted") or 0)
        previous = _prev_frames.get(cid)
        _prev_frames[cid] = emitted
        if previous is None:
            nodata += 1  # first observation of this id: no delta to judge yet
        elif emitted > previous:
            fresh += 1
        else:
            stalled += 1
    return fresh, stalled, nodata


def sample(n: int) -> dict:
    metrics = api("/system/metrics")
    hailo = metrics.get("hailo") or {}
    gpu = metrics.get("gpu") or {}
    rss_kb, threads = engine_rss_kb_and_threads()
    repeats, dserr, clip_encodes, clip_deadline_misses = source_errors()
    fresh, stalled, nodata = camera_health()
    stat = os.statvfs("/")
    return {
        "cameras": n,
        "publishers": len(publisher_pids()),
        "cpu_pct": round(metrics["cpu"]["usage_pct"], 1),
        "load1": metrics["cpu"].get("load_avg_1m"),
        "mem_used_gb": round(metrics["memory"]["used_bytes"] / 1e9, 2),
        "engine_rss_gb": round(rss_kb / 1e6, 2) if rss_kb > 0 else -1,
        "engine_threads": threads,
        "accel_util_pct": round(hailo.get("utilization_pct") or 0, 2),
        "accel_ips": round(hailo.get("inferences_per_sec") or 0, 1),
        "gpu_util_pct": gpu.get("utilization_pct"),
        "psi_cpu": psi("cpu"),
        "psi_io": psi("io"),
        "psi_mem": psi("memory"),
        "fresh": fresh,
        "stalled": stalled,
        "nodata": nodata,
        "repeat_warns": repeats,
        "datastream_errs": dserr,
        "clip_encodes": clip_encodes,
        "clip_deadline_misses": clip_deadline_misses,
        "free_gb": round(stat.f_bavail * stat.f_frsize / 1e9, 1),
    }


# --------------------------------------------------------------------------
# commands
# --------------------------------------------------------------------------


def cmd_run(args: argparse.Namespace) -> int:
    steps = [int(s) for s in args.steps.split(",")]
    csv_path = os.path.join(WORKDIR, "ramp.csv")
    rows: list[dict] = []
    knee: int | None = None

    existing = len(bench_cameras())
    for target in steps:
        for i in range(existing, target):
            path = f"{CAM_PREFIX}{i:03d}"
            start_publisher(path, offset=(i * 7) % 240)
            time.sleep(0.3)
            try:
                create_camera(path, path)
            except urllib.error.HTTPError as exc:
                print(f"camera {path} rejected: {exc.read().decode()[:200]}", flush=True)
        existing = max(existing, target)
        actual = len(bench_cameras())
        print(
            f"=== step target={target} cameras={actual} publishers={len(publisher_pids())} "
            f"settling {args.settle}s ===",
            flush=True,
        )
        time.sleep(args.settle)

        step_rows = []
        for _ in range(args.samples):
            row = sample(actual)
            step_rows.append(row)
            rows.append(row)
            print(json.dumps(row), flush=True)
            if row["free_gb"] < 10:
                print("ABORT: less than 10 GB free", flush=True)
                _write_csv(csv_path, rows)
                return 1
            time.sleep(args.interval)

        # A rung counts only if the cameras kept up on it. Judge on the last
        # half of the samples so a slow ramp-in doesn't fail an otherwise
        # healthy step.
        tail = step_rows[len(step_rows) // 2 :]
        bad = [r for r in tail if r["stalled"] + r["nodata"] > 0 or r["datastream_errs"] > 0]
        if bad:
            worst = max(tail, key=lambda r: r["stalled"] + r["nodata"])
            print(
                f"--- FAILED at {actual} cameras: "
                f"stalled={worst['stalled']} nodata={worst['nodata']} "
                f"datastream_errs={worst['datastream_errs']}",
                flush=True,
            )
            knee = actual
            break
        print(f"--- PASSED at {actual} cameras", flush=True)

    _write_csv(csv_path, rows)
    print(f"\nwrote {csv_path} ({len(rows)} samples)", flush=True)
    if knee is None:
        print(f"RESULT: sustained all {steps[-1]} cameras; raise --steps to find the knee")
        return 0
    passed = [s for s in steps if s < knee]
    last_good = passed[-1] if passed else 0
    print(f"RESULT: sustained {last_good} cameras; failed at {knee}")
    return 2


def _write_csv(path: str, rows: list[dict]) -> None:
    if not rows:
        return
    with open(path, "w", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=list(rows[0].keys()))
        writer.writeheader()
        writer.writerows(rows)


def cmd_status(_args: argparse.Namespace) -> int:
    cams = bench_cameras()
    print(f"bench cameras: {len(cams)}")
    print(f"publishers:    {len(publisher_pids())}")
    if cams:
        print(json.dumps(sample(len(cams)), indent=2))
    return 0


def cmd_teardown(_args: argparse.Namespace) -> int:
    for pid in publisher_pids():
        try:
            os.killpg(os.getpgid(pid), signal.SIGTERM)
        except Exception:
            pass
    removed = failed = 0
    for cam in bench_cameras():
        try:
            api(f"/admin/cameras/{cam['id']}", "DELETE")
            removed += 1
        except Exception as exc:
            print(f"delete {cam.get('name')} failed: {exc}", flush=True)
            failed += 1
    print(f"removed {removed} bench cameras ({failed} failed)")
    return 1 if failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)

    run = sub.add_parser("run")
    run.add_argument("--steps", default="10,20,30,45,60")
    run.add_argument("--settle", type=int, default=45, help="seconds after adding cameras")
    run.add_argument("--samples", type=int, default=8, help="samples per step")
    run.add_argument("--interval", type=int, default=15, help="seconds between samples")
    run.set_defaults(func=cmd_run)

    sub.add_parser("status").set_defaults(func=cmd_status)
    sub.add_parser("teardown").set_defaults(func=cmd_teardown)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())

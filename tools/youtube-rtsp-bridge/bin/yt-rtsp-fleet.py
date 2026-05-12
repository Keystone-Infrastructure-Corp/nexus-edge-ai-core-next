#!/usr/bin/env python3
"""yt-rtsp-fleet — supervise multiple yt-rtsp-bridge.sh processes with backoff.

Reads a JSON file like examples/streams.json:
    [
      { "name": "cam1", "url": "https://www.youtube.com/watch?v=..." },
      { "name": "cam2", "url": "https://www.youtube.com/watch?v=..." }
    ]
"""
from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
BRIDGE = ROOT / "yt-rtsp-bridge.sh"


def supervise(stream: dict[str, str]) -> None:
    name, url = stream["name"], stream["url"]
    backoff = 1.0
    while True:
        print(f"[fleet] starting {name} → {url}", flush=True)
        try:
            rc = subprocess.call([str(BRIDGE), url, name])
        except KeyboardInterrupt:
            return
        print(f"[fleet] {name} exited rc={rc}; sleeping {backoff:.1f}s", flush=True)
        time.sleep(backoff)
        backoff = min(backoff * 2.0, 60.0)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("config", type=Path, help="streams.json")
    args = parser.parse_args()
    streams = json.loads(args.config.read_text())
    pids: list[int] = []
    try:
        for s in streams:
            pid = os.fork()
            if pid == 0:
                supervise(s)
                sys.exit(0)
            pids.append(pid)
        for pid in pids:
            os.waitpid(pid, 0)
    except KeyboardInterrupt:
        for pid in pids:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    return 0


if __name__ == "__main__":
    sys.exit(main())

"""eval-labeler — judge open-vocab prompt precision against engine snapshots.

Pulls /api/cameras/<id>/frames/latest from a running engine, lets the user
mark each detection True / False / Skip, and at the end reports per-prompt
precision. Designed to be run *next to* a soak engine to grade prompt lists
without a full eval harness.
"""
from __future__ import annotations

import argparse
import csv
import io
import json
import sys
import time
import tkinter as tk
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from tkinter import messagebox

from PIL import Image, ImageDraw, ImageTk


@dataclass
class Snapshot:
    image: Image.Image
    objects: list[dict]


def fetch_snapshot(engine: str, camera: int) -> Snapshot:
    img_url = f"{engine}/api/cameras/{camera}/frames/latest"
    meta_url = f"{engine}/api/cameras/{camera}/frames/latest.json"
    img_bytes = urllib.request.urlopen(img_url, timeout=5).read()
    meta = json.loads(urllib.request.urlopen(meta_url, timeout=5).read())
    return Snapshot(image=Image.open(io.BytesIO(img_bytes)).convert("RGB"),
                    objects=meta.get("objects", []))


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--engine", default="http://127.0.0.1:8089")
    p.add_argument("--camera", type=int, required=True)
    p.add_argument("--out", default="labels.csv")
    p.add_argument("--max", type=int, default=50, help="stop after this many objects")
    args = p.parse_args()

    out_path = Path(args.out)
    if out_path.exists():
        if not messagebox.askyesno("overwrite?", f"{out_path} exists. Overwrite?"):
            return 1

    rows: list[dict] = []
    try:
        for _ in range(args.max):
            try:
                snap = fetch_snapshot(args.engine, args.camera)
            except Exception as e:
                print(f"snapshot fetch failed: {e}", file=sys.stderr)
                time.sleep(1)
                continue
            for obj in snap.objects:
                verdict = ask(snap, obj)
                if verdict is None:
                    break
                rows.append({"label": obj["label"], "confidence": obj["confidence"], "verdict": verdict})
            time.sleep(2)
    finally:
        with out_path.open("w", newline="") as fh:
            w = csv.DictWriter(fh, fieldnames=["label", "confidence", "verdict"])
            w.writeheader()
            w.writerows(rows)

    print(f"wrote {len(rows)} rows to {out_path}")
    summarize(rows)
    return 0


def ask(snap: Snapshot, obj: dict) -> str | None:
    win = tk.Toplevel()
    win.title(f"{obj['label']} · {obj['confidence']:.2f}")

    img = snap.image.copy()
    draw = ImageDraw.Draw(img)
    bb = obj["bbox"]
    draw.rectangle([bb["x1"], bb["y1"], bb["x2"], bb["y2"]], outline="cyan", width=3)
    img.thumbnail((900, 900))
    tk_img = ImageTk.PhotoImage(img)
    tk.Label(win, image=tk_img).pack()
    win.tk_img = tk_img  # keep reference

    result: dict[str, str | None] = {"v": None}

    def click(v: str | None) -> None:
        result["v"] = v
        win.destroy()

    bar = tk.Frame(win)
    bar.pack(fill=tk.X)
    for label, val in [("True (T)", "tp"), ("False (F)", "fp"), ("Skip (S)", "skip"), ("Quit (Q)", None)]:
        tk.Button(bar, text=label, command=lambda v=val: click(v)).pack(side=tk.LEFT, expand=True, fill=tk.X)
    win.bind("t", lambda _e: click("tp"))
    win.bind("f", lambda _e: click("fp"))
    win.bind("s", lambda _e: click("skip"))
    win.bind("q", lambda _e: click(None))
    win.wait_window()
    return result["v"]


def summarize(rows: list[dict]) -> None:
    by_label: dict[str, dict[str, int]] = {}
    for r in rows:
        b = by_label.setdefault(r["label"], {"tp": 0, "fp": 0, "skip": 0})
        b[r["verdict"]] = b.get(r["verdict"], 0) + 1
    print(f"{'label':<24}  {'precision':>10}  tp / (tp+fp)")
    for label, b in sorted(by_label.items()):
        denom = b["tp"] + b["fp"]
        prec = b["tp"] / denom if denom else 0.0
        print(f"{label:<24}  {prec:>10.2%}  {b['tp']} / {denom}")


if __name__ == "__main__":
    sys.exit(main())

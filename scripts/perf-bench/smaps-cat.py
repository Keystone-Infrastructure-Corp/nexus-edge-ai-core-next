#!/usr/bin/env python3
"""Categorise a process's resident memory by mapping, for leak attribution.

Splits /proc/<pid>/smaps RSS into named file mappings, the brk heap, thread
stacks, and anonymous mmaps, and histograms the anonymous mappings by virtual
size -- glibc's per-thread arenas are 64 MiB reservations, so a pile of 65536 kB
anon mappings is allocator fragmentation rather than a live allocation.

Usage:  smaps-cat.py <pid> [label]
        smaps-cat.py --diff <before.json> <after.json>
"""

from __future__ import annotations

import collections
import json
import re
import sys

MAP_RE = re.compile(r"^([0-9a-f]+)-([0-9a-f]+) (\S{4}) \S+ \S+ \S+\s*(.*)$")


def bucket(name: str) -> str:
    if not name:
        return "anon"
    if name in ("[heap]", "[stack]", "[vvar]", "[vdso]", "[vsyscall]"):
        return name
    if name.startswith("[stack:"):
        return "[stack]"
    if name.startswith("/dev/"):
        return "dev:" + name
    if name.startswith("/memfd") or name.startswith("/anon_hugepage"):
        return "memfd"
    if name.startswith("/"):
        return "file:" + name
    return name


def profile(pid: str) -> dict:
    rss = collections.Counter()
    count = collections.Counter()
    anon_hist = collections.Counter()
    cur = None
    cur_vsize = 0
    with open(f"/proc/{pid}/smaps", encoding="utf-8") as fh:
        for line in fh:
            m = MAP_RE.match(line)
            if m:
                start, end, _perms, name = m.groups()
                cur_vsize = (int(end, 16) - int(start, 16)) // 1024
                cur = bucket(name.strip())
                continue
            if cur is not None and line.startswith("Rss:"):
                kb = int(line.split()[1])
                rss[cur] += kb
                count[cur] += 1
                if cur == "anon":
                    anon_hist[cur_vsize] += 1
    return {
        "rss": dict(rss),
        "count": dict(count),
        "anon_vsize_hist": {str(k): v for k, v in anon_hist.items()},
        "total_kb": sum(rss.values()),
    }


def show(p: dict, label: str) -> None:
    print(f"=== {label}: total RSS {p['total_kb'] // 1024} MB ===")
    for k, v in sorted(p["rss"].items(), key=lambda kv: -kv[1])[:18]:
        print(f"{v // 1024:>7} MB  n={p['count'][k]:<5} {k}")
    print("--- anon mapping vsize histogram (top 10) ---")
    hist = sorted(p["anon_vsize_hist"].items(), key=lambda kv: -kv[1])[:10]
    for vsz, n in hist:
        print(f"    vsize={vsz:>9} kB   count={n}")


def diff(before: dict, after: dict) -> None:
    keys = set(before["rss"]) | set(after["rss"])
    rows = []
    for k in keys:
        d = after["rss"].get(k, 0) - before["rss"].get(k, 0)
        dn = after["count"].get(k, 0) - before["count"].get(k, 0)
        rows.append((d, dn, k))
    rows.sort(key=lambda r: -r[0])
    total = after["total_kb"] - before["total_kb"]
    print(f"=== RSS delta: {total // 1024:+} MB ===")
    for d, dn, k in rows:
        if abs(d) < 1024:
            continue
        print(f"{d // 1024:>+7} MB  n={dn:>+5}  {k}")
    print("--- anon vsize histogram delta ---")
    hk = set(before["anon_vsize_hist"]) | set(after["anon_vsize_hist"])
    hrows = [
        (
            after["anon_vsize_hist"].get(k, 0) - before["anon_vsize_hist"].get(k, 0),
            int(k),
        )
        for k in hk
    ]
    hrows.sort(key=lambda r: -r[0] * r[1])
    for dn, vsz in hrows[:10]:
        if dn == 0:
            continue
        print(f"    vsize={vsz:>9} kB   count {dn:+}   (~{dn * vsz // 1024:+} MB reserved)")


def main() -> int:
    if sys.argv[1:2] == ["--diff"]:
        with open(sys.argv[2], encoding="utf-8") as fh:
            before = json.load(fh)
        with open(sys.argv[3], encoding="utf-8") as fh:
            after = json.load(fh)
        show(before, "BEFORE")
        print()
        show(after, "AFTER")
        print()
        diff(before, after)
        return 0

    pid = sys.argv[1]
    label = sys.argv[2] if len(sys.argv) > 2 else pid
    p = profile(pid)
    show(p, label)
    out = f"/tmp/smaps-{label}.json"
    with open(out, "w", encoding="utf-8") as fh:
        json.dump(p, fh)
    print(f"[saved {out}]")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

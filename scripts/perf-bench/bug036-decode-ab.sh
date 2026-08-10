#!/usr/bin/env bash
# BUG-036 discriminator: is the retained RSS coming from the decode path?
#
# The flap test says ~39 MB is retained per RTSP pipeline built, with threads
# fully released. The two candidate owners are the hardware decode branch (VA
# surface pool behind `vah26Xdec ! vapostproc`) and the encoded tap branch
# (rtspsrc jitterbuffer + 200-buffer queue + 200-buffer appsink). They are
# separable without a rebuild: `[decode] mode` picks the decoder chain, so
# running the same flap under `va` and under `software` attributes the
# retention to one branch or the other.
#
# Run under sudo. Restores the original config and restarts the engine on exit.
set -uo pipefail

CONF=${CONF:-/etc/nexus/nexus.toml}
MODES=${MODES:-"va software"}
OUT=/tmp/bug036-decode-ab.log
BACKUP=/tmp/nexus.toml.bak.$$

[[ -r $CONF ]] || { echo "no config at $CONF"; exit 1; }
cp -a "$CONF" "$BACKUP"
restore() {
  cp -a "$BACKUP" "$CONF"
  systemctl restart nexus-engine
  echo "restored $CONF from $BACKUP" | tee -a "$OUT"
}
trap restore EXIT

set_mode() {
  python3 - "$CONF" "$1" <<'PY'
import re
import sys

path, mode = sys.argv[1], sys.argv[2]
src = open(path, encoding="utf-8").read()
if re.search(r"^\[runtime\.decode\]|^\[decode\]", src, re.M):
    src = re.sub(r'(?ms)(^\[(?:runtime\.)?decode\][^\[]*?^mode\s*=\s*)"[^"]*"',
                 rf'\1"{mode}"', src)
else:
    src = src.rstrip() + f'\n\n[runtime.decode]\nmode = "{mode}"\n'
open(path, "w", encoding="utf-8").write(src)
print(f"decode mode -> {mode}")
PY
}

: > "$OUT"
for mode in $MODES; do
  echo "=== decode mode: $mode ===" | tee -a "$OUT"
  set_mode "$mode" | tee -a "$OUT"
  systemctl restart nexus-engine
  sleep 20
  PROFILE=1 N=${N:-4} CYCLES=${CYCLES:-3} bash /tmp/bug036-flap.sh >/dev/null 2>&1
  {
    grep -E 'baseline|after 4 cameras|settled|RSS growth|teardown log' /tmp/bug036-flap.log
    echo "--- backend actually selected ---"
    journalctl -u nexus-engine --since "-8 min" --no-pager 2>/dev/null \
      | grep -o 'decode_backend=[^ ]*' | sort | uniq -c | head -5
  } | tee -a "$OUT"
  cp -a /tmp/bug036-flap.log "/tmp/bug036-flap-$mode.log"
  for f in baseline started settled; do
    [[ -r /tmp/smaps-$f.json ]] && cp -a "/tmp/smaps-$f.json" "/tmp/smaps-$mode-$f.json"
  done
done

echo "=== summary ===" | tee -a "$OUT"
grep -E '^=== decode mode|RSS growth' "$OUT"

#!/usr/bin/env bash
# BUG-036: name what survives a source teardown, using GStreamer's leaks tracer.
#
# The flap test proves ~4.7 GB is retained in anonymous mappings holding decoded
# pixel data, with every GStreamer thread released. That shape says objects are
# reaching NULL and then never being finalised. The leaks tracer answers which
# ones directly: it keeps a live-object table and, since 1.8, dumps it on
# SIGUSR1 -- so unlike the gst_deinit() report (which this engine never reaches,
# because it never calls gst_deinit) it works on a running process.
#
# Run under sudo. Removes its drop-in and restarts the engine on exit.
set -uo pipefail

DROPIN=/etc/systemd/system/nexus-engine.service.d/99-leaks-tracer.conf
LOG=/tmp/gst-leaks.log
OUT=/tmp/bug036-leaks.log

cleanup() {
  rm -f "$DROPIN"
  systemctl daemon-reload
  systemctl restart nexus-engine
  echo "removed $DROPIN, engine restarted" | tee -a "$OUT"
}
trap cleanup EXIT

: > "$OUT"
mkdir -p "$(dirname "$DROPIN")"
cat >"$DROPIN" <<EOF
[Service]
Environment=GST_TRACERS=leaks
Environment=GST_DEBUG=GST_TRACER:7
Environment=GST_DEBUG_FILE=$LOG
Environment=GST_DEBUG_NO_COLOR=1
EOF
systemctl daemon-reload
rm -f "$LOG"
systemctl restart nexus-engine
sleep 20

PID=$(pgrep -x nexus-engine)
echo "engine pid=$PID, tracer active=$(tr '\0' '\n' </proc/"$PID"/environ | grep -c GST_TRACERS)" | tee -a "$OUT"

# Two cycles is enough: the retention is per-session and shows up on the first.
PROFILE=0 N=${N:-4} CYCLES=${CYCLES:-2} bash /tmp/bug036-flap.sh >/dev/null 2>&1

PID=$(pgrep -x nexus-engine)
echo "--- flap done, dumping live objects (pid $PID) ---" | tee -a "$OUT"
kill -USR1 "$PID"
sleep 15

{
  echo "=== leak-report lines: $(grep -c . "$LOG" 2>/dev/null || echo 0) ==="
  echo "--- live object counts by type ---"
  grep -oE '\b(Gst[A-Za-z]+|[A-Za-z]*(Pipeline|Buffer|Memory|Sample|Caps|Pad|Bin))\b' "$LOG" 2>/dev/null \
    | sort | uniq -c | sort -rn | head -25
  echo "--- raw tail ---"
  tail -n 40 "$LOG" 2>/dev/null
} | tee -a "$OUT"

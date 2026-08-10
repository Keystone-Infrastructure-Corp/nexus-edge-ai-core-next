#!/usr/bin/env bash
# BUG-036 verification: does the reconciler survive a bulk delete of cameras
# whose sources died mid-read, and does the process give the memory back?
#
# Run on the edge box. Assumes the perf-bench rig (mediamtx + ffmpeg
# publishers) is already up at rtsp://127.0.0.1:9554/perfbench-NNN.
set -uo pipefail

API="http://127.0.0.1:8089/api/v1"
RIG=/tmp/nexus-perf-bench
N=${N:-6}
OUT=/tmp/bug036-verify.log

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT"; }

TOKEN=$(python3 - <<'PY'
import base64, hashlib, hmac, json, time
secret = open('/var/lib/nexus/state/admin-secret').read().strip().encode()
b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b'=').decode()
hdr = b64(json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(',', ':')).encode())
now = int(time.time())
pl = b64(json.dumps({"sub": "bug036", "iat": now, "exp": now + 3600},
                    separators=(',', ':')).encode())
msg = f"{hdr}.{pl}".encode()
print(f"{hdr}.{pl}." + b64(hmac.new(secret, msg, hashlib.sha256).digest()))
PY
) || { echo "cannot mint token"; exit 1; }

api() { # method path [body]
  local m=$1 p=$2 b=${3:-}
  if [[ -n $b ]]; then
    curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
         -d "$b" "$API$p"
  else
    curl -s -X "$m" -H "Authorization: Bearer $TOKEN" "$API$p"
  fi
}

engine_pid() { pgrep -x nexus-engine; }
rss_kb()  { awk '/VmRSS/{print $2}'    "/proc/$(engine_pid)/status"; }
threads() { awk '/Threads/{print $2}'  "/proc/$(engine_pid)/status"; }
sample()  { echo "rss_mb=$(( $(rss_kb) / 1024 )) threads=$(threads)"; }

bench_ids() {
  api GET /admin/cameras | python3 -c \
    'import json,sys; print(" ".join(str(c["id"]) for c in json.load(sys.stdin) if "perfbench" in c.get("url","")))'
}
frames() { api GET "/cameras/$1/stats" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("frames_emitted",0))'; }

start_publishers() { # n
  pkill -f 'ffmpeg .*perfbench-' 2>/dev/null; sleep 2
  for i in $(seq 0 $(($1 - 1))); do
    nohup ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -ss $((i * 7)) \
      -i "$RIG/source.mp4" -c copy -f rtsp -rtsp_transport tcp \
      "rtsp://127.0.0.1:9554/perfbench-$(printf '%03d' "$i")" >/dev/null 2>&1 &
  done
  sleep 10
  log "publishers up: $(pgrep -cf 'ffmpeg .*perfbench-')"
}

: > "$OUT"
MARK=$(date --iso-8601=seconds)
log "=== BUG-036 verification, engine $(readlink -f /opt/nexus/current) ==="

# Clear any bench cameras left by an earlier run so the counts mean something.
for id in $(bench_ids); do api DELETE "/admin/cameras/$id" >/dev/null; done
sleep 10
log "baseline $(sample)"
start_publishers "$N"

# 1. cameras against the live rig
log "--- creating $N cameras ---"
for i in $(seq 0 $((N - 1))); do
  slot=$(printf '%03d' "$i")
  api POST /admin/cameras "{\"name\":\"bug036-$slot\",\"url\":\"rtsp://127.0.0.1:9554/perfbench-$slot\",\"enabled\":true,\"codec\":\"h264\",\"max_fps\":15}" >/dev/null
done
sleep 45
IDS=$(bench_ids)
log "cameras: $IDS"
log "after start $(sample)"
FRESH=0
for id in $IDS; do a=$(frames "$id"); sleep 1; done
sleep 10
for id in $IDS; do (( $(frames "$id") > 0 )) && FRESH=$((FRESH + 1)); done
log "cameras emitting frames before the kill: $FRESH/$N"
if (( FRESH == 0 )); then
  log "ABORT: no camera ever got a source; the rig is broken, not the engine"
  exit 3
fi

# 2. kill every publisher so each source is parked mid-read — the trigger
log "--- killing publishers ---"
pkill -f 'ffmpeg .*perfbench-' ; sleep 8
log "after publisher kill $(sample)"

# 3. bulk delete: the call that used to wedge the reconciler
log "--- deleting $N cameras ---"
DEL_START=$(date +%s)
for id in $IDS; do api DELETE "/admin/cameras/$id" >/dev/null; done
DEL_MS=$(( ($(date +%s) - DEL_START) * 1000 ))
log "deletes returned in ${DEL_MS}ms; $(sample)"

# 4. the decisive assertion: does a NEW camera actually start?
log "--- restarting one publisher + creating a probe camera ---"
nohup ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -i "$RIG/source.mp4" \
      -c copy -f rtsp -rtsp_transport tcp rtsp://127.0.0.1:9554/perfbench-000 \
      >/dev/null 2>&1 &
sleep 5
api POST /admin/cameras '{"name":"bug036-probe","url":"rtsp://127.0.0.1:9554/perfbench-000","enabled":true,"codec":"h264","max_fps":15}' >/dev/null
sleep 30
PROBE=$(api GET /admin/cameras | python3 -c \
  'import json,sys; print(next((c["id"] for c in json.load(sys.stdin) if c["name"]=="bug036-probe"), ""))')
if [[ -z $PROBE ]]; then log "FAIL: probe camera was not created"; exit 1; fi
F1=$(frames "$PROBE"); sleep 15; F2=$(frames "$PROBE")
log "probe camera id=$PROBE frames $F1 -> $F2"
if (( F2 > F1 )); then
  log "PASS reconciler-alive: probe camera is emitting frames after the bulk delete"
  RC_REC=0
else
  log "FAIL reconciler-wedged: probe camera emitted no frames (delta $((F2 - F1)))"
  RC_REC=1
fi

# 5. memory: does the teardown actually give it back?
api DELETE "/admin/cameras/$PROBE" >/dev/null
pkill -f 'ffmpeg .*perfbench-'
sleep 60
log "settled $(sample)"

# 6. what the engine said about it — both were silent before the fix
log "--- reconciler lines since $MARK ---"
journalctl -u nexus-engine --since "$MARK" --no-pager 2>/dev/null | grep -ci reconcil | \
  xargs -I{} log "reconciler log lines: {}"
journalctl -u nexus-engine --since "$MARK" --no-pager 2>/dev/null | grep -c 'gst teardown blocked' | \
  xargs -I{} log "slow-teardown WARNs: {}"
journalctl -u nexus-engine --since "$MARK" --no-pager 2>/dev/null | grep 'gst teardown blocked' | tail -5 | tee -a "$OUT"

log "=== rc=$RC_REC ==="
exit $RC_REC

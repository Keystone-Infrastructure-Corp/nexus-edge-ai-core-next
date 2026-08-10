#!/usr/bin/env bash
# BUG-036: can glibc malloc tunables alone stop the steady-state growth?
#
# The control run showed RSS climbing 3413 -> 4925 MB over 200 s of *steady*
# operation, with teardown count irrelevant. That is the signature of glibc's
# dynamic mmap threshold. The RGB tap allocates a fresh ~1.8 MB frame buffer per
# frame (preroll_ingester.rs builds `Vec::with_capacity(width*height*3)`); at
# 15 fps across 4 cameras that is ~60 large allocations a second. glibc serves
# the first ones with mmap, but on free it RAISES M_MMAP_THRESHOLD to that size
# (up to 32 MB) and trim_threshold to twice it. From then on the same
# allocations come out of the arenas and are only returned to the OS if the
# arena top happens to hold a contiguous free run bigger than trim_threshold --
# which, interleaved with everything else the engine allocates, it never does.
# The arenas grow to the concurrency high-water and stay there.
#
# If that is the mechanism, pinning the threshold so frame-sized buffers always
# mmap (and therefore always munmap on free) should collapse the retention with
# no code change at all.
#
# Arms are (label, env) pairs. Each runs the zero-teardown soak, which is the
# reproducer -- flapping is not needed and only adds noise.
#
# Run under sudo. Removes its drop-in and restarts the engine on exit.
set -uo pipefail

DROPIN=/etc/systemd/system/nexus-engine.service.d/97-malloc-ab.conf
OUT=/tmp/bug036-malloc-ab.log
N=${N:-4}
SOAK=${SOAK:-200}

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT"; }

cleanup() {
  rm -f "$DROPIN"
  systemctl daemon-reload
  systemctl restart nexus-engine
  log "removed $DROPIN, engine restarted"
}
trap cleanup EXIT

# label:env-assignments (space-separated inside the field)
ARMS=${ARMS:-"mmap-threshold:MALLOC_MMAP_THRESHOLD_=131072 both:MALLOC_MMAP_THRESHOLD_=131072,MALLOC_ARENA_MAX=2"}

: > "$OUT"
log "=== malloc tunable A/B on $(readlink -f /opt/nexus/current), N=$N SOAK=${SOAK}s ==="

for spec in $ARMS; do
  label=${spec%%:*}
  envs=${spec#*:}
  mkdir -p "$(dirname "$DROPIN")"
  {
    echo '[Service]'
    IFS=',' read -ra kvs <<<"$envs"
    for kv in "${kvs[@]}"; do echo "Environment=$kv"; done
  } >"$DROPIN"
  systemctl daemon-reload
  log "=== arm: $label ($envs) ==="

  PROFILE=1 N="$N" CYCLES=0 SOAK="$SOAK" bash /tmp/bug036-flap.sh >/dev/null 2>&1
  grep -E 'baseline \(real|after .* cameras started|after soak|settled back|RSS growth' \
    /tmp/bug036-flap.log | sed "s/^/[$label] /" | tee -a "$OUT"
  cp -a /tmp/bug036-flap.log "/tmp/bug036-soak-$label.log"
done

log "=== summary (compare against the uncapped control: retained 4487 MB) ==="
grep -E 'RSS growth' "$OUT"

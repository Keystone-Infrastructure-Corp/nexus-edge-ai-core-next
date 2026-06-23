#!/usr/bin/env bash
# run-capped.sh — run a command with its output captured to a log file and only
# the tail surfaced. Keeps large build/test/diagnostic output out of an agent
# chat transcript (the dominant token-cost driver in this workspace). See the
# "Terminal output discipline (cost control)" section in AGENTS.md.
#
# Usage:
#   scripts/run-capped.sh <command> [args...]
#   TAIL=80 LOG=/tmp/my.log scripts/run-capped.sh cargo test --workspace
#
# Env:
#   LOG   path to the capture file        (default: /tmp/nexus-cmd.log)
#   TAIL  number of trailing lines shown   (default: 40)
#
# Exits with the wrapped command's own exit code.
set -uo pipefail

LOG="${LOG:-/tmp/nexus-cmd.log}"
TAIL="${TAIL:-40}"

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <command> [args...]" >&2
  exit 2
fi

"$@" >"$LOG" 2>&1
rc=$?

lines=$(wc -l <"$LOG" | tr -d ' ')
echo "── ${*} → exit ${rc}; ${lines} line(s) in ${LOG} (showing last ${TAIL}) ──"
tail -n "$TAIL" "$LOG"
exit "$rc"

#!/bin/bash
# PROTOTYPE -- NOT WIRED INTO THE VALIDATE DAG, DELIBERATELY.
#
# Records ONE guest N times as INDEPENDENT recordings, retaining each via
# --data-dir, then feeds the independently-produced DETLOGs to `hermit log-diff`
# pairwise. This is the producer that `log-diff` never had: the primitive
# already accepted two arbitrary logs, but nothing generated two independent
# recordings to hand it.
#
# WHY IT IS NOT IN THE DAG: gate.manifest is already over budget on devbig030.
# Adding measurement to the gate is the standing problem, not a fix for it.
#
# Usage: cross-recording-compare.sh <hermit> <guest> <N> <workdir>
set -uo pipefail
HERMIT="$1"; GUEST="$2"; N="${3:-3}"; WORK="$4"
mkdir -p "$WORK/rec" "$WORK/logs"
for i in $(seq 1 "$N"); do
  "$HERMIT" --log=info record --data-dir "$WORK/rec/r$i" -- "$GUEST" \
    > "$WORK/logs/r$i.out" 2> "$WORK/logs/r$i.log"
done
rc=0
for a in $(seq 1 "$N"); do
  for b in $(seq $((a+1)) "$N"); do
    if "$HERMIT" log-diff "$WORK/logs/r$a.log" "$WORK/logs/r$b.log" 2>&1 \
         | grep -q 'no substantive differences found'; then
      echo "AGREE    recording $a vs $b"
    else
      echo "DIVERGE  recording $a vs $b"; rc=1
    fi
  done
done
# Fail-closed: any divergence between independent recordings is a failure.
exit $rc

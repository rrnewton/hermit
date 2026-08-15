#!/bin/bash
# PROTOTYPE -- NOT WIRED INTO THE VALIDATE DAG, DELIBERATELY.
#
# Records ONE guest N times as INDEPENDENT recordings, retaining each via
# `record --data-dir`, then feeds the independently-produced DETLOGs to
# `hermit log-diff` pairwise. `log-diff` already accepted two arbitrary logs;
# nothing produced two independent recordings to hand it. This is that producer.
#
# POLICY IS MANDATORY AND NOT DEFAULTED. There is ONE comparator
# (detcore/src/logdiff.rs) with THREE policies that return OPPOSITE verdicts on
# identical files. A verdict without its policy is uninterpretable, so this
# script requires the policy explicitly and prints it with every result.
#
#   detlog      bare `log-diff`        DETLOG/COMMIT subset, strip_lines=false
#   stripped    --unsafe-strip-lines   erases numbers and temp paths
#   canonical   --canonical-info       canonical full INFO stream
#
# MEASURED BEHAVIOUR ON A KNOWN TIME LEAK (guest calling clock_gettime under
# `record`, which does not virtualize time), three independent recordings:
#   detlog     FLAGS it   -- differs on the clock_gettime value
#   stripped   MISSES it  -- the divergence is numeric-only and stripping erases it
#   canonical  FLAGS it   -- differs on the clock_gettime value AND on the
#                            per-recording replay id AND on scheduler-startup
#                            line ordering (6 differing lines, 2 clock-bearing)
# DO NOT USE `stripped` FOR THIS QUESTION. It cannot see the defect the check exists for.
#
# On a time-INDEPENDENT guest the same three recordings agree under detlog and
# stripped; under canonical they differ ONLY on the per-recording replay id
# (11214-byte streams, 2 differing lines). Canonical therefore cannot currently
# return a clean verdict for ANY cross-recording pair, because that id is unique
# by construction -- a real limitation of using canonical for this purpose, and
# separate from whether it can see a genuine defect (it can).
#
# Usage: cross-recording-compare.sh <hermit> <guest> <N> <workdir> <detlog|stripped|canonical>
set -uo pipefail
HERMIT="$1"; GUEST="$2"; N="${3:-3}"; WORK="$4"; POLICY="${5:?policy required: detlog|stripped|canonical}"
case "$POLICY" in
  detlog)    FLAG="" ;;
  stripped)  FLAG="--unsafe-strip-lines" ;;
  canonical) FLAG="--canonical-info" ;;
  *) echo "unknown policy: $POLICY (expected detlog|stripped|canonical)" >&2; exit 2 ;;
esac
if [ "$POLICY" = stripped ]; then
  echo "WARNING[policy=stripped]: measured to MISS a numeric-only cross-recording divergence." >&2
fi
mkdir -p "$WORK/rec" "$WORK/logs"
for i in $(seq 1 "$N"); do
  "$HERMIT" --log=info record --data-dir "$WORK/rec/r$i" -- "$GUEST" \
    > "$WORK/logs/r$i.out" 2> "$WORK/logs/r$i.log"
done
rc=0
for a in $(seq 1 "$N"); do
  for b in $(seq $((a+1)) "$N"); do
    if "$HERMIT" log-diff $FLAG "$WORK/logs/r$a.log" "$WORK/logs/r$b.log" 2>&1 \
         | grep -q 'no substantive differences found'; then
      echo "[policy=$POLICY] AGREE    recording $a vs $b"
    else
      echo "[policy=$POLICY] DIVERGE  recording $a vs $b"; rc=1
    fi
  done
done
exit $rc

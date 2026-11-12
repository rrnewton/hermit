#!/bin/bash
# Matched-argv record/replay on MAIN hermit (b4ce9e92), guest 25000 25000 both sides.
HB=/home/newton/work/dev-hermit/worktrees/slots/musedrain-20261101/temp/cargo-target-base/debug/hermit
G=/home/newton/work/dev-hermit/worktrees/slots/musedrain-20261101/temp/5252-exp/guest
E=/home/newton/work/dev-hermit/worktrees/slots/musedrain-20261101/temp/5252-exp
run_one() {
  local s=$1 d=$E/rr-$1
  mkdir -p "$d"
  HERMIT_EPOCH=2020-01-01T00:00:00Z "$HB" run --strict --chaos --seed $s --sched-seed $s --target-timeslice 20000 --max-timeslice 100000 --record-preemptions-to "$d/rec.tar" -- "$G" 25000 25000 >"$d/rec.out" 2>"$d/rec.err"
  local rc=$?
  HERMIT_EPOCH=2020-01-01T00:00:00Z "$HB" run --strict --chaos --seed $s --sched-seed $s --target-timeslice 20000 --max-timeslice 100000 --replay-preemptions-from "$d/rec.tar" -- "$G" 25000 25000 >"$d/rep.out" 2>"$d/rep.err"
  local rp=$?
  echo "seed=$s record_rc=$rc replay_rc=$rp"
}
export -f run_one; export HB G E
seq 1 20 | xargs -P4 -n1 bash -c 'run_one "$@"' _ 2>&1 | sort -t= -k2 -n

#!/bin/bash
# Test-form matched-argv experiment on MAIN (b4ce9e92), guest 25000 both sides.
cd /home/newton/work/dev-hermit/worktrees/slots/musedrain-20261101/temp/wt-5252-base
export CARGO_TARGET_DIR=/home/newton/work/dev-hermit/worktrees/slots/musedrain-20261101/temp/cargo-target-base
E=/home/newton/work/dev-hermit/worktrees/slots/musedrain-20261101/temp/5252-exp
run_one() {
  local n=$1
  cargo test -p hermit-run --features liteinst --test cli a_chaos_instruction_offset_run_verifies_and_replays_at_its_offsets -- --exact >"$E/tf-$n.log" 2>&1
  if grep -q "1 passed" "$E/tf-$n.log"; then echo "run=$n PASS"; else echo "run=$n FAIL"; fi
}
export -f run_one; export E
seq 1 20 | xargs -P4 -n1 bash -c 'run_one "$@"' _ 2>&1 | sort -t= -k2 -n

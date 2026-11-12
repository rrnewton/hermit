#!/bin/bash
cd /home/newton/work/dev-hermit/worktrees/slots/musedrain-20261101/temp/wt-322bp
export CARGO_TARGET_DIR=/home/newton/work/dev-hermit/worktrees/slots/musedrain-20261101/temp/cargo-target-322bp
BIN=$(ls $CARGO_TARGET_DIR/debug/deps/cli-* | grep -v '\.d$' | head -1)
echo "BIN=$BIN"
run_one() {
  local spec=$1 t=${spec%%:*} r=${spec##*:}
  if $BIN "$t" --exact > /tmp/run-$t-$r.log 2>&1 && grep -q "1 passed" /tmp/run-$t-$r.log; then
    echo "round=$r $t PASS"
  else
    echo "round=$r $t FAIL"
  fi
}
export -f run_one; export BIN
python3 -c "
tests=['run_kvm_timerfd_virtual_abstime','run_kvm_timerfd_virtual_nonblock','run_kvm_timerfd_virtual_scm_rights_epoll_is_refused_by_name']
print('\n'.join(f'{t}:{r}' for r in range(1,6) for t in tests))
" | xargs -P3 -n1 bash -c 'run_one $1' _ | sort
ls $CARGO_TARGET_DIR/tmp/timerfd-virtual-time-fixture-*/ 2>/dev/null

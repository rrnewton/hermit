#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

known_gaps_ok=0
if [[ ${1:-} == --known-gaps-ok ]]; then
  known_gaps_ok=1
  shift
fi
(($# == 0)) || fail "usage: $0 [--known-gaps-ok]"

known_gap() {
  [[ $1 == quick-wins || $1 == resources ]]
}

cases=(
  'quick-wins|tests/c/syscall_quick_wins.c|'
  'file-io|tests/c/syscall_file_io.c|'
  'file-metadata|tests/c/syscall_file_metadata.c|'
  'writev|tests/c/writev_determinism.c|'
  'epoll-multi|tests/c/epoll_determinism.c|multi'
  'epoll-edge|tests/c/epoll_determinism.c|edge'
  'epoll-oneshot|tests/c/epoll_determinism.c|oneshot'
  'epoll-mixed|tests/c/epoll_determinism.c|mixed'
  'epoll-nested|tests/c/epoll_determinism.c|nested'
  'epoll-control-fds|tests/c/epoll_determinism.c|control-fds'
  'epoll-dupfd|tests/c/epoll_determinism.c|dupfd'
  'mmap-multiple|tests/c/mmap_determinism.c|multiple'
  'mmap-fixed|tests/c/mmap_determinism.c|fixed'
  'mmap-heap|tests/c/mmap_determinism.c|heap'
  'mmap-shared|tests/c/mmap_determinism.c|shared'
  'mmap-reuse|tests/c/mmap_determinism.c|reuse'
  'resources|tests/c/resource_determinism.c|'
  'ipc-pipe-order|tests/c/ipc_determinism.c|pipe-order'
  'ipc-pipe-capacity|tests/c/ipc_determinism.c|pipe-capacity'
  'ipc-socketpair|tests/c/ipc_determinism.c|socketpair'
  'ipc-eventfd|tests/c/ipc_determinism.c|eventfd'
  'ipc-epoll|tests/c/ipc_determinism.c|epoll'
  'arch-prctl|tests/c/arch_prctl_determinism.c|'
  'getitimer|tests/c/getitimer_determinism_probe.c|'
  'setitimer|tests/c/setitimer_determinism.c|'
  'posix-timer|tests/c/timer_create_determinism.c|'
)

failures=0
expected_failures=0
passed=0
for entry in "${cases[@]}"; do
  IFS='|' read -r name source_file arguments <<<"$entry"
  read -r -a guest_args <<<"$arguments"
  compile_flags=(-lrt)
  if [[ $source_file == tests/c/epoll_determinism.c ||
        $source_file == tests/c/ipc_determinism.c ||
        $source_file == tests/c/resource_determinism.c ]]; then
    compile_flags+=(-D_GNU_SOURCE)
  fi
  guest=$(compile_c "$source_file" "syscall-$name" "${compile_flags[@]}")
  if ! verify_guest "syscall target: $name" "$guest" "${guest_args[@]}"; then
    if ((known_gaps_ok == 1)) && known_gap "$name"; then
      printf 'XFAIL: syscall target %s is a documented current-main product gap\n' "$name"
      expected_failures=$((expected_failures + 1))
    else
      failures=$((failures + 1))
    fi
  else
    passed=$((passed + 1))
  fi
done

((failures == 0)) || fail "$failures syscall target(s) failed strict L2"
printf '\nPASS: targeted syscall matrix (%d passed, %d documented XFAIL, ptrace L2, log=info)\n' \
  "$passed" "$expected_failures"

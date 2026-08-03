#!/usr/bin/env bash

set -euo pipefail

stress_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
mode=${1:-portable}

case $mode in
  portable)
    tests=(examples.sh random.sh thread-racing.sh time-clock.sh pid-tid.sh pipe-chain.sh)
    ;;
  occasional)
    tests=(signals.sh syscalls.sh)
    ;;
  all)
    tests=(examples.sh random.sh thread-racing.sh time-clock.sh pid-tid.sh signals.sh pipe-chain.sh syscalls.sh)
    ;;
  *)
    echo "usage: $0 [portable|occasional|all]" >&2
    exit 2
    ;;
esac
readonly mode
readonly -a tests

failures=0
for test_script in "${tests[@]}"; do
  printf '\n==================== %s ====================\n' "$test_script"
  test_args=()
  if [[ $mode == occasional && ( $test_script == signals.sh || $test_script == syscalls.sh ) ]]; then
    test_args=(--known-gaps-ok)
  fi
  if ! "$stress_dir/$test_script" "${test_args[@]}"; then
    printf 'FAIL: %s\n' "$test_script" >&2
    failures=$((failures + 1))
  fi
done

if ((failures > 0)); then
  printf '\nFAIL: %d determinism stress categor%s failed\n' \
    "$failures" "$([[ $failures -eq 1 ]] && printf 'y' || printf 'ies')" >&2
  exit 1
fi

printf '\nPASS: complete %s targeted determinism stress suite\n' "$mode"

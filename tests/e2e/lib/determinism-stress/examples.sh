#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/lib/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

expected=(date.sh devrand.sh race.sh rand.py timed-progress-bar.py)
# Enumerate through GIT, not a bare filesystem walk, for the same reason
# ci/test_harness.sh audit_inventory does (61edbef42).
#
# `find` reported every file ON DISK, so any ignored output landing in
# examples/ tripped this strict-equality gate. That is not hypothetical here:
# `*.preempts` is gitignored and Hermit itself WRITES preemption recordings, so
# recording against an example program reds the next determinism-stress run --
# through a file .gitignore hides, so `git status` still reads clean and the
# tree looks pristine. `.tmp*` and `*.orig` (merge leftovers) are the same trap.
#
# `--cached --others --exclude-standard` is tracked files PLUS genuinely new
# untracked ones, MINUS ignored output, so an unclassified NEW example program
# is still caught -- that is the whole point of the gate and it must not be
# relaxed. The pathspec plus the `[^/]+$` filter reproduce `-maxdepth 1 -type f`.
mapfile -t actual < <(
  git -C "$repo_root" ls-files --cached --others --exclude-standard -- examples \
    | grep -E '^examples/[^/]+$' \
    | sed 's|^examples/||' \
    | grep -vx 'README.md' \
    | LC_ALL=C sort
)
if [[ ${actual[*]} != "${expected[*]}" ]]; then
  printf 'expected example programs: %s\n' "${expected[*]}" >&2
  printf 'actual example programs:   %s\n' "${actual[*]}" >&2
  fail "examples manifest changed; classify every program in examples.sh"
fi

failures=0
for example in "${expected[@]}"; do
  program=$repo_root/examples/$example
  show_native_variation "example/$example" "$program"
  if ! verify_guest "example/$example" "$program"; then
    failures=$((failures + 1))
  fi
done

((failures == 0)) || fail "$failures example program(s) failed strict L2"
stress_success "all examples/ programs"

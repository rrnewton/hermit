#!/usr/bin/env bash
set -u
for i in $(seq 1 80); do
  git rev-parse -q --verify REBASE_HEAD >/dev/null 2>&1 || { git rebase --show-current-patch >/dev/null 2>&1 && true; }
  if [ ! -d .git/rebase-merge ] && [ ! -d .git/rebase-apply ]; then echo REBASE-DONE; exit 0; fi
  conf=$(git diff --name-only --diff-filter=U)
  if [ -z "$conf" ]; then GIT_EDITOR=true git rebase --continue >/dev/null 2>&1; continue; fi
  real=$(echo "$conf" | grep -v -x -e ci/dag/validate.json -e ci/manifest-plan/src/validation_dag_static.rs -e Cargo.lock -e ci/test-footprints.json || true)
  if [ -n "$real" ]; then echo "REAL-CONFLICT: $real"; git log --oneline -1 REBASE_HEAD 2>/dev/null; exit 3; fi
  git checkout --ours ci/dag/validate.json ci/manifest-plan/src/validation_dag_static.rs Cargo.lock ci/test-footprints.json 2>/dev/null
  git add ci/dag/validate.json ci/manifest-plan/src/validation_dag_static.rs Cargo.lock ci/test-footprints.json 2>/dev/null
  out=$(GIT_EDITOR=true git rebase --continue 2>&1 | tail -1); echo "$out" | grep -q "nothing to commit" && git rebase --skip >/dev/null 2>&1
done
echo LOOP-EXHAUSTED; exit 4

#!/usr/bin/env bash
# pick PR range; resolve ONLY generated-DAG conflicts by ours+regen-later; stop on real conflicts
set -u
n=$1
mb=$(git merge-base origin/main refs/pr/h/$n)
git cherry-pick $mb..refs/pr/h/$n && exit 0
for i in $(seq 1 40); do
  git rev-parse -q --verify CHERRY_PICK_HEAD >/dev/null || exit 0
  conf=$(git diff --name-only --diff-filter=U)
  real=$(echo "$conf" | grep -v -x -e ci/dag/validate.json -e ci/manifest-plan/src/validation_dag_static.rs -e Cargo.lock || true)
  if [ -n "$real" ]; then echo "REAL-CONFLICT $n: $real"; exit 3; fi
  git checkout --ours ci/dag/validate.json ci/manifest-plan/src/validation_dag_static.rs Cargo.lock 2>/dev/null
  git add ci/dag/validate.json ci/manifest-plan/src/validation_dag_static.rs Cargo.lock 2>/dev/null
  GIT_EDITOR=true git cherry-pick --continue >/dev/null 2>&1 || true
done
echo "LOOP-EXHAUSTED $n"; exit 4

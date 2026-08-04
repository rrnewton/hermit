#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
SHELL_CLASSIFIER="$ROOT_DIR/scripts/classify-required-check.sh"
PYTHON_CLASSIFIER="$ROOT_DIR/agent-utils/py/ci_hub_check_outcome.py"

check() {
    local expected=$1 status=$2 conclusion=$3 python_result shell_result
    python_result=$("$PYTHON_CLASSIFIER" --status "$status" --conclusion "$conclusion")
    shell_result=$("$SHELL_CLASSIFIER" "$status" "$conclusion")
    [[ $python_result == "$expected" && $shell_result == "$expected" ]] || {
        echo "mismatch: $status/$conclusion expected=$expected python=$python_result shell=$shell_result" >&2
        exit 1
    }
}

check PASSED completed success
check PASSED "" success
for conclusion in failure timed_out error startup_failure; do
    check FAILED completed "$conclusion"
done
while IFS=: read -r status conclusion; do
    check NO_RESULT "$status" "$conclusion"
done <<'EOF'
completed:cancelled
completed:skipped
completed:neutral
completed:stale
completed:action_required
queued:
in_progress:
waiting:
requested:
missing:
completed:future_state
EOF

fixture='[{"statusCheckRollup":[{"status":"COMPLETED","conclusion":"CANCELLED"},{"state":"SUCCESS"}]}]'
annotated=$(printf '%s' "$fixture" | "$PYTHON_CLASSIFIER" --annotate-rollups)
[[ $(jq -r '.[0].statusCheckRollup[0]._checkOutcome' <<<"$annotated") == NO_RESULT ]]
[[ $(jq -r '.[0].statusCheckRollup[1]._checkOutcome' <<<"$annotated") == PASSED ]]

echo "PASS: one classifier handles N=2 passed, N=4 failed, N=11 no-result and rollups"

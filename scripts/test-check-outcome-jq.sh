#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
JQ_DIR="$ROOT_DIR/scripts"
SHELL_CLASSIFIER="$ROOT_DIR/scripts/classify-required-check.sh"

check() {
    local expected=$1 status=$2 conclusion=$3 jq_result shell_result
    jq_result=$(jq -n -L "$JQ_DIR" --arg status "$status" --arg conclusion "$conclusion" \
        'include "check_outcome"; {$status, $conclusion} | check_outcome')
    jq_result=${jq_result//\"/}
    shell_result=$("$SHELL_CLASSIFIER" "$status" "$conclusion")
    [[ $jq_result == "$expected" && $shell_result == "$expected" ]] || {
        echo "mismatch: $status/$conclusion expected=$expected jq=$jq_result shell=$shell_result" >&2
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

echo "PASS: jq and shell agree on N=2 passed, N=4 failed, N=11 no-result"

#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Pin the fail-closed wiring around require-check-success.sh. The exhaustive
# state test proves the predicate; this lint proves merge-gate still uses it.
set -euo pipefail

ROOT_DIR=${1:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}
WORKFLOW="$ROOT_DIR/.github/workflows/merge-gate.yml"

function fail {
    echo "check-merge-gate-policy.sh: $*" >&2
    exit 1
}

[[ -f $WORKFLOW ]] || fail "missing $WORKFLOW"
[[ $(grep -Fxc '  cancel-in-progress: false' "$WORKFLOW") == 1 ]] ||
    fail "required merge-gate runs must never cancel one another"
[[ $(grep -Fc 'if require_check_success' "$WORKFLOW") == 2 ]] ||
    fail "portable and demo required checks must both use the success-only predicate"
grep -Fq "if [ \"\$PROTOCOL_RESULT\" != success ]; then" "$WORKFLOW" ||
    fail "core-review-protocol must require success"
grep -Fq "if [ \"\$SAFETY_RESULT\" != success ]; then" "$WORKFLOW" ||
    fail "merge safety guards must require success"
if grep -Fq "if [ \"\$locally_validated\" = true ]; then" "$WORKFLOW"; then
    fail "locally-validated must not bypass required GitHub checks"
fi

echo "check-merge-gate-policy.sh: OK - all required states fail closed"

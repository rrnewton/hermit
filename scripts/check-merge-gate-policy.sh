#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Pin the workflow wiring around the tested trinary predicate. The exhaustive
# state test proves the predicate; this lint proves every gate leg uses it.
set -euo pipefail

ROOT_DIR=${1:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}
WORKFLOW="$ROOT_DIR/.github/workflows/merge-gate.yml"

fail() {
    echo "check-merge-gate-policy.sh: $*" >&2
    exit 1
}

[[ -f $WORKFLOW ]] || fail "missing $WORKFLOW"
grep -Fq 'actions: write' "$WORKFLOW" || fail "NO_RESULT must be able to re-dispatch and cancel"
grep -Fq 'ref: f9e61247e83bb07c11297541b591606de24a89a8' "$WORKFLOW" ||
    fail "gate must pin the parent authority commit"
grep -Fq '.dev-hermit-policy/ci-hub/check_outcome.py' "$WORKFLOW" ||
    fail "gate must call the parent check-status authority"
grep -Fq 'agent-utils/py/ci_hub_check_outcome.py' "$ROOT_DIR/scripts/classify-required-check.sh" ||
    fail "local shell adapter must delegate to the parent status authority"
grep -Fq 'from ci_hub_check_outcome import' "$ROOT_DIR/scripts/pr_status.py" ||
    fail "PR rollup must import the parent-authority adapter"
grep -Fq 'agent-utils/py/ci_hub_check_outcome.py" --annotate-rollups' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "lander rollup must call the parent-authority adapter"
[[ ! -e $ROOT_DIR/scripts/check_outcome.jq ]] ||
    fail "duplicate jq status classifier must not exist"
[[ ! -e $ROOT_DIR/scripts/check_status_outcome.py ]] ||
    fail "duplicate Hermit status adapter must not exist"
grep -Fq '.dev-hermit-policy/ci-hub/validation/verify_receipt.sh' "$WORKFLOW" ||
    fail "local alternate leg must call the parent receipt verifier"
if grep -Eq 'scripts/(check|verify)-local-validation' "$WORKFLOW"; then
    fail "gate must not call a PR-local validation-evidence verifier"
fi
grep -Fq 'NO_RESULT)' "$WORKFLOW" || fail "gate must handle NO_RESULT explicitly"
grep -Fq 'dispatch_no_result' "$WORKFLOW" || fail "NO_RESULT must re-dispatch"
grep -Fq 'cancel_no_result_gate' "$WORKFLOW" || fail "NO_RESULT must not exit red or green"
grep -Fq '/force-cancel' "$WORKFLOW" || fail "if: always() gate requires force-cancel for NO_RESULT"
grep -Fq 'GATE_RUN_ID' "$WORKFLOW" || fail "self-cancellation must identify the exact gate run"
if grep -Eq 'success[[:space:]]*\|[[:space:]]*skipped|success[[:space:]]+or[[:space:]]+skipped' "$WORKFLOW"; then
    fail "skipped must never satisfy a required check"
fi

echo "check-merge-gate-policy.sh: OK - PASSED/FAILED/NO_RESULT gate wiring enforced"

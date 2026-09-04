#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
CLASSIFIER="$ROOT_DIR/scripts/classify-required-check.sh"

# ⚠️ THE AUTHORITY MUST BE REACHABLE BEFORE ANY CASE HERE MEANS ANYTHING. If it
# is not, every case below is unevaluable, so this declares that and exits 0
# rather than failing. `make lint-checks` then still passes and
# ci/lint-checks-node.sh classifies the run no_result (exit 75), which the
# ledger records as no_result rather than fail.
#
# ⚠️ EXIT 0 IS REQUIRED HERE, NOT MERELY TIDY. classify_run in
# ci/lint-checks-node.sh deliberately lets a real failure outrank any marker --
# "A REAL FAILURE ALWAYS WINS" -- so a nonzero exit could never be reported as
# no_result no matter what this printed. The marker channel only works on a
# passing target.
#
# Only exit 3 from the probe takes this path; see scripts/authority-available.sh
# for why 3 cannot be a refusal, a bug, or a tampered authority.
if ! "$ROOT_DIR/scripts/authority-available.sh"; then
    echo "NO-RESULT-CASE: test-required-check-outcomes.sh: the pinned check-status authority could not be consulted; no case in this checker was evaluated"
    exit 0
fi

# N=2 legitimate GitHub pass representations remain PASSED.
[[ $("$CLASSIFIER" completed success) == PASSED ]]
[[ $("$CLASSIFIER" "" success) == PASSED ]]

# N=4 conclusions contain a genuine failed result.
for conclusion in failure timed_out error startup_failure; do
    [[ $("$CLASSIFIER" completed "$conclusion") == FAILED ]] || exit 1
done

# N=12 terminal, active, absent, and unknown representations have NO_RESULT.
for state in \
    completed:cancelled completed:skipped completed:neutral \
    completed:stale completed:action_required queued:none \
    in_progress:none waiting:none requested:none pending:none missing:none \
    completed:future_state; do
    status=${state%%:*}
    conclusion=${state#*:}
    [[ $conclusion != none ]] || conclusion=
    [[ $("$CLASSIFIER" "$status" "$conclusion") == NO_RESULT ]] || exit 1
done

echo "PASS: N=2 passed, N=4 failed, N=12 no-result"

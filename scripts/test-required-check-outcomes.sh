#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
CLASSIFIER="$ROOT_DIR/scripts/classify-required-check.sh"
EVIDENCE="$ROOT_DIR/scripts/check-local-validation-evidence.sh"
SHA=0123456789abcdef0123456789abcdef01234567

# N=2 legitimate GitHub pass representations remain PASSED.
[[ $("$CLASSIFIER" completed success) == PASSED ]]
[[ $("$CLASSIFIER" "" success) == PASSED ]]

# N=4 conclusions contain a genuine failed result.
for conclusion in failure timed_out error startup_failure; do
    [[ $("$CLASSIFIER" completed "$conclusion") == FAILED ]] || exit 1
done

# N=11 terminal, active, absent, and unknown representations have NO_RESULT.
for state in \
    completed:cancelled completed:skipped completed:neutral \
    completed:stale completed:action_required queued:none \
    in_progress:none waiting:none requested:none missing:none \
    completed:future_state; do
    status=${state%%:*}
    conclusion=${state#*:}
    [[ $conclusion != none ]] || conclusion=
    [[ $("$CLASSIFIER" "$status" "$conclusion") == NO_RESULT ]] || exit 1
done

body="[impl agent, validate.sh]

Local validation passed - locally-validated label applied.

- SHA: \`$SHA\`
- Profile: \`full\`
- Results: 42 checks passed, 0 failed
- Hostname: \`devbig014\`
- Log: \`devbig014:/tmp/hermit-validate.slot247.log\`
- Timestamp (UTC): \`2026-08-04T20:00:00Z\`

<!-- locally-validated-evidence sha=$SHA profile=full host=devbig014 log=/tmp/hermit-validate.slot247.log ts=2026-08-04T20:00:00Z -->"
comments=$(jq -n --arg body "$body" '[{body:$body}]')
printf '%s\n' "$comments" | "$EVIDENCE" "$SHA" >/dev/null

for mutation in wrong-sha partial-profile missing-log failed-result; do
    case "$mutation" in
        wrong-sha) invalid=${body//$SHA/ffffffffffffffffffffffffffffffffffffffff} ;;
        partial-profile) invalid=${body/Profile: \`full\`/Profile: \`affected\`} ;;
        missing-log) invalid=$(sed '/^- Log:/d' <<<"$body") ;;
        failed-result) invalid=${body/0 failed/1 failed} ;;
    esac
    invalid_comments=$(jq -n --arg body "$invalid" '[{body:$body}]')
    if printf '%s\n' "$invalid_comments" | "$EVIDENCE" "$SHA" >/dev/null 2>&1; then
        echo "FAIL: accepted $mutation local-validation evidence" >&2
        exit 1
    fi
done

echo "PASS: N=2 passed, N=4 failed, N=11 no-result; local substitute is exact-head full evidence"

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

[[ $("$CLASSIFIER" completed success) == success ]]
for conclusion in failure timed_out error action_required startup_failure stale future_state; do
    [[ $("$CLASSIFIER" completed "$conclusion") == hard-failure ]] || exit 1
done
for state in \
    completed:cancelled completed:skipped completed:neutral \
    queued:none in_progress:none waiting:none missing:none; do
    status=${state%%:*}
    conclusion=${state#*:}
    [[ $conclusion != none ]] || conclusion=
    [[ $("$CLASSIFIER" "$status" "$conclusion") == substitutable ]] || exit 1
done

body="[impl agent, validate.sh]

Local validation passed - locally-validated label applied.

- SHA: \`$SHA\`
- Profile: \`full\`
- Results: 42 checks passed, 0 failed
- Hostname: \`devbig014\`
- Log: \`devbig014:/tmp/hermit-validate.slot247.log\`
- Timestamp (UTC): \`2026-08-03T13:00:00Z\`

<!-- locally-validated-evidence sha=$SHA profile=full host=devbig014 log=/tmp/hermit-validate.slot247.log ts=2026-08-03T13:00:00Z -->"
comments=$(jq -n --arg body "$body" '[{body:$body}]')
printf '%s\n' "$comments" | "$EVIDENCE" "$SHA" >/dev/null

for mutation in wrong-sha missing-log failed-result; do
    case "$mutation" in
        wrong-sha) invalid=${body//$SHA/ffffffffffffffffffffffffffffffffffffffff} ;;
        missing-log) invalid=$(sed '/^- Log:/d' <<<"$body") ;;
        failed-result) invalid=${body/0 failed/1 failed} ;;
    esac
    invalid_comments=$(jq -n --arg body "$invalid" '[{body:$body}]')
    if printf '%s\n' "$invalid_comments" | "$EVIDENCE" "$SHA" >/dev/null 2>&1; then
        echo "FAIL: accepted $mutation local-validation evidence" >&2
        exit 1
    fi
done

echo "PASS: hard failures block; soft states require complete exact-head local-validation evidence"

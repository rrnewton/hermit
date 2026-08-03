#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
CHECKER="$ROOT_DIR/scripts/require-check-success.sh"

"$CHECKER" required completed success >/dev/null
"$CHECKER" required COMPLETED SUCCESS >/dev/null

cases=(
    completed:cancelled
    completed:skipped
    completed:neutral
    completed:timed_out
    completed:stale
    completed:failure
    completed:action_required
    completed:startup_failure
    completed:unknown_future_conclusion
    queued:none
    in_progress:none
    waiting:none
)

for state in "${cases[@]}"; do
    status=${state%%:*}
    conclusion=${state#*:}
    [[ $conclusion != none ]] || conclusion=
    if "$CHECKER" required "$status" "$conclusion" >/dev/null 2>&1; then
        echo "FAIL: required check was accepted as $status/${conclusion:-none}" >&2
        exit 1
    fi
done

echo "PASS: only completed/success satisfies a required check; ${#cases[@]} non-pass states block"

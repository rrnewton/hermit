#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -u

work=$(mktemp -d "${TMPDIR:-/tmp}/cat-proc-uptime-verify.XXXXXX")
trap 'rm -r -- "$work"' EXIT
report=$work/verify.json

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

"$hermit" run --verify --verify-json "$report" --no-sequentialize-threads --no-deterministic-io -- bash -c 'echo hello; cat /proc/uptime; cat /proc/uptime; cat /proc/uptime'
res=$?

if [ "$res" == 0 ] || ! jq -e '
    (.verified == false)
    and (.verdict == "diverged")
    and (.bitwise_parity == false)
    and (.comparison.strictness == "canonical")
    and (.comparison.compare_logs == true)
    and ((.compared_log_messages.left // 0) > 0)
    and ((.compared_log_messages.right // 0) > 0)
' "$report" >/dev/null 2>&1; then
    echo "Error!  Zero exit code where differences expected."
    exit 1
else
    echo "Differences found, as expected."
    exit 0
fi

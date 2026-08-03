#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# A required GitHub check passes in exactly one state. Keep this policy in one
# executable so merge-gate and its exhaustive regression test cannot drift.
set -euo pipefail

if (($# != 3)); then
    echo "usage: $0 LABEL STATUS CONCLUSION" >&2
    exit 2
fi

label=$1
status=${2,,}
conclusion=${3,,}

if [[ $status == completed && $conclusion == success ]]; then
    echo "$label passed (completed/success)."
    exit 0
fi

echo "$label is NOT PASSED ($status/${conclusion:-none}); re-run is required." >&2
exit 1

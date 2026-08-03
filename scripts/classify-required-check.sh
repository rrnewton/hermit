#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Classify one required GitHub check for merge-gate. Hard failures are never
# overridable. Infrastructure/unavailable states may use exact-head local
# validation evidence. Prints: success | hard-failure | substitutable.
set -euo pipefail

if (($# != 2)); then
    echo "usage: $0 STATUS CONCLUSION" >&2
    exit 2
fi

status=${1,,}
conclusion=${2,,}

if [[ $status == completed && $conclusion == success ]]; then
    echo success
    exit 0
fi

case "$conclusion" in
    failure | timed_out | error | action_required | startup_failure | stale)
        echo hard-failure
        ;;
    cancelled | skipped | neutral | "")
        echo substitutable
        ;;
    success)
        # A success conclusion without completed status is not finished.
        echo substitutable
        ;;
    *)
        # Unknown terminal conclusions fail closed.
        echo hard-failure
        ;;
esac

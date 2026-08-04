#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Classify one required GitHub check without forcing absence into a pass/fail
# binary. Output is exactly one of: PASSED, FAILED, NO_RESULT.
set -euo pipefail

if (($# != 2)); then
    echo "usage: $0 STATUS CONCLUSION" >&2
    exit 2
fi

status=${1,,}
conclusion=${2,,}

# CheckRun supplies completed/success. A legacy StatusContext has no separate
# status field, so an empty status with a terminal conclusion is also valid.
if [[ $conclusion == success && ($status == completed || -z $status) ]]; then
    echo PASSED
    exit 0
fi

if [[ $status == completed || -z $status ]]; then
    case "$conclusion" in
        failure | timed_out | error | startup_failure)
            echo FAILED
            exit 0
            ;;
    esac
fi

# Cancelled/skipped/neutral/stale/action_required, active states, absent checks,
# and future tokens contain no trustworthy product result. They block without
# being reported as failures and must be re-dispatched or allowed to finish.
echo NO_RESULT

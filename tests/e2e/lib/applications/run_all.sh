#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
readonly SCRIPT_DIR
readonly COUNTS_WRITER="$SCRIPT_DIR/../../../../ci/write-structured-test-counts.sh"

executed_tests=0

function publish_test_counts {
    local status=$? count_status
    trap - EXIT
    set +e
    "$COUNTS_WRITER" "$executed_tests" 0
    count_status=$?
    if ((status == 0 && count_status != 0)); then
        status=$count_status
    fi
    exit "$status"
}

trap publish_test_counts EXIT

# Keep every-commit application coverage bounded. Full server/client sessions
# are occasional validation: Redis takes about one minute even when healthy,
# and both tests retain long-lived server processes while a client exits. Run
# them explicitly with:
#
# cargo test -p hermit --test frontier_app_benchmarks \
#   redis_deep_session_is_nondeterministic_natively_and_l2_under_hermit -- \
#   --ignored --exact --nocapture
# cargo test -p hermit --test frontier_app_benchmarks \
#   http_server_session_is_nondeterministic_natively_and_l2_under_hermit -- \
#   --ignored --exact --nocapture
for test_script in sqlite_on_disk.sh sqlite_deep.sh build_tools.sh; do
    printf '==> %s\n' "$test_script"
    executed_tests=$((executed_tests + 1))
    "$SCRIPT_DIR/$test_script"
done

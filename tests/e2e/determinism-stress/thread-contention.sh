#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
# HERMIT_E2E_META_BEGIN
# {
#   "schema": 1,
#   "id": "determinism-stress/thread-contention",
#   "category": "determinism-stress",
#   "description": "Mutex contention and multi-fd poll/epoll readiness repeat under strict verification",
#   "lane": "portable",
#   "requires": ["linux", "x86_64", "userns", "ptrace", "cc"],
#   "timeout_seconds": 120,
#   "observation": {"status": true, "stdout": true, "stderr": false, "artifacts": []},
#   "modes": {
#     "naked": {"runs": 10, "assert": {"min_distinct": 2}},
#     "verify": {"backends": ["ptrace"]}
#   },
#   "disabled_modes": {
#     "replay": "The focused C replay sentinel owns the blocking record/replay contract",
#     "chaos": "The order-violation test provides the seeded schedule-diversity oracle"
#   },
#   "occasional": false
# }
# HERMIT_E2E_META_END
set -euo pipefail

case ${1:-} in
    --prepare)
        ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
        cc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread \
            "$ROOT_DIR/tests/e2e/determinism-stress/thread_contention.c" \
            -o "$E2E_FIXTURE_DIR/thread-contention"
        ;;
    --run)
        "$E2E_FIXTURE_DIR/thread-contention" contention
        exec "$E2E_FIXTURE_DIR/thread-contention" epoll
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac

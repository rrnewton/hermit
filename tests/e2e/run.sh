#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
MODE=${1:-portable}
HERMIT_BIN=${HERMIT_BIN:-$ROOT_DIR/target/debug/hermit}
readonly ROOT_DIR MODE HERMIT_BIN

cd "$ROOT_DIR"

run_category() {
    local category=$1
    shift
    printf '\n==================== e2e/%s (%s) ====================\n' "$category" "$MODE"
    "$@"
}

case $MODE in
    portable)
        run_category applications ./tests/e2e/applications/run_all.sh
        run_category data-handling ./tests/e2e/data-handling/run.sh portable
        run_category determinism-stress env HERMIT_BIN="$HERMIT_BIN" \
            ./tests/e2e/determinism-stress/run.sh portable
        run_category language-runtimes env HERMIT_BIN="$HERMIT_BIN" \
            HERMIT_LANGUAGE_RUNTIME_BACKEND=ptrace ./tests/e2e/language-runtimes/run.sh
        run_category system-utils ./tests/e2e/system-utils/run.sh "$HERMIT_BIN" ptrace
        ;;
    privileged)
        run_category system-utils ./tests/e2e/system-utils/run.sh "$HERMIT_BIN" kvm
        ;;
    occasional)
        run_category data-handling ./tests/e2e/data-handling/run.sh occasional
        run_category determinism-stress env HERMIT_BIN="$HERMIT_BIN" \
            ./tests/e2e/determinism-stress/run.sh occasional
        ;;
    *)
        echo "usage: $0 [portable|privileged|occasional]" >&2
        exit 2
        ;;
esac

printf '\nPASS: e2e %s lane completed\n' "$MODE"

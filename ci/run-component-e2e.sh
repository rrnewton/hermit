#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
LANE=${1:-}
CATEGORY=${2:-all}
HERMIT_BIN=${HERMIT_BIN:-$ROOT_DIR/target/debug/hermit}
readonly ROOT_DIR LANE CATEGORY HERMIT_BIN

categories=(applications data-handling determinism-stress language-runtimes system-utils)

usage() {
    echo "usage: $0 <portable|privileged|occasional> [all|CATEGORY]" >&2
    exit 2
}

contains_category() {
    local candidate
    for candidate in "${categories[@]}"; do
        [[ $candidate == "$1" ]] && return 0
    done
    return 1
}

run_category() {
    local category=$1
    printf '\n==================== component e2e/%s (%s) ====================\n' \
        "$category" "$LANE"
    case "$LANE/$category" in
        portable/applications)
            "$ROOT_DIR/tests/e2e/lib/applications/run_all.sh"
            ;;
        portable/data-handling)
            "$ROOT_DIR/tests/e2e/lib/data-handling/run.sh" portable
            ;;
        portable/determinism-stress)
            "$ROOT_DIR/tests/e2e/lib/determinism-stress/run.sh" portable
            ;;
        portable/language-runtimes)
            env HERMIT_BIN="$HERMIT_BIN" HERMIT_LANGUAGE_RUNTIME_BACKEND=ptrace \
                "$ROOT_DIR/tests/e2e/lib/language-runtimes/run.sh"
            ;;
        portable/system-utils)
            "$ROOT_DIR/tests/e2e/lib/system-utils/run.sh" "$HERMIT_BIN" ptrace
            ;;
        privileged/system-utils)
            "$ROOT_DIR/tests/e2e/lib/system-utils/run.sh" "$HERMIT_BIN" kvm
            ;;
        occasional/data-handling)
            "$ROOT_DIR/tests/e2e/lib/data-handling/run.sh" occasional
            ;;
        occasional/determinism-stress)
            "$ROOT_DIR/tests/e2e/lib/determinism-stress/run.sh" occasional
            ;;
        *)
            echo "category $category is not assigned to the $LANE component lane" >&2
            return 2
            ;;
    esac
}

case $LANE in
    portable)
        selected=("${categories[@]}")
        ;;
    privileged)
        selected=(system-utils)
        ;;
    occasional)
        selected=(data-handling determinism-stress)
        ;;
    *) usage ;;
esac

if [[ $CATEGORY != all ]]; then
    contains_category "$CATEGORY" || usage
    selected=("$CATEGORY")
fi

for category in "${selected[@]}"; do
    run_category "$category"
done

printf '\nPASS: component E2E %s lane completed\n' "$LANE"

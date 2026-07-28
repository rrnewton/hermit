#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
mode=${1:-portable}

case $mode in
    portable)
        tests=(compression.sh archive.sh text-processing.sh numerical.sh)
        ;;
    occasional)
        tests=(reproducible-build.sh)
        ;;
    all)
        tests=(compression.sh archive.sh text-processing.sh numerical.sh reproducible-build.sh)
        ;;
    *)
        echo "usage: $0 [portable|occasional|all]" >&2
        exit 2
        ;;
esac
readonly mode
readonly -a tests

for test in "${tests[@]}"; do
    printf '\n==> data handling: %s\n' "$test"
    "$here/$test"
done

printf '\nPASS: %s/%s data-handling %s tests proved naked variation and strict determinism\n' \
    "${#tests[@]}" "${#tests[@]}" "$mode"

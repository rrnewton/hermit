#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

case ${1:-} in
    --prepare)
        ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
        cc -std=c11 -O2 -Wall -Wextra -Werror "$ROOT_DIR/tests/c/getpid.c" \
            -o "$E2E_FIXTURE_DIR/getpid"
        ;;
    --run) exec "$E2E_FIXTURE_DIR/getpid" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac

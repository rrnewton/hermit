#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

case ${1:-} in
    --prepare)
        : "${E2E_FIXTURE_DIR:?E2E_FIXTURE_DIR must be set during preparation}"
        mkdir -p -- "$E2E_FIXTURE_DIR"
        ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
        cc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread \
            "$ROOT_DIR/tests/e2e/determinism-stress/fork_tree.c" \
            -o "$E2E_FIXTURE_DIR/fork-tree"
        cc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread \
            "$ROOT_DIR/tests/e2e/determinism-stress/pipe_chain.c" \
            -o "$E2E_FIXTURE_DIR/pipe-chain"
        ;;
    --run)
        shift
        if (($# == 2)); then
            fork_tree=$1
            pipe_chain=$2
            for fixture in "$fork_tree" "$pipe_chain"; do
                if [[ $fixture != /* || ! -x $fixture ]]; then
                    echo "run fixture must be an absolute executable path: $fixture" >&2
                    exit 2
                fi
            done
        elif (($# == 0)) && [[ -n ${E2E_FIXTURE_DIR:-} ]]; then
            # Backward-compatible manual/naked entrypoint. Hermetic manifest
            # runs pass both guest-visible paths explicitly instead of
            # forwarding this preparation-only environment variable.
            fork_tree="$E2E_FIXTURE_DIR/fork-tree"
            pipe_chain="$E2E_FIXTURE_DIR/pipe-chain"
        else
            echo "usage: $0 --run [<fork-tree> <pipe-chain>]" >&2
            exit 2
        fi
        "$fork_tree"
        exec "$pipe_chain"
        ;;
    *) echo "usage: $0 --prepare|--run [fixture paths]" >&2; exit 2 ;;
esac

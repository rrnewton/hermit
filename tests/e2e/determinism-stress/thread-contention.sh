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
            "$ROOT_DIR/tests/e2e/determinism-stress/thread_contention.c" \
            -o "$E2E_FIXTURE_DIR/thread-contention"
        cc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread \
            "$ROOT_DIR/tests/e2e/determinism-stress/thread_stress.c" \
            -o "$E2E_FIXTURE_DIR/thread-stress"
        cc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread \
            "$ROOT_DIR/tests/e2e/determinism-stress/mmap_fork_shared.c" \
            -o "$E2E_FIXTURE_DIR/mmap-fork-shared"
        ;;
    --run)
        shift
        if (($# == 3)); then
            thread_contention=$1
            thread_stress=$2
            mmap_fork_shared=$3
            for fixture in "$thread_contention" "$thread_stress" "$mmap_fork_shared"; do
                if [[ $fixture != /* || ! -x $fixture ]]; then
                    echo "run fixture must be an absolute executable path: $fixture" >&2
                    exit 2
                fi
            done
        elif (($# == 0)) && [[ -n ${E2E_FIXTURE_DIR:-} ]]; then
            # Backward-compatible manual/naked entrypoint. Hermetic manifest
            # runs pass all guest-visible paths explicitly instead of
            # forwarding this preparation-only environment variable.
            thread_contention="$E2E_FIXTURE_DIR/thread-contention"
            thread_stress="$E2E_FIXTURE_DIR/thread-stress"
            mmap_fork_shared="$E2E_FIXTURE_DIR/mmap-fork-shared"
        else
            echo "usage: $0 --run [<thread-contention> <thread-stress> <mmap-fork-shared>]" >&2
            exit 2
        fi
        "$thread_contention" contention
        "$thread_contention" epoll
        "$thread_stress"
        exec "$mmap_fork_shared"
        ;;
    *) echo "usage: $0 --prepare|--run [fixture paths]" >&2; exit 2 ;;
esac

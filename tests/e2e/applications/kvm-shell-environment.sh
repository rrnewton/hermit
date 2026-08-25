#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        # Required manifest cells now exercise the CLI's exact minimal base
        # environment instead of rebuilding the old runner-specific one.
        [[ -z ${LC_ALL+x} ]]
        [[ -z ${TZ+x} ]]
        [[ ${ASAN_OPTIONS:-} == detect_leaks=0 ]]
        [[ ${HOME:-} == /root ]]
        [[ ${HOSTNAME:-} == hermetic-container.local ]]
        [[ ${LSAN_OPTIONS:-} == detect_leaks=0 ]]
        [[ ${PATH:-} == /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin ]]
        printf 'kvm-shell:minimal\n'
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac

#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Verify a published artifact, export exact paths, then run one consumer.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly private_test_root_marker=HERMIT_VALIDATE_PRIVATE_TEST_ROOT
readonly private_test_root_helper_var=HERMIT_VALIDATE_PRIVATE_TEST_ROOT_HELPER
readonly private_test_root_helper_sha_var=HERMIT_VALIDATE_PRIVATE_TEST_ROOT_HELPER_SHA256
readonly private_test_root_value=canonical-full-manifest-v1
private_test_root=0
private_test_root_helper=
private_test_root_helper_sha=
require_install=0
if [[ ${1:-} == --require-install ]]; then
    require_install=1
    shift
fi
[[ $# -gt 0 ]] || { echo "usage: $0 [--require-install] COMMAND..." >&2; exit 2; }
if [[ -n ${!private_test_root_marker+x} || -n ${!private_test_root_helper_var+x} ||
      -n ${!private_test_root_helper_sha_var+x} ]]; then
    [[ -n ${!private_test_root_marker+x} && -n ${!private_test_root_helper_var+x} &&
       -n ${!private_test_root_helper_sha_var+x} ]] || {
        echo "run-with-hermit-e2e-artifact.sh: incomplete private-/test helper credentials" >&2
        exit 2
    }
    [[ ${!private_test_root_marker} == "$private_test_root_value" ]] || {
        echo "run-with-hermit-e2e-artifact.sh: invalid private-/test marker" >&2
        exit 2
    }
    private_test_root_helper=${!private_test_root_helper_var}
    private_test_root_helper_sha=${!private_test_root_helper_sha_var}
    [[ $private_test_root_helper_sha =~ ^[0-9a-f]{64}$ ]] || {
        echo "run-with-hermit-e2e-artifact.sh: private-/test helper digest is invalid" >&2
        exit 2
    }
    private_test_root_helper_expected="/usr/local/libexec/hermit-private-test-root-$private_test_root_helper_sha"
    [[ $private_test_root_helper == "$private_test_root_helper_expected" &&
       -f $private_test_root_helper && ! -L $private_test_root_helper &&
       -x $private_test_root_helper ]] || {
        echo "run-with-hermit-e2e-artifact.sh: private-/test helper path does not match its digest" >&2
        exit 2
    }
    helper_uid=$(/usr/bin/stat -c %u -- "$private_test_root_helper")
    helper_mode=$(/usr/bin/stat -c %a -- "$private_test_root_helper")
    ((helper_uid == 0 && (8#$helper_mode & 0022) == 0 && (8#$helper_mode & 0111) != 0)) || {
        echo "run-with-hermit-e2e-artifact.sh: private-/test helper ownership or mode is unsafe" >&2
        exit 2
    }
    helper_actual_sha=$(/usr/bin/sha256sum -- "$private_test_root_helper")
    helper_actual_sha=${helper_actual_sha%% *}
    [[ $helper_actual_sha == "$private_test_root_helper_sha" ]] || {
        echo "run-with-hermit-e2e-artifact.sh: private-/test helper digest mismatch" >&2
        exit 2
    }
    [[ ${1:-} == target/debug/test-harness && ${2:-} == run ]] || {
        echo "run-with-hermit-e2e-artifact.sh: private /test is restricted to the final test-harness run" >&2
        exit 2
    }
    private_test_root=1
fi
pointer=${HERMIT_E2E_ARTIFACT_POINTER:-$ROOT_DIR/target/ci/hermit-e2e-artifact.path}
bundle=$("$ROOT_DIR/ci/verify-hermit-e2e-artifact.sh" "$pointer")
export HERMIT_BIN="$bundle/hermit"
if [[ -d $bundle/install ]]; then
    export HERMIT_INSTALL_DIR="$bundle/install"
elif ((require_install)); then
    echo "run-with-hermit-e2e-artifact.sh: consumer requires a complete resource bundle: $bundle" >&2
    exit 2
else
    unset HERMIT_INSTALL_DIR
fi
printf 'hermit-e2e-artifact: verified bin=%s install=%s\n' \
    "$HERMIT_BIN" "${HERMIT_INSTALL_DIR:-none}" >&2

if ((private_test_root)); then
    exec "$private_test_root_helper" -- "$@"
fi
exec "$@"

#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Run one canonical E2E manifest consumer in a private mount namespace whose
# `/test` starts empty. The host's PID and cgroup namespaces stay unchanged:
# Hermit's PMU/cgroup access and dagrun's accounting therefore continue to see
# the same processes. Only the mount namespace is private.

set -euo pipefail

readonly MARKER=HERMIT_VALIDATE_PRIVATE_TEST_ROOT
readonly MARKER_VALUE=canonical-full-manifest-v1

fail() {
    printf 'run-with-private-test-root.sh: %s\n' "$*" >&2
    exit 2
}

[[ ${!MARKER:-} == "$MARKER_VALUE" ]] ||
    fail "refusing outside an admitted top-level full-profile manifest step"

if [[ ${1:-} != --inside ]]; then
    unset "$MARKER"
    [[ ${1:-} == -- ]] || fail "usage: $0 -- COMMAND..."
    shift
    (($# > 0)) || fail "a command is required after --"
    ((EUID != 0)) || fail "the outer launcher must run as the invoking non-root user"

    for tool in sudo unshare; do
        command -v "$tool" >/dev/null 2>&1 || fail "required command is unavailable: $tool"
    done
    sudo -n true >/dev/null 2>&1 || fail "passwordless sudo is required"

    invoking_uid=$(id -u)
    readonly invoking_uid
    invoking_gid=$(id -g)
    readonly invoking_gid
    invoking_user=$(id -un)
    readonly invoking_user
    readonly invoking_cwd=$PWD
    readonly invoking_path=$PATH
    self=$(readlink -f -- "$0")
    readonly self
    private_root=$(mktemp -d /tmp/hermit-private-test-root.XXXXXX) ||
        fail "cannot allocate the private root mountpoint"
    readonly private_root
    # shellcheck disable=SC2317 # Called by the EXIT/signal trap below.
    cleanup() {
        # The mount lives only in the child's namespace. Its teardown leaves an
        # empty host directory, so rmdir is both sufficient and fail-safe: this
        # cleanup can never recursively remove an attacker-chosen path.
        rmdir -- "$private_root" 2>/dev/null || true
    }
    trap cleanup EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

    status=0
    env "$MARKER=$MARKER_VALUE" sudo -n --preserve-env -- \
        unshare --mount --propagation private -- \
        "$self" --inside "$invoking_uid" "$invoking_gid" "$invoking_user" \
        "$invoking_cwd" "$invoking_path" "$private_root" -- "$@" || status=$?
    exit "$status"
fi

shift
[[ $# -ge 7 ]] || fail "malformed internal invocation"
readonly invoking_uid=$1
readonly invoking_gid=$2
readonly invoking_user=$3
readonly invoking_cwd=$4
readonly invoking_path=$5
readonly private_root=$6
shift 6
[[ ${1:-} == -- ]] || fail "malformed internal command delimiter"
shift
(($# > 0)) || fail "missing internal command"

# `sudo` owns the root setup process, so never resolve privileged utilities
# through an invoking-user PATH. The original path is restored only after all
# mount work is complete and setpriv is about to drop privileges.
PATH=/usr/sbin:/usr/bin:/sbin:/bin
export PATH

((EUID == 0)) || fail "internal mount setup requires root"
[[ $invoking_uid =~ ^[0-9]+$ && $invoking_gid =~ ^[0-9]+$ ]] ||
    fail "invoking uid/gid must be numeric"
[[ $invoking_user != */* && -n $invoking_user ]] || fail "invalid invoking user"
[[ $invoking_cwd == /* && -d $invoking_cwd ]] ||
    fail "invoking cwd is not an existing absolute directory: $invoking_cwd"
[[ $private_root == /tmp/hermit-private-test-root.* && -d $private_root && ! -L $private_root ]] ||
    fail "unsafe private-root mountpoint: $private_root"
[[ $(readlink -f -- "$private_root") == "$private_root" ]] ||
    fail "private-root mountpoint is not canonical: $private_root"
[[ $(stat -c %u:%a -- "$private_root") == "$invoking_uid:700" ]] ||
    fail "private-root mountpoint ownership or mode changed before setup"

for tool in mount umount pivot_root setpriv; do
    command -v "$tool" >/dev/null 2>&1 || fail "required util-linux command is unavailable: $tool"
done

# Nothing may propagate back into the host namespace. The root is a tiny tmpfs
# carrying mountpoints and symlinks; every host top-level entry except `/test`
# is then made visible at the same absolute path. Recursive binds preserve host
# submounts such as this box's separately mounted `/home/newton`, plus devpts and
# cgroup2. `/tmp` alone is a plain bind because the temporary new root is itself
# a submount beneath `/tmp`; recursively cloning that mount would nest the root
# inside its own `/tmp` view.
mount --make-rprivate /
mount -t tmpfs -o mode=0755,nosuid,nodev,size=16M private-test-root "$private_root"
shopt -s nullglob
for source in /* /.[!.]* /..?*; do
    [[ $source == /test ]] && continue
    target=$private_root$source
    if [[ -L $source ]]; then
        ln -s -- "$(readlink -- "$source")" "$target"
    elif [[ -d $source ]]; then
        mkdir -p -- "$target"
        case "$source" in
            /tmp) mount --bind "$source" "$target" ;;
            *)
                mount --rbind "$source" "$target"
                mount --make-rslave "$target"
                ;;
        esac
    elif [[ -f $source ]]; then
        : >"$target"
        mount --bind "$source" "$target"
    fi
done
mkdir -- "$private_root/test"
mount -t tmpfs -o mode=1777,nosuid,nodev private-test-workdir "$private_root/test"
mkdir -- "$private_root/.old-root"

cd -- "$private_root"
pivot_root . .old-root
cd /
umount -l /.old-root
rmdir /.old-root
cd -- "$invoking_cwd"

# The marker is an admission instruction for this wrapper, never guest or test
# state. Drop it before the final command, then restore the invoking identity and
# supplementary groups. No PID or cgroup namespace was created above.
unset "$MARKER"
export PATH=$invoking_path
export USER=$invoking_user
export LOGNAME=$invoking_user
exec setpriv --reuid "$invoking_uid" --regid "$invoking_gid" --init-groups -- "$@"

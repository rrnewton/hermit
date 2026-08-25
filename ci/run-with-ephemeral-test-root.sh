#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Give one unprivileged command an empty host mountpoint for Hermit's private
# tmpfs /test. Only fixed, absolute mkdir/rmdir operations run through sudo;
# caller bytes are always executed directly under the invoking uid.
set -euo pipefail

readonly sudo_bin=/usr/bin/sudo
readonly mkdir_bin=/usr/bin/mkdir
readonly rmdir_bin=/usr/bin/rmdir
readonly stat_bin=/usr/bin/stat
readonly mountpoint_bin=/usr/bin/mountpoint
readonly find_bin=/usr/bin/find
readonly id_bin=/usr/bin/id
readonly setsid_bin=/usr/bin/setsid
readonly kill_bin=/usr/bin/kill
readonly sleep_bin=/usr/bin/sleep
ephemeral_test_root_path=/test
ephemeral_test_root_expected_uid=0
ephemeral_test_root_expected_gid=0
ephemeral_test_root_privileged=1
ephemeral_test_root_identity=
ephemeral_test_root_active=0
ephemeral_test_root_child_pid=
ephemeral_test_root_child_pgid=
ephemeral_test_root_pending_signal=
ephemeral_test_root_pending_status=0
ephemeral_test_root_fail_after_create=0

ephemeral_test_root_absent() {
    [[ ! -e $ephemeral_test_root_path && ! -L $ephemeral_test_root_path ]]
}

ephemeral_test_root_same_object_empty() {
    ((ephemeral_test_root_active)) || return 1
    [[ -d $ephemeral_test_root_path && ! -L $ephemeral_test_root_path ]] || return 1
    local metadata mount_status first
    metadata=$(LC_ALL=C "$stat_bin" -c '%d:%i' -- "$ephemeral_test_root_path") || return 1
    [[ $metadata == "$ephemeral_test_root_identity" ]] || return 1
    if "$mountpoint_bin" -q -- "$ephemeral_test_root_path"; then
        return 1
    else
        mount_status=$?
    fi
    # util-linux mountpoint uses 32 for an existing non-mount. Any other
    # result is an observation failure, not proof that cleanup is safe.
    [[ $mount_status -eq 32 ]] || return 1
    first=$("$find_bin" "$ephemeral_test_root_path" -mindepth 1 -maxdepth 1 -print -quit) || return 1
    [[ -z $first ]]
}

ephemeral_test_root_exact() {
    ephemeral_test_root_same_object_empty || return 1
    local metadata
    metadata=$(LC_ALL=C "$stat_bin" -c '%u:%g:%a' -- "$ephemeral_test_root_path") || return 1
    [[ $metadata == "$ephemeral_test_root_expected_uid:$ephemeral_test_root_expected_gid:755" ]]
}

create_ephemeral_test_root() {
    ephemeral_test_root_absent || {
        echo "run-with-ephemeral-test-root.sh: refusing pre-existing $ephemeral_test_root_path" >&2
        return 2
    }
    if ((ephemeral_test_root_privileged)); then
        if [[ $("$id_bin" -u) -eq 0 ]]; then
            echo 'run-with-ephemeral-test-root.sh: refusing to run the checkout command as root' >&2
            return 2
        fi
        "$sudo_bin" -n "$mkdir_bin" --mode=0755 -- "$ephemeral_test_root_path"
    else
        "$mkdir_bin" --mode=0755 -- "$ephemeral_test_root_path"
    fi
    ephemeral_test_root_active=1
    local metadata device inode remainder
    metadata=$(LC_ALL=C "$stat_bin" -c '%d:%i:%u:%g:%a' -- "$ephemeral_test_root_path")
    device=${metadata%%:*}
    remainder=${metadata#*:}
    inode=${remainder%%:*}
    ephemeral_test_root_identity="$device:$inode"
    ephemeral_test_root_exact || {
        echo "run-with-ephemeral-test-root.sh: new $ephemeral_test_root_path is not the exact empty non-mount directory" >&2
        return 2
    }
    if ((ephemeral_test_root_fail_after_create)); then
        echo 'run-with-ephemeral-test-root.sh: planted post-create failure' >&2
        return 2
    fi
}

remove_ephemeral_test_root() {
    ephemeral_test_root_exact || {
        echo "run-with-ephemeral-test-root.sh: refusing to remove changed or nonempty $ephemeral_test_root_path" >&2
        return 1
    }
    if ((ephemeral_test_root_privileged)); then
        "$sudo_bin" -n "$rmdir_bin" -- "$ephemeral_test_root_path"
    else
        "$rmdir_bin" -- "$ephemeral_test_root_path"
    fi
    ephemeral_test_root_active=0
    ephemeral_test_root_absent || {
        echo "run-with-ephemeral-test-root.sh: $ephemeral_test_root_path remained after cleanup" >&2
        return 1
    }
}

terminate_ephemeral_test_root_child_group() {
    if [[ $ephemeral_test_root_child_pgid =~ ^[0-9]+$ ]]; then
        "$kill_bin" -s TERM -- "-$ephemeral_test_root_child_pgid" 2>/dev/null || true
        "$sleep_bin" 0.05
        "$kill_bin" -s KILL -- "-$ephemeral_test_root_child_pgid" 2>/dev/null || true
    fi
    if [[ -n $ephemeral_test_root_child_pid ]]; then
        wait "$ephemeral_test_root_child_pid" 2>/dev/null || true
    fi
    ephemeral_test_root_child_pid=
    ephemeral_test_root_child_pgid=
}

record_ephemeral_test_root_signal() {
    local signal=$1
    local status=$2
    if ((ephemeral_test_root_pending_status == 0)); then
        ephemeral_test_root_pending_signal=$signal
        ephemeral_test_root_pending_status=$status
    fi
    # This handler is installed before mkdir and remains installed through
    # cleanup. Before the process-group id is published it only records the
    # signal; immediately after publication the main path forwards it. Thus a
    # signal cannot land in the launch gap and let cleanup race a live child.
    if [[ $ephemeral_test_root_child_pgid =~ ^[0-9]+$ ]]; then
        "$kill_bin" -s "$signal" -- "-$ephemeral_test_root_child_pgid" 2>/dev/null || true
    fi
}

cleanup_ephemeral_test_root_on_exit() {
    local status=$?
    trap - EXIT
    terminate_ephemeral_test_root_child_group
    local cleanup_failed=0
    if ((ephemeral_test_root_active)) && ! remove_ephemeral_test_root; then
        cleanup_failed=1
    fi
    trap - HUP INT TERM
    if ((cleanup_failed)); then
        status=125
    elif ((ephemeral_test_root_pending_status != 0)); then
        status=$ephemeral_test_root_pending_status
    fi
    exit "$status"
}

run_with_ephemeral_test_root() {
    ephemeral_test_root_active=0
    ephemeral_test_root_identity=
    ephemeral_test_root_child_pid=
    ephemeral_test_root_child_pgid=
    ephemeral_test_root_pending_signal=
    ephemeral_test_root_pending_status=0
    trap cleanup_ephemeral_test_root_on_exit EXIT
    trap 'record_ephemeral_test_root_signal HUP 129' HUP
    trap 'record_ephemeral_test_root_signal INT 130' INT
    trap 'record_ephemeral_test_root_signal TERM 143' TERM
    local status
    if create_ephemeral_test_root; then
        :
    else
        status=$?
        local cleanup_failed=0
        if ((ephemeral_test_root_active)) && ! remove_ephemeral_test_root; then
            cleanup_failed=1
        fi
        trap - HUP INT TERM
        if ((cleanup_failed)); then
            status=125
        elif ((ephemeral_test_root_pending_status != 0)); then
            status=$ephemeral_test_root_pending_status
        fi
        trap - EXIT
        return "$status"
    fi

    if ((ephemeral_test_root_pending_status != 0)); then
        status=$ephemeral_test_root_pending_status
        if ! remove_ephemeral_test_root; then
            status=125
        fi
        trap - HUP INT TERM
        if ((status != 125 && ephemeral_test_root_pending_status != 0)); then
            status=$ephemeral_test_root_pending_status
        fi
        trap - EXIT
        return "$status"
    fi

    "$setsid_bin" --wait -- "$@" &
    ephemeral_test_root_child_pid=$!
    # A background child of this non-interactive shell is not a process-group
    # leader, so setsid(2) makes its pid both session and process-group id.
    # Publish that identity immediately; a signal caught in the tiny launch
    # window was recorded above and is forwarded here before waiting.
    ephemeral_test_root_child_pgid=$ephemeral_test_root_child_pid
    if [[ -n $ephemeral_test_root_pending_signal ]]; then
        "$kill_bin" -s "$ephemeral_test_root_pending_signal" \
            -- "-$ephemeral_test_root_child_pgid" 2>/dev/null || true
    fi
    if wait "$ephemeral_test_root_child_pid"; then
        status=0
    else
        status=$?
    fi
    terminate_ephemeral_test_root_child_group
    local cleanup_failed=0
    if ! remove_ephemeral_test_root; then
        cleanup_failed=1
    fi
    trap - HUP INT TERM
    if ((cleanup_failed)); then
        status=125
    elif ((ephemeral_test_root_pending_status != 0)); then
        status=$ephemeral_test_root_pending_status
    fi
    trap - EXIT
    return "$status"
}

self_test_ephemeral_test_root() {
    local parent
    parent=$(/usr/bin/mktemp -d)
    ephemeral_test_root_path="$parent/test"
    ephemeral_test_root_expected_uid=$("$id_bin" -u)
    ephemeral_test_root_expected_gid=$("$id_bin" -g)
    ephemeral_test_root_privileged=0

    run_with_ephemeral_test_root /usr/bin/test -d "$ephemeral_test_root_path"
    ephemeral_test_root_absent

    local status
    if run_with_ephemeral_test_root /bin/sh -c 'exit 23'; then
        echo 'ephemeral /test self-test accepted a planted child failure' >&2
        return 1
    else
        status=$?
    fi
    [[ $status -eq 23 ]] || {
        echo "ephemeral /test self-test changed child status 23 to $status" >&2
        return 1
    }
    ephemeral_test_root_absent

    ephemeral_test_root_fail_after_create=1
    if run_with_ephemeral_test_root /bin/true; then
        echo 'ephemeral /test self-test accepted a planted post-create failure' >&2
        return 1
    else
        status=$?
    fi
    [[ $status -eq 2 ]] || {
        echo "ephemeral /test self-test changed post-create status 2 to $status" >&2
        return 1
    }
    ephemeral_test_root_absent
    ephemeral_test_root_fail_after_create=0

    local descendant_marker="$parent/descendant-survived"
    run_with_ephemeral_test_root /bin/sh -c \
        "(/usr/bin/sleep 0.2; /usr/bin/touch '$descendant_marker') & exit 0"
    "$sleep_bin" 0.3
    [[ ! -e $descendant_marker ]] || {
        echo 'ephemeral /test self-test left a descendant process running' >&2
        return 1
    }

    (
        run_with_ephemeral_test_root /bin/sh -c \
            'trap "" HUP INT TERM; /usr/bin/sleep 10'
    ) &
    local wrapper_pid=$!
    local spin
    for ((spin = 0; spin < 100; spin++)); do
        [[ -d $ephemeral_test_root_path ]] && break
        "$sleep_bin" 0.01
    done
    "$kill_bin" -s TERM -- "$wrapper_pid"
    if wait "$wrapper_pid"; then
        echo 'ephemeral /test self-test swallowed SIGTERM' >&2
        return 1
    else
        status=$?
    fi
    [[ $status -eq 143 ]] || {
        echo "ephemeral /test self-test changed SIGTERM status 143 to $status" >&2
        return 1
    }
    ephemeral_test_root_absent

    if run_with_ephemeral_test_root /usr/bin/touch "$ephemeral_test_root_path/sentinel"; then
        echo 'ephemeral /test self-test accepted unsafe nonempty cleanup' >&2
        return 1
    else
        status=$?
    fi
    [[ $status -eq 125 ]] || {
        echo "ephemeral /test self-test changed cleanup failure status 125 to $status" >&2
        return 1
    }
    [[ -f $ephemeral_test_root_path/sentinel ]] || return 1
    /usr/bin/unlink -- "$ephemeral_test_root_path/sentinel"
    "$rmdir_bin" -- "$ephemeral_test_root_path"
    ephemeral_test_root_active=0

    "$mkdir_bin" --mode=0755 -- "$ephemeral_test_root_path"
    /usr/bin/touch -- "$ephemeral_test_root_path/sentinel"
    if run_with_ephemeral_test_root /bin/true; then
        echo 'ephemeral /test self-test accepted a pre-existing path' >&2
        return 1
    fi
    [[ -f $ephemeral_test_root_path/sentinel ]]
    /usr/bin/unlink -- "$ephemeral_test_root_path/sentinel"
    "$rmdir_bin" -- "$ephemeral_test_root_path"

    /usr/bin/ln -s -- "$parent/missing" "$ephemeral_test_root_path"
    if run_with_ephemeral_test_root /bin/true; then
        echo 'ephemeral /test self-test accepted a pre-existing symlink' >&2
        return 1
    fi
    [[ -L $ephemeral_test_root_path ]] || return 1
    /usr/bin/unlink -- "$ephemeral_test_root_path"
    "$rmdir_bin" -- "$parent"
    echo 'run-with-ephemeral-test-root self-test: PASS'
}

if [[ ${1:-} == --self-test ]]; then
    [[ $# -eq 1 ]] || {
        echo 'usage: run-with-ephemeral-test-root.sh --self-test' >&2
        exit 2
    }
    self_test_ephemeral_test_root
    exit
fi

[[ ${1:-} == -- && $# -gt 1 ]] || {
    echo 'usage: run-with-ephemeral-test-root.sh -- COMMAND [ARGS...]' >&2
    exit 2
}
shift
run_with_ephemeral_test_root "$@"

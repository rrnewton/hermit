#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Give one unprivileged command an empty host mountpoint for Hermit's private
# tmpfs /test. Only fixed, absolute mkdir/rmdir operations run through sudo;
# caller bytes are always executed directly under the invoking uid.
# Catchable signals and child/scope timeouts are reaped before cleanup. Like any
# unprivileged process-owned cleanup, SIGKILL or an OOM kill of this outside-
# scope owner cannot run traps and can leave /test behind; the next owner then
# refuses the pre-existing path instead of silently adopting it.
set -euo pipefail

readonly sudo_bin=/usr/bin/sudo
readonly mkdir_bin=/usr/bin/mkdir
readonly rmdir_bin=/usr/bin/rmdir
readonly stat_bin=/usr/bin/stat
readonly mountpoint_bin=/usr/bin/mountpoint
readonly find_bin=/usr/bin/find
readonly flock_bin=/usr/bin/flock
readonly cmp_bin=/usr/bin/cmp
readonly id_bin=/usr/bin/id
readonly env_bin=/usr/bin/env
readonly mktemp_bin=/usr/bin/mktemp
readonly mkfifo_bin=/usr/bin/mkfifo
readonly mv_bin=/usr/bin/mv
readonly readlink_bin=/usr/bin/readlink
readonly unlink_bin=/usr/bin/unlink
readonly setsid_bin=/usr/bin/setsid
readonly kill_bin=/usr/bin/kill
readonly sleep_bin=/usr/bin/sleep
readonly timeout_bin=/usr/bin/timeout
readonly ephemeral_test_root_lease_var=HERMIT_EPHEMERAL_TEST_ROOT_LEASE_V2
readonly ephemeral_test_root_lease_arg=--internal-ephemeral-test-root-lease-v2
readonly ephemeral_test_root_holder_arg=--internal-ephemeral-test-root-lock-holder
readonly ephemeral_test_root_early_failure_arg=--internal-ephemeral-test-root-early-failure
ephemeral_test_root_wrapper_path=$("$readlink_bin" -f -- "${BASH_SOURCE[0]}")
readonly ephemeral_test_root_wrapper_path
ephemeral_test_root_bash_path=$("$readlink_bin" -f -- /usr/bin/bash)
readonly ephemeral_test_root_bash_path
ephemeral_test_root_path=/test
ephemeral_test_root_expected_uid=0
ephemeral_test_root_expected_gid=0
ephemeral_test_root_privileged=1
ephemeral_test_root_identity=
ephemeral_test_root_active=0
ephemeral_test_root_child_pid=
ephemeral_test_root_child_pgid=
ephemeral_test_root_creator_fd=
ephemeral_test_root_holder_pid=
ephemeral_test_root_holder_start=
ephemeral_test_root_holder_fd=
ephemeral_test_root_holder_state_dir=
ephemeral_test_root_holder_identity_file=
ephemeral_test_root_holder_armed_file=
ephemeral_test_root_holder_ready=
ephemeral_test_root_holder_log=
ephemeral_test_root_holder_timer=
ephemeral_test_root_holder_armed=0
ephemeral_test_root_holder_never_ready=0
ephemeral_test_root_holder_ready_spins=500
ephemeral_test_root_fail_after_holder_identity_capture=0
ephemeral_test_root_lock_observation_delay=0
ephemeral_test_root_pending_signal=
ephemeral_test_root_pending_status=0
ephemeral_test_root_fail_after_create=0
ephemeral_test_root_create_count=0
ephemeral_test_root_remove_count=0
ephemeral_test_root_forward_lease_argv=0
ephemeral_test_root_proc_identity_reader=ephemeral_test_root_proc_identity
ephemeral_test_root_between_chain_passes_hook=
ephemeral_test_root_cgroup_comparer=ephemeral_test_root_processes_share_cgroup
ephemeral_test_root_last_holder_error=
ephemeral_test_root_proc_locks_checker=ephemeral_test_root_proc_locks_has_exact_lock

parse_ephemeral_test_root_proc_identity() {
    local line=$1 remainder
    [[ $line == *") "* ]] || return 1
    remainder=${line##*) }
    local -a fields
    read -r -a fields <<< "$remainder"
    ((${#fields[@]} >= 20)) || return 1
    # After stripping pid/comm, array[1] is field 4 (ppid) and array[19]
    # is field 22 (starttime).
    [[ ${fields[1]} =~ ^[0-9]+$ && ${fields[19]} =~ ^[0-9]+$ ]] || return 1
    printf '%s:%s\n' "${fields[1]}" "${fields[19]}"
}

ephemeral_test_root_proc_identity() {
    local pid=$1 line
    [[ $pid =~ ^[0-9]+$ && -r /proc/$pid/stat ]] || return 1
    IFS= read -r line < "/proc/$pid/stat" || return 1
    parse_ephemeral_test_root_proc_identity "$line"
}

ephemeral_test_root_processes_share_cgroup() {
    local owner_pid=$1 holder_pid=$2
    [[ -r /proc/$owner_pid/cgroup && -r /proc/$holder_pid/cgroup ]] || return 1
    "$cmp_bin" -s -- "/proc/$owner_pid/cgroup" "/proc/$holder_pid/cgroup"
}

ephemeral_test_root_process_is_canonical_wrapper() {
    local pid=$1 stage=${2:-owner} cwd candidate executable
    local -a argv=()
    [[ -r /proc/$pid/cmdline && -L /proc/$pid/exe ]] || return 1
    executable=$("$readlink_bin" -f -- "/proc/$pid/exe") || return 1
    [[ $executable == "$ephemeral_test_root_bash_path" ]] || return 1
    mapfile -d '' -t argv < "/proc/$pid/cmdline" || return 1
    ((${#argv[@]} >= 2)) || return 1
    [[ ${argv[0]##*/} == bash ]] || return 1
    cwd=$("$readlink_bin" -f -- "/proc/$pid/cwd") || return 1
    if [[ ${argv[1]} == /* ]]; then
        candidate=$("$readlink_bin" -f -- "${argv[1]}") || return 1
    else
        candidate=$("$readlink_bin" -f -- "$cwd/${argv[1]}") || return 1
    fi
    [[ $candidate == "$ephemeral_test_root_wrapper_path" ]] || return 1
    if [[ $stage == holder ]]; then
        [[ ${argv[2]:-} == "$ephemeral_test_root_holder_arg" ]] || return 1
    else
        [[ ${argv[2]:-} != "$ephemeral_test_root_holder_arg" ]] || return 1
    fi
}

ephemeral_test_root_fdinfo_has_exact_lock() {
    local holder_pid=$1 holder_fd=$2 expected_inode=$3
    local label _lock_id kind advisory access lock_pid device start end extra found=0
    [[ -r /proc/$holder_pid/fdinfo/$holder_fd ]] || return 1
    while read -r label _lock_id kind advisory access lock_pid device start end extra; do
        [[ $label == lock: ]] || continue
        [[ $kind == FLOCK && $advisory == ADVISORY && $access == WRITE &&
           $lock_pid == "$holder_pid" && ${device##*:} == "$expected_inode" &&
           $start == 0 && $end == EOF && -z ${extra:-} ]] || continue
        found=$((found + 1))
    done < "/proc/$holder_pid/fdinfo/$holder_fd"
    [[ $found -eq 1 ]]
}

ephemeral_test_root_proc_locks_has_exact_lock() {
    local holder_pid=$1 expected_inode=$2
    local _lock_id kind advisory access lock_pid device start end extra found=0
    while read -r _lock_id kind advisory access lock_pid device start end extra; do
        [[ $kind == FLOCK && $advisory == ADVISORY && $access == WRITE &&
           $lock_pid == "$holder_pid" && ${device##*:} == "$expected_inode" &&
           $start == 0 && $end == EOF && -z ${extra:-} ]] || continue
        found=$((found + 1))
    done < /proc/locks
    [[ $found -eq 1 ]]
}

ephemeral_test_root_delayed_proc_locks_has_exact_lock() {
    if ((ephemeral_test_root_lock_observation_delay > 0)); then
        ephemeral_test_root_lock_observation_delay=$((
            ephemeral_test_root_lock_observation_delay - 1
        ))
        return 1
    fi
    ephemeral_test_root_proc_locks_has_exact_lock "$@"
}

ephemeral_test_root_holder_is_exact() {
    local owner_pid=$1 owner_start=$2 holder_pid=$3 holder_start=$4 holder_fd=$5
    local metadata
    ephemeral_test_root_last_holder_error=
    [[ $holder_fd =~ ^[0-9]+$ ]] || {
        ephemeral_test_root_last_holder_error=malformed-fd
        return 1
    }
    ephemeral_test_root_holder_process_is_exact \
        "$owner_pid" "$owner_start" "$holder_pid" "$holder_start" || return 1
    [[ -d /proc/$holder_pid/fd/$holder_fd ]] || {
        ephemeral_test_root_last_holder_error=missing-fd
        return 1
    }
    metadata=$(LC_ALL=C "$stat_bin" -Lc '%d:%i' -- "/proc/$holder_pid/fd/$holder_fd") || {
        ephemeral_test_root_last_holder_error=unreadable-fd
        return 1
    }
    [[ $metadata == "$ephemeral_test_root_identity" ]] || {
        ephemeral_test_root_last_holder_error=changed-fd-inode
        return 1
    }
    ephemeral_test_root_fdinfo_has_exact_lock \
        "$holder_pid" "$holder_fd" "${ephemeral_test_root_identity#*:}" || {
        ephemeral_test_root_last_holder_error=fdinfo-lock
        return 1
    }
    "$ephemeral_test_root_proc_locks_checker" \
        "$holder_pid" "${ephemeral_test_root_identity#*:}" || {
        ephemeral_test_root_last_holder_error=proc-locks
        return 1
    }
}

ephemeral_test_root_holder_is_stably_exact() {
    local spin consecutive=0
    for ((spin = 0; spin < 100; spin++)); do
        if ephemeral_test_root_holder_is_exact "$@"; then
            consecutive=$((consecutive + 1))
            ((consecutive == 2)) && return 0
        else
            [[ $ephemeral_test_root_last_holder_error == proc-locks ||
               $ephemeral_test_root_last_holder_error == fdinfo-lock ]] || return 1
            consecutive=0
        fi
        "$sleep_bin" 0.01 || :
    done
    return 1
}

ephemeral_test_root_holder_process_is_exact() {
    local owner_pid=$1 owner_start=$2 holder_pid=$3 holder_start=$4
    local identity
    [[ $holder_pid != "$owner_pid" ]] || {
        ephemeral_test_root_last_holder_error=self-holder
        return 1
    }
    identity=$(ephemeral_test_root_proc_identity "$holder_pid") || {
        ephemeral_test_root_last_holder_error=missing-holder
        return 1
    }
    [[ ${identity%%:*} == "$owner_pid" && ${identity#*:} == "$holder_start" ]] || {
        ephemeral_test_root_last_holder_error=holder-parent-or-start
        return 1
    }
    ephemeral_test_root_capture_ancestor_chain \
        "$owner_pid" "$owner_start" "$holder_pid" >/dev/null || {
        ephemeral_test_root_last_holder_error=holder-ancestry
        return 1
    }
    "$ephemeral_test_root_cgroup_comparer" "$owner_pid" "$holder_pid" || {
        ephemeral_test_root_last_holder_error=holder-cgroup
        return 1
    }
    ephemeral_test_root_process_is_canonical_wrapper "$owner_pid" owner || {
        ephemeral_test_root_last_holder_error=owner-command
        return 1
    }
    ephemeral_test_root_process_is_canonical_wrapper "$holder_pid" holder || {
        ephemeral_test_root_last_holder_error=holder-command
        return 1
    }
}

ephemeral_test_root_holder_owner_is_live() {
    local owner_pid=$1 owner_start=$2 holder_pid=$3 holder_start=$4
    local owner_identity holder_identity
    owner_identity=$(ephemeral_test_root_proc_identity "$owner_pid") || return 1
    holder_identity=$(ephemeral_test_root_proc_identity "$holder_pid") || return 1
    [[ ${owner_identity#*:} == "$owner_start" &&
       ${holder_identity%%:*} == "$owner_pid" &&
       ${holder_identity#*:} == "$holder_start" ]]
}

ephemeral_test_root_capture_ancestor_chain() {
    local owner_pid=$1 owner_start=$2 current=$3 depth=0 identity identity_after parent start uid
    local caller_uid chain=
    caller_uid=$("$id_bin" -u) || return 1
    [[ $current != "$owner_pid" ]] || return 1
    while ((current > 1 && depth < 256)); do
        identity=$("$ephemeral_test_root_proc_identity_reader" "$current") || return 1
        parent=${identity%%:*}
        start=${identity#*:}
        uid=$(LC_ALL=C "$stat_bin" -c '%u' -- "/proc/$current") || return 1
        identity_after=$("$ephemeral_test_root_proc_identity_reader" "$current") || return 1
        [[ $identity_after == "$identity" ]] || return 1
        chain+="$current:$start:$uid:$parent"$'\n'
        if [[ $current == "$owner_pid" ]]; then
            [[ $start == "$owner_start" && $uid == "$caller_uid" ]] || return 1
            printf '%s' "$chain"
            return 0
        fi
        current=$parent
        depth=$((depth + 1))
    done
    return 1
}

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
        "$sudo_bin" -n "$mkdir_bin" --mode=0755 -- "$ephemeral_test_root_path"
    else
        "$mkdir_bin" --mode=0755 -- "$ephemeral_test_root_path"
    fi
    ephemeral_test_root_active=1
    ephemeral_test_root_create_count=$((ephemeral_test_root_create_count + 1))
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

run_ephemeral_test_root_lock_holder() {
    [[ $# -eq 8 ]] || return 2
    ephemeral_test_root_path=$1
    ephemeral_test_root_identity=$2
    ephemeral_test_root_expected_uid=$3
    ephemeral_test_root_expected_gid=$4
    local owner_pid=$5 owner_start=$6 state_dir=$7 never_ready=$8
    local identity_file=$state_dir/identity armed_file=$state_dir/armed
    local ready=$state_dir/ready timer=$state_dir/timer
    [[ $never_ready == 0 || $never_ready == 1 ]] || return 2
    ephemeral_test_root_active=1
    # Test-only parent injection must never weaken the holder's own check.
    ephemeral_test_root_proc_locks_checker=ephemeral_test_root_proc_locks_has_exact_lock
    local holder_pid=$BASHPID holder_identity holder_start fd fd_path fd_metadata timer_fd
    holder_identity=$(ephemeral_test_root_proc_identity "$holder_pid") || {
        echo 'lock holder: cannot read its process identity' >&2
        return 2
    }
    holder_start=${holder_identity#*:}
    ephemeral_test_root_capture_ancestor_chain \
        "$owner_pid" "$owner_start" "$holder_pid" >/dev/null || {
        echo 'lock holder: owner ancestry is not exact' >&2
        return 2
    }
    ephemeral_test_root_process_is_canonical_wrapper "$owner_pid" owner || {
        echo 'lock holder: owner command is not canonical' >&2
        return 2
    }
    ephemeral_test_root_process_is_canonical_wrapper "$holder_pid" holder || {
        echo 'lock holder: holder command is not canonical' >&2
        return 2
    }
    ephemeral_test_root_exact || {
        echo 'lock holder: test-root path is not exact' >&2
        return 2
    }

    local -a matching_fds=()
    for fd_path in "/proc/$holder_pid/fd/"*; do
        fd=${fd_path##*/}
        [[ $fd =~ ^[0-9]+$ ]] || continue
        fd_metadata=$(LC_ALL=C "$stat_bin" -Lc '%d:%i' -- "$fd_path" 2>/dev/null) || continue
        [[ $fd_metadata == "$ephemeral_test_root_identity" ]] || continue
        matching_fds+=("$fd")
    done
    ((${#matching_fds[@]} == 1)) || {
        echo "lock holder: found ${#matching_fds[@]} exact directory descriptors" >&2
        return 2
    }
    fd=${matching_fds[0]}
    exec {timer_fd}<>"$timer" || return 2
    umask 077
    printf '%s:%s:%s\n' "$holder_pid" "$holder_start" "$fd" > "$identity_file" || return 2
    trap 'exit 0' HUP INT TERM
    while [[ ! -e $armed_file ]]; do
        ephemeral_test_root_holder_owner_is_live \
            "$owner_pid" "$owner_start" "$holder_pid" "$holder_start" || return 0
        IFS= read -r -t 0.01 -u "$timer_fd" _ || :
    done
    local armed_exact=0 consecutive=0 spin
    for ((spin = 0; spin < 100; spin++)); do
        if ephemeral_test_root_holder_is_exact \
            "$owner_pid" "$owner_start" "$holder_pid" "$holder_start" "$fd"; then
            consecutive=$((consecutive + 1))
            if ((consecutive == 2)); then
                armed_exact=1
                break
            fi
        else
            [[ $ephemeral_test_root_last_holder_error == proc-locks ||
               $ephemeral_test_root_last_holder_error == fdinfo-lock ]] || break
            consecutive=0
        fi
        IFS= read -r -t 0.01 -u "$timer_fd" _ || :
    done
    ((armed_exact)) || {
        echo "lock holder: armed invariant failed ($ephemeral_test_root_last_holder_error)" >&2
        return 2
    }
    if ((never_ready == 0)); then
        printf '%s:%s:%s\n' "$holder_pid" "$holder_start" "$fd" > "$ready" || return 2
    fi
    while ephemeral_test_root_holder_owner_is_live \
        "$owner_pid" "$owner_start" "$holder_pid" "$holder_start"; do
        # A Bash builtin timeout avoids spawning a timer child that could
        # transiently inherit the lock or captured stdio.
        IFS= read -r -t 0.5 -u "$timer_fd" _ || :
    done
    echo 'lock holder: exact owner PID/start/PPID relation disappeared' >&2
}

start_ephemeral_test_root_lock_holder() {
    local owner_pid=$BASHPID owner_identity owner_start
    owner_identity=$(ephemeral_test_root_proc_identity "$owner_pid") || {
        echo 'run-with-ephemeral-test-root.sh: cannot read outer lease-owner identity' >&2
        return 2
    }
    owner_start=${owner_identity#*:}
    ephemeral_test_root_process_is_canonical_wrapper "$owner_pid" owner || {
        echo 'run-with-ephemeral-test-root.sh: outer lease owner is not the canonical wrapper process' >&2
        return 2
    }
    exec {ephemeral_test_root_creator_fd}<"$ephemeral_test_root_path" || return 2
    ephemeral_test_root_holder_state_dir=$(
        "$mktemp_bin" -d "${TMPDIR:-/tmp}/hermit-ephemeral-test-root-lock.XXXXXXXXXX"
    ) || return 2
    ephemeral_test_root_holder_identity_file=$ephemeral_test_root_holder_state_dir/identity
    ephemeral_test_root_holder_armed_file=$ephemeral_test_root_holder_state_dir/armed
    ephemeral_test_root_holder_ready=$ephemeral_test_root_holder_state_dir/ready
    ephemeral_test_root_holder_log=$ephemeral_test_root_holder_state_dir/holder.log
    ephemeral_test_root_holder_timer=$ephemeral_test_root_holder_state_dir/timer
    "$mkfifo_bin" --mode=0600 -- "$ephemeral_test_root_holder_timer" || return 2
    "$flock_bin" --exclusive --nonblock --no-fork "$ephemeral_test_root_path" \
        "$ephemeral_test_root_wrapper_path" "$ephemeral_test_root_holder_arg" \
        "$ephemeral_test_root_path" "$ephemeral_test_root_identity" \
        "$ephemeral_test_root_expected_uid" "$ephemeral_test_root_expected_gid" \
        "$owner_pid" "$owner_start" "$ephemeral_test_root_holder_state_dir" \
        "$ephemeral_test_root_holder_never_ready" \
        {ephemeral_test_root_creator_fd}<&- </dev/null >/dev/null \
        2>"$ephemeral_test_root_holder_log" &
    ephemeral_test_root_holder_pid=$!
    local spawned_identity spin
    for ((spin = 0; spin < ephemeral_test_root_holder_ready_spins; spin++)); do
        if spawned_identity=$(ephemeral_test_root_proc_identity \
            "$ephemeral_test_root_holder_pid"); then
            ephemeral_test_root_holder_start=${spawned_identity#*:}
            if ephemeral_test_root_holder_process_is_exact \
                "$owner_pid" "$owner_start" "$ephemeral_test_root_holder_pid" \
                "$ephemeral_test_root_holder_start"; then
                break
            fi
        fi
        if ! "$kill_bin" -s 0 -- "$ephemeral_test_root_holder_pid" 2>/dev/null; then
            wait "$ephemeral_test_root_holder_pid" 2>/dev/null || true
            ephemeral_test_root_holder_pid=
            ephemeral_test_root_holder_start=
            echo 'run-with-ephemeral-test-root.sh: lock holder exited before identity capture:' >&2
            while IFS= read -r line; do
                echo "  $line" >&2
            done < "$ephemeral_test_root_holder_log"
            return 2
        fi
        "$sleep_bin" 0.01 || :
    done
    if [[ -z $ephemeral_test_root_holder_start ]] ||
        ! ephemeral_test_root_holder_process_is_exact \
            "$owner_pid" "$owner_start" "$ephemeral_test_root_holder_pid" \
            "$ephemeral_test_root_holder_start"; then
        echo 'run-with-ephemeral-test-root.sh: lock holder never reached its exact canonical identity' >&2
        return 2
    fi
    for ((spin = 0; spin < ephemeral_test_root_holder_ready_spins; spin++)); do
        [[ -s $ephemeral_test_root_holder_identity_file ]] && break
        if ! "$kill_bin" -s 0 -- "$ephemeral_test_root_holder_pid" 2>/dev/null; then
            wait "$ephemeral_test_root_holder_pid" 2>/dev/null || true
            ephemeral_test_root_holder_pid=
            echo 'run-with-ephemeral-test-root.sh: lock holder exited before FD publication:' >&2
            while IFS= read -r line; do
                echo "  $line" >&2
            done < "$ephemeral_test_root_holder_log"
            return 2
        fi
        "$sleep_bin" 0.01 || :
    done
    [[ -s $ephemeral_test_root_holder_identity_file ]] || {
        echo 'run-with-ephemeral-test-root.sh: lock holder did not publish its FD identity' >&2
        return 2
    }
    local reported_pid reported_start
    IFS=: read -r reported_pid reported_start ephemeral_test_root_holder_fd \
        < "$ephemeral_test_root_holder_identity_file" || return 2
    [[ $reported_pid == "$ephemeral_test_root_holder_pid" &&
       $reported_start == "$ephemeral_test_root_holder_start" ]] || return 2
    if ((ephemeral_test_root_fail_after_holder_identity_capture)); then
        echo 'run-with-ephemeral-test-root.sh: planted post-identity holder failure' >&2
        return 2
    fi
    if ((ephemeral_test_root_lock_observation_delay > 0)); then
        ephemeral_test_root_proc_locks_checker=ephemeral_test_root_delayed_proc_locks_has_exact_lock
    fi
    ephemeral_test_root_holder_is_stably_exact \
        "$owner_pid" "$owner_start" "$ephemeral_test_root_holder_pid" \
        "$ephemeral_test_root_holder_start" "$ephemeral_test_root_holder_fd" || {
        echo "run-with-ephemeral-test-root.sh: lock holder never published its exact flock ($ephemeral_test_root_last_holder_error)" >&2
        while IFS= read -r line; do
            echo "  holder fdinfo: $line" >&2
        done < "/proc/$ephemeral_test_root_holder_pid/fdinfo/$ephemeral_test_root_holder_fd"
        while IFS= read -r line; do
            [[ $line == *":${ephemeral_test_root_identity#*:} "* ]] || continue
            echo "  proc locks: $line" >&2
        done < /proc/locks
        return 2
    }
    : > "$ephemeral_test_root_holder_armed_file"
    ephemeral_test_root_holder_armed=1
    for ((spin = 0; spin < ephemeral_test_root_holder_ready_spins; spin++)); do
        [[ -s $ephemeral_test_root_holder_ready ]] && break
        if ! "$kill_bin" -s 0 -- "$ephemeral_test_root_holder_pid" 2>/dev/null; then
            wait "$ephemeral_test_root_holder_pid" 2>/dev/null || true
            ephemeral_test_root_holder_pid=
            echo 'run-with-ephemeral-test-root.sh: lock holder exited before readiness:' >&2
            while IFS= read -r line; do
                echo "  $line" >&2
            done < "$ephemeral_test_root_holder_log"
            return 2
        fi
        # A catchable signal records its status but does not abandon an
        # incompletely identified child. Finish the holder handshake so cleanup
        # can prove PID/start before signaling it.
        "$sleep_bin" 0.01 || :
    done
    [[ -s $ephemeral_test_root_holder_ready ]] || {
        echo 'run-with-ephemeral-test-root.sh: lock holder did not become ready' >&2
        return 2
    }
    local ready_pid ready_start ready_fd
    IFS=: read -r ready_pid ready_start ready_fd \
        < "$ephemeral_test_root_holder_ready" || return 2
    [[ $ready_pid == "$ephemeral_test_root_holder_pid" &&
       $ready_start == "$ephemeral_test_root_holder_start" &&
       $ready_fd == "$ephemeral_test_root_holder_fd" ]] || return 2
    ephemeral_test_root_holder_is_exact \
        "$owner_pid" "$owner_start" "$ephemeral_test_root_holder_pid" \
        "$ephemeral_test_root_holder_start" "$ephemeral_test_root_holder_fd" || {
        echo "run-with-ephemeral-test-root.sh: lock holder failed exact identity validation ($ephemeral_test_root_last_holder_error)" >&2
        return 2
    }
    exec {ephemeral_test_root_creator_fd}<&-
    ephemeral_test_root_creator_fd=
    ((ephemeral_test_root_pending_status == 0)) || return "$ephemeral_test_root_pending_status"
}

stop_ephemeral_test_root_lock_holder() {
    local failed=0 owner_pid=$BASHPID owner_identity owner_start
    if [[ -n $ephemeral_test_root_holder_pid ]]; then
        owner_identity=$(ephemeral_test_root_proc_identity "$owner_pid") || return 1
        owner_start=${owner_identity#*:}
        ephemeral_test_root_process_is_canonical_wrapper "$owner_pid" owner || return 1
        # Never send a signal to a numeric PID after identity validation fails:
        # it may have been reused. Leave all holder state intact so the caller
        # also refuses to remove the directory.
        local holder_identity
        holder_identity=$(ephemeral_test_root_proc_identity \
            "$ephemeral_test_root_holder_pid") || return 1
        [[ ${holder_identity#*:} == "$ephemeral_test_root_holder_start" ]] || return 1
        if ((ephemeral_test_root_holder_armed)); then
            ephemeral_test_root_holder_is_exact \
                "$owner_pid" "$owner_start" "$ephemeral_test_root_holder_pid" \
                "$ephemeral_test_root_holder_start" "$ephemeral_test_root_holder_fd" || {
                echo "run-with-ephemeral-test-root.sh: refusing to signal changed lock holder ($ephemeral_test_root_last_holder_error)" >&2
                return 1
            }
        else
            ephemeral_test_root_holder_process_is_exact \
                "$owner_pid" "$owner_start" "$ephemeral_test_root_holder_pid" \
                "$ephemeral_test_root_holder_start" || {
                echo "run-with-ephemeral-test-root.sh: refusing to signal changed pre-ready holder ($ephemeral_test_root_last_holder_error)" >&2
                return 1
            }
        fi
        local stopped_pid=$ephemeral_test_root_holder_pid
        local stopped_start=$ephemeral_test_root_holder_start
        "$kill_bin" -s TERM -- "$stopped_pid" 2>/dev/null || true
        # An exactly identified pre-readiness holder may not yet have installed
        # its TERM trap, so wait(1) can legitimately report 143. Cleanup is
        # successful iff that exact PID@start is gone after our signal.
        wait "$stopped_pid" 2>/dev/null || true
        local stopped_identity
        stopped_identity=$(ephemeral_test_root_proc_identity "$stopped_pid" 2>/dev/null || :)
        [[ ${stopped_identity#*:} != "$stopped_start" ]] || failed=1
        ephemeral_test_root_holder_pid=
        ephemeral_test_root_holder_start=
        ephemeral_test_root_holder_fd=
        ephemeral_test_root_holder_armed=0
    fi
    if [[ -n $ephemeral_test_root_creator_fd ]]; then
        exec {ephemeral_test_root_creator_fd}<&-
        ephemeral_test_root_creator_fd=
    fi
    if [[ -n $ephemeral_test_root_holder_ready && -e $ephemeral_test_root_holder_ready ]]; then
        "$unlink_bin" -- "$ephemeral_test_root_holder_ready" || failed=1
    fi
    if [[ -n $ephemeral_test_root_holder_armed_file && -e $ephemeral_test_root_holder_armed_file ]]; then
        "$unlink_bin" -- "$ephemeral_test_root_holder_armed_file" || failed=1
    fi
    if [[ -n $ephemeral_test_root_holder_identity_file && -e $ephemeral_test_root_holder_identity_file ]]; then
        "$unlink_bin" -- "$ephemeral_test_root_holder_identity_file" || failed=1
    fi
    if [[ -n $ephemeral_test_root_holder_log && -e $ephemeral_test_root_holder_log ]]; then
        "$unlink_bin" -- "$ephemeral_test_root_holder_log" || failed=1
    fi
    if [[ -n $ephemeral_test_root_holder_timer && -p $ephemeral_test_root_holder_timer ]]; then
        "$unlink_bin" -- "$ephemeral_test_root_holder_timer" || failed=1
    fi
    if [[ -n $ephemeral_test_root_holder_state_dir && -d $ephemeral_test_root_holder_state_dir ]]; then
        "$rmdir_bin" -- "$ephemeral_test_root_holder_state_dir" || failed=1
    fi
    ephemeral_test_root_holder_ready=
    ephemeral_test_root_holder_log=
    ephemeral_test_root_holder_armed_file=
    ephemeral_test_root_holder_identity_file=
    ephemeral_test_root_holder_timer=
    ephemeral_test_root_holder_state_dir=
    ((failed == 0))
}

remove_ephemeral_test_root() {
    [[ -z $ephemeral_test_root_holder_pid && -z $ephemeral_test_root_creator_fd ]] || {
        echo "run-with-ephemeral-test-root.sh: refusing cleanup while a lease lock is still held" >&2
        return 1
    }
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
    ephemeral_test_root_remove_count=$((ephemeral_test_root_remove_count + 1))
    ephemeral_test_root_absent || {
        echo "run-with-ephemeral-test-root.sh: $ephemeral_test_root_path remained after cleanup" >&2
        return 1
    }
}

validate_existing_ephemeral_test_root_lease() {
    local lease=${!ephemeral_test_root_lease_var-}
    [[ $lease =~ ^v2:([0-9]+):([0-9]+):([0-9]+):([0-9]+):([0-9]+):([0-9]+):([0-9]+)$ ]] || {
        echo 'run-with-ephemeral-test-root.sh: malformed inherited /test lease' >&2
        return 2
    }
    ephemeral_test_root_identity="${BASH_REMATCH[1]}:${BASH_REMATCH[2]}"
    local owner_pid=${BASH_REMATCH[3]}
    local owner_start=${BASH_REMATCH[4]}
    local holder_pid=${BASH_REMATCH[5]}
    local holder_start=${BASH_REMATCH[6]}
    local holder_fd=${BASH_REMATCH[7]}
    local chain_before chain_after caller_pid=$BASHPID
    chain_before=$(ephemeral_test_root_capture_ancestor_chain \
        "$owner_pid" "$owner_start" "$caller_pid") || {
        echo 'run-with-ephemeral-test-root.sh: inherited /test lease owner is not a live ancestor' >&2
        return 2
    }
    local holder_chain_before holder_chain_after
    holder_chain_before=$(ephemeral_test_root_capture_ancestor_chain \
        "$owner_pid" "$owner_start" "$holder_pid") || {
        echo 'run-with-ephemeral-test-root.sh: inherited /test lock holder is not owned by the live wrapper' >&2
        return 2
    }
    ephemeral_test_root_process_is_canonical_wrapper "$owner_pid" owner || {
        echo 'run-with-ephemeral-test-root.sh: inherited /test lease owner is not the canonical wrapper process' >&2
        return 2
    }
    ephemeral_test_root_active=1
    ephemeral_test_root_exact || {
        ephemeral_test_root_active=0
        echo 'run-with-ephemeral-test-root.sh: inherited /test lease no longer names the exact empty root-owned non-mount directory' >&2
        return 2
    }
    ephemeral_test_root_holder_is_exact \
        "$owner_pid" "$owner_start" "$holder_pid" "$holder_start" "$holder_fd" || {
        ephemeral_test_root_active=0
        echo 'run-with-ephemeral-test-root.sh: inherited /test lease has no exact live lock holder' >&2
        return 2
    }
    if [[ -n $ephemeral_test_root_between_chain_passes_hook ]]; then
        "$ephemeral_test_root_between_chain_passes_hook"
    fi
    chain_after=$(ephemeral_test_root_capture_ancestor_chain \
        "$owner_pid" "$owner_start" "$caller_pid") || {
        ephemeral_test_root_active=0
        echo 'run-with-ephemeral-test-root.sh: inherited /test lease ancestry changed during validation' >&2
        return 2
    }
    holder_chain_after=$(ephemeral_test_root_capture_ancestor_chain \
        "$owner_pid" "$owner_start" "$holder_pid") || {
        ephemeral_test_root_active=0
        echo 'run-with-ephemeral-test-root.sh: inherited /test lock-holder ancestry changed during validation' >&2
        return 2
    }
    if [[ $chain_after != "$chain_before" || $holder_chain_after != "$holder_chain_before" ]]; then
        ephemeral_test_root_active=0
        echo 'run-with-ephemeral-test-root.sh: inherited /test lease process chain changed during validation' >&2
        return 2
    fi
    if ! ephemeral_test_root_exact; then
        ephemeral_test_root_active=0
        echo 'run-with-ephemeral-test-root.sh: inherited /test pathname changed during validation' >&2
        return 2
    fi
    if ! ephemeral_test_root_process_is_canonical_wrapper "$owner_pid" owner; then
        ephemeral_test_root_active=0
        echo 'run-with-ephemeral-test-root.sh: inherited /test owner changed during validation' >&2
        return 2
    fi
    if ! ephemeral_test_root_holder_is_exact \
        "$owner_pid" "$owner_start" "$holder_pid" "$holder_start" "$holder_fd"; then
        ephemeral_test_root_active=0
        echo "run-with-ephemeral-test-root.sh: inherited /test holder changed during validation ($ephemeral_test_root_last_holder_error)" >&2
        return 2
    fi
    ephemeral_test_root_active=0
}

run_with_existing_ephemeral_test_root_lease() {
    validate_existing_ephemeral_test_root_lease || return
    # The outer owner waits for this whole process tree and performs the sole
    # cleanup. Never install cleanup traps, call setsid, or invoke sudo on an
    # inherited lease.
    if ((ephemeral_test_root_forward_lease_argv)); then
        validate_ephemeral_test_root_child_argv "$@" || return
        exec "$env_bin" -u "$ephemeral_test_root_lease_var" -- \
            "$@" "$ephemeral_test_root_lease_arg=${!ephemeral_test_root_lease_var}"
    fi
    exec "$@"
}

validate_ephemeral_test_root_child_argv() {
    local argument
    for argument in "$@"; do
        if [[ $argument == "$ephemeral_test_root_lease_arg"* ]]; then
            echo 'run-with-ephemeral-test-root.sh: refusing caller-supplied internal /test lease argument' >&2
            return 2
        fi
    done
}

publish_ephemeral_test_root_lease() {
    local identity owner_start owner_pid=$BASHPID
    identity=$(ephemeral_test_root_proc_identity "$owner_pid") || {
        echo 'run-with-ephemeral-test-root.sh: cannot read outer lease-owner identity' >&2
        return 2
    }
    owner_start=${identity#*:}
    ephemeral_test_root_holder_is_exact \
        "$owner_pid" "$owner_start" "$ephemeral_test_root_holder_pid" \
        "$ephemeral_test_root_holder_start" "$ephemeral_test_root_holder_fd" || {
        echo "run-with-ephemeral-test-root.sh: cannot publish an unbound /test lock holder ($ephemeral_test_root_last_holder_error)" >&2
        return 2
    }
    export "$ephemeral_test_root_lease_var=v2:$ephemeral_test_root_identity:$owner_pid:$owner_start:$ephemeral_test_root_holder_pid:$ephemeral_test_root_holder_start:$ephemeral_test_root_holder_fd"
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
    if ! stop_ephemeral_test_root_lock_holder; then
        cleanup_failed=1
    fi
    if ((ephemeral_test_root_active)) && ! remove_ephemeral_test_root; then
        cleanup_failed=1
    fi
    if ((cleanup_failed == 0 && ephemeral_test_root_pending_status != 0)); then
        status=$ephemeral_test_root_pending_status
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
    ephemeral_test_root_creator_fd=
    ephemeral_test_root_holder_pid=
    ephemeral_test_root_holder_start=
    ephemeral_test_root_holder_fd=
    ephemeral_test_root_holder_state_dir=
    ephemeral_test_root_holder_identity_file=
    ephemeral_test_root_holder_armed_file=
    ephemeral_test_root_holder_ready=
    ephemeral_test_root_holder_log=
    ephemeral_test_root_holder_timer=
    ephemeral_test_root_holder_armed=0
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
        if ! stop_ephemeral_test_root_lock_holder; then
            cleanup_failed=1
        fi
        if ((ephemeral_test_root_active)) && ! remove_ephemeral_test_root; then
            cleanup_failed=1
        fi
        if ((cleanup_failed == 0 && ephemeral_test_root_pending_status != 0)); then
            status=$ephemeral_test_root_pending_status
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
        if ! stop_ephemeral_test_root_lock_holder; then
            status=125
        fi
        if ! remove_ephemeral_test_root; then
            status=125
        fi
        if ((status != 125 && ephemeral_test_root_pending_status != 0)); then
            status=$ephemeral_test_root_pending_status
        fi
        trap - HUP INT TERM
        if ((status != 125 && ephemeral_test_root_pending_status != 0)); then
            status=$ephemeral_test_root_pending_status
        fi
        trap - EXIT
        return "$status"
    fi

    if start_ephemeral_test_root_lock_holder; then
        :
    else
        status=$?
        if ! stop_ephemeral_test_root_lock_holder; then
            status=125
        fi
        if ! remove_ephemeral_test_root; then
            status=125
        fi
        if ((status != 125 && ephemeral_test_root_pending_status != 0)); then
            status=$ephemeral_test_root_pending_status
        fi
        trap - HUP INT TERM
        if ((status != 125 && ephemeral_test_root_pending_status != 0)); then
            status=$ephemeral_test_root_pending_status
        fi
        trap - EXIT
        return "$status"
    fi

    if ((ephemeral_test_root_pending_status != 0)); then
        status=$ephemeral_test_root_pending_status
        if ! stop_ephemeral_test_root_lock_holder; then
            status=125
        fi
        if ! remove_ephemeral_test_root; then
            status=125
        fi
        if ((status != 125 && ephemeral_test_root_pending_status != 0)); then
            status=$ephemeral_test_root_pending_status
        fi
        trap - HUP INT TERM
        if ((status != 125 && ephemeral_test_root_pending_status != 0)); then
            status=$ephemeral_test_root_pending_status
        fi
        trap - EXIT
        return "$status"
    fi

    if publish_ephemeral_test_root_lease; then
        :
    else
        status=$?
        if ! stop_ephemeral_test_root_lock_holder; then
            status=125
        fi
        if ! remove_ephemeral_test_root; then
            status=125
        fi
        if ((status != 125 && ephemeral_test_root_pending_status != 0)); then
            status=$ephemeral_test_root_pending_status
        fi
        trap - HUP INT TERM
        if ((status != 125 && ephemeral_test_root_pending_status != 0)); then
            status=$ephemeral_test_root_pending_status
        fi
        trap - EXIT
        return "$status"
    fi
    if ((ephemeral_test_root_forward_lease_argv)); then
        validate_ephemeral_test_root_child_argv "$@" || return
        "$setsid_bin" --wait -- \
            "$env_bin" -u "$ephemeral_test_root_lease_var" -- \
            "$@" "$ephemeral_test_root_lease_arg=${!ephemeral_test_root_lease_var}" &
    else
        "$setsid_bin" --wait -- "$@" &
    fi
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
    unset "$ephemeral_test_root_lease_var"
    local cleanup_failed=0
    if ! stop_ephemeral_test_root_lock_holder; then
        cleanup_failed=1
    fi
    if ! remove_ephemeral_test_root; then
        cleanup_failed=1
    fi
    if ((cleanup_failed == 0 && ephemeral_test_root_pending_status != 0)); then
        status=$ephemeral_test_root_pending_status
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

ephemeral_test_root_mutated_proc_identity() {
    local identity parent start
    identity=$(ephemeral_test_root_proc_identity "$1") || return 1
    parent=${identity%%:*}
    start=${identity#*:}
    printf '%s:%s\n' "$parent" "$((start + 1))"
}

ephemeral_test_root_install_mutated_proc_reader() {
    ephemeral_test_root_proc_identity_reader=ephemeral_test_root_mutated_proc_identity
}

ephemeral_test_root_reject_cgroup_pair() {
    return 1
}

ephemeral_test_root_install_mutated_cgroup_comparer() {
    ephemeral_test_root_cgroup_comparer=ephemeral_test_root_reject_cgroup_pair
}

ephemeral_test_root_replace_path_between_passes() {
    "$mv_bin" -- "$ephemeral_test_root_path" "$ephemeral_test_root_path.original"
    "$mkdir_bin" --mode=0755 -- "$ephemeral_test_root_path"
}

cleanup_ephemeral_test_root_manual_probe_on_exit() {
    local status=$? cleanup_failed=0
    trap - EXIT
    unset "$ephemeral_test_root_lease_var"
    if ! stop_ephemeral_test_root_lock_holder; then
        cleanup_failed=1
    fi
    if ((ephemeral_test_root_active)) && ! remove_ephemeral_test_root; then
        cleanup_failed=1
    fi
    if ((cleanup_failed)); then
        status=125
    fi
    exit "$status"
}

run_ephemeral_test_root_failure_probe() {
    [[ $# -eq 3 ]] || return 2
    local mode=$1 marker=$3 status
    ephemeral_test_root_path=$2
    ephemeral_test_root_expected_uid=$("$id_bin" -u)
    ephemeral_test_root_expected_gid=$("$id_bin" -g)
    ephemeral_test_root_privileged=0
    ephemeral_test_root_holder_never_ready=0
    ephemeral_test_root_holder_ready_spins=500
    ephemeral_test_root_fail_after_holder_identity_capture=0
    ephemeral_test_root_lock_observation_delay=0
    trap cleanup_ephemeral_test_root_manual_probe_on_exit EXIT
    create_ephemeral_test_root
    case "$mode" in
        early-failure)
            start_ephemeral_test_root_lock_holder
            status=23
            ;;
        never-ready)
            ephemeral_test_root_holder_never_ready=1
            ephemeral_test_root_holder_ready_spins=20
            if start_ephemeral_test_root_lock_holder; then
                status=99
            else
                status=$?
            fi
            ;;
        post-capture-failure)
            ephemeral_test_root_fail_after_holder_identity_capture=1
            if start_ephemeral_test_root_lock_holder; then
                status=99
            else
                status=$?
            fi
            ;;
        delayed-lock)
            ephemeral_test_root_lock_observation_delay=10
            start_ephemeral_test_root_lock_holder
            status=23
            ;;
        owner-sigkill)
            start_ephemeral_test_root_lock_holder
            printf '%s:%s:%s:%s:%s\n' \
                "$ephemeral_test_root_holder_pid" "$ephemeral_test_root_holder_start" \
                "${ephemeral_test_root_identity%%:*}" "${ephemeral_test_root_identity#*:}" \
                "$ephemeral_test_root_holder_state_dir" > "$marker"
            # No child process may inherit the capture pipe. The outer timeout
            # SIGKILLs this stopped owner, after which the detached holder must
            # observe the lost exact parent identity and exit on its own.
            "$kill_bin" -s STOP -- "$BASHPID"
            return 99
            ;;
        *)
            return 2
            ;;
    esac
    printf '%s:%s\n' "$ephemeral_test_root_holder_pid" \
        "$ephemeral_test_root_holder_start" > "$marker"
    return "$status"
}

assert_ephemeral_test_root_owner_sigkill_probe() {
    [[ $# -eq 2 ]] || return 2
    local path=$1 marker=$2 output status started=$SECONDS
    if output=$("$timeout_bin" --foreground --preserve-status --signal=KILL 2 \
        "$ephemeral_test_root_wrapper_path" "$ephemeral_test_root_early_failure_arg" \
        owner-sigkill "$path" "$marker" 2>&1); then
        echo 'ephemeral /test self-test owner-SIGKILL probe unexpectedly passed' >&2
        return 1
    else
        status=$?
    fi
    [[ $status -eq 137 ]] || {
        echo "ephemeral /test self-test owner-SIGKILL returned $status, expected 137: $output" >&2
        return 1
    }
    local holder_pid holder_start device inode state_dir identity spin
    IFS=: read -r holder_pid holder_start device inode state_dir < "$marker" || return 1
    [[ $holder_pid =~ ^[0-9]+$ && $holder_start =~ ^[0-9]+$ &&
       $device =~ ^[0-9]+$ && $inode =~ ^[0-9]+$ ]] || return 1
    for ((spin = 0; spin < 500; spin++)); do
        identity=$(ephemeral_test_root_proc_identity "$holder_pid" 2>/dev/null || :)
        [[ ${identity#*:} != "$holder_start" ]] && break
        "$sleep_bin" 0.01
    done
    identity=$(ephemeral_test_root_proc_identity "$holder_pid" 2>/dev/null || :)
    [[ ${identity#*:} != "$holder_start" ]] || {
        echo "ephemeral /test self-test owner-SIGKILL left holder $holder_pid@$holder_start live" >&2
        return 1
    }
    ((SECONDS - started <= 5)) || {
        echo 'ephemeral /test self-test owner-SIGKILL cleanup exceeded five seconds' >&2
        return 1
    }
    ephemeral_test_root_path=$path
    ephemeral_test_root_identity="$device:$inode"
    ephemeral_test_root_active=1
    ephemeral_test_root_exact || {
        echo 'ephemeral /test self-test owner-SIGKILL changed its stale path' >&2
        return 1
    }
    "$flock_bin" --exclusive --nonblock "$path" /bin/true || {
        echo 'ephemeral /test self-test owner-SIGKILL left the stale inode locked' >&2
        return 1
    }
    remove_ephemeral_test_root
    [[ $state_dir == "${TMPDIR:-/tmp}/hermit-ephemeral-test-root-lock."* ]] || return 1
    [[ -e $state_dir/ready ]] && "$unlink_bin" -- "$state_dir/ready"
    [[ -e $state_dir/armed ]] && "$unlink_bin" -- "$state_dir/armed"
    [[ -e $state_dir/identity ]] && "$unlink_bin" -- "$state_dir/identity"
    [[ -e $state_dir/holder.log ]] && "$unlink_bin" -- "$state_dir/holder.log"
    [[ -p $state_dir/timer ]] && "$unlink_bin" -- "$state_dir/timer"
    [[ -d $state_dir ]] && "$rmdir_bin" -- "$state_dir"
    "$unlink_bin" -- "$marker"
}

assert_ephemeral_test_root_failure_probe() {
    [[ $# -eq 4 ]] || return 2
    local mode=$1 path=$2 marker=$3 expected_status=$4 output status
    if output=$("$timeout_bin" --foreground 5 \
        "$ephemeral_test_root_wrapper_path" "$ephemeral_test_root_early_failure_arg" \
        "$mode" "$path" "$marker" 2>&1); then
        echo "ephemeral /test self-test accepted planted $mode" >&2
        return 1
    else
        status=$?
    fi
    [[ $status -eq $expected_status ]] || {
        echo "ephemeral /test self-test $mode returned $status, expected $expected_status: $output" >&2
        return 1
    }
    local holder_pid holder_start identity spin
    IFS=: read -r holder_pid holder_start < "$marker" || return 1
    [[ $holder_pid =~ ^[0-9]+$ && $holder_start =~ ^[0-9]+$ ]] || return 1
    for ((spin = 0; spin < 100; spin++)); do
        identity=$(ephemeral_test_root_proc_identity "$holder_pid" 2>/dev/null || :)
        [[ ${identity#*:} != "$holder_start" ]] && break
        "$sleep_bin" 0.01
    done
    identity=$(ephemeral_test_root_proc_identity "$holder_pid" 2>/dev/null || :)
    [[ ${identity#*:} != "$holder_start" ]] || {
        echo "ephemeral /test self-test $mode left holder $holder_pid@$holder_start live" >&2
        return 1
    }
    [[ ! -e $path && ! -L $path ]] || {
        echo "ephemeral /test self-test $mode left its test-root path behind" >&2
        return 1
    }
    "$unlink_bin" -- "$marker"
}

self_test_ephemeral_test_root() {
    local parent
    parent=$(/usr/bin/mktemp -d)
    ephemeral_test_root_path="$parent/test"
    ephemeral_test_root_expected_uid=$("$id_bin" -u)
    ephemeral_test_root_expected_gid=$("$id_bin" -g)
    ephemeral_test_root_privileged=0

    assert_ephemeral_test_root_failure_probe \
        early-failure "$parent/early-failure-test" "$parent/early-failure-holder" 23
    assert_ephemeral_test_root_failure_probe \
        never-ready "$parent/never-ready-test" "$parent/never-ready-holder" 2
    assert_ephemeral_test_root_failure_probe \
        post-capture-failure "$parent/post-capture-test" "$parent/post-capture-holder" 2
    assert_ephemeral_test_root_failure_probe \
        delayed-lock "$parent/delayed-lock-test" "$parent/delayed-lock-holder" 23
    assert_ephemeral_test_root_owner_sigkill_probe \
        "$parent/owner-sigkill-test" "$parent/owner-sigkill-holder"
    ephemeral_test_root_path="$parent/test"
    ephemeral_test_root_identity=
    ephemeral_test_root_active=0

    local hostile_stat='123 (hostile ) comm) S 77'
    local field
    for ((field = 5; field <= 21; field++)); do
        hostile_stat+=' 0'
    done
    hostile_stat+=' 987654'
    [[ $(parse_ephemeral_test_root_proc_identity "$hostile_stat") == 77:987654 ]]
    for hostile_stat in \
        '123 no-closing-delimiter S 77 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 987654' \
        '123 (short) S 77 0' \
        '123 (bad-parent) S nope 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 987654' \
        '123 (bad-start) S 77 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 nope'; do
        if parse_ephemeral_test_root_proc_identity "$hostile_stat" >/dev/null; then
            echo "ephemeral /test self-test accepted malformed /proc stat: $hostile_stat" >&2
            return 1
        fi
    done

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

    local creates_before=$ephemeral_test_root_create_count
    local removes_before=$ephemeral_test_root_remove_count
    trap cleanup_ephemeral_test_root_manual_probe_on_exit EXIT
    create_ephemeral_test_root
    start_ephemeral_test_root_lock_holder
    publish_ephemeral_test_root_lease
    local valid_lease=${!ephemeral_test_root_lease_var}
    local lease_version lease_device lease_inode lease_owner lease_owner_start
    local lease_holder lease_holder_start lease_holder_fd
    IFS=: read -r lease_version lease_device lease_inode lease_owner lease_owner_start \
        lease_holder lease_holder_start lease_holder_fd <<< "$valid_lease"
    [[ $lease_version == v2 ]]
    unset "$ephemeral_test_root_lease_var"
    if (
        run_with_existing_ephemeral_test_root_lease /bin/true
    ); then
        echo 'ephemeral /test self-test accepted a missing lease' >&2
        return 1
    fi
    export "$ephemeral_test_root_lease_var=malformed"
    if (
        run_with_existing_ephemeral_test_root_lease /bin/true
    ); then
        echo 'ephemeral /test self-test accepted a malformed lease' >&2
        return 1
    fi
    local init_identity init_start
    init_identity=$(ephemeral_test_root_proc_identity 1)
    init_start=${init_identity#*:}
    export "$ephemeral_test_root_lease_var=v2:$lease_device:$lease_inode:1:$init_start:$lease_holder:$lease_holder_start:$lease_holder_fd"
    if (
        run_with_existing_ephemeral_test_root_lease /bin/true
    ); then
        echo 'ephemeral /test self-test accepted a live non-ancestor lease owner' >&2
        return 1
    fi
    "$sleep_bin" 10 &
    local sibling_pid=$!
    local sibling_identity sibling_start
    sibling_identity=$(ephemeral_test_root_proc_identity "$sibling_pid")
    sibling_start=${sibling_identity#*:}
    export "$ephemeral_test_root_lease_var=v2:$lease_device:$lease_inode:$sibling_pid:$sibling_start:$lease_holder:$lease_holder_start:$lease_holder_fd"
    if (
        run_with_existing_ephemeral_test_root_lease /bin/true
    ); then
        "$kill_bin" -s KILL -- "$sibling_pid" 2>/dev/null || true
        wait "$sibling_pid" 2>/dev/null || true
        echo 'ephemeral /test self-test accepted a live same-uid non-ancestor lease owner' >&2
        return 1
    fi
    export "$ephemeral_test_root_lease_var=v2:$lease_device:$lease_inode:$lease_owner:$lease_owner_start:$sibling_pid:$sibling_start:0"
    if (
        run_with_existing_ephemeral_test_root_lease /bin/true
    ); then
        "$kill_bin" -s KILL -- "$sibling_pid" 2>/dev/null || true
        wait "$sibling_pid" 2>/dev/null || true
        echo 'ephemeral /test self-test accepted a substitute non-lock holder' >&2
        return 1
    fi
    "$kill_bin" -s TERM -- "$sibling_pid" 2>/dev/null || true
    wait "$sibling_pid" 2>/dev/null || true
    export "$ephemeral_test_root_lease_var=$valid_lease"
    if validate_existing_ephemeral_test_root_lease; then
        echo 'ephemeral /test self-test accepted the lease owner as its own consumer' >&2
        return 1
    fi
    ephemeral_test_root_between_chain_passes_hook=ephemeral_test_root_install_mutated_proc_reader
    if (
        validate_existing_ephemeral_test_root_lease
    ); then
        echo 'ephemeral /test self-test accepted ancestry mutation between validation passes' >&2
        return 1
    fi
    ephemeral_test_root_between_chain_passes_hook=
    ephemeral_test_root_proc_identity_reader=ephemeral_test_root_proc_identity
    (
        validate_existing_ephemeral_test_root_lease
    )
    ephemeral_test_root_between_chain_passes_hook=ephemeral_test_root_install_mutated_cgroup_comparer
    if (
        validate_existing_ephemeral_test_root_lease
    ); then
        echo 'ephemeral /test self-test accepted owner/holder cgroup mutation between validation passes' >&2
        return 1
    fi
    ephemeral_test_root_between_chain_passes_hook=
    ephemeral_test_root_cgroup_comparer=ephemeral_test_root_processes_share_cgroup
    ephemeral_test_root_between_chain_passes_hook=ephemeral_test_root_replace_path_between_passes
    if (
        validate_existing_ephemeral_test_root_lease
    ); then
        echo 'ephemeral /test self-test accepted pathname replacement between validation passes' >&2
        return 1
    fi
    ephemeral_test_root_between_chain_passes_hook=
    "$rmdir_bin" -- "$ephemeral_test_root_path"
    "$mv_bin" -- "$ephemeral_test_root_path.original" "$ephemeral_test_root_path"
    ephemeral_test_root_active=1
    ephemeral_test_root_exact
    local plain_fd
    exec {plain_fd}<"$ephemeral_test_root_path"
    if ephemeral_test_root_fdinfo_has_exact_lock \
        "$BASHPID" "$plain_fd" "$lease_inode"; then
        echo 'ephemeral /test self-test accepted a plain open ancestor fd without a flock' >&2
        return 1
    fi
    exec {plain_fd}<&-
    ephemeral_test_root_active=1
    (
        ephemeral_test_root_forward_lease_argv=1
        # shellcheck disable=SC2016 # The nested shell expands its lease environment.
        run_with_existing_ephemeral_test_root_lease /bin/sh -eu -c '
            case "$1" in --internal-ephemeral-test-root-lease-v2=v2:*) ;; *) exit 1;; esac
            test -z "${HERMIT_EPHEMERAL_TEST_ROOT_LEASE_V2+x}"
        ' _
    ) || {
        echo 'ephemeral /test self-test did not append the exact inherited lease argument' >&2
        return 1
    }
    if validate_ephemeral_test_root_child_argv \
        /bin/true '--internal-ephemeral-test-root-lease-v2=caller-supplied'; then
        echo 'ephemeral /test self-test accepted a caller-supplied internal lease argument' >&2
        return 1
    fi
    (
        (
            validate_existing_ephemeral_test_root_lease
        )
    ) || {
        echo 'ephemeral /test self-test lost the live owner across nested process ancestry' >&2
        return 1
    }
    local -a lease_pids=()
    local slot
    local can_unshare=0
    if /usr/bin/unshare --user --map-root-user --mount /bin/true 2>/dev/null; then
        can_unshare=1
    fi
    for ((slot = 0; slot < 4; slot++)); do
        if ((can_unshare)); then
            (
                # shellcheck disable=SC2016 # $1 expands in the nested shell.
                run_with_existing_ephemeral_test_root_lease \
                    /usr/bin/unshare --user --map-root-user --mount /bin/sh -eu -c '
                        /usr/bin/mount --make-rprivate /
                        /usr/bin/mount -t tmpfs none "$1"
                        /usr/bin/mountpoint -q -- "$1"
                        /usr/bin/umount -- "$1"
                    ' _ "$ephemeral_test_root_path"
            ) &
        else
            (
                # shellcheck disable=SC2016 # $1 expands in the nested shell.
                run_with_existing_ephemeral_test_root_lease \
                    /bin/sh -c '/usr/bin/test -d "$1"; /usr/bin/sleep 0.02' \
                    _ "$ephemeral_test_root_path"
            ) &
        fi
        lease_pids+=("$!")
    done
    for wrapper_pid in "${lease_pids[@]}"; do
        wait "$wrapper_pid"
    done
    if (
        run_with_existing_ephemeral_test_root_lease /bin/sh -c 'exit 23'
    ); then
        echo 'ephemeral /test self-test lost an inner lease child failure' >&2
        return 1
    else
        status=$?
    fi
    [[ $status -eq 23 ]] || return 1
    export "$ephemeral_test_root_lease_var=v2:$lease_device:$lease_inode:$lease_owner:$((lease_owner_start + 1)):$lease_holder:$lease_holder_start:$lease_holder_fd"
    if (
        run_with_existing_ephemeral_test_root_lease /bin/true
    ); then
        echo 'ephemeral /test self-test accepted a stale lease-owner identity' >&2
        return 1
    fi
    export "$ephemeral_test_root_lease_var=$valid_lease"
    ephemeral_test_root_exact
    unset "$ephemeral_test_root_lease_var"
    local actual_holder_start=$ephemeral_test_root_holder_start
    if (
        ephemeral_test_root_holder_start=$((actual_holder_start + 1))
        stop_ephemeral_test_root_lock_holder
    ); then
        echo 'ephemeral /test self-test accepted a reused lock-holder identity during cleanup' >&2
        return 1
    fi
    "$kill_bin" -s 0 -- "$ephemeral_test_root_holder_pid" || {
        echo 'ephemeral /test self-test signaled a mismatched lock-holder identity' >&2
        return 1
    }
    stop_ephemeral_test_root_lock_holder
    export "$ephemeral_test_root_lease_var=$valid_lease"
    if (
        run_with_existing_ephemeral_test_root_lease /bin/true
    ); then
        echo 'ephemeral /test self-test accepted a released lock-holder lease' >&2
        return 1
    fi
    unset "$ephemeral_test_root_lease_var"
    ephemeral_test_root_active=1
    remove_ephemeral_test_root
    [[ $ephemeral_test_root_create_count -eq $((creates_before + 1)) &&
       $ephemeral_test_root_remove_count -eq $((removes_before + 1)) ]] || {
        echo 'ephemeral /test self-test did not use exactly one outer create/cleanup for four concurrent users' >&2
        return 1
    }
    ephemeral_test_root_absent
    export "$ephemeral_test_root_lease_var=$valid_lease"
    if (
        run_with_existing_ephemeral_test_root_lease /bin/true
    ); then
        echo 'ephemeral /test self-test accepted a lease after outer cleanup' >&2
        return 1
    fi
    unset "$ephemeral_test_root_lease_var"
    trap - EXIT

    ephemeral_test_root_forward_lease_argv=1
    # shellcheck disable=SC2016 # The nested shell expands its lease environment.
    run_with_ephemeral_test_root /bin/sh -eu -c '
        case "$1" in --internal-ephemeral-test-root-lease-v2=v2:*) ;; *) exit 1;; esac
        test -z "${HERMIT_EPHEMERAL_TEST_ROOT_LEASE_V2+x}"
    ' _
    ephemeral_test_root_forward_lease_argv=0
    ephemeral_test_root_absent

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

if [[ ${1:-} == "$ephemeral_test_root_holder_arg" ]]; then
    shift
    run_ephemeral_test_root_lock_holder "$@"
    exit
fi

if [[ ${1:-} == "$ephemeral_test_root_early_failure_arg" ]]; then
    shift
    [[ $# -eq 3 ]] || exit 2
    run_ephemeral_test_root_failure_probe "$@"
    exit
fi

if [[ ${1:-} == --self-test ]]; then
    [[ $# -eq 1 ]] || {
        echo 'usage: run-with-ephemeral-test-root.sh --self-test' >&2
        exit 2
    }
    self_test_ephemeral_test_root
    exit
fi

if [[ ${1:-} == --forward-lease-argv ]]; then
    ephemeral_test_root_forward_lease_argv=1
    shift
fi

if [[ ${1:-} == --check-lease ]]; then
    [[ $# -eq 1 ]] || {
        echo 'usage: run-with-ephemeral-test-root.sh --check-lease' >&2
        exit 2
    }
    if ((ephemeral_test_root_privileged)) && [[ $("$id_bin" -u) -eq 0 ]]; then
        echo 'run-with-ephemeral-test-root.sh: refusing a root lease consumer' >&2
        exit 2
    fi
    validate_existing_ephemeral_test_root_lease
    exit
fi

[[ ${1:-} == -- && $# -gt 1 ]] || {
    echo 'usage: run-with-ephemeral-test-root.sh [--forward-lease-argv] -- COMMAND [ARGS...]' >&2
    exit 2
}
shift
if ((ephemeral_test_root_privileged)) && [[ $("$id_bin" -u) -eq 0 ]]; then
    echo 'run-with-ephemeral-test-root.sh: refusing to run the child command as root' >&2
    exit 2
fi
if [[ -n ${!ephemeral_test_root_lease_var+x} ]]; then
    run_with_existing_ephemeral_test_root_lease "$@"
fi
run_with_ephemeral_test_root "$@"

#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Make a hosted strict shard observable before its long-running step can be
# killed. The existing run-node/watchdog path is unchanged: this helper starts
# it between Actions steps, snapshots its files without signalling it, and
# finally returns its recorded status fail-closed.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
    echo "usage: ci/strict-shard-observer.sh start STATE_DIR portable test.strict_compat_N | observe STATE_DIR WAIT_SECONDS LABEL | finalize STATE_DIR | self-test" >&2
    exit 2
}

state_paths() {
    state_dir=$1
    perf_dir=$(dirname -- "$state_dir")
    marker="$state_dir/supervisor-start.txt"
    status_file="$state_dir/terminal-status.txt"
    launcher_pid_file="$state_dir/launcher-pid.txt"
    launcher_log="$state_dir/launcher.log"
}

# Copy only the byte length observed before the read starts. Plain `cp` is not
# a snapshot primitive for a live log: if the writer extends the file as fast as
# it is read, cp keeps moving its EOF target and can run forever. The fixed byte
# count plus an independent deadline makes every diagnostic read finite.
copy_regular_prefix() {
    local source=$1 destination=$2 size rc
    [[ -f $source ]] || return 0
    if ! size=$(timeout --signal=TERM --kill-after=1s 2s stat -c %s -- "$source"); then
        printf 'stat timed out or failed for %s\n' "$source" >"$destination.error"
        return 1
    fi
    [[ $size =~ ^[0-9]+$ ]] || {
        printf 'invalid size %s for %s\n' "$size" "$source" >"$destination.error"
        return 1
    }
    set +e
    timeout --signal=TERM --kill-after=1s 5s head -c "$size" -- "$source" >"$destination.partial"
    rc=$?
    set -e
    if (( rc == 0 )); then
        mv "$destination.partial" "$destination"
        return 0
    fi
    printf 'fixed-prefix read failed rc=%s source=%s latched_bytes=%s\n' \
        "$rc" "$source" "$size" >"$destination.error"
    return 1
}

copy_bounded_stream() {
    local source=$1 destination=$2 rc
    set +e
    timeout --signal=TERM --kill-after=1s 2s head -c 1048576 -- "$source" >"$destination.partial"
    rc=$?
    set -e
    if (( rc == 0 )); then
        mv "$destination.partial" "$destination"
        return 0
    fi
    printf 'bounded stream read failed rc=%s source=%s\n' "$rc" "$source" >"$destination.error"
    return 1
}

snapshot() {
    local label=$1 snapshot_dir supervisor_pid= supervisor_alive=false launcher_pid= launcher_alive=false
    snapshot_dir="$state_dir/snapshots/$label"
    mkdir -p "$snapshot_dir"
    if [[ -f $marker ]]; then
        copy_regular_prefix "$marker" "$snapshot_dir/supervisor-start.txt" || true
        supervisor_pid=$(sed -n 's/^supervisor_pid=//p' "$marker")
    fi
    if [[ -f $status_file ]]; then
        copy_regular_prefix "$status_file" "$snapshot_dir/terminal-status.txt" || true
    fi
    if [[ -f $launcher_pid_file ]]; then
        copy_regular_prefix "$launcher_pid_file" "$snapshot_dir/launcher-pid.txt" || true
        launcher_pid=$(<"$launcher_pid_file")
    fi
    if [[ -f $launcher_log ]]; then
        copy_regular_prefix "$launcher_log" "$snapshot_dir/launcher.log" || true
    fi
    local evidence
    for evidence in "$perf_dir"/run-node-*.raw.log "$perf_dir"/run-node-*.timestamped.log; do
        [[ -f $evidence ]] &&
            copy_regular_prefix "$evidence" "$snapshot_dir/${evidence##*/}" || true
    done
    if [[ $supervisor_pid =~ ^[1-9][0-9]*$ && -r /proc/$supervisor_pid/stat ]]; then
        supervisor_alive=true
        copy_bounded_stream "/proc/$supervisor_pid/stat" "$snapshot_dir/supervisor-proc-stat.txt" || true
        copy_bounded_stream "/proc/$supervisor_pid/status" "$snapshot_dir/supervisor-proc-status.txt" || true
        copy_bounded_stream "/proc/$supervisor_pid/cmdline" "$snapshot_dir/supervisor-proc-cmdline.bin" || true
        copy_bounded_stream "/proc/$supervisor_pid/cgroup" "$snapshot_dir/supervisor-proc-cgroup.txt" || true
    fi
    if [[ $launcher_pid =~ ^[1-9][0-9]*$ && -r /proc/$launcher_pid/stat ]]; then
        launcher_alive=true
    fi
    {
        printf 'observed_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'label=%s\n' "$label"
        printf 'marker_present=%s\n' "$([[ -f $marker ]] && echo true || echo false)"
        printf 'terminal_status_present=%s\n' "$([[ -f $status_file ]] && echo true || echo false)"
        printf 'supervisor_pid=%s\n' "$supervisor_pid"
        printf 'supervisor_alive=%s\n' "$supervisor_alive"
        printf 'launcher_pid=%s\n' "$launcher_pid"
        printf 'launcher_alive=%s\n' "$launcher_alive"
    } >"$snapshot_dir/observation.txt"
}

start_command() {
    local state=$1
    shift
    state_paths "$state"
    mkdir -p "$state_dir"
    [[ ! -e $marker && ! -e $status_file && ! -e $launcher_pid_file ]] || {
        echo "strict-shard-observer: refusing reused state directory: $state_dir" >&2
        exit 2
    }

    (
        trap '' HUP
        set +e
        STRICT_WATCHDOG_START_MARKER="$marker" "$@"
        rc=$?
        set -e
        tmp="$status_file.tmp.$$"
        {
            printf 'rc=%s\n' "$rc"
            printf 'finished_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        } >"$tmp"
        mv "$tmp" "$status_file"
    ) </dev/null >"$launcher_log" 2>&1 &
    launcher_pid=$!
    printf '%s\n' "$launcher_pid" >"$launcher_pid_file"

    local deadline=$((SECONDS + 60))
    while [[ ! -f $marker && ! -f $status_file && $SECONDS -lt $deadline ]]; do
        if [[ ! -r /proc/$launcher_pid/stat ]]; then
            break
        fi
        sleep 1
    done
    if [[ ! -f $marker ]]; then
        echo "strict-shard-observer: supervisor start marker was not published" >&2
        [[ -f $launcher_log ]] && tail -n 200 "$launcher_log" >&2
        exit 2
    fi
    grep -Fxq 'entered=true' "$marker" || {
        echo "strict-shard-observer: malformed start marker: entered flag missing" >&2
        exit 2
    }
    grep -Eq '^resolved_agent_utils_revision=[0-9a-f]{40}$' "$marker" || {
        echo "strict-shard-observer: malformed start marker: agent-utils revision missing" >&2
        exit 2
    }
    grep -Eq '^supervisor_pid=[1-9][0-9]*$' "$marker" || {
        echo "strict-shard-observer: malformed start marker: supervisor PID missing" >&2
        exit 2
    }
    grep -Eq '^configured_timeout_seconds=[1-9][0-9]*$' "$marker" || {
        echo "strict-shard-observer: malformed start marker: configured bound missing" >&2
        exit 2
    }
    snapshot start
}

observe_worker() {
    local state=$1 wait_seconds=$2 label=$3
    [[ $wait_seconds =~ ^[0-9]+$ && $label =~ ^[a-z0-9-]+$ ]] || usage
    state_paths "$state"
    local deadline=$((SECONDS + wait_seconds))
    while [[ ! -f $status_file && $SECONDS -lt $deadline ]]; do
        local supervisor_pid= launcher_pid=
        [[ -f $marker ]] && supervisor_pid=$(sed -n 's/^supervisor_pid=//p' "$marker")
        [[ -f $launcher_pid_file ]] && launcher_pid=$(<"$launcher_pid_file")
        if [[ ! ( $supervisor_pid =~ ^[1-9][0-9]*$ && -r /proc/$supervisor_pid/stat ) &&
              ! ( $launcher_pid =~ ^[1-9][0-9]*$ && -r /proc/$launcher_pid/stat ) ]]; then
            break
        fi
        sleep 5
    done
    snapshot "$label"
}

observe() {
    local state=$1 wait_seconds=$2 label=$3 rc limit
    [[ $wait_seconds =~ ^[0-9]+$ && $label =~ ^[a-z0-9-]+$ ]] || usage
    limit=$((wait_seconds + 15))
    set +e
    timeout --signal=TERM --kill-after=2s "${limit}s" \
        "$0" _observe "$state" "$wait_seconds" "$label"
    rc=$?
    set -e
    if (( rc == 124 )); then
        mkdir -p "$state/snapshots/$label"
        printf 'observer exceeded hard bound: wait=%ss total_bound=%ss\n' \
            "$wait_seconds" "$limit" >"$state/snapshots/$label/observer-timeout.txt"
    fi
    return "$rc"
}

finalize() {
    local state=$1 rc
    state_paths "$state"
    snapshot final
    if [[ ! -f $status_file ]]; then
        echo "strict-shard-observer: supervised shard has no terminal status after the observation window" >&2
        return 124
    fi
    rc=$(sed -n 's/^rc=//p' "$status_file")
    [[ $rc =~ ^[0-9]+$ && $rc -le 255 ]] || {
        echo "strict-shard-observer: invalid terminal status: $rc" >&2
        return 2
    }
    return "$rc"
}

self_test() (
    local scratch fixture_pid started elapsed deadline
    scratch=$(mktemp -d)
    trap 'rm -rf -- "$scratch"' EXIT
    start_command "$scratch/perf/strict-observer" bash -c '
        marker=$STRICT_WATCHDOG_START_MARKER
        {
          echo entered=true
          echo resolved_agent_utils_revision=0123456789abcdef0123456789abcdef01234567
          echo supervisor_pid=$$
          echo supervisor_starttime_ticks=1
          echo worker_pid=$$
          echo worker_starttime_ticks=1
          echo configured_timeout_seconds=10
          echo term_grace_seconds=2
          echo started_unix_seconds=1
        } >"$marker.tmp"
        mv "$marker.tmp" "$marker"
        trap "exit 0" TERM
        while :; do
          echo fixture-output
          sleep 0.05
        done
    '
    fixture_pid=$(sed -n 's/^supervisor_pid=//p' "$scratch/perf/strict-observer/supervisor-start.txt")
    started=$SECONDS
    observe "$scratch/perf/strict-observer" 1 after
    elapsed=$((SECONDS - started))
    (( elapsed <= 10 )) || {
        echo "strict-shard-observer: hanging-target snapshot took ${elapsed}s" >&2
        return 1
    }
    [[ -r /proc/$fixture_pid/stat ]] || {
        echo "strict-shard-observer: planted hanging target was not alive after observation" >&2
        return 1
    }
    [[ -s $scratch/perf/strict-observer/snapshots/after/launcher.log ]] || {
        echo "strict-shard-observer: live growing log was not snapshotted" >&2
        return 1
    }
    kill -TERM "$fixture_pid"
    deadline=$((SECONDS + 10))
    while [[ ! -f $scratch/perf/strict-observer/terminal-status.txt && $SECONDS -lt $deadline ]]; do
        sleep 1
    done
    finalize "$scratch/perf/strict-observer"
    [[ -f $scratch/perf/strict-observer/snapshots/start/supervisor-start.txt ]]
    [[ -f $scratch/perf/strict-observer/snapshots/final/terminal-status.txt ]]
    echo "strict-shard-observer: self-test PASS"
)

case ${1:-} in
    start)
        [[ $# == 4 && $3 == portable && $4 =~ ^test[.]strict_compat_[1-4]$ ]] || usage
        start_command "$2" "$ROOT_DIR/ci/run-node.sh" "$3" "$4"
        ;;
    observe)
        [[ $# == 4 ]] || usage
        observe "$2" "$3" "$4"
        ;;
    _observe)
        [[ $# == 4 ]] || usage
        observe_worker "$2" "$3" "$4"
        ;;
    finalize)
        [[ $# == 2 ]] || usage
        finalize "$2"
        ;;
    self-test)
        [[ $# == 1 ]] || usage
        self_test
        ;;
    *) usage ;;
esac

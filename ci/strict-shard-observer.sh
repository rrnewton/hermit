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

snapshot() {
    local label=$1 snapshot_dir supervisor_pid= supervisor_alive=false launcher_pid= launcher_alive=false
    snapshot_dir="$state_dir/snapshots/$label"
    mkdir -p "$snapshot_dir"
    if [[ -f $marker ]]; then
        cp "$marker" "$snapshot_dir/"
        supervisor_pid=$(sed -n 's/^supervisor_pid=//p' "$marker")
    fi
    if [[ -f $status_file ]]; then
        cp "$status_file" "$snapshot_dir/"
    fi
    if [[ -f $launcher_pid_file ]]; then
        cp "$launcher_pid_file" "$snapshot_dir/"
        launcher_pid=$(<"$launcher_pid_file")
    fi
    if [[ -f $launcher_log ]]; then
        cp "$launcher_log" "$snapshot_dir/"
    fi
    local evidence
    for evidence in "$perf_dir"/run-node-*.raw.log "$perf_dir"/run-node-*.timestamped.log; do
        [[ -f $evidence ]] && cp "$evidence" "$snapshot_dir/"
    done
    if [[ $supervisor_pid =~ ^[1-9][0-9]*$ && -r /proc/$supervisor_pid/stat ]]; then
        supervisor_alive=true
        cp "/proc/$supervisor_pid/stat" "$snapshot_dir/supervisor-proc-stat.txt"
        cp "/proc/$supervisor_pid/status" "$snapshot_dir/supervisor-proc-status.txt"
        tr '\0' ' ' <"/proc/$supervisor_pid/cmdline" >"$snapshot_dir/supervisor-proc-cmdline.txt" || true
        cp "/proc/$supervisor_pid/cgroup" "$snapshot_dir/supervisor-proc-cgroup.txt" || true
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

observe() {
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
    local scratch
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
        sleep 1
        echo fixture-output
    '
    observe "$scratch/perf/strict-observer" 5 after
    finalize "$scratch/perf/strict-observer"
    [[ -f $scratch/perf/strict-observer/snapshots/start/supervisor-start.txt ]]
    [[ -f $scratch/perf/strict-observer/snapshots/after/terminal-status.txt ]]
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

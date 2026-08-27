#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail
export LC_ALL=C
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export TZ=UTC

function process_is_running {
    local pid=$1 stat state
    [[ -r /proc/$pid/stat ]] || return 1
    IFS= read -r stat <"/proc/$pid/stat" || return 1
    state=${stat##*) }
    state=${state%% *}
    [[ $state != Z && $state != X && $state != x ]]
}

function run_http_workload {
    local work_dir=$1
    local requested_port=${2:-0}
    local hold_until=${3:--}
    local server_pid='' response_hash port

    rm -rf -- "$work_dir"
    mkdir -p -- "$work_dir"
    trap 'if [[ -n ${server_pid:-} ]]; then kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; fi' EXIT

    cat >"$work_dir/server.py" <<'PY'
import http.server
import os
import pathlib
import sys
import time


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/payload":
            self.send_error(404)
            return
        body = (
            "hermit-http-server\n"
            f"observed-ns={time.time_ns()}\n"
            f"nonce={os.urandom(16).hex()}\n"
        ).encode("ascii")
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass


server = http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
pathlib.Path(sys.argv[2]).write_text(f"{server.server_port}\n", encoding="ascii")
if sys.argv[3] != "-":
    release = pathlib.Path(sys.argv[3])
    while not release.exists():
        time.sleep(0.01)
server.handle_request()
server.server_close()
PY

    python3 "$work_dir/server.py" "$requested_port" "$work_dir/ready" "$hold_until" \
        >"$work_dir/server.log" 2>&1 &
    server_pid=$!

    for _ in $(seq 1 500); do
        [[ -s $work_dir/ready ]] && break
        if ! process_is_running "$server_pid"; then
            wait "$server_pid" || true
            server_pid=
            cat "$work_dir/server.log" >&2
            return 1
        fi
        sleep 0.01
    done
    if [[ ! -s $work_dir/ready ]]; then
        cat "$work_dir/server.log" >&2
        return 1
    fi
    read -r port <"$work_dir/ready"
    if [[ ! $port =~ ^[1-9][0-9]*$ ]] || ((port > 65535)); then
        printf 'HTTP server reported an invalid bound port: %s\n' "$port" >&2
        return 1
    fi

    curl --fail --silent --show-error \
        "http://127.0.0.1:$port/payload" >"$work_dir/response.txt"
    wait "$server_pid"
    server_pid=

    [[ $(sed -n '1p' "$work_dir/response.txt") == hermit-http-server ]]
    [[ $(wc -l <"$work_dir/response.txt") -eq 3 ]]
    response_hash=$(sha256sum "$work_dir/response.txt" | cut -d' ' -f1)
    printf 'http-server:%s\n' "$response_hash"
}

function old_pid_port {
    local pid=$1
    printf '%s\n' $((20000 + pid % 20000))
}

function wait_for_ready {
    local ready=$1
    local pid=$2
    local log=$3

    for _ in $(seq 1 500); do
        [[ -s $ready ]] && return 0
        if ! process_is_running "$pid"; then
            wait "$pid" || true
            cat "$log" >&2
            return 1
        fi
        sleep 0.01
    done
    printf 'HTTP server did not report its bound port\n' >&2
    cat "$log" >&2
    return 1
}

function self_test_port_allocation {
    local root script first_pid='' second_pid='' old_port='' fake_pid_a='' fake_pid_b=''
    local first_status=0 second_status=0 port_a port_b
    root=$(mktemp -d "${TMPDIR:-/tmp}/hermit-http-port-self-test.XXXXXX")
    script=$(readlink -f -- "${BASH_SOURCE[0]}")
    trap 'if [[ -n ${first_pid:-} ]]; then kill "$first_pid" 2>/dev/null || true; fi; if [[ -n ${second_pid:-} ]]; then kill "$second_pid" 2>/dev/null || true; fi; rm -rf -- "$root"' EXIT

    # REMOVED BEHAVIOR. Two possible process ids separated by 20000 select the
    # same port. Keep the first server bound while the second instance starts,
    # so the collision is mandatory rather than dependent on scheduling.
    for fake_pid_a in $(seq 101 164); do
        fake_pid_b=$((fake_pid_a + 20000))
        old_port=$(old_pid_port "$fake_pid_a")
        [[ $old_port == "$(old_pid_port "$fake_pid_b")" ]] || return 1
        "$script" --native-once "$root/old-a" "$old_port" "$root/old-release" \
            >"$root/old-a.out" 2>"$root/old-a.err" &
        first_pid=$!
        if wait_for_ready "$root/old-a/ready" "$first_pid" "$root/old-a/server.log"; then
            break
        fi
        first_pid=''
    done
    [[ -n $first_pid && -s $root/old-a/ready ]] || {
        printf 'could not bind any PID-derived control port\n' >&2
        return 1
    }

    "$script" --native-once "$root/old-b" "$(old_pid_port "$fake_pid_b")" \
        >"$root/old-b.out" 2>"$root/old-b.err" || second_status=$?
    if ((second_status == 0)); then
        printf 'PID-derived control unexpectedly let both servers bind port %s\n' "$old_port" >&2
        return 1
    fi
    grep -Fq 'Address already in use' "$root/old-b/server.log" || {
        printf 'PID-derived control failed for the wrong reason:\n' >&2
        cat "$root/old-b.err" "$root/old-b/server.log" >&2
        return 1
    }
    : >"$root/old-release"
    wait "$first_pid" || first_status=$?
    first_pid=''
    ((first_status == 0)) || return 1

    # FIXED BEHAVIOR. Both instances bind port zero while held concurrently,
    # then record the two ports the kernel assigned before either serves.
    first_status=0
    second_status=0
    "$script" --native-once "$root/new-a" "" "$root/new-release" \
        >"$root/new-a.out" 2>"$root/new-a.err" &
    first_pid=$!
    "$script" --native-once "$root/new-b" "" "$root/new-release" \
        >"$root/new-b.out" 2>"$root/new-b.err" &
    second_pid=$!
    wait_for_ready "$root/new-a/ready" "$first_pid" "$root/new-a/server.log"
    wait_for_ready "$root/new-b/ready" "$second_pid" "$root/new-b/server.log"
    read -r port_a <"$root/new-a/ready"
    read -r port_b <"$root/new-b/ready"
    [[ $port_a != "$port_b" ]] || {
        printf 'kernel assigned one port to two concurrent listeners: %s\n' "$port_a" >&2
        return 1
    }
    : >"$root/new-release"
    wait "$first_pid" || first_status=$?
    wait "$second_pid" || second_status=$?
    first_pid=''
    second_pid=''
    ((first_status == 0 && second_status == 0)) || {
        cat "$root/new-a.err" "$root/new-b.err" >&2
        return 1
    }

    printf 'PID-derived collision: fake-pids=%s,%s port=%s second-instance=refused\n' \
        "$fake_pid_a" "$fake_pid_b" "$old_port"
    printf 'kernel-assigned concurrent ports: first=%s second=%s both=passed\n' \
        "$port_a" "$port_b"
    rm -rf -- "$root"
    trap - EXIT
}

if [[ ${1:-} == --guest ]]; then
    run_http_workload "$2"
    exit
fi

if [[ ${1:-} == --native-once ]]; then
    run_http_workload "$2" "${3:-0}" "${4:--}"
    exit
fi

if [[ ${1:-} == --self-test-port-allocation ]]; then
    self_test_port_allocation
    exit
fi

# shellcheck source=tests/e2e/lib/applications/common.sh
source "$(dirname -- "$0")/common.sh"
require_commands curl python3 sed seq sha256sum timeout

work_root=$(mktemp -d "${TMPDIR:-/tmp}/hermit-http-e2e.XXXXXX")
trap 'rm -rf -- "$work_root"' EXIT

native_first=$(run_http_workload "$work_root/native")
native_second=$(run_http_workload "$work_root/native")
assert_native_nondeterminism 'HTTP server workload' "$native_first" "$native_second"

run_hermit_verify 'HTTP server workload' \
    --require-absolute-arg 1 --require-absolute-arg 2 --require-absolute-arg 4 -- \
    /bin/bash "$(readlink -f -- "$0")" --guest "$work_root/verified" >/dev/null
printf 'http-server:verified\n'

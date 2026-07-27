#!/usr/bin/env bash
# Shared QMP, snapshot, and log helpers for the QEMU demos.

set -euo pipefail

qemu_snapshot_require_tools() {
  local tool
  for tool in nc python3 qemu-img sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || {
      printf 'missing required tool: %s\n' "$tool" >&2
      return 1
    }
  done
}

qemu_wait_for_socket() {
  local socket_path=$1
  local owner_pid=$2
  local timeout_seconds=$3
  local attempts=$((timeout_seconds * 10))
  local attempt

  for ((attempt = 0; attempt < attempts; attempt++)); do
    [ -S "$socket_path" ] && return 0
    kill -0 "$owner_pid" 2>/dev/null || break
    sleep 0.1
  done
  printf 'timed out waiting for Unix socket: %s\n' "$socket_path" >&2
  return 1
}

qemu_wait_for_log_line() {
  local log_path=$1
  local marker=$2
  local owner_pid=$3
  local timeout_seconds=$4
  local attempts=$((timeout_seconds * 10))
  local attempt

  for ((attempt = 0; attempt < attempts; attempt++)); do
    grep -Fq "$marker" "$log_path" 2>/dev/null && return 0
    kill -0 "$owner_pid" 2>/dev/null || break
    sleep 0.1
  done
  printf 'timed out waiting for %q in %s\n' "$marker" "$log_path" >&2
  return 1
}

qemu_qmp_command() {
  local socket_path=$1
  local execute=$2
  local argument_name=${3:-}
  local argument_value=${4:-}

  python3 - "$socket_path" "$execute" "$argument_name" "$argument_value" <<'PY'
import json
import socket
import sys

socket_path, execute, argument_name, argument_value = sys.argv[1:]
sock = socket.socket(socket.AF_UNIX)
sock.settimeout(20)
sock.connect(socket_path)
stream = sock.makefile("rwb", buffering=0)


def send(payload):
    stream.write(json.dumps(payload).encode() + b"\n")


def receive(message_id):
    while True:
        line = stream.readline()
        if not line:
            raise RuntimeError("QMP disconnected before replying")
        message = json.loads(line)
        if message.get("id") != message_id:
            continue
        if "error" in message:
            raise RuntimeError(message["error"])
        return message.get("return")


greeting = json.loads(stream.readline())
if "QMP" not in greeting:
    raise RuntimeError(f"invalid QMP greeting: {greeting!r}")

send({"execute": "qmp_capabilities", "id": "capabilities"})
receive("capabilities")

request = {"execute": execute, "id": "command"}
if argument_name:
    request["arguments"] = {argument_name: argument_value}
send(request)
receive("command")
PY
}

qemu_snapshot_exists() {
  local disk=$1
  local snapshot_name=$2
  qemu-img snapshot -l "$disk" \
    | awk -v snapshot_name="$snapshot_name" \
      '$2 == snapshot_name { found = 1 } END { exit !found }'
}

qemu_write_stable_info_tail() {
  local hermit_log=$1
  local output=$2
  local artifact_prefix=${DEMO_ARTIFACTS:-}
  local root_prefix=${ROOT:-}

  grep -Fq ' COMMIT turn ' "$hermit_log" || {
    printf 'Hermit INFO log contains no scheduler COMMIT event: %s\n' \
      "$hermit_log" >&2
    return 1
  }
  grep -Fq 'Final virtual global (cpu) time:' "$hermit_log" || {
    printf 'Hermit INFO log contains no virtual-time report: %s\n' "$hermit_log" >&2
    return 1
  }

  awk -v artifacts="$artifact_prefix" -v root="$root_prefix" '
    function emit(line) {
      if (line == "") return
      if (artifacts != "") gsub(artifacts, "<demo-artifacts>", line)
      if (root != "") gsub(root "/", "./", line)
      print line
    }
    {
      sub(/^[0-9T:.Z-]+ +/, "")
      if ($0 ~ /^ COMMIT turn /) commit = $0
      if ($0 ~ /INFO detcore::tool_global: Scheduler authorized/) authorized = $0
      if ($0 ~ /INFO reverie_ptrace::task: .*tail_inject of syscall:/) tail_inject = $0
      if ($0 ~ /INFO detcore::scheduler: logically_kill:/) {
        previous_kill = last_kill
        last_kill = $0
      }
      if ($0 ~ /INFO detcore::scheduler: scheduler \(step2_process_blocked\):/) blocked = $0
      if ($0 ~ /INFO detcore::scheduler: \[scheduler\] run queue empty/) empty = $0
      if ($0 ~ /INFO detcore::tool_global: detcore shut down/) shutdown = $0
      if ($0 ~ /hermit run report/ ||
          $0 ~ /^Final thread-tree/ ||
          $0 ~ /^There were / ||
          $0 ~ /^Internally,/ ||
          $0 ~ /^Final virtual global \(cpu\) time:/ ||
          $0 ~ /^Elapsed virtual global \(cpu\) time:/ ||
          $0 ~ /^Timeslice stats:/) {
        report[++report_lines] = $0
      }
    }
    END {
      emit(commit)
      emit(authorized)
      emit(tail_inject)
      emit(previous_kill)
      emit(last_kill)
      emit(blocked)
      emit(empty)
      emit(shutdown)
      for (i = 1; i <= report_lines; i++) emit(report[i])
    }
  ' "$hermit_log" >"$output"

  grep -Fq ' COMMIT turn ' "$output" || return 1
  grep -Fq 'tail_inject of syscall:' "$output" || return 1
  grep -Fq 'hermit run report' "$output" || return 1
}

qemu_normalize_info_for_comparison() {
  local input=$1
  local output=$2

  sed -E \
    -e 's/(COMMIT turn )[0-9]+/\1<turn>/' \
    -e 's/(previously committed )[0-9_.]+s/\1<virtual-time>/' \
    -e 's/(scheduler ran )[0-9]+ turns/\1<turns> turns/' \
    -e 's/^(Final virtual global \(cpu\) time:).*/\1 <virtual-time>/' \
    -e 's/^(Elapsed virtual global \(cpu\) time:).*/\1 <virtual-time>/' \
    -e 's/^Timeslice stats:.*/Timeslice stats: <normalized>/' \
    "$input" >"$output"
}

qemu_stop_pid() {
  local pid=${1:-}
  [ -n "$pid" ] || return 0
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

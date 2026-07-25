#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
BUILD_ROOT="$TARGET_DIR/jvm-thread-interleaving"
CLASS_DIR="$BUILD_ROOT/classes"
RUN_DIR="$BUILD_ROOT/runs"
HERMIT_BIN="${HERMIT:-$TARGET_DIR/debug/hermit}"
JAVA_BIN="${JAVA:-$(command -v java)}"
JAVAC_BIN="${JAVAC:-$(command -v javac)}"
NATIVE_RUNS="${NATIVE_RUNS:-12}"
STRICT_RUNS="${STRICT_RUNS:-5}"
THREADS="${THREADS:-12}"
ROUNDS="${ROUNDS:-48}"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-180}"

mkdir -p "$CLASS_DIR"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR/native" "$RUN_DIR/strict"

if [[ ! -x "$HERMIT_BIN" ]]; then
  cargo build -p hermit --bin hermit
fi

"$JAVAC_BIN" -d "$CLASS_DIR" "$SCRIPT_DIR/ThreadInterleaving.java"
JAVA_BIN="$(readlink -f "$JAVA_BIN")"

JAVA_COMMAND=(
  "$JAVA_BIN"
  -Xint
  -Xms32m
  -Xmx32m
  -XX:+UseSerialGC
  -cp
  "$CLASS_DIR"
  ThreadInterleaving
  "$THREADS"
  "$ROUNDS"
)

run_one() {
  local mode="$1"
  local run="$2"
  shift 2

  local stdout="$RUN_DIR/$mode/$run.stdout"
  local stderr="$RUN_DIR/$mode/$run.stderr"

  if timeout "$TIMEOUT_SECONDS" "$@" >"$stdout" 2>"$stderr"; then
    :
  else
    local status=$?
    printf '%s run %s failed with status %s\n' "$mode" "$run" "$status" >&2
    sed -n '1,160p' "$stderr" >&2
    return 1
  fi

  if ! grep -Eq "^THREAD_TRACE threads=$THREADS rounds=$ROUNDS events=$((THREADS * ROUNDS))$" "$stdout"; then
    printf '%s run %s did not produce the expected completion marker\n' "$mode" "$run" >&2
    sed -n '1,20p' "$stdout" >&2
    sed -n '1,160p' "$stderr" >&2
    return 1
  fi
}

for ((run = 1; run <= NATIVE_RUNS; run++)); do
  run_one native "$run" "${JAVA_COMMAND[@]}"
done

for ((run = 1; run <= STRICT_RUNS; run++)); do
  run_one strict "$run" "$HERMIT_BIN" run --strict -- "${JAVA_COMMAND[@]}"
done

mapfile -t native_hashes < <(
  sha256sum "$RUN_DIR"/native/*.stdout | awk '{print $1}' | sort -u
)
mapfile -t strict_hashes < <(
  sha256sum "$RUN_DIR"/strict/*.stdout | awk '{print $1}' | sort -u
)

printf 'NONDET_SOURCE: thread scheduling\n'
printf 'native runs: %s, unique traces: %s\n' "$NATIVE_RUNS" "${#native_hashes[@]}"
printf 'strict runs: %s, unique traces: %s\n' "$STRICT_RUNS" "${#strict_hashes[@]}"
printf 'strict trace sha256: %s\n' "${strict_hashes[0]:-missing}"

if ((${#native_hashes[@]} < 2)); then
  printf 'native JVM runs did not expose multiple schedules; increase NATIVE_RUNS or ROUNDS\n' >&2
  exit 1
fi

if ((${#strict_hashes[@]} != 1)); then
  printf 'strict Hermit runs produced different thread schedules:\n' >&2
  printf '  %s\n' "${strict_hashes[@]}" >&2
  exit 1
fi

printf 'PASS: native scheduling varied while strict Hermit output was byte-identical\n'

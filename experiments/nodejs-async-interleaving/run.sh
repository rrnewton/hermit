#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
RUN_DIR="$TARGET_DIR/nodejs-async-interleaving/runs"
HERMIT_BIN="${HERMIT:-$TARGET_DIR/debug/hermit}"
NODE_BIN="${NODE:-$(command -v node)}"
NATIVE_RUNS="${NATIVE_RUNS:-12}"
STRICT_RUNS="${STRICT_RUNS:-5}"
WORKERS="${WORKERS:-8}"
ROUNDS="${ROUNDS:-24}"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-180}"

rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR/native" "$RUN_DIR/strict"

if [[ ! -x "$HERMIT_BIN" ]]; then
  cargo build -p hermit --bin hermit
fi

NODE_BIN="$(readlink -f "$NODE_BIN")"
NODE_COMMAND=(
  "$NODE_BIN"
  --jitless
  "$SCRIPT_DIR/async_interleaving.js"
  "$WORKERS"
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

  if ! grep -Eq "^ASYNC_TRACE workers=$WORKERS rounds=$ROUNDS events=$((WORKERS * ROUNDS))$" "$stdout"; then
    printf '%s run %s did not produce the expected completion marker\n' "$mode" "$run" >&2
    sed -n '1,20p' "$stdout" >&2
    sed -n '1,160p' "$stderr" >&2
    return 1
  fi
}

for ((run = 1; run <= NATIVE_RUNS; run++)); do
  run_one native "$run" "${NODE_COMMAND[@]}"
done

for ((run = 1; run <= STRICT_RUNS; run++)); do
  run_one strict "$run" "$HERMIT_BIN" run --strict -- "${NODE_COMMAND[@]}"
done

mapfile -t native_hashes < <(
  sha256sum "$RUN_DIR"/native/*.stdout | awk '{print $1}' | sort -u
)
mapfile -t strict_hashes < <(
  sha256sum "$RUN_DIR"/strict/*.stdout | awk '{print $1}' | sort -u
)

printf 'NONDET_SOURCE: async scheduling\n'
printf 'native runs: %s, unique traces: %s\n' "$NATIVE_RUNS" "${#native_hashes[@]}"
printf 'strict runs: %s, unique traces: %s\n' "$STRICT_RUNS" "${#strict_hashes[@]}"
printf 'strict trace sha256: %s\n' "${strict_hashes[0]:-missing}"

if ((${#native_hashes[@]} < 2)); then
  printf 'native Node.js runs did not expose multiple schedules; increase NATIVE_RUNS or ROUNDS\n' >&2
  exit 1
fi

if ((${#strict_hashes[@]} != 1)); then
  printf 'strict Hermit runs produced different async completion orders:\n' >&2
  printf '  %s\n' "${strict_hashes[@]}" >&2
  exit 1
fi

printf 'PASS: native async order varied while strict Hermit output was byte-identical\n'

#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

readonly LULESH_REPOSITORY=https://github.com/LLNL/LULESH.git
readonly LULESH_TAG=2.0.3
readonly LULESH_REVISION=46c2a1d6db9171f9637d79f407212e0f176e8194

usage() {
  cat <<'USAGE'
Usage: run.sh [OPTIONS]

Build LULESH with OpenMP, execute it repeatedly under Hermit strict mode, and
compare the complete stdout, stderr, and exit status from every run.

Options:
  --source DIR       LULESH checkout (default: target/lulesh-openmp/source)
  --hermit PATH      Hermit binary (default: target/release/hermit)
  --output DIR       New evidence directory (default: timestamped under target)
  --runs N           Number of strict-mode executions (default: 2)
  --threads N        OpenMP thread count (default: 4)
  --size N           LULESH cube mesh side length (default: 10)
  --iterations N     LULESH cycle count (default: 10)
  --timeout SECONDS  Per-run timeout (default: 180)
  --skip-build       Use existing Hermit and LULESH binaries
  -h, --help         Show this help

The runner clones the pinned LULESH 2.0.3 revision when --source does not
exist. It exits 0 only when all runs succeed and produce byte-identical output.
USAGE
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

require_positive_integer() {
  local name=$1
  local value=$2
  [[ $value =~ ^[1-9][0-9]*$ ]] || fail "$name must be a positive integer: $value"
}

sha256_file() {
  sha256sum "$1" | awk '{print $1}'
}

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
source_input=$repo_root/target/lulesh-openmp/source
hermit_input=$repo_root/target/release/hermit
output_input=
runs=2
threads=4
size=10
iterations=10
timeout_seconds=180
skip_build=false

while (($# > 0)); do
  case $1 in
    --source)
      (($# >= 2)) || fail '--source requires a value'
      source_input=$2
      shift 2
      ;;
    --hermit)
      (($# >= 2)) || fail '--hermit requires a value'
      hermit_input=$2
      shift 2
      ;;
    --output)
      (($# >= 2)) || fail '--output requires a value'
      output_input=$2
      shift 2
      ;;
    --runs)
      (($# >= 2)) || fail '--runs requires a value'
      runs=$2
      shift 2
      ;;
    --threads)
      (($# >= 2)) || fail '--threads requires a value'
      threads=$2
      shift 2
      ;;
    --size)
      (($# >= 2)) || fail '--size requires a value'
      size=$2
      shift 2
      ;;
    --iterations)
      (($# >= 2)) || fail '--iterations requires a value'
      iterations=$2
      shift 2
      ;;
    --timeout)
      (($# >= 2)) || fail '--timeout requires a value'
      timeout_seconds=$2
      shift 2
      ;;
    --skip-build)
      skip_build=true
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1"
      ;;
  esac
done

require_positive_integer runs "$runs"
require_positive_integer threads "$threads"
require_positive_integer size "$size"
require_positive_integer iterations "$iterations"
require_positive_integer timeout "$timeout_seconds"

for tool in git make g++ cargo ldd sha256sum awk cmp grep timeout realpath date head tail cut sort wc uname; do
  command -v "$tool" >/dev/null || fail "required tool not found: $tool"
done

source_dir=$(realpath -m "$source_input")
hermit_bin=$(realpath -m "$hermit_input")
if [[ -z $output_input ]]; then
  timestamp=$(date -u +%Y%m%dT%H%M%SZ)
  output_input=$repo_root/target/lulesh-openmp/evidence-$timestamp
fi
output_dir=$(realpath -m "$output_input")
[[ ! -e $output_dir ]] || fail "evidence path already exists: $output_dir"

if [[ ! -e $source_dir ]]; then
  mkdir -p "$(dirname "$source_dir")"
  git clone --depth 1 --branch "$LULESH_TAG" "$LULESH_REPOSITORY" "$source_dir"
fi
[[ -d $source_dir/.git ]] || fail "LULESH source is not a Git checkout: $source_dir"
source_revision=$(git -C "$source_dir" rev-parse HEAD)
[[ $source_revision == "$LULESH_REVISION" ]] ||
  fail "LULESH source must be revision $LULESH_REVISION, found $source_revision"

if [[ $skip_build == false ]]; then
  cargo build --manifest-path "$repo_root/Cargo.toml" --release -p hermit --bin hermit
  build_jobs=${BUILD_JOBS:-4}
  require_positive_integer BUILD_JOBS "$build_jobs"
  make -C "$source_dir" clean
  make -C "$source_dir" -j"$build_jobs" 'CXX=g++ -DUSE_MPI=0'
fi

[[ -x $hermit_bin ]] || fail "Hermit binary is not executable: $hermit_bin"
lulesh_bin=$source_dir/lulesh2.0
[[ -x $lulesh_bin ]] || fail "LULESH binary is not executable: $lulesh_bin"
openmp_runtime=$(ldd "$lulesh_bin" | awk '/libgomp/{print $3; exit}')
[[ -n $openmp_runtime ]] || fail 'LULESH binary is not linked with libgomp'
mkdir -p "$output_dir/runs"

export LC_ALL=C
hermit_args=(
  --log=error
  run
  --strict
  --base-env=minimal
  --env=LC_ALL=C
  "--env=OMP_NUM_THREADS=$threads"
  --env=OMP_DYNAMIC=false
  "--tmp=$source_dir"
  --
  /tmp/lulesh2.0
  -s "$size"
  -i "$iterations"
)
printf -v command_line '%q ' "$hermit_bin" "${hermit_args[@]}"
command_line=${command_line% }

{
  printf 'schema_version=1\n'
  printf 'host_arch=%s\n' "$(uname -m)"
  printf 'cpu_model=%s\n' "$(awk -F ': ' '/model name/ {print $2; exit}' /proc/cpuinfo)"
  printf 'repository_commit=%s\n' "$(git -C "$repo_root" rev-parse HEAD)"
  printf 'lulesh_repository=%s\n' "$LULESH_REPOSITORY"
  printf 'lulesh_tag=%s\n' "$LULESH_TAG"
  printf 'lulesh_revision=%s\n' "$source_revision"
  printf 'lulesh_sha256=%s\n' "$(sha256_file "$lulesh_bin")"
  printf 'compiler=%s\n' "$(g++ --version | head -n 1)"
  printf 'openmp_runtime=%s\n' "$openmp_runtime"
  printf 'hermit=%s\n' "$hermit_bin"
  printf 'hermit_sha256=%s\n' "$(sha256_file "$hermit_bin")"
  printf 'runs=%s\n' "$runs"
  printf 'threads=%s\n' "$threads"
  printf 'mesh_size=%s\n' "$size"
  printf 'iterations=%s\n' "$iterations"
  printf 'timeout_seconds=%s\n' "$timeout_seconds"
  printf 'command=%s\n' "$command_line"
} >"$output_dir/metadata.txt"

manifest=$output_dir/runs.tsv
printf 'run\texit_code\tstdout_sha256\tstderr_sha256\tfingerprint_sha256\n' >"$manifest"
reference_stdout=
reference_stderr=
reference_fingerprint=
result=DETERMINISTIC

for ((run = 1; run <= runs; run++)); do
  run_name=$(printf 'run-%04d' "$run")
  run_dir=$output_dir/runs/$run_name
  mkdir "$run_dir"

  set +e
  timeout --signal=TERM --kill-after=10s "${timeout_seconds}s" \
    "$hermit_bin" "${hermit_args[@]}" \
    >"$run_dir/stdout" 2>"$run_dir/stderr"
  exit_code=$?
  set -e

  stdout_hash=$(sha256_file "$run_dir/stdout")
  stderr_hash=$(sha256_file "$run_dir/stderr")
  fingerprint=$(
    printf 'exit_code=%s\nstdout_sha256=%s\nstderr_sha256=%s\n' \
      "$exit_code" "$stdout_hash" "$stderr_hash" |
      sha256sum |
      awk '{print $1}'
  )
  printf '%s\t%s\t%s\t%s\t%s\n' \
    "$run_name" "$exit_code" "$stdout_hash" "$stderr_hash" "$fingerprint" \
    >>"$manifest"

  if ((exit_code != 0)); then
    result=FAILED
  fi
  if ! grep -Fq "Num threads: $threads" "$run_dir/stdout" ||
    ! grep -Fq "Iteration count     =  $iterations" "$run_dir/stdout" ||
    ! grep -Fq 'Final Origin Energy =' "$run_dir/stdout"; then
    result=FAILED
  fi

  if [[ -z $reference_stdout ]]; then
    reference_stdout=$run_dir/stdout
    reference_stderr=$run_dir/stderr
    reference_fingerprint=$fingerprint
  elif ! cmp -s "$reference_stdout" "$run_dir/stdout" ||
    ! cmp -s "$reference_stderr" "$run_dir/stderr" ||
    [[ $fingerprint != "$reference_fingerprint" ]]; then
    result=NON-DETERMINISTIC
  fi
done

unique_fingerprints=$(tail -n +2 "$manifest" | cut -f5 | sort -u | wc -l)
{
  printf 'result=%s\n' "$result"
  printf 'runs=%s\n' "$runs"
  printf 'unique_fingerprints=%s\n' "$unique_fingerprints"
  printf 'reference_fingerprint=%s\n' "$reference_fingerprint"
  printf 'manifest=runs.tsv\n'
} >"$output_dir/summary.txt"

printf '%s\n' "$result"
printf 'Evidence: %s\n' "$output_dir"
[[ $result == DETERMINISTIC ]]

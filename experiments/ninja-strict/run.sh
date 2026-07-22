#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

readonly NINJA_REPOSITORY=https://github.com/ninja-build/ninja.git
readonly NINJA_TAG=v1.13.1
readonly NINJA_REVISION=79feac0f3e3bc9da9effc586cd5fea41e7550051
readonly GTEST_VERSION=1.16.0
readonly GTEST_ARCHIVE_URL=https://github.com/google/googletest/archive/refs/tags/v1.16.0.tar.gz
readonly GTEST_ARCHIVE_SHA256=78c676fc63881529bf97bf9d45948d905a66833fbfa5318ea2cd7478cb98f399
readonly STRICT_FILTER='-SubprocessTest.*:DiskInterfaceTest.*:BuildWithDepsLogTest.*'
readonly EXPECTED_NATIVE_TESTS=410
readonly EXPECTED_STRICT_TESTS=378

usage() {
  cat <<'USAGE'
Usage: run.sh [OPTIONS]

Build Ninja's upstream test binary, run its supported fixtures repeatedly
under Hermit strict mode, and compare complete observations.

Options:
  --source DIR        Ninja checkout (default: target/ninja-strict/source)
  --build DIR         Ninja CMake build (default: target/ninja-strict/build)
  --gtest-source DIR  GoogleTest source (default: target/ninja-strict/googletest-1.16.0)
  --hermit PATH       Hermit binary (default: target/release/hermit)
  --output DIR        New evidence directory (default: timestamped under target)
  --runs N            Number of strict executions (default: 2)
  --timeout SECONDS   Per-run timeout (default: 30)
  --skip-build        Use existing Hermit, Ninja, and GoogleTest artifacts
  --probe-full        Run the complete strict suite once with the same timeout
  -h, --help          Show this help

The runner exits 0 only when the native control and every supported strict run
succeed and all strict observations are byte-identical. The optional full
probe is diagnostic and does not change that result.
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

has_pass_marker() {
  local expected=$1
  local file=$2
  awk -v expected="$expected" '
    $0 == "[  PASSED  ] " expected " tests." { found = 1 }
    END { exit !found }
  ' "$file"
}

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
source_input=$repo_root/target/ninja-strict/source
build_input=$repo_root/target/ninja-strict/build
gtest_source_input=$repo_root/target/ninja-strict/googletest-$GTEST_VERSION
hermit_input=$repo_root/target/release/hermit
output_input=
runs=2
timeout_seconds=30
skip_build=false
probe_full=false

while (($# > 0)); do
  case $1 in
    --source)
      (($# >= 2)) || fail '--source requires a value'
      source_input=$2
      shift 2
      ;;
    --build)
      (($# >= 2)) || fail '--build requires a value'
      build_input=$2
      shift 2
      ;;
    --gtest-source)
      (($# >= 2)) || fail '--gtest-source requires a value'
      gtest_source_input=$2
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
    --timeout)
      (($# >= 2)) || fail '--timeout requires a value'
      timeout_seconds=$2
      shift 2
      ;;
    --skip-build)
      skip_build=true
      shift
      ;;
    --probe-full)
      probe_full=true
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
require_positive_integer timeout "$timeout_seconds"

for tool in git cmake cargo curl tar c++ sha256sum awk cmp timeout realpath date head cut sort wc uname with-proxy; do
  command -v "$tool" >/dev/null || fail "required tool not found: $tool"
done

source_dir=$(realpath -m "$source_input")
build_dir=$(realpath -m "$build_input")
gtest_source_dir=$(realpath -m "$gtest_source_input")
hermit_bin=$(realpath -m "$hermit_input")
if [[ -z $output_input ]]; then
  timestamp=$(date -u +%Y%m%dT%H%M%SZ)
  output_input=$repo_root/target/ninja-strict/evidence-$timestamp
fi
output_dir=$(realpath -m "$output_input")
[[ ! -e $output_dir ]] || fail "evidence path already exists: $output_dir"
scratch_root=$repo_root/target/ninja-strict/run-tmp/$(basename "$output_dir")
mkdir -p "$scratch_root/native"

if [[ ! -e $source_dir ]]; then
  mkdir -p "$(dirname "$source_dir")"
  with-proxy git clone --depth 1 --branch "$NINJA_TAG" \
    "$NINJA_REPOSITORY" "$source_dir"
fi
[[ -d $source_dir/.git ]] || fail "Ninja source is not a Git checkout: $source_dir"
source_revision=$(git -C "$source_dir" rev-parse HEAD)
[[ $source_revision == "$NINJA_REVISION" ]] ||
  fail "Ninja source must be revision $NINJA_REVISION, found $source_revision"

if [[ ! -e $gtest_source_dir ]]; then
  archive_dir=$(dirname "$gtest_source_dir")
  archive=$archive_dir/googletest-$GTEST_VERSION.tar.gz
  mkdir -p "$archive_dir"
  with-proxy curl --fail --location --retry 3 \
    --output "$archive" "$GTEST_ARCHIVE_URL"
  archive_hash=$(sha256_file "$archive")
  [[ $archive_hash == "$GTEST_ARCHIVE_SHA256" ]] ||
    fail "GoogleTest archive SHA-256 mismatch: $archive_hash"
  tar -xzf "$archive" -C "$archive_dir"
fi
[[ -f $gtest_source_dir/CMakeLists.txt ]] ||
  fail "GoogleTest source is incomplete: $gtest_source_dir"

if [[ $skip_build == false ]]; then
  cargo build --manifest-path "$repo_root/Cargo.toml" --release -p hermit --bin hermit
  cmake -S "$source_dir" -B "$build_dir" \
    -DCMAKE_BUILD_TYPE=Release \
    "-DFETCHCONTENT_SOURCE_DIR_GOOGLETEST=$gtest_source_dir"
  build_jobs=${BUILD_JOBS:-4}
  require_positive_integer BUILD_JOBS "$build_jobs"
  cmake --build "$build_dir" --target ninja ninja_test --parallel "$build_jobs"
fi

ninja_bin=$build_dir/ninja
ninja_test_bin=$build_dir/ninja_test
[[ -x $hermit_bin ]] || fail "Hermit binary is not executable: $hermit_bin"
[[ -x $ninja_bin ]] || fail "Ninja binary is not executable: $ninja_bin"
[[ -x $ninja_test_bin ]] || fail "Ninja test binary is not executable: $ninja_test_bin"
mkdir -p "$output_dir/native" "$output_dir/runs"

export LC_ALL=C
set +e
(
  cd "$scratch_root/native"
  "$ninja_test_bin" --gtest_color=no \
    >"$output_dir/native/stdout" 2>"$output_dir/native/stderr"
)
native_exit=$?
set -e
((native_exit == 0)) || fail "native ninja_test failed with status $native_exit"
has_pass_marker "$EXPECTED_NATIVE_TESTS" "$output_dir/native/stdout" ||
  fail "native ninja_test did not report $EXPECTED_NATIVE_TESTS passing tests"

strict_args=(
  --log=error
  run
  --strict
  --base-env=minimal
  --env=LC_ALL=C
)
test_args=(
  --gtest_color=no
  "--gtest_filter=$STRICT_FILTER"
)
printf -v command_line '%q ' "$hermit_bin" "${strict_args[@]}" \
  '--tmp=<per-run-directory>' -- "$ninja_test_bin" "${test_args[@]}"
command_line=${command_line% }

{
  printf 'schema_version=1\n'
  printf 'host_arch=%s\n' "$(uname -m)"
  printf 'cpu_model=%s\n' "$(awk -F ': ' '/model name/ {print $2; exit}' /proc/cpuinfo)"
  printf 'repository_commit=%s\n' "$(git -C "$repo_root" rev-parse HEAD)"
  printf 'ninja_repository=%s\n' "$NINJA_REPOSITORY"
  printf 'ninja_tag=%s\n' "$NINJA_TAG"
  printf 'ninja_revision=%s\n' "$source_revision"
  printf 'ninja_sha256=%s\n' "$(sha256_file "$ninja_bin")"
  printf 'ninja_test_sha256=%s\n' "$(sha256_file "$ninja_test_bin")"
  printf 'gtest_version=%s\n' "$GTEST_VERSION"
  printf 'gtest_archive_sha256=%s\n' "$GTEST_ARCHIVE_SHA256"
  printf 'compiler=%s\n' "$(c++ --version | head -n 1)"
  printf 'hermit=%s\n' "$hermit_bin"
  printf 'hermit_sha256=%s\n' "$(sha256_file "$hermit_bin")"
  printf 'native_exit_code=%s\n' "$native_exit"
  printf 'native_tests=%s\n' "$EXPECTED_NATIVE_TESTS"
  printf 'strict_tests=%s\n' "$EXPECTED_STRICT_TESTS"
  printf 'excluded_tests=%s\n' "$((EXPECTED_NATIVE_TESTS - EXPECTED_STRICT_TESTS))"
  printf 'strict_filter=%s\n' "$STRICT_FILTER"
  printf 'runs=%s\n' "$runs"
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
  run_tmp=$scratch_root/$run_name
  mkdir -p "$run_dir" "$run_tmp"

  set +e
  (
    cd "$run_tmp"
    timeout --signal=TERM --kill-after=5s "${timeout_seconds}s" \
      "$hermit_bin" "${strict_args[@]}" "--tmp=$run_tmp" -- \
      "$ninja_test_bin" "${test_args[@]}" \
      >"$run_dir/stdout" 2>"$run_dir/stderr"
  )
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

  if ((exit_code != 0)) ||
    ! has_pass_marker "$EXPECTED_STRICT_TESTS" "$run_dir/stdout"; then
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

unique_fingerprints=$(cut -f5 "$manifest" | awk 'NR > 1' | sort -u | wc -l)
{
  printf 'result=%s\n' "$result"
  printf 'runs=%s\n' "$runs"
  printf 'unique_fingerprints=%s\n' "$unique_fingerprints"
  printf 'reference_fingerprint=%s\n' "$reference_fingerprint"
  printf 'manifest=runs.tsv\n'
} >"$output_dir/summary.txt"

if [[ $probe_full == true ]]; then
  probe_dir=$output_dir/full-probe
  probe_tmp=$scratch_root/full-probe
  mkdir -p "$probe_dir" "$probe_tmp"
  set +e
  (
    cd "$probe_tmp"
    timeout --signal=TERM --kill-after=5s "${timeout_seconds}s" \
      "$hermit_bin" "${strict_args[@]}" "--tmp=$probe_tmp" -- \
      "$ninja_test_bin" --gtest_color=no \
      >"$probe_dir/stdout" 2>"$probe_dir/stderr"
  )
  probe_exit=$?
  set -e

  probe_result=FAILED
  if ((probe_exit == 0)) &&
    has_pass_marker "$EXPECTED_NATIVE_TESTS" "$probe_dir/stdout"; then
    probe_result=PASSED
  elif ((probe_exit == 124 || probe_exit == 137)); then
    probe_result=TIMED_OUT
  fi
  {
    printf 'result=%s\n' "$probe_result"
    printf 'exit_code=%s\n' "$probe_exit"
    printf 'stdout_sha256=%s\n' "$(sha256_file "$probe_dir/stdout")"
    printf 'stderr_sha256=%s\n' "$(sha256_file "$probe_dir/stderr")"
  } >"$probe_dir/observation.txt"
fi

printf '%s\n' "$result"
printf 'Evidence: %s\n' "$output_dir"
[[ $result == DETERMINISTIC ]]

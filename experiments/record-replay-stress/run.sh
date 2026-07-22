#!/usr/bin/env bash
set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

sha256_file() {
  sha256sum "$1" | awk '{ print $1 }'
}

normalize_stderr() {
  "${PYTHON_BIN}" -c 'from pathlib import Path; import sys; data = Path(sys.argv[1]).read_bytes().split(b"\nRECORDING COMPLETE!", 1)[0]; Path(sys.argv[2]).write_bytes(b"".join(line for line in data.splitlines(keepends=True) if not line.startswith(b"timeout: ")))' "$1" "$2"
}

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
FIXTURES="${SCRIPT_DIR}/fixtures"
HERMIT_BIN="${HERMIT_BIN:-${REPO_ROOT}/target/debug/hermit}"
PYTHON_BIN="${PYTHON_BIN:-/usr/bin/python3}"
CASE_TIMEOUT_SECONDS="${CASE_TIMEOUT_SECONDS:-30}"
REPLAYS_PER_RECORD=3
TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_ROOT="${ARTIFACT_ROOT:-${REPO_ROOT}/target/record-replay-stress/${TIMESTAMP}}"
RESULTS_FILE="${RESULTS_FILE:-${SCRIPT_DIR}/results.tsv}"
METADATA_FILE="${METADATA_FILE:-${SCRIPT_DIR}/metadata.txt}"
BUILD_ROOT="${ARTIFACT_ROOT}/build"

[[ -x "${HERMIT_BIN}" ]] || fail "Hermit binary is not executable: ${HERMIT_BIN}"
[[ -x "${PYTHON_BIN}" ]] || fail "Python binary is not executable: ${PYTHON_BIN}"
[[ ! -e "${ARTIFACT_ROOT}" ]] || fail "artifact directory already exists: ${ARTIFACT_ROOT}"
[[ "${CASE_TIMEOUT_SECONDS}" =~ ^[1-9][0-9]*$ ]] ||
  fail "CASE_TIMEOUT_SECONDS must be a positive integer"
for command in awk cc cmp make sha256sum timeout; do
  command -v "${command}" >/dev/null || fail "required command not found: ${command}"
done

mkdir -p "${ARTIFACT_ROOT}" "${BUILD_ROOT}"
cc -O2 -g -pthread -Wall -Wextra -Werror "${FIXTURES}/pthread_pipe.c" -o "${BUILD_ROOT}/pthread_pipe"
cc -O2 -g -Wall -Wextra -Werror "${FIXTURES}/make_worker.c" -o "${BUILD_ROOT}/make_worker"
cp -R "${FIXTURES}/make_parallel" "${BUILD_ROOT}/make_parallel"

export LC_ALL=C
{
  printf 'schema_version=1\n'
  printf 'started_at_utc=%s\n' "${TIMESTAMP}"
  printf 'repository_commit=%s\n' "$(git -C "${REPO_ROOT}" rev-parse HEAD)"
  printf 'repository_branch=%s\n' "$(git -C "${REPO_ROOT}" branch --show-current)"
  printf 'hermit=%s\n' "${HERMIT_BIN}"
  printf 'hermit_sha256=%s\n' "$(sha256_file "${HERMIT_BIN}")"
  printf 'replays_per_record=%s\n' "${REPLAYS_PER_RECORD}"
  printf 'case_timeout_seconds=%s\n' "${CASE_TIMEOUT_SECONDS}"
  printf 'host_kernel=%s\n' "$(uname -srmo)"
  printf 'python=%s\n' "$("${PYTHON_BIN}" --version 2>&1)"
  printf 'make=%s\n' "$(make --version | awk 'NR == 1 { print; exit }')"
  printf 'cc=%s\n' "$(cc --version | awk 'NR == 1 { print; exit }')"
  printf 'artifact_root=%s\n' "${ARTIFACT_ROOT}"
} >"${METADATA_FILE}"

printf 'workload\trecord_exit\treplay_exits\tstdout_sha256\tstderr_sha256\tbyte_identical\tresult\n' >"${RESULTS_FILE}"

TOTAL=0
PASSED=0

run_case() {
  local name="$1"
  local program="$2"
  shift 2
  local args=("$@")
  local case_root="${ARTIFACT_ROOT}/${name}"
  local data_dir="${case_root}/data"
  local record_exit recording_id recording_dir
  local replay_exit replay_exits replay_index
  local byte_identical=yes
  local result=fail
  local stdout_hash=- stderr_hash=-

  mkdir -p "${case_root}" "${data_dir}"
  printf '%q ' "${program}" "${args[@]}" >"${case_root}/command.txt"
  printf '\n' >>"${case_root}/command.txt"

  set +e
  timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_SECONDS}s" env HERMIT_MODE=record "${HERMIT_BIN}" --log off record start --data-dir="${data_dir}" -- "${program}" "${args[@]}" >"${case_root}/record.stdout" 2>"${case_root}/record.stderr"
  record_exit=$?
  set -e
  printf '%s\n' "${record_exit}" >"${case_root}/record.status"
  normalize_stderr "${case_root}/record.stderr" "${case_root}/record.guest.stderr"

  recording_id=
  if [[ -f "${data_dir}/last" ]]; then
    recording_id="$(tr -d '\n' <"${data_dir}/last")"
  fi
  recording_dir="${data_dir}/${recording_id}"
  replay_exits=

  if [[ -z "${recording_id}" || ! -f "${recording_dir}/metadata.json" ]]; then
    byte_identical=no
    printf 'recording unavailable; record exit=%s\n' "${record_exit}" >"${case_root}/diagnostic.txt"
  else
    for ((replay_index = 1; replay_index <= REPLAYS_PER_RECORD; replay_index++)); do
      set +e
      timeout --signal=TERM --kill-after=5s "${CASE_TIMEOUT_SECONDS}s" env HERMIT_MODE=replay "${HERMIT_BIN}" --log off replay --autopilot --data-dir="${data_dir}" "${recording_id}" >"${case_root}/replay-${replay_index}.stdout" 2>"${case_root}/replay-${replay_index}.stderr"
      replay_exit=$?
      set -e
      printf '%s\n' "${replay_exit}" >"${case_root}/replay-${replay_index}.status"
      normalize_stderr "${case_root}/replay-${replay_index}.stderr" "${case_root}/replay-${replay_index}.guest.stderr"
      replay_exits+="${replay_exits:+,}${replay_exit}"

      if [[ "${replay_exit}" -ne "${record_exit}" ]] ||
        ! cmp -s "${case_root}/record.stdout" "${case_root}/replay-${replay_index}.stdout" ||
        ! cmp -s "${case_root}/record.guest.stderr" "${case_root}/replay-${replay_index}.guest.stderr"; then
        byte_identical=no
      fi
    done
  fi

  stdout_hash="$(sha256_file "${case_root}/record.stdout")"
  stderr_hash="$(sha256_file "${case_root}/record.guest.stderr")"
  if [[ "${record_exit}" -eq 0 && "${byte_identical}" == yes &&
        "${replay_exits}" == 0,0,0 ]]; then
    result=pass
    ((PASSED += 1))
  else
    {
      printf 'record_exit=%s\n' "${record_exit}"
      printf 'replay_exits=%s\n' "${replay_exits:-not_run}"
      printf 'byte_identical=%s\n' "${byte_identical}"
      printf 'record_stderr:\n'
      cat "${case_root}/record.stderr"
      for ((replay_index = 1; replay_index <= REPLAYS_PER_RECORD; replay_index++)); do
        if [[ -f "${case_root}/replay-${replay_index}.stderr" ]]; then
          printf '\nreplay_%s_stderr:\n' "${replay_index}"
          cat "${case_root}/replay-${replay_index}.stderr"
        fi
      done
    } >"${case_root}/diagnostic.txt"
  fi

  ((TOTAL += 1))
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "${name}" "${record_exit}" "${replay_exits:-not_run}" "${stdout_hash}" "${stderr_hash}" "${byte_identical}" "${result}" >>"${RESULTS_FILE}"
  printf '%-24s record=%-3s replays=%-8s identical=%-3s result=%s\n' "${name}" "${record_exit}" "${replay_exits:-not_run}" "${byte_identical}" "${result}"
}

run_case python_multiprocessing "${PYTHON_BIN}" "${FIXTURES}/python_multiprocessing.py"
run_case pthread_pipe "${BUILD_ROOT}/pthread_pipe"
run_case make_parallel "$(command -v make)" --no-print-directory -j4 -f "${BUILD_ROOT}/make_parallel/Makefile" "BUILD_DIR=${BUILD_ROOT}/make_parallel/output" "WORKER=${BUILD_ROOT}/make_worker"

{
  printf 'total=%s\n' "${TOTAL}"
  printf 'passed=%s\n' "${PASSED}"
  printf 'failed=%s\n' "$((TOTAL - PASSED))"
} >>"${METADATA_FILE}"

printf 'Record/replay stress complete: %s/%s workloads passed.\n' "${PASSED}" "${TOTAL}"
printf 'Results: %s\nArtifacts: %s\n' "${RESULTS_FILE}" "${ARTIFACT_ROOT}"
[[ "${PASSED}" -eq "${TOTAL}" ]]

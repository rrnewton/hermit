#!/usr/bin/env bash

set -uo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/progress-report.sh [options]

Required inputs are two existing worktrees. No branch is checked out or modified.

  --main-worktree DIR       Worktree for rrnewton/hermit main (default: repository root)
  --frontier-worktree DIR   Worktree for the speculative/frontier branch (required)
  --output FILE             Generated Markdown report (default: docs/PROGRESS_REPORT.md)
  --data FILE               Generated TSV result ledger (default: docs/PROGRESS_REPORT.tsv)
  --artifacts DIR           Per-command logs (default: /tmp/hermit-progress-report-<timestamp>)
  --case-timeout SECONDS    Wall-clock limit per command (default: 90)
  --skip-build              Reuse existing workspace builds
  -h, --help                Show this help

Set HTTPS_PROXY when GitHub CI status should be included.
EOF
}

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
main_root=$repo_root
frontier_root=
output=$repo_root/docs/PROGRESS_REPORT.md
data=$repo_root/docs/PROGRESS_REPORT.tsv
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
artifacts=/tmp/hermit-progress-report-$timestamp
case_timeout=90
skip_build=0

while (($#)); do
  case "$1" in
    --main-worktree)
      main_root=$2
      shift 2
      ;;
    --frontier-worktree)
      frontier_root=$2
      shift 2
      ;;
    --output)
      output=$2
      shift 2
      ;;
    --data)
      data=$2
      shift 2
      ;;
    --artifacts)
      artifacts=$2
      shift 2
      ;;
    --case-timeout)
      case_timeout=$2
      shift 2
      ;;
    --skip-build)
      skip_build=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z $frontier_root ]]; then
  echo "--frontier-worktree is required" >&2
  exit 2
fi

for root in "$main_root" "$frontier_root"; do
  if [[ ! -x $root/target/debug/hermit && $skip_build -eq 1 ]]; then
    echo "missing $root/target/debug/hermit; omit --skip-build" >&2
    exit 2
  fi
  git -C "$root" rev-parse --verify HEAD >/dev/null || exit 2
done

mkdir -p "$(dirname "$output")" "$(dirname "$data")" "$artifacts"
printf 'branch\tmode\tbackend\tcategory\tlanguage\ttest\tstatus\texit_code\tduration_seconds\tcommand\tlog\n' >"$data"

sanitize() {
  printf '%s' "$1" | tr '\t\r\n' '   '
}

record_result() {
  local branch=$1 mode=$2 backend=$3 category=$4 language=$5 test=$6 status=$7
  local exit_code=$8 duration=$9 command=${10} log=${11}
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$(sanitize "$branch")" "$(sanitize "$mode")" "$(sanitize "$backend")" \
    "$(sanitize "$category")" "$(sanitize "$language")" "$(sanitize "$test")" \
    "$(sanitize "$status")" "$(sanitize "$exit_code")" "$(sanitize "$duration")" \
    "$(sanitize "$command")" "$(sanitize "$log")" >>"$data"
}

command_string() {
  printf '%q ' "$@"
}

elapsed_seconds() {
  local start=$1 end=$2
  awk -v start="$start" -v end="$end" 'BEGIN { printf "%.3f", end - start }'
}

backend_args() {
  local root=$1 backend=$2
  if [[ $backend == ptrace ]]; then
    if "$root/target/debug/hermit" --help 2>&1 | grep -q -- '--backend'; then
      printf '%s\n' --backend ptrace
    fi
  else
    printf '%s\n' --backend "$backend"
  fi
}

run_strict_probe() {
  local branch=$1 root=$2 backend=$3 language=$4 test=$5 marker=$6
  shift 6
  local -a backend_opts=()
  while IFS= read -r arg; do
    [[ -n $arg ]] && backend_opts+=("$arg")
  done < <(backend_args "$root" "$backend")
  local -a prefix=(timeout "${case_timeout}s" "$root/target/debug/hermit" --log error)
  prefix+=("${backend_opts[@]}")
  prefix+=(run --base-env=minimal --no-virtualize-cpuid --preemption-timeout=disabled)
  local -a marker_command=("${prefix[@]}" -- "$@")
  local -a verify_command=("${prefix[@]}" --verify -- "$@")

  local stem=$artifacts/${branch}-strict-${backend}-${test}
  local start end duration marker_rc verify_rc status=FAIL
  start=$(date +%s.%N)
  "${marker_command[@]}" >"$stem.marker.stdout" 2>"$stem.marker.stderr"
  marker_rc=$?
  "${verify_command[@]}" >"$stem.verify.stdout" 2>"$stem.verify.stderr"
  verify_rc=$?
  end=$(date +%s.%N)
  duration=$(elapsed_seconds "$start" "$end")

  if [[ $marker_rc -eq 0 && $verify_rc -eq 0 ]] \
    && { grep -Fq -- "$marker" "$stem.marker.stdout" || grep -Fq -- "$marker" "$stem.marker.stderr"; } \
    && { grep -Fq 'Success: deterministic' "$stem.verify.stdout" || grep -Fq 'Success: deterministic' "$stem.verify.stderr"; }; then
    status=PASS
  fi
  record_result "$branch" strict "$backend" basic_system_binaries "$language" "$test" \
    "$status" "$marker_rc/$verify_rc" "$duration" \
    "$(command_string "${marker_command[@]}") ; $(command_string "${verify_command[@]}")" "$stem"
}

run_record_probe() {
  local branch=$1 root=$2 language=$3 test=$4
  shift 4
  local stem=$artifacts/${branch}-record-${test}
  local -a command=(timeout "${case_timeout}s" "$root/target/debug/hermit" --log error record start --verify -- "$@")
  local start end duration rc status=FAIL
  start=$(date +%s.%N)
  "${command[@]}" >"$stem.stdout" 2>"$stem.stderr"
  rc=$?
  end=$(date +%s.%N)
  duration=$(elapsed_seconds "$start" "$end")
  if [[ $rc -eq 0 ]] && grep -Fq 'Success: replay matched recording.' "$stem.stderr"; then
    status=PASS
  fi
  record_result "$branch" record_replay ptrace basic_system_binaries "$language" "$test" \
    "$status" "$rc" "$duration" "$(command_string "${command[@]}")" "$stem"
}

run_command_case() {
  local branch=$1 root=$2 mode=$3 backend=$4 category=$5 language=$6 test=$7 timeout_seconds=$8
  shift 8
  local stem=$artifacts/${branch}-${mode}-${test}
  local -a command=(timeout "${timeout_seconds}s" "$@")
  local start end duration rc status=FAIL
  start=$(date +%s.%N)
  (cd "$root" && "${command[@]}") >"$stem.stdout" 2>"$stem.stderr"
  rc=$?
  end=$(date +%s.%N)
  duration=$(elapsed_seconds "$start" "$end")
  [[ $rc -eq 0 ]] && status=PASS
  record_result "$branch" "$mode" "$backend" "$category" "$language" "$test" \
    "$status" "$rc" "$duration" "$(command_string "${command[@]}")" "$stem"
}

run_cargo_tests() {
  local branch=$1 root=$2 mode=$3 backend=$4 category=$5 language=$6 suite=$7 timeout_seconds=$8
  shift 8
  local stem=$artifacts/${branch}-${mode}-${suite}
  local -a command=(timeout "${timeout_seconds}s" cargo test "$@")
  local start end duration rc parsed=0 failures=0
  start=$(date +%s.%N)
  (cd "$root" && "${command[@]}") >"$stem.stdout" 2>"$stem.stderr"
  rc=$?
  end=$(date +%s.%N)
  duration=$(elapsed_seconds "$start" "$end")

  while IFS=$'\t' read -r status test_name; do
    [[ -z ${status:-} ]] && continue
    parsed=$((parsed + 1))
    [[ $status == FAIL ]] && failures=$((failures + 1))
    record_result "$branch" "$mode" "$backend" "$category" "$language" \
      "$suite::$test_name" "$status" "$rc" "$duration" "$(command_string "${command[@]}")" "$stem"
  done < <(
    awk '
      /^test .* \.\.\. ok$/ { line=$0; sub(/^test /, "", line); sub(/ \.\.\. ok$/, "", line); print "PASS\t" line }
      /^test .* \.\.\. FAILED$/ { line=$0; sub(/^test /, "", line); sub(/ \.\.\. FAILED$/, "", line); print "FAIL\t" line }
      /^test .* \.\.\. ignored/ { line=$0; sub(/^test /, "", line); sub(/ \.\.\. ignored.*/, "", line); print "SKIP\t" line }
    ' "$stem.stdout"
  )

  if [[ $parsed -eq 0 ]]; then
    local status=FAIL
    [[ $rc -eq 0 ]] && status=SKIP
    record_result "$branch" "$mode" "$backend" "$category" "$language" "$suite" \
      "$status" "$rc" "$duration" "$(command_string "${command[@]}")" "$stem"
  elif [[ $rc -ne 0 && $failures -eq 0 ]]; then
    record_result "$branch" "$mode" "$backend" "$category" "$language" "$suite::harness" \
      FAIL "$rc" "$duration" "$(command_string "${command[@]}")" "$stem"
  fi
}

skip_case() {
  record_result "$1" "$2" "$3" "$4" "$5" "$6" SKIP NA 0 "$7" NA
}

language_probes() {
  local branch=$1 root=$2 backend=$3
  run_strict_probe "$branch" "$root" "$backend" C/C++ c_ls coreutils /usr/bin/ls --version
  run_strict_probe "$branch" "$root" "$backend" C/C++ cpp_gpp 'Free Software Foundation' /usr/bin/g++ --version
  run_strict_probe "$branch" "$root" "$backend" Rust rustc_version 'rustc 1.' "$(rustup which rustc)" --version
  run_strict_probe "$branch" "$root" "$backend" Python python_hello python-ok /usr/bin/python3 -c "print('python-ok')"
  run_strict_probe "$branch" "$root" "$backend" Java java_version version /usr/bin/java -version
  run_strict_probe "$branch" "$root" "$backend" Go go_version 'go version' /usr/bin/go version
  run_strict_probe "$branch" "$root" "$backend" Ruby ruby_hello ruby-ok /usr/bin/ruby -e "puts 'ruby-ok'"
  run_strict_probe "$branch" "$root" "$backend" OCaml ocaml_version OCaml /usr/bin/ocaml -version
  run_strict_probe "$branch" "$root" "$backend" Node.js node_hello node-ok /usr/bin/node -e "console.log('node-ok')"
  run_strict_probe "$branch" "$root" "$backend" Other shell_hello shell-ok /usr/bin/sh -c "printf 'shell-ok\\n'"
}

record_language_probes() {
  local branch=$1 root=$2
  run_record_probe "$branch" "$root" C/C++ c_ls /usr/bin/ls --version
  run_record_probe "$branch" "$root" Rust rustc_version "$(rustup which rustc)" --version
  run_record_probe "$branch" "$root" Python python_hello /usr/bin/python3 -c "print('python-ok')"
  run_record_probe "$branch" "$root" Java java_version /usr/bin/java -version
  run_record_probe "$branch" "$root" Go go_version /usr/bin/go version
  run_record_probe "$branch" "$root" Ruby ruby_hello /usr/bin/ruby -e "puts 'ruby-ok'"
  run_record_probe "$branch" "$root" OCaml ocaml_version /usr/bin/ocaml -version
  run_record_probe "$branch" "$root" Node.js node_hello /usr/bin/node -e "console.log('node-ok')"
  run_record_probe "$branch" "$root" Other shell_hello /usr/bin/sh -c "printf 'shell-ok\\n'"
}

run_integration_suites() {
  local branch=$1 root=$2
  local spec suite language
  local -a suites=(
    clock_determinism:C/C++
    epoll_determinism:C/C++
    fork_exec_determinism:C/C++
    fp_reduction_determinism:C/C++
    ipc_determinism:C/C++
    mmap_determinism:C/C++
    random_determinism:C/C++
    signal_determinism:C/C++
    thread_sync_determinism:C/C++
    hashseed_determinism:Python
    thread_scheduling_fairness:Rust
    no_silent_skips:Rust
    cli:Rust
    integration_matrix:Other
  )
  for spec in "${suites[@]}"; do
    suite=${spec%%:*}
    language=${spec#*:}
    if [[ -f $root/hermit-cli/tests/$suite.rs ]]; then
      run_cargo_tests "$branch" "$root" strict ptrace integration_tests "$language" "$suite" 300 \
        -p hermit --test "$suite" -- --test-threads=1
    else
      skip_case "$branch" strict ptrace integration_tests "$language" "$suite" 'test target absent on this branch'
    fi
  done
}

run_oss_suites() {
  local branch=$1 root=$2
  if [[ -f $root/hermit-cli/tests/compression.rs ]]; then
    run_cargo_tests "$branch" "$root" strict ptrace oss_full_apps C/C++ compression 300 \
      -p hermit --test compression -- --test-threads=1
  else
    skip_case "$branch" strict ptrace oss_full_apps C/C++ compression 'test target absent on this branch'
  fi
  if [[ -f $root/hermit-cli/tests/redis_strict.rs ]]; then
    run_cargo_tests "$branch" "$root" strict ptrace oss_full_apps C/C++ redis_strict 300 \
      -p hermit --test redis_strict -- --test-threads=1
  else
    skip_case "$branch" strict ptrace oss_full_apps C/C++ redis_strict 'test target absent on this branch'
  fi
  if [[ -f $root/hermit-cli/tests/sqlite_veryquick.rs ]]; then
    run_cargo_tests "$branch" "$root" strict ptrace oss_full_apps C/C++ sqlite_veryquick 300 \
      -p hermit --test sqlite_veryquick sqlite_fast_subset_is_deterministic_under_strict_hermit -- --exact
  else
    skip_case "$branch" strict ptrace oss_full_apps C/C++ sqlite_veryquick 'test target absent on this branch'
  fi
  if [[ -f $root/hermit-cli/tests/leveldb.rs && -n ${LEVELDB_BUILD_DIR:-} ]]; then
    run_cargo_tests "$branch" "$root" strict ptrace oss_full_apps C/C++ leveldb 300 \
      -p hermit --test leveldb focused_leveldb_tests_are_deterministic_under_strict -- --exact
  else
    skip_case "$branch" strict ptrace oss_full_apps C/C++ leveldb 'LEVELDB_BUILD_DIR or test target unavailable'
  fi
  if [[ -f $root/hermit-cli/tests/python_stdlib.rs ]]; then
    run_cargo_tests "$branch" "$root" strict ptrace oss_full_apps Python python_stdlib 300 \
      -p hermit --test python_stdlib strict_python_stdlib_is_deterministic -- --ignored --exact
  else
    skip_case "$branch" strict ptrace oss_full_apps Python python_stdlib 'test target absent on this branch'
  fi
}

run_rr_suite() {
  local branch=$1 root=$2
  if [[ -f $root/hermit-cli/tests/rr_suite.rs && -f $root/third-party/rr/src/test/util.h ]]; then
    run_cargo_tests "$branch" "$root" strict ptrace rr_test_suite C/C++ rr_suite 1200 \
      -p hermit --test rr_suite -- --ignored --test-threads=1
  else
    skip_case "$branch" strict ptrace rr_test_suite C/C++ rr_suite 'rr target or initialized third-party/rr submodule unavailable'
  fi
}

run_special_sections() {
  local branch=$1 root=$2
  if [[ -f $root/hermit-cli/tests/hermit_modes.rs ]]; then
    run_cargo_tests "$branch" "$root" chaos ptrace chaos Rust chaos_mode_matrix 300 \
      -p hermit --test hermit_modes chaos_mode_matrix -- --exact
    run_cargo_tests "$branch" "$root" chaos ptrace chaos Rust hello_race_chaos_verify 300 \
      -p hermit --test hermit_modes hello_race_chaos_verify -- --exact
  fi
  if [[ -f $root/hermit-cli/tests/stress_suite.rs ]]; then
    run_cargo_tests "$branch" "$root" chaos ptrace chaos Rust fast_chaos_matrix 600 \
      -p hermit --test stress_suite fast_chaos_matrix -- --exact
  fi

  run_command_case "$branch" "$root" debugger ptrace debugger_attachment Other debugger_record_replay 180 \
    "$root/target/debug/hermit" record start --verify-with-gdbex=continue -- /bin/true

  if [[ -f $root/hermit-cli/tests/stress_suite.rs ]] \
    && grep -q 'schedule_bisect_localizes_publish_ordering_race' "$root/hermit-cli/tests/stress_suite.rs"; then
    run_cargo_tests "$branch" "$root" bisection ptrace schedule_bisection Rust schedule_bisect 900 \
      -p hermit --test stress_suite schedule_bisect_localizes_publish_ordering_race -- --exact
  else
    skip_case "$branch" bisection ptrace schedule_bisection Rust schedule_bisect 'test target absent on this branch'
  fi
}

run_branch() {
  local branch=$1 root=$2
  echo "==> $branch: $root"
  if [[ $skip_build -eq 0 ]]; then
    run_command_case "$branch" "$root" build host build Rust workspace_build 900 cargo build --workspace
  fi

  language_probes "$branch" "$root" ptrace
  language_probes "$branch" "$root" dbi
  language_probes "$branch" "$root" kvm

  run_cargo_tests "$branch" "$root" strict ptrace unit_tests Rust workspace_libs 900 \
    --workspace --lib -- --test-threads=1
  run_integration_suites "$branch" "$root"
  run_oss_suites "$branch" "$root"
  run_rr_suite "$branch" "$root"

  record_language_probes "$branch" "$root"
  if [[ -f $root/hermit-cli/tests/record_replay.rs ]]; then
    run_cargo_tests "$branch" "$root" record_replay ptrace integration_tests Rust record_replay 900 \
      -p hermit --test record_replay -- --test-threads=1
  fi
  run_special_sections "$branch" "$root"
}

run_branch main "$main_root"
run_branch frontier "$frontier_root"

cell() {
  local branch=$1 mode=$2 backend=$3 category=${4:-} language=${5:-}
  awk -F '\t' -v branch="$branch" -v mode="$mode" -v backend="$backend" \
    -v category="$category" -v language="$language" '
      NR > 1 && $1 == branch && $2 == mode && $3 == backend &&
      (category == "" || $4 == category) && (language == "" || $5 == language) {
        if ($7 == "PASS") { passed++; attempted++ }
        else if ($7 == "FAIL") { attempted++ }
      }
      END { if (attempted == 0) print "-"; else printf "%d/%d", passed, attempted }
    ' "$data"
}

ci_status() {
  local branch=$1
  if command -v gh >/dev/null 2>&1; then
    local result
    result=$(timeout 30s gh run list --repo rrnewton/hermit --branch "$branch" --limit 1 \
      --json workflowName,status,conclusion,headSha,createdAt,url \
      --jq 'if length == 0 then "no run found" else .[0] | [.workflowName, (.status + "/" + (.conclusion // "pending")), .headSha[0:7], .createdAt, .url] | join(" | ") end' 2>/dev/null) || true
    [[ -n $result ]] && printf '%s' "$result" && return
  fi
  printf 'unavailable (gh/proxy/auth or no matching run)'
}

emit_matrix() {
  local branch=$1 mode=$2 backend=$3 title=$4
  local -a categories=(basic_system_binaries rr_test_suite oss_full_apps unit_tests integration_tests)
  local -a labels=('Basic system binaries' 'rr test suite' 'OSS full apps' 'Unit tests' 'Integration tests')
  local -a languages=('C/C++' Rust Python Java Go Ruby OCaml 'Node.js' Other)
  {
    echo "#### $title"
    echo
    printf '| Category | Total |'
    local language
    for language in "${languages[@]}"; do printf ' %s |' "$language"; done
    echo
    printf '|---|---:'
    for language in "${languages[@]}"; do printf '|---:'; done
    echo '|'
    printf '| **Grand total** | **%s** |' "$(cell "$branch" "$mode" "$backend")"
    for language in "${languages[@]}"; do printf ' %s |' "$(cell "$branch" "$mode" "$backend" '' "$language")"; done
    echo
    local i
    for i in "${!categories[@]}"; do
      printf '| %s | %s |' "${labels[$i]}" "$(cell "$branch" "$mode" "$backend" "${categories[$i]}")"
      for language in "${languages[@]}"; do
        printf ' %s |' "$(cell "$branch" "$mode" "$backend" "${categories[$i]}" "$language")"
      done
      echo
    done
    echo
  } >>"$output"
}

emit_special() {
  local branch=$1 mode=$2 title=$3
  {
    echo "### $title"
    echo
    echo "Result: **$(cell "$branch" "$mode" ptrace)**"
    echo
    echo '| Test | Language | Status | Evidence |'
    echo '|---|---|---|---|'
    awk -F '\t' -v branch="$branch" -v mode="$mode" '
      NR > 1 && $1 == branch && $2 == mode {
        printf "| `%s` | %s | %s | `%s` |\n", $6, $5, $7, $11
      }
    ' "$data"
    echo
  } >>"$output"
}

main_sha=$(git -C "$main_root" rev-parse HEAD)
frontier_sha=$(git -C "$frontier_root" rev-parse HEAD)
generated_at=$(date -u +'%Y-%m-%dT%H:%M:%SZ')

cat >"$output" <<EOF
# Hermit Progress Report

Generated: **$generated_at**

This is generated evidence, not an estimate. Regenerate it with
[\`.llms/skills/progress-rubric.md\`](../.llms/skills/progress-rubric.md) and
[\`scripts/progress-report.sh\`](../scripts/progress-report.sh). Raw case results are in
[\`PROGRESS_REPORT.tsv\`](PROGRESS_REPORT.tsv); command logs for this run are in \`$artifacts\`.

## Measurement Contract

- Cells are **passed/attempted named cases**. \`-\` means no runnable case was present; ignored,
  missing-dependency, and absent-target cases are recorded as \`SKIP\` in the TSV and excluded from
  attempted totals.
- Strict basic-binary cases require Hermit's built-in normalized \`--verify\` result plus an
  independent workload-marker run. This rejects a backend that exits zero without running the
  requested program while excluding timestamped tool diagnostics from guest determinism.
- C and C++ share **C/C++**, as requested, because their guest-visible syscall surface is the
  relevant unit here.
- Hardware/environment failures remain failures. They are not converted into passes.

## Environment

- Host: \`$(uname -a)\`
- Main: \`$main_sha\` from \`$main_root\`
- Frontier: \`$frontier_sha\` from \`$frontier_root\`
- Main CI: $(ci_status main)
- Frontier CI: $(ci_status frontier)

EOF

for branch in main frontier; do
  {
    echo "## Branch: $branch"
    echo
  } >>"$output"
  emit_matrix "$branch" strict ptrace "1. \`hermit run\` strict determinism (ptrace)"
  emit_matrix "$branch" strict dbi "2. \`hermit run --backend dbi\`"
  emit_matrix "$branch" strict kvm "3. \`hermit run --backend kvm\`"
  emit_matrix "$branch" record_replay ptrace '4. Record/replay'
  emit_special "$branch" chaos '5. Chaos mode tests'
  emit_special "$branch" debugger '6. Debugger attachment tests'
  emit_special "$branch" bisection '7. Schedule bisection examples'
done

{
  echo '## Observed Failures'
  echo
  echo '| Branch | Mode | Backend | Category | Language | Cases | Example | Exit | Evidence |'
  echo '|---|---|---|---|---|---:|---|---:|---|'
  awk -F '\t' '
    NR > 1 && $7 == "FAIL" {
      key = $1 SUBSEP $2 SUBSEP $3 SUBSEP $4 SUBSEP $5 SUBSEP $8 SUBSEP $11
      count[key]++
      if (!(key in example)) example[key] = $6
    }
    END {
      for (key in count) {
        split(key, values, SUBSEP)
        printf "| %s | %s | %s | %s | %s | %d | `%s` | %s | `%s` |\n",
          values[1], values[2], values[3], values[4], values[5], count[key],
          example[key], values[6], values[7]
      }
    }
  ' "$data"
  echo
  echo '## Skipped Coverage'
  echo
  echo '| Branch | Mode | Category | Language | Cases | Example | Reason/command |'
  echo '|---|---|---|---|---:|---|---|'
  awk -F '\t' '
    NR > 1 && $7 == "SKIP" {
      key = $1 SUBSEP $2 SUBSEP $4 SUBSEP $5 SUBSEP $10
      count[key]++
      if (!(key in example)) example[key] = $6
    }
    END {
      for (key in count) {
        split(key, values, SUBSEP)
        printf "| %s | %s | %s | %s | %d | `%s` | %s |\n",
          values[1], values[2], values[3], values[4], count[key], example[key], values[5]
      }
    }
  ' "$data"
} >>"$output"

echo "Wrote $output"
echo "Wrote $data"
echo "Logs: $artifacts"

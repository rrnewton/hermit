#!/usr/bin/env bash
set -uo pipefail

root=/home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-hermit
reverie_root=/home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-reverie
baseline=/home/newton/work/dev-hermit/worktrees/slots/kvm-ratchet-90-run8/ignored/kvm-ratchet-90-qualification-9d4bb692-run13/evidence
measurement="$root/integration-evidence/kvm-pr529-pr538-90ad-diagnostic"
split="$measurement/split"
cargo_home="$split/cargo"
expected_head=6c90880ebebcfff80478b555c05f500d6e307716
expected_parent=f8bc4909ffb84277ebd51817fc173b9aba1f13a2
expected_reverie=d0decf28738521b9ae0a33c4b930c5f3f3d43d27
expected_pr529=c632c111619cb47922a72235b3e1130b91355603
expected_pr538=90ad5b98fa897f03e817d74b1fa66e68f1b758fb
expected_reverie_tree=209cf49e90bb83787865e300fb3c56f3add1813d
expected_pr538_tree=962f40b6e487e82feb56b0da113f772376cba389
expected_lock_sha=fe7f86f4258654d4269fdb16a885348a0b8239f52f9ce37c7655032eda8863c9
expected_config_sha=30cec349c9e5cfb9147d8026be90a0ebb58988ec399125e7152f976de7f21733
expected_cell_ids_sha=2b2e885fac606893172f716f06489c32e98622c44099db4ee0a16df4fe70aa75
expected_executor_sha=238897d98024378a514c1870abccc9072d9b8776e0b9ff56a2eb60401162dccf
expected_image='localhost/hermit-hermetic-validate@sha256:e38c3b2d5cd8a17ed2f99b4a24dd76c9ee63329cdc8c9723e4650ab085fae985'
machine=devbig014
required_deadline_seconds=5400
kernel=$(uname -r)

die() {
  printf 'measure-pr529-pr538-cells: %s\n' "$*" >&2
  exit 2
}

[[ $(hostname -s) == "$machine" ]] || die "this measurement requires devbig014"
[[ ${KVM_MEASURE_CHILD_DEADLINE_SECONDS:-} == "$required_deadline_seconds" ]] ||
  die "the 90-minute whole-run deadline was not declared"
[[ ${CI_HUB_VALIDATE_LOCK_OWNER_PID:-} =~ ^[1-9][0-9]*$ ]] ||
  die "validate-lock owner PID is absent"
[[ ${CI_HUB_VALIDATE_LOCK_OWNER_FILE:-} == /* &&
   -r ${CI_HUB_VALIDATE_LOCK_OWNER_FILE:-} ]] ||
  die "validate-lock owner record is absent"
[[ $PPID == "$CI_HUB_VALIDATE_LOCK_OWNER_PID" ]] ||
  die "this process is not the direct child of validate-lock"
[[ $(sed -n 's/^pid=//p' "$CI_HUB_VALIDATE_LOCK_OWNER_FILE") ==
   "$CI_HUB_VALIDATE_LOCK_OWNER_PID" ]] || die "validate-lock owner record does not match"
[[ ! -e "$measurement" ]] || die "output already exists: $measurement"
[[ $(git -C "$root" rev-parse 'HEAD^{commit}') == "$expected_head" ]] ||
  die "Hermit measurement commit moved"
[[ $(git -C "$root" rev-parse 'HEAD^1') == "$expected_parent" ]] ||
[[ $(git -C "$reverie_root" rev-parse "HEAD^{commit}") == "$expected_reverie" ]] ||
  die "Reverie integration commit moved"
[[ $(git -C "$reverie_root" show -s --format=%T HEAD) == "$expected_reverie_tree" ]] ||
  die "Reverie integration tree moved"
[[ $(git -C "$reverie_root" show -s --format=%P HEAD) == "$expected_pr529 $expected_pr538" ]] ||
  die "Reverie integration parent pair or order changed"
[[ $(git -C "$reverie_root" show -s --format=%T "$expected_pr538") == "$expected_pr538_tree" ]] ||
  die "approved PR538 tree changed"
  die "Hermit base moved"
tracked_status=$(git -C "$root" status --porcelain=v1 --untracked-files=no --ignore-submodules=none)
[[ $tracked_status == " M Cargo.lock" ]] ||
  die "unexpected tracked source status: $tracked_status"
[[ $(sha256sum "$root/Cargo.lock" | awk "{print \$1}") == "$expected_lock_sha" ]] ||
  die "Cargo.lock overlay changed"
[[ $(sha256sum "$root/.cargo/config.toml" | awk "{print \$1}") == "$expected_config_sha" ]] ||
  die "Cargo patch overlay changed"
[[ $(sha256sum "$root/integration-evidence/reverie-source/reverie-kvm/src/executor.rs" | awk "{print \$1}") == "$expected_executor_sha" ]] ||
  die "copied Reverie source changed"
[[ $(<"$root/ci/hermetic/image.digest") == "$expected_image" ]] ||
  die "pinned validation image changed"

mkdir -p "$measurement" "$split"
tr '\0' '\n' <"/proc/$CI_HUB_VALIDATE_LOCK_OWNER_PID/cmdline" \
  >"$measurement/validate-lock-argv.txt" || die "could not record validate-lock argv"
awk -v deadline="$required_deadline_seconds" '
  previous == "--child-deadline" && $0 == deadline { found = 1 }
  { previous = $0 }
  END { exit !found }
' "$measurement/validate-lock-argv.txt" || die "validate-lock does not carry the 90-minute child deadline"
cp -a --reflink=always "$baseline/split/target" "$split/" ||
  die "could not copy the prior build and fixture target"
cp -a --reflink=always "$baseline/split/home" "$split/" ||
  die "could not copy the prior build home"
mkdir -p "$cargo_home/git/db"
cp -a --reflink=always "$baseline/split/cargo/registry" "$cargo_home/" ||
  die "could not copy the prior Cargo registry"
cp -a --reflink=always /home/newton/.cargo/git/db/. "$cargo_home/git/db/" ||
  die "could not add the cached Reverie commit objects"
mkdir -p "$measurement/results" "$measurement/preparation"

jq -s -e '
  [.[] | select((.attempts // []) |
    any((.stderr // "") | contains("previous guard is live")))] as $rows
  | ($rows | length) == 54
    and ($rows | map(.test) | unique | length) == 27
    and all($rows[];
      .outcome == "ERROR" and .result == "timeout"
      and .error_kind == "wall-timeout"
      and (.attempt == 1 or .attempt == 2))
' "$baseline/all-results.jsonl" >/dev/null ||
  die "the prior result set no longer identifies the expected 27 cells and 54 timeout rows"

jq -r '
  select((.attempts // []) |
    any((.stderr // "") | contains("previous guard is live")))
  | .test
' "$baseline/all-results.jsonl" | sort -u >"$measurement/affected-cell-ids.txt"

[[ $(sha256sum "$root/integration-evidence/expected-27-cell-ids.txt" | awk "{print \$1}") == "$expected_cell_ids_sha" ]] ||
  die "reviewed 27-cell identity file changed"
cmp -s "$measurement/affected-cell-ids.txt" "$root/integration-evidence/expected-27-cell-ids.txt" ||
  die "derived 27-cell identity differs from reviewed list"
cp "$root/integration-evidence/expected-27-cell-ids.txt" "$measurement/reviewed-expected-cell-ids.txt"
[[ $(wc -l <"$measurement/affected-cell-ids.txt") -eq 27 ]] ||
  die "affected-cell count is not 27"

printf 'test\tselector\n' >"$measurement/affected-cells.tsv"
while IFS= read -r test_id; do
  selector=$(awk -F '\t' -v wanted="$test_id" '
    NR > 1 && $1 == wanted { print $2 }
  ' "$baseline/population.tsv")
  [[ -n "$selector" ]] || die "missing selector for $test_id"
  [[ $(printf '%s\n' "$selector" | wc -l) -eq 1 ]] ||
    die "ambiguous selector for $test_id"
  printf '%s\t%s\n' "$test_id" "$selector" >>"$measurement/affected-cells.tsv"
done <"$measurement/affected-cell-ids.txt"

{
  printf 'hermit_measurement_commit=%s\n' "$expected_head"
  printf 'hermit_base_commit=%s\n' "$expected_parent"
  printf 'reverie_reviewed_head=%s\n' "$expected_reverie"
  printf 'pinned_image=%s\n' "$expected_image"
  printf 'machine=%s\n' "$machine"
  printf 'kernel=%s\n' "$kernel"
  printf 'cell_count=27\n'
  printf 'baseline_rows=54\n'
  printf 'baseline_attempts=1,2\n'
  printf 'comparison=Hermit base held at 9d4bb692; only the Reverie revision changes\n'
  printf 'status=diagnostic-only; the Reverie revision is not yet on main\n'
} >"$measurement/environment.txt"

"$root/ci/hermetic/run-in-pinned-root.sh" \
  --src "$root" --out "$split" --cargo-home "$cargo_home" -- \
  bash -c '
    set -euo pipefail
    /src/ci/hermetic/assert-no-network.sh
    /lib64/ld-linux-x86-64.so.2 --version
    [[ -c /dev/kvm && -r /dev/kvm && -w /dev/kvm ]]
  ' >"$measurement/pinned-environment.log" 2>&1 ||
  die "pinned environment check failed"
grep -Eq '(^|[^0-9])2\.42([^0-9]|$)' "$measurement/pinned-environment.log" ||
  die "pinned environment did not report glibc 2.42"

"$root/ci/hermetic/run-in-pinned-root.sh" \
  --src "$root" --out "$split" --cargo-home "$cargo_home" -- \
  bash -c '
    set -euo pipefail
    /src/ci/hermetic/assert-no-network.sh
    /src/ci/hermetic/assert-build-dependencies.sh
    export CI_DAG_BUILD_JOBS=1
    source /src/ci/configure-build-jobs.sh launcher
    cargo build --locked -p hermit-manifest-plan --bins
    CARGO_BUILD_JOBS=1 cargo build --release --locked -p hermit --bin hermit
  ' >"$measurement/build.log" 2>&1 ||
  die "Hermit build failed"

mkdir -p "$split/target/pr529-pr538-cell-measure"
cp "$split/target/release/hermit" "$split/target/pr529-pr538-cell-measure/hermit"
chmod 0555 "$split/target/pr529-pr538-cell-measure/hermit"
sha256sum "$split/target/pr529-pr538-cell-measure/hermit" >"$measurement/hermit.sha256"
binary_sha=$(awk '{print $1}' "$measurement/hermit.sha256")

mapfile -t tests <"$measurement/affected-cell-ids.txt"
E2E_RESULT_ROOT="$measurement/preparation" \
E2E_BUILD_ROOT="$split/target/e2e-build" \
E2E_MACHINE_SHORTNAME="$machine" \
E2E_KERNEL_VERSION="$kernel" \
HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER=1 \
HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER=1 \
"$root/ci/hermetic/run-in-pinned-root.sh" \
  --src "$root" --out "$split" --cargo-home "$cargo_home" \
  --env E2E_RESULT_ROOT --env E2E_BUILD_ROOT \
  --env E2E_MACHINE_SHORTNAME --env E2E_KERNEL_VERSION \
  --env HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER \
  --env HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER -- \
  bash -c '
    set -uo pipefail
    /src/ci/hermetic/assert-no-network.sh || exit $?
    export HERMIT_E2E_EMPTY_WORKDIR=/test
    overall=0
    for test_id in "$@"; do
      slug=${test_id//\//-}
      timeout --kill-after=10s 600s \
        target/debug/test-harness build \
          --include-manual --include-occasional \
          --test "$test_id" --mode verify --backend ptrace --jobs 1 \
          >"/results/$slug.log" 2>&1
      rc=$?
      printf "%s\t%s\n" "$test_id" "$rc" >>/results/invocations.tsv
      [[ $rc -eq 0 ]] || overall=1
    done
    exit "$overall"
  ' bash "${tests[@]}" >"$measurement/preparation-wrapper.log" 2>&1 ||
  die "fixture preparation failed"

printf 'test\tselector\trepetition\trc\telapsed_seconds\tresult_rows\tfirst_attempt_pass\tresult_dir\n' \
  >"$measurement/invocations.tsv"
: >"$measurement/all-results.jsonl"

assert_no_summary_residue() {
  local residue
  shopt -s nullglob
  residue=("$root"/.hermit-verify-summary-* "$root"/.hermit-backend-engagement-summary-*)
  shopt -u nullglob
  ((${#residue[@]} == 0)) || die "pre-existing Hermit summary output would make attribution ambiguous"
}

assert_no_summary_residue

run_one() {
  local test_id=$1 selector=$2 repetition=$3
  local slug=${test_id//\//-}
  local result_dir="$measurement/results/$slug/repetition-$repetition"
  local run_id="pr529-pr538-90ad-$slug-first-only"
  local start finish rc rows first_pass
  mkdir -p "$result_dir"
  assert_no_summary_residue
  start=$(date +%s)
  E2E_RESULT_ROOT="$result_dir" \
  E2E_BUILD_ROOT="$split/target/e2e-build" \
  E2E_RUN_ID="$run_id" \
  E2E_RUN_INDEX="$repetition" \
  E2E_MACHINE_SHORTNAME="$machine" \
  E2E_KERNEL_VERSION="$kernel" \
  E2E_KEEP_VERIFY_LOGS=1 \
  HERMIT_LOG_MAX_BYTES=67108864 \
  HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER=1 \
  HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER=1 \
  timeout --kill-after=10s 180s \
    "$root/ci/hermetic/run-in-pinned-root.sh" \
      --src "$root" --out "$split" --src-rw --cargo-home "$cargo_home" \
      --env E2E_RESULT_ROOT --env E2E_BUILD_ROOT --env E2E_RUN_ID \
      --env E2E_RUN_INDEX --env E2E_MACHINE_SHORTNAME --env E2E_KERNEL_VERSION \
      --env E2E_KEEP_VERIFY_LOGS --env HERMIT_LOG_MAX_BYTES \
      --env HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER \
      --env HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER -- \
      bash -c '
        set -euo pipefail
        /src/ci/hermetic/assert-no-network.sh
        export HERMIT_E2E_EMPTY_WORKDIR=/test
        selector=$1
        test_id=$2
        exec env HERMIT_BIN=/src/target/pr529-pr538-cell-measure/hermit \
          target/debug/test-harness run \
            "$selector" --include-occasional --prebuilt \
            --lane portable --test "$test_id" --mode verify --backend kvm \
            --results /results/results.jsonl \
            --junit /results/junit.xml --jobs 1
      ' bash "$selector" "$test_id" >"$result_dir/invocation.log" 2>&1
  rc=$?
  finish=$(date +%s)

  shopt -s nullglob
  summaries=("$root"/.hermit-verify-summary-* "$root"/.hermit-backend-engagement-summary-*)
  for path in "${summaries[@]}"; do
    mv "$path" "$result_dir/"
  done
  shopt -u nullglob
  assert_no_summary_residue

  rows=0
  first_pass=no
  if [[ -s "$result_dir/results.jsonl" ]]; then
    rows=$(jq -s 'length' "$result_dir/results.jsonl")
    cat "$result_dir/results.jsonl" >>"$measurement/all-results.jsonl"
    if jq -s -e '
      length == 1
      and .[0].attempt == 1
      and .[0].outcome == "PASS"
      and .[0].result == "pass"
      and ((.[0].relaxations // []) | length) == 0
      and ((.[0].attempts // []) | length) == 1
      and .[0].attempts[0].outcome == "PASS"
    ' "$result_dir/results.jsonl" >/dev/null; then
      first_pass=yes
    fi
    if ! jq -s -e \
      --arg test "$test_id" \
      --arg run_id "$run_id" \
      --arg machine "$machine" \
      --arg kernel "$kernel" \
      --arg hermit "$expected_head" \
      --arg binary "$binary_sha" \
      --argjson repetition "$repetition" '
        . as $rows
        | ($rows | length) >= 1
          and ($rows | map(.attempt)) == [range(1; ($rows | length) + 1)]
          and all($rows[];
            .test == $test
            and .run_id == $run_id
            and .machine_shortname == $machine
            and .kernel_version == $kernel
            and .hermit_sha == $hermit
            and .binary_sha256 == $binary
            and .source_tree_dirty == true
            and .run_index == $repetition
            and .backend == "kvm"
            and .mode == "verify"
            and ((.relaxations // []) | length) == 0)
      ' "$result_dir/results.jsonl" >/dev/null; then
      return 2
    fi
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$test_id" "$selector" "$repetition" "$rc" "$((finish - start))" \
    "$rows" "$first_pass" "$result_dir" >>"$measurement/invocations.tsv"

  [[ $rc -eq 0 || $rc -eq 1 ]] || return 2
  [[ $rows -ge 1 ]] || return 2
  [[ $first_pass == yes ]]
}

infrastructure_error=0
: >"$measurement/round1-passes.tsv"
while IFS=$'\t' read -r test_id selector; do
  [[ $test_id != test ]] || continue
  if run_one "$test_id" "$selector" 1; then
    printf '%s\t%s\n' "$test_id" "$selector" >>"$measurement/round1-passes.tsv"
  else
    rc=$?
    [[ $rc -eq 1 ]] || infrastructure_error=1
  fi
done <"$measurement/affected-cells.tsv"

while IFS=$'\t' read -r test_id selector; do
  for repetition in 2 3; do
    run_one "$test_id" "$selector" "$repetition"
    rc=$?
    [[ $rc -eq 0 || $rc -eq 1 ]] || infrastructure_error=1
  done
done <"$measurement/round1-passes.tsv"

invocation_count=$(($(wc -l <"$measurement/invocations.tsv") - 1))
round1_pass_count=$(wc -l <"$measurement/round1-passes.tsv")
expected_invocation_count=$((27 + 2 * round1_pass_count))
[[ $invocation_count -eq $expected_invocation_count ]] || infrastructure_error=1

jq -R -s 'split("\n") | map(select(length > 0))' \
  "$measurement/affected-cell-ids.txt" >"$measurement/affected-cell-ids.json"
jq -s --slurpfile expected "$measurement/affected-cell-ids.json" '
  (sort_by(.test, .run_index, .attempt)) as $rows
  | [
      $rows | group_by(.test)[]
      | select(
          length == 3
          and ([.[].run_index] | sort) == [1, 2, 3]
          and all(.[];
            .attempt == 1
            and .outcome == "PASS"
            and .result == "pass"
            and ((.relaxations // []) | length) == 0
            and ((.attempts // []) | length) == 1
            and .attempts[0].outcome == "PASS"))
      | .[0].test
    ] as $passing
  | {
      expected_cell_count: ($expected[0] | length),
      expected_cells_match: (($rows | map(.test) | unique | sort) == ($expected[0] | sort)),
      observed_cell_count: ($rows | map(.test) | unique | length),
      result_row_count: ($rows | length),
      retry_row_count: ($rows | map(select(.attempt > 1)) | length),
      invocation_count: $invocations,
      expected_invocation_count: $expected_invocations,
      cells_passing_three_first_attempts_without_retries: ($passing | length),
      passing_cell_ids: $passing,
      nonpassing_cell_ids: (($expected[0] - $passing) | sort)
    }
' --argjson invocations "$invocation_count" \
  --argjson expected_invocations "$expected_invocation_count" \
  "$measurement/all-results.jsonl" >"$measurement/summary.json"

[[ $(git -C "$root" rev-parse 'HEAD^{commit}') == "$expected_head" ]] ||
  infrastructure_error=1
git -C "$root" status --porcelain=v1 --untracked-files=all --ignore-submodules=none \
  >"$measurement/source-status-after.txt"
[[ ! -s "$measurement/source-status-after.txt" ]] || infrastructure_error=1
jq -e '.expected_cells_match == true' "$measurement/summary.json" >/dev/null ||
  infrastructure_error=1
find "$measurement" -type f ! -name artifact-hashes.tsv ! -name completion.txt -print0 |
  sort -z | xargs -0 sha256sum >"$measurement/artifact-hashes.tsv" ||
  die "artifact hashing failed"

if [[ $infrastructure_error -ne 0 ]]; then
  printf 'measurement_infrastructure=failed\n' >"$measurement/completion.txt"
  exit 2
fi
printf 'measurement_infrastructure=complete\n' >"$measurement/completion.txt"
exit 0

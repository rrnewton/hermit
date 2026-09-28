#!/usr/bin/env bash
set -uo pipefail
export LC_ALL=C

root=/home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-diagnostic
reverie_root=/home/newton/work/dev-hermit/worktrees/slots/kvm-stack-checkout-release
baseline=/home/newton/work/dev-hermit/worktrees/slots/kvm-ratchet-90-run8/ignored/kvm-ratchet-90-qualification-9d4bb692-run13/evidence
measurement=${KVM_MEASUREMENT_DIR:-}
split="$measurement/split"
cargo_home="$split/cargo"
expected_head=5c0bae832d515ce2b8b9cfcf2d7b97f010d641c6
expected_parent=9d4bb692ddfe02241e6341a2faeb83782215e1a5
expected_reverie=c632c111619cb47922a72235b3e1130b91355603
expected_reverie_parent=cf60111cb2c3781d98c7af1c85c6f7f395db8d4b
expected_reverie_tree=93961fe62f708118e3a8b1d40a207c93c055bfa9
expected_lock_sha=fe7f86f4258654d4269fdb16a885348a0b8239f52f9ce37c7655032eda8863c9
expected_config_sha=6778bfc6395071ea9770fe32f5631216456cfe2c3703ecf7184064647c2103bf
expected_ambient_config_sha=30cec349c9e5cfb9147d8026be90a0ebb58988ec399125e7152f976de7f21733
expected_cell_ids_sha=2b2e885fac606893172f716f06489c32e98622c44099db4ee0a16df4fe70aa75
expected_cell_table_sha=1a5a2779f13bfaf4a763d10bf287f434653c3c3d77938df03dfbee06235838bf
expected_result_guard_sha=46a63c82b8921045a5e91cc8da313d6c94ecca0952d318048d3074b0d959fb3e
expected_executor_sha=6ab166ad8a3a7b3d991a8a8c4334d084dd65f324f296459ce4d74768422bad28
expected_runner_sha=53b4a31aabc6e3270637a8e4551cbb8fb455537c0f8c90360582bc8ae8d060c6
expected_reverie_archive_sha=d745e59626ff9bb49cc84c1afdf5fe6b24086a29841888e48ac43e770501186b
expected_bounded_tool_sha=64f1da9c8e9dca2952083aee2849e81dc2929637dd8e0531ae49515c64eaeb42
expected_qgroup_limit_bytes=12884901888
required_free_reserve_bytes=68719476736
expected_image='localhost/hermit-hermetic-validate@sha256:e38c3b2d5cd8a17ed2f99b4a24dd76c9ee63329cdc8c9723e4650ab085fae985'
machine=devbig014
required_deadline_seconds=5400
kernel=$(uname -r)

die() {
  printf 'measure-pr529-only-cells: %s\n' "$*" >&2
  exit 2
}

guard_eq() {
  local label=$1 actual=$2 expected=$3
  [[ $actual == "$expected" ]] || die "$label changed: got $actual expected $expected"
}

result_guard="$root/integration-evidence/comparison-arms/result-guard.sh"
guard_eq result-guard-sha "$(sha256sum "$result_guard" | awk "{print \$1}")" "$expected_result_guard_sha"
# Dynamic absolute path is hash-bound above.
# shellcheck disable=SC1090,SC1091
source "$result_guard"
if [[ ${1:-} == --self-test ]]; then
  [[ $# -eq 1 ]] || die "--self-test accepts no additional arguments"
  run_result_guard_self_tests
  exit 0
fi
[[ $# -eq 0 ]] || die "unexpected launcher arguments"

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
[[ $(sed -n "s/^pid=//p" "$CI_HUB_VALIDATE_LOCK_OWNER_FILE") == "$CI_HUB_VALIDATE_LOCK_OWNER_PID" ]] ||
  die "validate-lock owner record does not match"
[[ -n $measurement ]] || die "KVM_MEASUREMENT_DIR is required"
case "$measurement" in
  "$root"/integration-evidence/brs-pr529-only-*) ;;
  *) die "measurement is outside the owned bounded evidence prefix: $measurement" ;;
esac
[[ -d $measurement ]] || die "bounded measurement directory does not exist: $measurement"
[[ -z $(ls -A "$measurement") ]] || die "bounded measurement directory is not empty"

hermit_head=$(git -C "$root" rev-parse "HEAD^{commit}")
hermit_parent=$(git -C "$root" rev-parse "HEAD^1")
reverie_head=$(git -C "$reverie_root" rev-parse "HEAD^{commit}")
reverie_tree=$(git -C "$reverie_root" show -s --format=%T HEAD)
reverie_parents=$(git -C "$reverie_root" show -s --format=%P HEAD)
guard_eq hermit-head "$hermit_head" "$expected_head"
guard_eq hermit-parent "$hermit_parent" "$expected_parent"
guard_eq reverie-head "$reverie_head" "$expected_reverie"
guard_eq reverie-tree "$reverie_tree" "$expected_reverie_tree"
guard_eq reverie-parent "$reverie_parents" "$expected_reverie_parent"
tracked_status=$(git -C "$root" status --porcelain=v1 --untracked-files=no --ignore-submodules=none)
guard_eq tracked-status "$tracked_status" " M Cargo.lock"
guard_eq Cargo.lock-sha "$(sha256sum "$root/Cargo.lock" | awk "{print \$1}")" "$expected_lock_sha"
guard_eq ambient-cargo-config-sha "$(sha256sum "$root/.cargo/config.toml" | awk "{print \$1}")" "$expected_ambient_config_sha"
guard_eq cargo-config-sha "$(sha256sum "$root/integration-evidence/comparison-arms/pr529-only/cargo-config.toml" | awk "{print \$1}")" "$expected_config_sha"
guard_eq cell-table-sha "$(sha256sum "$root/integration-evidence/brs-pr529-pr538-90ad-r2-canonical-1900877-1789013353477743636/affected-cells.tsv" | awk "{print \$1}")" "$expected_cell_table_sha"
guard_eq result-guard-sha "$(sha256sum "$result_guard" | awk "{print \$1}")" "$expected_result_guard_sha"
guard_eq copied-executor-sha "$(sha256sum "$root/integration-evidence/comparison-arms/pr529-only/reverie-source/reverie-kvm/src/executor.rs" | awk "{print \$1}")" "$expected_executor_sha"
guard_eq pinned-image "$(<"$root/ci/hermetic/image.digest")" "$expected_image"
guard_eq bounded-tool-sha "$(sha256sum "$root/integration-evidence/bounded-run-space" | awk "{print \$1}")" "$expected_bounded_tool_sha"
"$root/integration-evidence/bounded-run-space" report "$measurement" >"$measurement/bounded-space-before.txt" 2>&1
bounded_report_rc=$?
[[ $bounded_report_rc -eq 0 ]] || die "measurement qgroup is not bounded and below allowance (rc=$bounded_report_rc)"
grep -q "limit=$expected_qgroup_limit_bytes" "$measurement/bounded-space-before.txt" ||
  die "measurement qgroup limit is not the required $expected_qgroup_limit_bytes bytes"
available_bytes=$(df -B1 --output=avail "$measurement" | tail -1 | tr -d " ")
[[ $available_bytes =~ ^[0-9]+$ && $available_bytes -ge $required_free_reserve_bytes ]] ||
  die "filesystem reserve below $required_free_reserve_bytes bytes: $available_bytes"
guard_eq reverie-archive-sha "$(sha256sum "$root/integration-evidence/comparison-arms/pr529-only/reverie-source.tar" | awk "{print \$1}")" "$expected_reverie_archive_sha"
git -C "$reverie_root" archive --format=tar --output="$measurement/reverie-source-from-commit.tar" "$expected_reverie" ||
  die "could not archive the guarded PR529 commit"
guard_eq fresh-reverie-archive-sha "$(sha256sum "$measurement/reverie-source-from-commit.tar" | awk "{print \$1}")" "$expected_reverie_archive_sha"
mkdir -p "$measurement/reverie-source-expected"
tar -xf "$measurement/reverie-source-from-commit.tar" -C "$measurement/reverie-source-expected" ||
  die "could not extract the exact PR529 Reverie archive"
if ! diff -qr --no-dereference "$measurement/reverie-source-expected" "$root/integration-evidence/comparison-arms/pr529-only/reverie-source" >"$measurement/reverie-source-full-tree.diff"; then
  die "compiled Reverie source differs from the exact c632c111 archive"
fi
cp -a --reflink=always "$measurement/reverie-source-expected" "$measurement/reverie-source-mutated" ||
  die "could not stage the copied-tree mutation self-test"
printf "unexpected\n" >"$measurement/reverie-source-mutated/UNEXPECTED-SOURCE-FILE"
printf "content mutation\n" >>"$measurement/reverie-source-mutated/reverie-kvm/src/executor.rs"
if diff -qr --no-dereference "$measurement/reverie-source-expected" "$measurement/reverie-source-mutated" >"$measurement/reverie-source-mutation.diff"; then
  die "full copied-tree guard accepted changed content and an unexpected file"
fi

assert_guard_rejects() {
  local label=$1 actual=$2 expected=$3
  if (guard_eq "$label-mutation" "$actual" "mutated-$expected") >/dev/null 2>&1; then
    die "$label mutation was not rejected"
  fi
  printf "%s\trejected\n" "$label"
}
{
  assert_guard_rejects hermit-head "$hermit_head" "$expected_head"
  assert_guard_rejects hermit-parent "$hermit_parent" "$expected_parent"
  assert_guard_rejects reverie-head "$reverie_head" "$expected_reverie"
  assert_guard_rejects reverie-tree "$reverie_tree" "$expected_reverie_tree"
  assert_guard_rejects reverie-parent "$reverie_parents" "$expected_reverie_parent"
  assert_guard_rejects ambient-cargo-config-sha "$expected_ambient_config_sha" "$expected_ambient_config_sha"
  assert_guard_rejects cell-table-sha "$expected_cell_table_sha" "$expected_cell_table_sha"
} >"$measurement/source-guard-self-test.tsv"
cp "$root/.cargo/config.toml" "$measurement/ambient-cargo-config-mutated.toml"
printf '\n# rejected mutation\n' >>"$measurement/ambient-cargo-config-mutated.toml"
[[ $(sha256sum "$measurement/ambient-cargo-config-mutated.toml" | awk "{print \$1}") != "$expected_ambient_config_sha" ]] ||
  die "ambient Cargo config mutation was not rejected"

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
' "$baseline/all-results.jsonl" | LC_ALL=C sort -u >"$measurement/affected-cell-ids.txt"

[[ $(sha256sum "$root/integration-evidence/expected-27-cell-ids.txt" | awk "{print \$1}") == "$expected_cell_ids_sha" ]] ||
  die "reviewed 27-cell identity file changed"
reviewed_cell_table="$root/integration-evidence/brs-pr529-pr538-90ad-r2-canonical-1900877-1789013353477743636/affected-cells.tsv"
guard_eq reviewed-cell-table-sha "$(sha256sum "$reviewed_cell_table" | awk "{print \$1}")" "$expected_cell_table_sha"
awk -F '\t' '
  NR == 1 { if ($1 != "test" || $2 != "selector" || NF != 2) exit 1; next }
  NF != 2 { exit 1 }
  $2 != "--include-manual" && $2 != "--probe-disabled" { exit 1 }
  { print $1 }
' "$reviewed_cell_table" >"$measurement/reviewed-table-cell-ids.txt" ||
  die "reviewed cell table has an invalid header, row, or selector"
guard_eq reviewed-cell-table-count "$(wc -l <"$measurement/reviewed-table-cell-ids.txt")" 27
guard_eq reviewed-cell-table-unique-count "$(LC_ALL=C sort -u "$measurement/reviewed-table-cell-ids.txt" | wc -l)" 27
LC_ALL=C sort -u "$root/integration-evidence/expected-27-cell-ids.txt" \
  >"$measurement/reviewed-expected-cell-ids.set.txt"
cmp -s "$measurement/affected-cell-ids.txt" \
  "$measurement/reviewed-expected-cell-ids.set.txt" ||
  die "derived 27-cell membership differs from reviewed set"
cmp -s "$measurement/affected-cell-ids.txt" \
  "$root/integration-evidence/expected-27-cell-ids.txt" ||
  die "derived 27-cell C order differs from reviewed order"
cmp -s "$measurement/affected-cell-ids.txt" \
  "$measurement/reviewed-table-cell-ids.txt" ||
  die "derived 27-cell C order differs from the bound selector table"
cp "$root/integration-evidence/expected-27-cell-ids.txt" "$measurement/reviewed-expected-cell-ids.txt"
[[ $(wc -l <"$measurement/affected-cell-ids.txt") -eq 27 ]] ||
  die "affected-cell count is not 27"

cp "$reviewed_cell_table" "$measurement/reviewed-cell-table.selector-mutated.tsv"
sed -i '2s/--include-manual/--probe-disabled/' \
  "$measurement/reviewed-cell-table.selector-mutated.tsv"
[[ $(sha256sum "$measurement/reviewed-cell-table.selector-mutated.tsv" | awk "{print \$1}") != "$expected_cell_table_sha" ]] ||
  die "selector mutation did not change the bound table digest"
{
  sed -n '1p' "$reviewed_cell_table"
  sed -n '3p' "$reviewed_cell_table"
  sed -n '2p' "$reviewed_cell_table"
  sed -n '4,$p' "$reviewed_cell_table"
} >"$measurement/reviewed-cell-table.order-mutated.tsv"
tail -n +2 "$measurement/reviewed-cell-table.order-mutated.tsv" | cut -f1 \
  >"$measurement/reviewed-cell-table.order-mutated.ids.txt"
cmp -s "$measurement/reviewed-expected-cell-ids.set.txt" \
  <(LC_ALL=C sort -u "$measurement/reviewed-cell-table.order-mutated.ids.txt") ||
  die "order mutation unexpectedly changed membership"
if cmp -s "$measurement/affected-cell-ids.txt" \
    "$measurement/reviewed-cell-table.order-mutated.ids.txt"; then
  die "ordered selector-table mutation was not rejected"
fi
{
  printf 'selector-change\trejected-by-table-digest\n'
  printf 'same-set-different-order\trejected\n'
} >"$measurement/cell-table-guard-self-test.tsv"

cp "$reviewed_cell_table" "$measurement/affected-cells.tsv"
planned_cell_count=$(($(wc -l <"$measurement/affected-cells.tsv") - 1))
guard_eq planned-cell-count "$planned_cell_count" 27
assert_guard_rejects planned-cell-count "$planned_cell_count" 28 >"$measurement/planned-schedule-guard-self-test.tsv"

{
  printf "hermit_measurement_commit=%s\n" "$expected_head"
  printf "hermit_base_commit=%s\n" "$expected_parent"
  printf "reverie_arm_commit=%s\n" "$expected_reverie"
  printf "reverie_arm_tree=%s\n" "$expected_reverie_tree"
  printf "reverie_arm_parent=%s\n" "$expected_reverie_parent"
  printf "pinned_image=%s\n" "$expected_image"
  printf "machine=%s\n" "$machine"
  printf "kernel=%s\n" "$kernel"
  printf "cell_count=27\n"
  printf "baseline_rows=54\n"
  printf "baseline_attempts=1,2\n"
  printf "execution_policy=one invocation per cell; attempt 1 only; harness retry cap compiled to 1\n"
  printf "scheduled_jobs=1\n"
  printf "qgroup_limit_bytes=%s\n" "$expected_qgroup_limit_bytes"
  printf "required_free_reserve_bytes=%s\n" "$required_free_reserve_bytes"
  printf "comparison=Hermit measurement source held at 5c0bae83 over base 9d4bb692; the exact PR529-only Reverie source is measured as one causal comparison arm\n"
  printf "status=diagnostic-only; PR529 is not merged or selected by this diagnostic\n"
} >"$measurement/environment.txt"

"$root/ci/hermetic/run-in-pinned-root.sh" \
  --src "$root" --out "$split" --cargo-home "$cargo_home" -- \
  bash -c '
    set -euo pipefail
    /src/ci/hermetic/assert-no-network.sh
    /lib64/ld-linux-x86-64.so.2 --version
    /src/ci/hermetic/assert-build-dependencies.sh
    export CI_DAG_BUILD_JOBS=1
    source /src/ci/configure-build-jobs.sh launcher
    CARGO_BUILD_JOBS=1 cargo --config /src/integration-evidence/comparison-arms/pr529-only/cargo-config.toml build --release --locked -p hermit --bin hermit
    [[ -c /dev/kvm && -r /dev/kvm && -w /dev/kvm ]]
  ' >"$measurement/pinned-environment-and-hermit-build.log" 2>&1 ||
  die "pinned environment or Hermit build failed"
grep -Eq '(^|[^0-9])2\.42([^0-9]|$)' "$measurement/pinned-environment-and-hermit-build.log" ||
  die "pinned environment and build log did not report glibc 2.42"
runner_src="$root/ci/manifest-plan/src/runner.rs"
guard_eq runner-source-sha "$(sha256sum "$runner_src" | awk "{print \$1}")" "$expected_runner_sha"
cp "$runner_src" "$measurement/runner.rs.original" || die "could not preserve retry-policy source"
restore_runner() {
  cp "$measurement/runner.rs.original" "$runner_src"
}
trap restore_runner EXIT INT TERM
sed -i "s/pub const MAX_ATTEMPTS_PER_CELL: u64 = 2;/pub const MAX_ATTEMPTS_PER_CELL: u64 = 1;/" "$runner_src"
[[ $(rg -c "^pub const MAX_ATTEMPTS_PER_CELL: u64 = 1;$" "$runner_src") -eq 1 ]] ||
  die "could not compile a one-attempt diagnostic harness"
cp "$runner_src" "$measurement/runner.rs.one-attempt"

"$root/ci/hermetic/run-in-pinned-root.sh" \
  --src "$root" --out "$split" --cargo-home "$cargo_home" -- \
  bash -c '
    set -euo pipefail
    /src/ci/hermetic/assert-no-network.sh
    /src/ci/hermetic/assert-build-dependencies.sh
    export CI_DAG_BUILD_JOBS=1
    source /src/ci/configure-build-jobs.sh launcher
    cargo --config /src/integration-evidence/comparison-arms/pr529-only/cargo-config.toml build --locked -p hermit-manifest-plan --bins
  ' >"$measurement/build.log" 2>&1 ||
  die "one-attempt diagnostic harness build failed"
restore_runner
trap - EXIT INT TERM
guard_eq restored-runner-source-sha "$(sha256sum "$runner_src" | awk "{print \$1}")" "$expected_runner_sha"
sha256sum "$measurement/runner.rs.original" "$measurement/runner.rs.one-attempt" >"$measurement/harness-source-sha256.txt"

mkdir -p "$split/target/pr529-only-cell-measure"
cp "$split/target/release/hermit" "$split/target/pr529-only-cell-measure/hermit"
chmod 0555 "$split/target/pr529-only-cell-measure/hermit"
sha256sum "$split/target/pr529-only-cell-measure/hermit" >"$measurement/hermit.sha256"
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

printf 'test\tselector\trepetition\trc\telapsed_seconds\tresult_rows\tresult_class\tresult_dir\n' \
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
  local run_id="pr529-only-c632-$slug-first-only"
  local start finish rc rows result_class
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
        exec env HERMIT_BIN=/src/target/pr529-only-cell-measure/hermit \
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
  result_class=infrastructure-error
  if [[ -s "$result_dir/results.jsonl" ]]; then
    rows=$(jq -s 'length' "$result_dir/results.jsonl")
    cat "$result_dir/results.jsonl" >>"$measurement/all-results.jsonl"
    if ! jq -s -e \
      --arg test "$test_id" \
      --arg run_id "$run_id" \
      --arg machine "$machine" \
      --arg kernel "$kernel" \
      --arg hermit "$expected_head" \
      --arg binary "$binary_sha" \
      --argjson repetition "$repetition" '
        . as $rows
        | ($rows | length) == 1
          and ($rows | map(.attempt)) == [1]
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
            and ((.attempts // []) | length) == 1
            and .attempts[0].index == "1"
            and ((.relaxations // []) | length) == 0)
      ' "$result_dir/results.jsonl" >/dev/null; then
      return 2
    fi
    result_class=$(classify_result_row "$result_dir/results.jsonl") ||
      result_class=infrastructure-error
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$test_id" "$selector" "$repetition" "$rc" "$((finish - start))" \
    "$rows" "$result_class" "$result_dir" >>"$measurement/invocations.tsv"

  invocation_verdict "$rc" "$result_class" "$rows"
}

run_result_guard_self_tests >"$measurement/result-classification-guard-self-test.tsv"

infrastructure_error=0
: >"$measurement/round1-passes.tsv"
while IFS=$'\t' read -r test_id selector; do
  [[ $test_id != test ]] || continue
  if run_one "$test_id" "$selector" 1; then
    printf "%s\t%s\n" "$test_id" "$selector" >>"$measurement/round1-passes.tsv"
  else
    rc=$?
    [[ $rc -eq 1 ]] || infrastructure_error=1
  fi
done <"$measurement/affected-cells.tsv"

invocation_count=$(($(wc -l <"$measurement/invocations.tsv") - 1))
expected_invocation_count=27
[[ $invocation_count -eq $expected_invocation_count ]] || infrastructure_error=1
guard_eq schedule-count "$invocation_count" "$expected_invocation_count"
assert_guard_rejects schedule-count "$invocation_count" "$((expected_invocation_count + 1))" >"$measurement/schedule-guard-self-test.tsv"

jq -R -s 'split("\n") | map(select(length > 0))' \
  "$measurement/affected-cell-ids.txt" >"$measurement/affected-cell-ids.json"
cut -f1 "$measurement/round1-passes.tsv" |
  jq -R -s 'split("\n") | map(select(length > 0))' >"$measurement/strict-passing-cell-ids.json"
jq -s --slurpfile expected "$measurement/affected-cell-ids.json" \
  --slurpfile passing "$measurement/strict-passing-cell-ids.json" '
  (sort_by(.test, .run_index, .attempt)) as $rows
  | {
      expected_cell_count: ($expected[0] | length),
      expected_cells_match: (($rows | map(.test) | unique | sort) == ($expected[0] | sort)),
      observed_cell_count: ($rows | map(.test) | unique | length),
      result_row_count: ($rows | length),
      retry_row_count: ($rows | map(select(.attempt > 1)) | length),
      attempt_one_only: (all($rows[]; .attempt == 1 and ((.attempts // []) | length) == 1)),
      outcome_counts: ($rows | group_by(.outcome) | map({key: .[0].outcome, value: length}) | from_entries),
      invocation_count: $invocations,
      expected_invocation_count: $expected_invocations,
      cells_passing_first_attempt_without_retry: ($passing[0] | length),
      passing_cell_ids: $passing[0],
      nonpassing_cell_ids: (($expected[0] - $passing[0]) | sort)
    }
' --argjson invocations "$invocation_count" \
  --argjson expected_invocations "$expected_invocation_count" \
  "$measurement/all-results.jsonl" >"$measurement/summary.json"

guard_eq final-hermit-head "$(git -C "$root" rev-parse HEAD)" "$expected_head"
final_tracked_status=$(git -C "$root" status --porcelain=v1 --untracked-files=no --ignore-submodules=none)
printf "%s\n" "$final_tracked_status" >"$measurement/source-status-after.txt"
guard_eq final-tracked-status "$final_tracked_status" " M Cargo.lock"
guard_eq final-Cargo.lock-sha "$(sha256sum "$root/Cargo.lock" | awk "{print \$1}")" "$expected_lock_sha"
guard_eq final-cargo-config-sha "$(sha256sum "$root/integration-evidence/comparison-arms/pr529-only/cargo-config.toml" | awk "{print \$1}")" "$expected_config_sha"
guard_eq final-ambient-cargo-config-sha "$(sha256sum "$root/.cargo/config.toml" | awk "{print \$1}")" "$expected_ambient_config_sha"
guard_eq final-cell-table-sha "$(sha256sum "$root/integration-evidence/brs-pr529-pr538-90ad-r2-canonical-1900877-1789013353477743636/affected-cells.tsv" | awk "{print \$1}")" "$expected_cell_table_sha"
guard_eq final-result-guard-sha "$(sha256sum "$result_guard" | awk "{print \$1}")" "$expected_result_guard_sha"
assert_guard_rejects final-tracked-status "$final_tracked_status" " M Cargo.lock mutated" >"$measurement/final-source-guard-self-test.tsv"
git -C "$root" status --porcelain=v1 --untracked-files=normal --ignore-submodules=none >"$measurement/source-status-inventory-after.txt"
jq -e '.expected_cells_match == true' "$measurement/summary.json" >/dev/null ||
  infrastructure_error=1
guard_eq summary-expected-cell-count "$(jq -r .expected_cell_count "$measurement/summary.json")" 27
guard_eq summary-observed-cell-count "$(jq -r .observed_cell_count "$measurement/summary.json")" 27
guard_eq summary-result-row-count "$(jq -r .result_row_count "$measurement/summary.json")" 27
guard_eq summary-retry-row-count "$(jq -r .retry_row_count "$measurement/summary.json")" 0
guard_eq summary-invocation-count "$(jq -r .invocation_count "$measurement/summary.json")" 27
guard_eq summary-attempt-one-only "$(jq -r .attempt_one_only "$measurement/summary.json")" true
"$root/integration-evidence/bounded-run-space" report "$measurement" >"$measurement/bounded-space-after.txt" 2>&1
bounded_report_rc=$?
[[ $bounded_report_rc -eq 0 ]] || infrastructure_error=1
find "$measurement" -type f ! -name artifact-hashes.tsv ! -name completion.txt -print0 |
  sort -z | xargs -0 sha256sum >"$measurement/artifact-hashes.tsv" ||
  die "artifact hashing failed"

if [[ $infrastructure_error -ne 0 ]]; then
  printf 'measurement_infrastructure=failed\n' >"$measurement/completion.txt"
  exit 2
fi
printf 'measurement_infrastructure=complete\n' >"$measurement/completion.txt"
exit 0

#!/usr/bin/env bash
set -uo pipefail
export LC_ALL=C
shopt -s nullglob

# The already-loaded launcher checks this path after the validate-lock queue
# wait and passes the reviewed digest.  Recheck it in the newly opened Bash
# process before the full campaign can perform any side effect.  Resolve the
# campaign and checkout from the script itself so the reviewed files can move
# between an identically based staging slot and the attributed run slot.
readonly payload_source=${BASH_SOURCE[0]}
script_dir=$(CDPATH= cd -- "$(dirname -- "$payload_source")" && pwd -P) || {
  printf 'KVM ratchet campaign: cannot resolve payload directory\n' >&2
  exit 2
}
readonly script_dir
readonly payload_path="$script_dir/${payload_source##*/}"
readonly payload_expected_sha256=${KVM_RATCHET_PAYLOAD_SHA256:-}
payload_startup_sha256=
case ${1:-} in
  --static-check | --self-test-invocation-gate) ;;
  *)
    [[ "$payload_expected_sha256" =~ ^[0-9a-f]{64}$ ]] || {
      printf 'KVM ratchet campaign: missing or malformed launcher-bound payload SHA256\n' >&2
      exit 2
    }
    payload_hash_line=$(sha256sum -- "$payload_path") || exit 2
    payload_startup_sha256=${payload_hash_line%% *}
    [[ "$payload_startup_sha256" == "$payload_expected_sha256" ]] || {
      printf 'KVM ratchet campaign: payload changed before startup: expected %s, got %s\n' \
        "$payload_expected_sha256" "$payload_startup_sha256" >&2
      exit 2
    }
    ;;
esac

root=$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null) || {
  printf 'KVM ratchet campaign: cannot resolve checkout from %s\n' "$script_dir" >&2
  exit 2
}
readonly root

readonly campaign="$script_dir/evidence"
readonly out="$campaign/split"
readonly cargo_home="$out/cargo"
readonly expected_sha=086d41a2f2f76e5a2cccceea342feb6957311c2b
readonly expected_tree=94490fd2cee758395150590a8ba25ff86443dbfd
readonly expected_machine=devbig014
readonly campaign_prefix=kvm-ratchet-90-086d41a2
readonly expected_image=localhost/hermit-hermetic-validate@sha256:e38c3b2d5cd8a17ed2f99b4a24dd76c9ee63329cdc8c9723e4650ab085fae985
readonly retained_log_max_bytes=1073741824
readonly cpu_timeout_multiplier=1
readonly wall_timeout_multiplier=1
readonly campaign_child_deadline_seconds=7200
readonly cells_sha256=84c5dfa8aebe71b65396134e09986d9e88bf8dbabc18b7fd2e08e28c73b4aa71
readonly plan_sha256=bc48a92f8d3981619bb9211eee12c42b5cfd2af202080293556c0f57c5824e3f
readonly image_file_sha256=761e73803bbea4333b9c316234a9d9f86ac9982ea111737293c7be59cfb0ee93
readonly submodule_status_sha256=134f1b5725890ecccc3dd6764cd49bc3b0f75aa87db31f687a9c2b3e821d1428
readonly expected_cells_sha256=a8d9d8e83eb03d3757e2f87e3d0b88c0387e7f505b19e5b7987fbaf2ca2b4f26
readonly population_validator_sha256=7db7ead41b484473386b5abf93a84020186f821d691743deffe72bb94da62d2f
readonly results_validator_sha256=b9e3a9d7c25883589f97268cb498140a0aa9a47f817dfb69f8deb654d0b6913f
readonly evidence_validator_sha256=24c4f0136275fc0afb58a405503531e400bebc7a3799ed788763a00b761ff269
readonly strict_artifact_validator_sha256=0230ae477c01a64c8aed105976f1b4f3af8f5ae36d1696849f3cc80d9d2388e3
readonly denominator_sha256=dabeb9601aeee07c40dde62763eecc97c5184be70c0ff3fe3a90d3deddfdf723
readonly overlap_sha256=ba32abd030a58b52f4cd73cf4b6dd327c7437ab1c1324f37a37e8cbce29b8727
readonly complement_ptrace_sha256=5e90830fec51df5d99c072e0a121ea997634ef2a230a9b79833e667fe9bc1f64
readonly complement_kvm_sha256=57e02a11097863186b0055a06ecd9ebb9888e024d644ec9b7b855f66590454ab
readonly classified_complement_sha256=85d180b33c937b3fcc533d6a98f7ef58a9ad76afd8fe5c09bbeb46218deb91aa

fail() {
  printf 'KVM ratchet campaign: %s\n' "$*" >&2
  exit 2
}

file_sha256() {
  local line
  line=$(sha256sum -- "$1") || fail "cannot hash $1"
  printf '%s' "${line%% *}"
}

require_sha256() {
  local path=$1
  local expected=$2
  local actual
  [[ -f "$path" ]] || fail "missing frozen input: $path"
  actual=$(file_sha256 "$path")
  [[ "$actual" == "$expected" ]] ||
    fail "$path changed: expected sha256 $expected, got $actual"
}

verify_frozen_subordinates() {
  local phase=$1
  require_sha256 "$script_dir/expected-cells.json" "$expected_cells_sha256"
  require_sha256 "$script_dir/validate-population.jq" "$population_validator_sha256"
  require_sha256 "$script_dir/validate-results.jq" "$results_validator_sha256"
  require_sha256 "$script_dir/validate-evidence.sh" "$evidence_validator_sha256"
  require_sha256 "$script_dir/validate-strict-invocation-artifacts.sh" "$strict_artifact_validator_sha256"
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$phase" "$expected_cells_sha256" "$population_validator_sha256" \
    "$results_validator_sha256" "$evidence_validator_sha256" \
    "$strict_artifact_validator_sha256" \
    >>"$campaign/frozen-input-checks.tsv"
}

verify_payload_self() {
  local phase=$1
  local actual
  actual=$(file_sha256 "$payload_path")
  [[ "$actual" == "$payload_expected_sha256" ]] ||
    fail "payload changed before $phase: expected $payload_expected_sha256, got $actual"
  printf '%s\t%s\t%s\n' "$phase" "$payload_expected_sha256" "$actual" \
    >>"$campaign/payload-input-checks.tsv"
}

verify_frozen_inputs() {
  local phase=$1
  verify_payload_self "$phase"
  verify_frozen_subordinates "$phase"
}

population_digest() {
  local operation=$1
  jq -cS --arg operation "$operation" \
    --slurpfile expected "$script_dir/expected-cells.json" \
    --slurpfile plan "$root/ci/expected-e2e-plan.json" \
    -f "$script_dir/validate-population.jq" \
    "$root/ci/compat-envelope/cells.json" |
    LC_ALL=C sort |
    sha256sum |
    awk '{print $1}'
}

static_check() {
  local head tree status image population_summary
  local denominator overlap complement_ptrace complement_kvm classified
  local submodule_status_hash
  command -v git >/dev/null || fail "git is required"
  command -v jq >/dev/null || fail "jq is required"
  command -v sha256sum >/dev/null || fail "sha256sum is required"
  [[ -d "$root" ]] || fail "checkout is absent: $root"
  [[ $(hostname -s) == "$expected_machine" ]] ||
    fail "campaign is frozen for $expected_machine"
  head=$(git -C "$root" rev-parse 'HEAD^{commit}') || fail "cannot resolve checkout HEAD"
  tree=$(git -C "$root" rev-parse 'HEAD^{tree}') || fail "cannot resolve checkout tree"
  [[ "$head" == "$expected_sha" ]] || fail "checkout is not at $expected_sha"
  [[ "$tree" == "$expected_tree" ]] || fail "checkout tree is not $expected_tree"
  status=$(git -C "$root" status --porcelain=v1 --untracked-files=all --ignore-submodules=none) ||
    fail "cannot inspect checkout status"
  [[ -z "$status" ]] || fail "checkout has non-ignored changes"

  require_sha256 "$root/ci/compat-envelope/cells.json" "$cells_sha256"
  require_sha256 "$root/ci/expected-e2e-plan.json" "$plan_sha256"
  require_sha256 "$root/ci/hermetic/image.digest" "$image_file_sha256"
  require_sha256 "$script_dir/expected-cells.json" "$expected_cells_sha256"
  require_sha256 "$script_dir/validate-population.jq" "$population_validator_sha256"
  require_sha256 "$script_dir/validate-results.jq" "$results_validator_sha256"
  require_sha256 "$script_dir/validate-evidence.sh" "$evidence_validator_sha256"
  require_sha256 "$script_dir/validate-strict-invocation-artifacts.sh" "$strict_artifact_validator_sha256"

  submodule_status_hash=$(git -C "$root" submodule status --recursive | sha256sum | awk '{print $1}')
  [[ "$submodule_status_hash" == "$submodule_status_sha256" ]] ||
    fail "submodule status changed: expected $submodule_status_sha256, got $submodule_status_hash"

  IFS= read -r image <"$root/ci/hermetic/image.digest"
  [[ "$image" == "$expected_image" ]] || fail "pinned image content changed"

  population_summary=$(jq --arg operation summary \
    --slurpfile expected "$script_dir/expected-cells.json" \
    --slurpfile plan "$root/ci/expected-e2e-plan.json" \
    -f "$script_dir/validate-population.jq" \
    "$root/ci/compat-envelope/cells.json") || fail "population validator failed"
  jq -e '.ok == true' >/dev/null <<<"$population_summary" ||
    fail "population contract failed"

  denominator=$(population_digest denominator)
  overlap=$(population_digest overlap)
  complement_ptrace=$(population_digest complement-ptrace)
  complement_kvm=$(population_digest complement-kvm)
  classified=$(population_digest classified-complement)
  [[ "$denominator" == "$denominator_sha256" ]] || fail "denominator digest changed"
  [[ "$overlap" == "$overlap_sha256" ]] || fail "overlap digest changed"
  [[ "$complement_ptrace" == "$complement_ptrace_sha256" ]] || fail "ptrace complement digest changed"
  [[ "$complement_kvm" == "$complement_kvm_sha256" ]] || fail "KVM complement digest changed"
  [[ "$classified" == "$classified_complement_sha256" ]] || fail "classified complement digest changed"
  [[ $(population_digest frozen) == "$classified_complement_sha256" ]] ||
    fail "frozen expected-cells digest changed"

  jq -n \
    --arg source_sha "$head" \
    --arg source_tree "$tree" \
    --arg machine "$expected_machine" \
    --arg image "$image" \
    --arg cells_sha256 "$cells_sha256" \
    --arg plan_sha256 "$plan_sha256" \
    --arg image_file_sha256 "$image_file_sha256" \
    --arg submodule_status_sha256 "$submodule_status_sha256" \
    --arg expected_cells_sha256 "$expected_cells_sha256" \
    --arg population_validator_sha256 "$population_validator_sha256" \
    --arg results_validator_sha256 "$results_validator_sha256" \
    --arg evidence_validator_sha256 "$evidence_validator_sha256" \
    --arg strict_artifact_validator_sha256 "$strict_artifact_validator_sha256" \
    --arg denominator "$denominator" \
    --arg overlap "$overlap" \
    --arg complement_ptrace "$complement_ptrace" \
    --arg complement_kvm "$complement_kvm" \
    --arg classified "$classified" \
    --argjson population "$population_summary" '
      {
        schema: 1,
        mode: "static-only-no-build-no-hermit-no-kvm-no-systemd",
        source_sha: $source_sha,
        source_tree: $source_tree,
        source_clean: true,
        machine: $machine,
        image: $image,
        hashes: {
          cells: $cells_sha256,
          expected_plan: $plan_sha256,
          image_file: $image_file_sha256,
          submodule_status: $submodule_status_sha256,
          expected_cells: $expected_cells_sha256,
          population_validator: $population_validator_sha256,
          results_validator: $results_validator_sha256,
          evidence_validator: $evidence_validator_sha256,
          strict_artifact_validator: $strict_artifact_validator_sha256
        },
        digests: {
          denominator: $denominator,
          overlap: $overlap,
          complement_ptrace: $complement_ptrace,
          complement_kvm: $complement_kvm,
          classified_complement: $classified
        },
        population: $population,
        ok: $population.ok
      }
    '
}

strict_invocation_eligible() {
  local row_strict=$1
  local artifacts_strict=$2
  local actual_rc=$3
  local expected_rc=$4
  local infrastructure_fault_delta=$5
  [[ "$row_strict" == yes && "$artifacts_strict" == yes &&
     "$actual_rc" == 0 && "$expected_rc" == 0 && "$actual_rc" == "$expected_rc" &&
     "$infrastructure_fault_delta" == 0 ]]
}

if [[ ${1:-} == --self-test-invocation-gate ]]; then
  [[ $# -eq 6 ]] || fail "--self-test-invocation-gate requires ROW_OK ARTIFACTS_OK ACTUAL_RC EXPECTED_RC INFRA_FAULT_DELTA"
  if strict_invocation_eligible "$2" "$3" "$4" "$5" "$6"; then
    exit 0
  fi
  exit 1
fi

if [[ ${1:-} == --self-test-payload-hash ]]; then
  [[ $# -eq 1 ]] || fail "--self-test-payload-hash takes no additional arguments"
  exit 0
fi

if [[ ${1:-} == --static-check ]]; then
  [[ $# -eq 1 ]] || fail "--static-check takes no additional arguments"
  static_check
  exit 0
fi
[[ $# -eq 0 ]] || fail "usage: $0 [--static-check]"

static_check_output=$(static_check) || exit $?
[[ ! -e "$campaign" ]] ||
  fail "evidence directory already exists; refusing any markerless or partial reuse: $campaign"

command -v with-proxy >/dev/null || fail "with-proxy is required for the locked fetch"
[[ -c /dev/kvm && -r /dev/kvm && -w /dev/kvm ]] ||
  fail "/dev/kvm is not a readable and writable character device"
preexisting_private_summaries=(
  "$root"/.hermit-verify-summary-*
  "$root"/.hermit-backend-engagement-summary-*
)
[[ ${#preexisting_private_summaries[@]} -eq 0 ]] ||
  fail "preexisting private Hermit summary residue would be unattributable"

mapfile -t all_tests < <(jq -r '.[].test' "$script_dir/expected-cells.json")
declare -A selector_for=()
while IFS=$'\t' read -r test_id selector; do
  selector_for["$test_id"]=$selector
done < <(jq -r '.[] | [.test,.selector] | @tsv' "$script_dir/expected-cells.json")
[[ ${#all_tests[@]} -eq 119 && ${#selector_for[@]} -eq 119 ]] ||
  fail "frozen population did not load as 119 unique tests"

qualification_started=$(date +%s)
mkdir -p "$campaign" "$cargo_home" "$out/target"
printf '%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$campaign/.qualification-started"
printf 'phase\texpected_cells_sha256\tpopulation_validator_sha256\tresults_validator_sha256\tevidence_validator_sha256\tstrict_artifact_validator_sha256\n' \
  >"$campaign/frozen-input-checks.tsv"
printf 'phase\texpected_sha256\tobserved_sha256\nstartup\t%s\t%s\n' \
  "$payload_expected_sha256" "$payload_startup_sha256" \
  >"$campaign/payload-input-checks.tsv"
printf '%s\n' "$static_check_output" >"$campaign/static-population-check.json"
{
  printf 'test\tselector\n'
  jq -r '.[] | [.test,.selector] | @tsv' "$script_dir/expected-cells.json"
} >"$campaign/population.tsv"

git -C "$root" status --porcelain=v1 --untracked-files=all --ignore-submodules=none \
  >"$campaign/source-status.before" 2>"$campaign/source-status.before.stderr"
source_status_before_rc=$?
printf '%s\n' "$source_status_before_rc" >"$campaign/source-status.before.rc"
[[ $source_status_before_rc -eq 0 ]] ||
  fail "cannot inspect initial source cleanliness; see $campaign/source-status.before.stderr"
[[ ! -s "$campaign/source-status.before" ]] ||
  fail "checkout has non-ignored changes; see $campaign/source-status.before"

machine=$(hostname -s)
kernel=$(uname -r)
{
  printf 'started_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'source_sha=%s\n' "$(git -C "$root" rev-parse HEAD)"
  printf 'source_tree=%s\n' "$(git -C "$root" rev-parse 'HEAD^{tree}')"
  printf 'source_status=clean\n'
  printf 'image_digest=%s\n' "$expected_image"
  printf 'machine_shortname=%s\n' "$machine"
  printf 'kernel_version=%s\n' "$kernel"
  printf 'kvm_type=%s\n' "$(stat -Lc '%F' /dev/kvm)"
  printf 'kvm_mode=%s\n' "$(stat -Lc '%a' /dev/kvm)"
  printf 'kvm_owner=%s\n' "$(stat -Lc '%U:%G' /dev/kvm)"
  printf 'cpuid_fault_cpu_count=%s\n' "$(awk '/^flags[[:space:]]*:/ && /(^|[[:space:]])cpuid_fault([[:space:]]|$)/ { count++ } END { print count + 0 }' /proc/cpuinfo)"
  printf 'backend=kvm\nmode=verify\nlog_level=info\nrelaxations=none\n'
  printf 'retained_log_max_bytes=%s\n' "$retained_log_max_bytes"
  printf 'cpu_timeout_multiplier=%s\n' "$cpu_timeout_multiplier"
  printf 'wall_timeout_multiplier=%s\n' "$wall_timeout_multiplier"
  printf 'round1_cell_count=119\nround1_probe_disabled=102\nround1_include_manual=17\n'
  printf 'continuation_rule=exactly-one-attempt-1-canonical-pass\n'
  printf 'deadline_prior_outer_invocations=120\n'
  printf 'deadline_prior_attempts=130\n'
  printf 'deadline_prior_elapsed_seconds=1169\n'
  printf 'deadline_max_outer_invocations=357\n'
  printf 'deadline_max_attempts=595\n'
  printf 'deadline_retry_aware_projection_seconds=5350\n'
  printf 'deadline_hard_attempt_exposure_seconds=33915\n'
  printf 'deadline_policy=operational-stop-for-investigation\n'
  printf 'invocation_backstop_seconds=600\n'
  printf 'invocation_retry_aware_modeled_seconds=248\n'
  printf 'invocation_backstop_policy=infrastructure-only\n'
  printf 'campaign_child_deadline_seconds=%s\n' "$campaign_child_deadline_seconds"
  printf 'payload_sha256=%s\n' "$payload_startup_sha256"
  printf 'harness_jobs=1\n'
  git -C "$root" submodule status --recursive
} >"$campaign/environment.log"

environment_started=$(date +%s)
"$root/ci/hermetic/run-in-pinned-root.sh" \
  --src "$root" --out "$out" -- \
  bash -c '
    set -euo pipefail
    /src/ci/hermetic/assert-no-network.sh
    printf "container_kernel=%s\n" "$(uname -r)"
    printf "container_kvm_type=%s\n" "$(stat -Lc %F /dev/kvm)"
    printf "container_kvm_mode=%s\n" "$(stat -Lc %a /dev/kvm)"
    [[ -c /dev/kvm && -r /dev/kvm && -w /dev/kvm ]]
    /lib64/ld-linux-x86-64.so.2 --version
  ' >"$campaign/pinned-environment.log" 2>&1
environment_rc=$?
environment_finished=$(date +%s)
printf 'pinned_environment_rc=%s\npinned_environment_seconds=%s\n' \
  "$environment_rc" "$((environment_finished - environment_started))" \
  >>"$campaign/environment.log"
[[ $environment_rc -eq 0 ]] || fail "pinned environment check failed"
grep -Eq '(^|[^0-9])2\.42([^0-9]|$)' "$campaign/pinned-environment.log" ||
  fail "pinned environment did not report glibc 2.42"

printf 'phase\trc\telapsed_seconds\n' >"$campaign/phase-status.tsv"
fetch_started=$(date +%s)
(
  set -euo pipefail
  "$root/ci/hermetic/assert-no-network.sh" --expect-network
  seed_cargo=/home/newton/.cargo
  for subdir in registry git/db; do
    if [[ -d "$seed_cargo/$subdir" && ! -e "$cargo_home/$subdir" ]]; then
      mkdir -p "$(dirname -- "$cargo_home/$subdir")"
      cp -a --reflink=auto "$seed_cargo/$subdir" "$cargo_home/$subdir" || true
    fi
  done
  cd "$root"
  with-proxy env CARGO_HOME="$cargo_home" cargo fetch --locked --manifest-path Cargo.toml
  with-proxy env CARGO_HOME="$cargo_home" cargo fetch --locked --manifest-path liteinst-runtime-build/Cargo.toml
) >"$campaign/fetch.log" 2>&1
fetch_rc=$?
fetch_finished=$(date +%s)
printf 'fetch\t%s\t%s\n' "$fetch_rc" "$((fetch_finished - fetch_started))" \
  >>"$campaign/phase-status.tsv"
[[ $fetch_rc -eq 0 ]] || fail "locked fetch failed; see $campaign/fetch.log"

build_started=$(date +%s)
"$root/ci/hermetic/run-in-pinned-root.sh" \
  --src "$root" --out "$out" --src-rw --cargo-home "$cargo_home" -- \
  bash -c '
    set -euo pipefail
    /src/ci/hermetic/assert-no-network.sh
    /src/ci/hermetic/assert-build-dependencies.sh
    export CI_DAG_BUILD_JOBS=8
    source /src/ci/configure-build-jobs.sh launcher
    cargo build -p hermit-manifest-plan --bins
    CARGO_BUILD_JOBS=8 cargo build --release --locked -p hermit --bin hermit
  ' >"$campaign/build.log" 2>&1
build_rc=$?
build_finished=$(date +%s)
printf 'build\t%s\t%s\n' "$build_rc" "$((build_finished - build_started))" \
  >>"$campaign/phase-status.tsv"
[[ $build_rc -eq 0 ]] || fail "lean exact-cell build failed; see $campaign/build.log"
[[ -x "$out/target/debug/test-harness" ]] || fail "test-harness was not built"
[[ -x "$out/target/debug/hermit-manifest-plan" ]] || fail "manifest planner was not built"
[[ -x "$out/target/release/hermit" ]] || fail "release Hermit was not built"

manifest_validate_started=$(date +%s)
"$root/ci/hermetic/run-in-pinned-root.sh" \
  --src "$root" --out "$out" --src-rw --cargo-home "$cargo_home" -- \
  bash -c '
    set -euo pipefail
    /src/ci/hermetic/assert-no-network.sh
    target/debug/test-harness validate
  ' >"$campaign/manifest-validate.log" 2>&1
manifest_validate_rc=$?
manifest_validate_finished=$(date +%s)
printf 'manifest_validate\t%s\t%s\n' \
  "$manifest_validate_rc" "$((manifest_validate_finished - manifest_validate_started))" \
  >>"$campaign/phase-status.tsv"
[[ $manifest_validate_rc -eq 0 ]] ||
  fail "live manifest/plan validation failed; see $campaign/manifest-validate.log"

mkdir -p "$campaign/preparation"
printf 'test\trc\telapsed_seconds\tlog\n' >"$campaign/preparation/invocations.tsv"
preparation_started=$(date +%s)
E2E_RESULT_ROOT="$campaign/preparation" \
E2E_BUILD_ROOT="$out/target/e2e-build" \
E2E_MACHINE_SHORTNAME="$machine" \
E2E_KERNEL_VERSION="$kernel" \
HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER="$cpu_timeout_multiplier" \
HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER="$wall_timeout_multiplier" \
  "$root/ci/hermetic/run-in-pinned-root.sh" \
    --src "$root" --out "$out" --src-rw --cargo-home "$cargo_home" \
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
        started=$(date +%s)
        timeout --kill-after=10s 600s \
          target/debug/test-harness build \
            --include-manual --include-occasional \
            --test "$test_id" --mode verify --backend ptrace --jobs 1 \
            >"/results/$slug.log" 2>&1
        rc=$?
        finished=$(date +%s)
        printf "%s\t%s\t%s\t%s\n" \
          "$test_id" "$rc" "$((finished - started))" "/results/$slug.log" \
          >>/results/invocations.tsv
        [[ $rc -eq 0 ]] || overall=1
      done
      exit "$overall"
    ' bash "${all_tests[@]}" >"$campaign/preparation-wrapper.log" 2>&1
preparation_rc=$?
preparation_finished=$(date +%s)
printf 'preparation\t%s\t%s\n' \
  "$preparation_rc" "$((preparation_finished - preparation_started))" \
  >>"$campaign/phase-status.tsv"
[[ $preparation_rc -eq 0 ]] || fail "fixture preparation failed; no KVM measurements were started"

mkdir -p "$campaign/results"
printf 'test\tselector\trepetition\trc\telapsed_seconds\tresult_rows\tstrict_single_pass\tleaked_summary_count\tresult_dir\n' \
  >"$campaign/results/invocations.tsv"
infrastructure_faults=0
invocation_count=0
invocation_strict=no
leaked_summary_count=0

assert_source_clean() {
  local phase=$1
  local status
  status=$(git -C "$root" status --porcelain=v1 --untracked-files=all --ignore-submodules=none) ||
    fail "cannot inspect source cleanliness before $phase"
  [[ -z "$status" ]] || fail "source became dirty before $phase"
  local leaked=(
    "$root"/.hermit-verify-summary-*
    "$root"/.hermit-backend-engagement-summary-*
  )
  [[ ${#leaked[@]} -eq 0 ]] ||
    fail "preexisting private Hermit summary residue before $phase"
  printf '%s\t%s\t%s\tclean\n' \
    "$phase" "$(git -C "$root" rev-parse HEAD)" "$(git -C "$root" rev-parse 'HEAD^{tree}')" \
    >>"$campaign/round-cleanliness.tsv"
}

preserve_leaked_summaries() {
  local result_dir=$1
  local leak_dir="$result_dir/leaked-private-summaries"
  local manifest="$result_dir/leaked-summaries.tsv"
  local source_path destination basename kind mtime size hash links
  local leaked=(
    "$root"/.hermit-verify-summary-*
    "$root"/.hermit-backend-engagement-summary-*
  )
  leaked_summary_count=0
  mkdir -p "$leak_dir"
  printf 'kind\tpath\tmtime_epoch_seconds\tsize_bytes\tsha256\n' >"$manifest"
  for source_path in "${leaked[@]}"; do
    if [[ ! -f "$source_path" || -L "$source_path" ]]; then
      infrastructure_faults=$((infrastructure_faults + 1))
      continue
    fi
    links=$(stat -Lc '%h' "$source_path")
    if [[ "$links" != 1 ]]; then
      infrastructure_faults=$((infrastructure_faults + 1))
      continue
    fi
    basename=${source_path##*/}
    case "$basename" in
      .hermit-verify-summary-*) kind=verify ;;
      .hermit-backend-engagement-summary-*) kind=backend-engagement ;;
      *)
        infrastructure_faults=$((infrastructure_faults + 1))
        continue
        ;;
    esac
    destination="$leak_dir/$basename"
    [[ ! -e "$destination" ]] || {
      infrastructure_faults=$((infrastructure_faults + 1))
      continue
    }
    mtime=$(stat -Lc '%Y' "$source_path")
    size=$(stat -Lc '%s' "$source_path")
    hash=$(file_sha256 "$source_path")
    mv -- "$source_path" "$destination"
    [[ $(file_sha256 "$destination") == "$hash" ]] ||
      infrastructure_faults=$((infrastructure_faults + 1))
    printf '%s\t%s\t%s\t%s\t%s\n' \
      "$kind" "${destination#"$campaign/"}" "$mtime" "$size" "$hash" >>"$manifest"
    leaked_summary_count=$((leaked_summary_count + 1))
  done
  local residue=(
    "$root"/.hermit-verify-summary-*
    "$root"/.hermit-backend-engagement-summary-*
  )
  [[ ${#residue[@]} -eq 0 ]] || infrastructure_faults=$((infrastructure_faults + 1))
}

run_invocation() {
  local test_id=$1
  local selector=$2
  local repetition=$3
  local slug result_dir run_id started finished rc result_rows expected_harness_rc
  local infrastructure_faults_before
  local row_strict=no artifacts_strict=no
  local preexisting_invocation_summaries=(
    "$root"/.hermit-verify-summary-*
    "$root"/.hermit-backend-engagement-summary-*
  )
  [[ ${#preexisting_invocation_summaries[@]} -eq 0 ]] ||
    fail "private Hermit summary residue exists before $test_id repetition $repetition"
  infrastructure_faults_before=$infrastructure_faults
  slug=${test_id//\//-}
  result_dir="$campaign/results/$slug/repetition-$repetition"
  run_id="$campaign_prefix-$slug-repetition-$repetition"
  mkdir -p "$result_dir"
  started=$(date +%s)
  E2E_RESULT_ROOT="$result_dir" \
  E2E_BUILD_ROOT="$out/target/e2e-build" \
  E2E_RUN_ID="$run_id" \
  E2E_RUN_INDEX="$repetition" \
  E2E_MACHINE_SHORTNAME="$machine" \
  E2E_KERNEL_VERSION="$kernel" \
  E2E_KEEP_VERIFY_LOGS=1 \
  HERMIT_LOG_MAX_BYTES="$retained_log_max_bytes" \
  HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER="$cpu_timeout_multiplier" \
  HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER="$wall_timeout_multiplier" \
    timeout --kill-after=10s 600s \
    "$root/ci/hermetic/run-in-pinned-root.sh" \
      --src "$root" --out "$out" --src-rw --cargo-home "$cargo_home" \
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
        exec env HERMIT_BIN="$PWD/target/release/hermit" \
          target/debug/test-harness run \
            "$selector" --include-occasional --prebuilt \
            --lane portable --test "$test_id" --mode verify --backend kvm \
            --results /results/results.jsonl \
            --junit /results/junit.xml \
            --jobs 1
      ' bash "$selector" "$test_id" >"$result_dir/invocation.log" 2>&1
  rc=$?
  finished=$(date +%s)
  preserve_leaked_summaries "$result_dir"
  result_rows=0
  invocation_strict=no
  if [[ -f "$result_dir/results.jsonl" ]] &&
     jq -s -e 'length > 0 and all(.[]; type == "object")' \
       "$result_dir/results.jsonl" >/dev/null 2>&1; then
    result_rows=$(jq -s 'length' "$result_dir/results.jsonl")
    if jq -s -e \
        --arg operation eligibility \
        --slurpfile expected "$script_dir/expected-cells.json" \
        --arg source_sha "$expected_sha" \
        --arg machine "$machine" \
        --arg kernel "$kernel" \
        --arg campaign_prefix "$campaign_prefix" \
        --arg test "$test_id" \
        --argjson run_index "$repetition" \
        -f "$script_dir/validate-results.jq" \
        "$result_dir/results.jsonl" >/dev/null; then
      row_strict=yes
      if bash "$script_dir/validate-strict-invocation-artifacts.sh" \
          "$result_dir" "$result_dir/results.jsonl" >/dev/null 2>&1; then
        artifacts_strict=yes
      else
        infrastructure_faults=$((infrastructure_faults + 1))
      fi
    fi
  else
    infrastructure_faults=$((infrastructure_faults + 1))
  fi
  expected_harness_rc=$(jq -sr '
    sort_by(.attempt) |
    if length > 0 and .[-1].outcome == "PASS" then "0" else "1" end
  ' "$result_dir/results.jsonl" 2>/dev/null || printf INVALID)
  if [[ "$rc" != "$expected_harness_rc" ]]; then
    infrastructure_faults=$((infrastructure_faults + 1))
  fi
  if strict_invocation_eligible \
      "$row_strict" "$artifacts_strict" "$rc" "$expected_harness_rc" \
      "$((infrastructure_faults - infrastructure_faults_before))"; then
    invocation_strict=yes
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$test_id" "$selector" "$repetition" "$rc" "$((finished - started))" \
    "$result_rows" "$invocation_strict" "$leaked_summary_count" "$result_dir" \
    >>"$campaign/results/invocations.tsv"
  invocation_count=$((invocation_count + 1))
}

round1_eligible_tests=()
printf 'phase\tsource_sha\tsource_tree\tstatus\n' >"$campaign/round-cleanliness.tsv"
verify_frozen_inputs round-1
assert_source_clean round-1
for test_id in "${all_tests[@]}"; do
  run_invocation "$test_id" "${selector_for[$test_id]}" 1
  if [[ "$invocation_strict" == yes ]]; then
    round1_eligible_tests+=("$test_id")
  fi
done

{
  printf 'test\tselector\n'
  for test_id in "${round1_eligible_tests[@]}"; do
    printf '%s\t%s\n' "$test_id" "${selector_for[$test_id]}"
  done
} >"$campaign/round1-eligible.tsv"

# Every strict round-1 entrant gets both later rounds. Round 3 is not skipped
# when round 2 fails, retries, or times out.
verify_frozen_inputs round-2
assert_source_clean round-2
for test_id in "${round1_eligible_tests[@]}"; do
  run_invocation "$test_id" "${selector_for[$test_id]}" 2
done
verify_frozen_inputs round-3
assert_source_clean round-3
for test_id in "${round1_eligible_tests[@]}"; do
  run_invocation "$test_id" "${selector_for[$test_id]}" 3
done

: >"$campaign/all-results.jsonl"
printf 'test\trun_index\tattempt\tresults_jsonl\tresults_sha256\tartifact_dir\tartifact_state\tverification_report\tverification_report_sha256\trun1_log\trun2_log\n' \
  >"$campaign/artifacts.tsv"
artifact_extract=$(mktemp "$campaign/.artifact-extract.XXXXXX")
artifact_paths=$(mktemp "$campaign/.artifact-paths.XXXXXX")
artifact_paths_sorted=$(mktemp "$campaign/.artifact-paths-sorted.XXXXXX")
artifact_special_nodes=$(mktemp "$campaign/.artifact-special-nodes.XXXXXX")
trap 'rm -f -- "$artifact_extract" "$artifact_paths" "$artifact_paths_sorted" "$artifact_special_nodes"' EXIT
while IFS=$'\t' read -r test_id selector repetition rc _elapsed result_rows _strict_single_pass _leaked_count result_dir; do
  [[ "$test_id" == test ]] && continue
  result_file="$result_dir/results.jsonl"
  if [[ ! -f "$result_file" ]]; then
    infrastructure_faults=$((infrastructure_faults + 1))
    continue
  fi
  cat -- "$result_file" >>"$campaign/all-results.jsonl"
  results_hash=$(file_sha256 "$result_file")
  if ! jq -r '[
      .test,
      (.run_index|tostring),
      (.attempt|tostring),
      .artifact_dir,
      (.attempts[0].verification_report_sha256 // "-"),
      (if (.outcome == "ERROR" and .result == "timeout" and
           .attempts[0].timed_out == true and
           .attempts[0].status == null and .attempts[0].signal == null and
           ((.attempts[0].verification_report | fromjson).verdict == "no_result") and
           ((.attempts[0].verification_report | fromjson).no_result_reason == {"kind":"not_run"}))
       then "pre-attempt-not-run"
       elif (.attempts[0].verification_report == null and
             .attempts[0].verification_report_sha256 == null)
       then "executed-no-report"
       else "executed-report"
       end),
      .result,
      (if .attempts[0].verification_report == null
       then "NONE"
       else (try (.attempts[0].verification_report | fromjson | .verdict) catch "INVALID")
       end),
      (if .attempts[0].verification_report == null
       then "NONE"
       else (try ((.attempts[0].verification_report | fromjson |
         .no_result_reason.kind) // "NONE") catch "INVALID")
       end)
    ] | @tsv' \
      "$result_file" >"$artifact_extract"; then
    infrastructure_faults=$((infrastructure_faults + 1))
    continue
  fi
  while IFS=$'\t' read -r row_test row_run_index attempt artifact_container report_hash artifact_state row_result report_verdict report_no_result_kind; do
    if [[ "$artifact_container" == /results/runs/* ]]; then
      artifact_dir="$result_dir/${artifact_container#/results/}"
    else
      artifact_dir="$result_dir/INVALID_ARTIFACT_DIR"
      infrastructure_faults=$((infrastructure_faults + 1))
    fi
    log_dir="$artifact_dir/verify-logs/verify-1"
    if [[ ! -d "$artifact_dir" || -L "$artifact_dir" ]]; then
      infrastructure_faults=$((infrastructure_faults + 1))
    fi
    case "$artifact_state" in
      pre-attempt-not-run)
        report_file=-
        run1_log=-
        run2_log=-
        if [[ -e "$artifact_dir/verify-1.json" || ! -d "$log_dir" || -L "$log_dir" ]]; then
          infrastructure_faults=$((infrastructure_faults + 1))
        elif [[ -n $(find "$log_dir" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
          infrastructure_faults=$((infrastructure_faults + 1))
        fi
        ;;
      executed-report)
        report_file="$artifact_dir/verify-1.json"
        if [[ "$row_result" == timeout && "$report_verdict" == no_result ]]; then
          run1_log=-
          run2_log=-
          if [[ ! -d "$log_dir" || -L "$log_dir" ]]; then
            infrastructure_faults=$((infrastructure_faults + 1))
          else
              shopt -s nullglob dotglob
              log_entries=("$log_dir"/*)
              run1_candidates=("$log_dir"/run1_log_*)
              run2_candidates=("$log_dir"/run2_log_*)
              shopt -u nullglob dotglob
              [[ ${#run1_candidates[@]} -le 1 && ${#run2_candidates[@]} -le 1 &&
                 ${#log_entries[@]} -eq $((${#run1_candidates[@]} + ${#run2_candidates[@]})) ]] ||
                infrastructure_faults=$((infrastructure_faults + 1))
              [[ ${#run1_candidates[@]} -eq 0 ]] || run1_log=${run1_candidates[0]}
              [[ ${#run2_candidates[@]} -eq 0 ]] || run2_log=${run2_candidates[0]}
              if [[ "$run1_log" == - && "$run2_log" != - ]]; then
                infrastructure_faults=$((infrastructure_faults + 1))
              fi
              if [[ "$report_no_result_kind" == first_run_rejected &&
                    "$run1_log" == - ]]; then
                infrastructure_faults=$((infrastructure_faults + 1))
              fi
          fi
        else
          if [[ ! -d "$log_dir" || -L "$log_dir" ]]; then
            run1_log="$log_dir/run1_log_MISSING_OR_MULTIPLE"
            run2_log="$log_dir/run2_log_MISSING_OR_MULTIPLE"
            infrastructure_faults=$((infrastructure_faults + 1))
          else
            shopt -s nullglob dotglob
            log_entries=("$log_dir"/*)
            run1_candidates=("$log_dir"/run1_log_*)
            run2_candidates=("$log_dir"/run2_log_*)
            shopt -u nullglob dotglob
            if [[ ${#run1_candidates[@]} -eq 1 ]]; then
              run1_log=${run1_candidates[0]}
            else
              run1_log="$log_dir/run1_log_MISSING_OR_MULTIPLE"
              infrastructure_faults=$((infrastructure_faults + 1))
            fi
            if [[ ${#run2_candidates[@]} -eq 1 ]]; then
              run2_log=${run2_candidates[0]}
            elif [[ ${#run2_candidates[@]} -eq 0 ]]; then
              run2_log=-
            else
              run2_log="$log_dir/run2_log_MISSING_OR_MULTIPLE"
              infrastructure_faults=$((infrastructure_faults + 1))
            fi
            case "$report_verdict" in
              no_result)
                [[ "$run2_log" == - && ${#log_entries[@]} -eq 1 ]] ||
                  infrastructure_faults=$((infrastructure_faults + 1))
                ;;
              matched | diverged)
                [[ "$run2_log" != - && ${#log_entries[@]} -eq 2 ]] ||
                  infrastructure_faults=$((infrastructure_faults + 1))
                ;;
              *) infrastructure_faults=$((infrastructure_faults + 1)) ;;
            esac
          fi
        fi
        ;;
      executed-no-report)
        report_file=-
        run1_log=-
        run2_log=-
        if [[ -e "$artifact_dir/verify-1.json" || ! -d "$log_dir" || -L "$log_dir" ]]; then
          infrastructure_faults=$((infrastructure_faults + 1))
        elif [[ -n $(find "$log_dir" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
          infrastructure_faults=$((infrastructure_faults + 1))
        fi
        ;;
      *)
        report_file=-
        run1_log=-
        run2_log=-
        infrastructure_faults=$((infrastructure_faults + 1))
        ;;
    esac
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$row_test" "$row_run_index" "$attempt" "$result_file" "$results_hash" \
      "$artifact_container" "$artifact_state" "$report_file" "$report_hash" \
      "$run1_log" "$run2_log" \
      >>"$campaign/artifacts.tsv"
  done <"$artifact_extract"
done <"$campaign/results/invocations.tsv"

printf 'sha256\tpath\n' >"$campaign/artifact-hashes.tsv"
if ! find "$campaign/results" ! -type d ! -type f -print \
    >"$artifact_special_nodes" 2>"$campaign/artifact-hashes.stderr"; then
  infrastructure_faults=$((infrastructure_faults + 1))
elif [[ -s "$artifact_special_nodes" ]]; then
  printf 'retained results contain symlinks or special nodes:\n' \
    >>"$campaign/artifact-hashes.stderr"
  sed -n '1,20p' "$artifact_special_nodes" >>"$campaign/artifact-hashes.stderr"
  infrastructure_faults=$((infrastructure_faults + 1))
fi
if ! find "$campaign/results" -type f -print0 \
    >"$artifact_paths" 2>>"$campaign/artifact-hashes.stderr"; then
  infrastructure_faults=$((infrastructure_faults + 1))
fi
if ! LC_ALL=C sort -z "$artifact_paths" >"$artifact_paths_sorted" \
    2>>"$campaign/artifact-hashes.stderr"; then
  infrastructure_faults=$((infrastructure_faults + 1))
fi
while IFS= read -r -d '' path; do
  relative=${path#"$campaign/"}
  if [[ "$relative" == *$'\t'* || "$relative" == *$'\n'* ]]; then
    infrastructure_faults=$((infrastructure_faults + 1))
    continue
  fi
  if hash_line=$(sha256sum -- "$path" 2>>"$campaign/artifact-hashes.stderr"); then
    hash=${hash_line%% *}
  else
    hash=INVALID
    infrastructure_faults=$((infrastructure_faults + 1))
  fi
  if [[ ! "$hash" =~ ^[0-9a-f]{64}$ ]]; then
    infrastructure_faults=$((infrastructure_faults + 1))
  fi
  printf '%s\t%s\n' "$hash" "$relative" >>"$campaign/artifact-hashes.tsv"
done <"$artifact_paths_sorted"

verify_frozen_inputs final

git -C "$root" status --porcelain=v1 --untracked-files=all --ignore-submodules=none \
  >"$campaign/source-status.after" 2>"$campaign/source-status.after.stderr"
source_status_after_rc=$?
printf '%s\n' "$source_status_after_rc" >"$campaign/source-status.after.rc"
if [[ $source_status_after_rc -ne 0 || -s "$campaign/source-status.after" ]]; then
  infrastructure_faults=$((infrastructure_faults + 1))
fi

round1_eligible=${#round1_eligible_tests[@]}
continuation_invocations=$((round1_eligible * 2))
expected_invocations=$((119 + continuation_invocations))
recorded_invocations=$(awk 'END { print (NR > 0 ? NR - 1 : 0) }' "$campaign/results/invocations.tsv")
[[ $recorded_invocations -eq $expected_invocations ]] ||
  infrastructure_faults=$((infrastructure_faults + 1))
if [[ $infrastructure_faults -eq 0 ]]; then
  evidence_complete=true
else
  evidence_complete=false
fi
{
  printf 'round1_invocations=119\n'
  printf 'round1_eligible=%s\n' "$round1_eligible"
  printf 'continuation_invocations=%s\n' "$continuation_invocations"
  printf 'expected_invocations=%s\n' "$expected_invocations"
  printf 'recorded_invocations=%s\n' "$recorded_invocations"
  printf 'driver_infrastructure_faults=%s\n' "$infrastructure_faults"
  printf 'evidence_complete=%s\n' "$evidence_complete"
} >"$campaign/execution-complete.txt"

validator_started=$(date +%s)
KVM_QUALIFICATION_CAMPAIGN="$campaign" bash "$script_dir/validate-evidence.sh" \
  >"$campaign/strict-validation.json" 2>"$campaign/strict-validation.stderr"
validator_rc=$?
validator_finished=$(date +%s)
qualified_cell_count=INVALID
if [[ -f "$campaign/strict-validation.json" ]]; then
  qualified_cell_count=$(jq -r '.qualified_cell_count // "INVALID"' "$campaign/strict-validation.json" 2>/dev/null || printf INVALID)
fi
campaign_finished=$(date +%s)
if [[ $validator_rc -eq 0 ]]; then
  campaign_evidence_status=complete
else
  campaign_evidence_status=invalid
fi
{
  printf 'finished_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'elapsed_seconds=%s\n' "$((campaign_finished - qualification_started))"
  printf 'validator_seconds=%s\n' "$((validator_finished - validator_started))"
  printf 'campaign_evidence_status=%s\n' "$campaign_evidence_status"
  printf 'validator_rc=%s\n' "$validator_rc"
  printf 'round1_eligible=%s\n' "$round1_eligible"
  printf 'qualified_cell_count=%s\n' "$qualified_cell_count"
  printf 'qualification_rule=three-of-three-first-attempt-canonical-pass-zero-retry\n'
  printf 'product_failures_do_not_invalidate_complete_evidence=true\n'
} >"$campaign/completion.txt"

exit "$validator_rc"

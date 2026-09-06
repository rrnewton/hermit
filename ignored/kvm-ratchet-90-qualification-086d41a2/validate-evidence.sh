#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
shopt -s nullglob

script_dir_source=${KVM_RATCHET_CAMPAIGN_DIR:-$(dirname -- "${BASH_SOURCE[0]}")}
script_dir=$(cd -- "$script_dir_source" && pwd -P)
unset script_dir_source
here=${KVM_QUALIFICATION_CAMPAIGN:-"$script_dir/evidence"}
rows=${1:-"$here/all-results.jsonl"}
environment=${2:-"$here/environment.log"}
expected=${3:-${KVM_RATCHET_EXPECTED_CELLS_PATH:-"$script_dir/expected-cells.json"}}
population_validator=${KVM_RATCHET_POPULATION_VALIDATOR_PATH:-"$script_dir/validate-population.jq"}
results_validator=${KVM_RATCHET_RESULTS_VALIDATOR_PATH:-"$script_dir/validate-results.jq"}
evidence_validator=${KVM_RATCHET_EVIDENCE_VALIDATOR_PATH:-"$script_dir/validate-evidence.sh"}
strict_artifact_validator=${KVM_RATCHET_STRICT_ARTIFACT_VALIDATOR_PATH:-"$script_dir/validate-strict-invocation-artifacts.sh"}
payload_path=${KVM_RATCHET_PAYLOAD_PATH:-"$script_dir/run-kvm-ratchet-90.sh"}
pinned_environment="$here/pinned-environment.log"
built_binary="$here/split/target/release/hermit"
built_binary_ledger="$here/built-binary.tsv"
source_root=$(git -C "$script_dir" rev-parse --show-toplevel)

readonly source_sha=086d41a2f2f76e5a2cccceea342feb6957311c2b
readonly source_tree=94490fd2cee758395150590a8ba25ff86443dbfd
readonly machine=devbig014
readonly campaign_prefix=kvm-ratchet-90-086d41a2-run6
readonly image=localhost/hermit-hermetic-validate@sha256:e38c3b2d5cd8a17ed2f99b4a24dd76c9ee63329cdc8c9723e4650ab085fae985
readonly cells_sha256=84c5dfa8aebe71b65396134e09986d9e88bf8dbabc18b7fd2e08e28c73b4aa71
readonly plan_sha256=bc48a92f8d3981619bb9211eee12c42b5cfd2af202080293556c0f57c5824e3f
readonly image_file_sha256=761e73803bbea4333b9c316234a9d9f86ac9982ea111737293c7be59cfb0ee93
readonly submodule_status_sha256=134f1b5725890ecccc3dd6764cd49bc3b0f75aa87db31f687a9c2b3e821d1428
readonly expected_cells_sha256=ce945b0e134dc6ca0b7089d3c439d00672d322a94f5efc8730bf18cb59fa4e72
readonly population_validator_sha256=e9769e1f46b04118d242fda5cd6f66f893065cd3163b410c90877ac43b0cd484
readonly results_validator_sha256=2f06da4497ea1dfd19c144b39b54a5db54f912b6d3f871f21e27fa781635753c
readonly strict_artifact_validator_sha256=b1d3a0ab1e686160f7da617f871a68bb93780b6463e0c57ec105350be43a465a
readonly denominator_sha256=dabeb9601aeee07c40dde62763eecc97c5184be70c0ff3fe3a90d3deddfdf723
readonly overlap_sha256=ba32abd030a58b52f4cd73cf4b6dd327c7437ab1c1324f37a37e8cbce29b8727
readonly complement_ptrace_sha256=5e90830fec51df5d99c072e0a121ea997634ef2a230a9b79833e667fe9bc1f64
readonly complement_kvm_sha256=57e02a11097863186b0055a06ecd9ebb9888e024d644ec9b7b855f66590454ab
readonly classified_complement_sha256=85d180b33c937b3fcc533d6a98f7ef58a9ad76afd8fe5c09bbeb46218deb91aa
payload_hash_line=$(sha256sum -- "$payload_path") || exit 1
readonly payload_sha256=${payload_hash_line%% *}
freeze_manifest_hash_line=$(sha256sum -- "${KVM_RATCHET_FREEZE_MANIFEST_PATH:-$script_dir/FROZEN_SHA256SUMS}") || exit 1
readonly freeze_manifest_sha256=${KVM_RATCHET_FREEZE_MANIFEST_SHA256:-${freeze_manifest_hash_line%% *}}

scratch=$(mktemp -d /tmp/kvm-ratchet-90-validator.XXXXXX)
trap 'rm -rf -- "$scratch"' EXIT

environment_failures=()
evidence_failures=()

record_environment_failure() {
  environment_failures+=("$1")
}

record_evidence_failure() {
  evidence_failures+=("$1")
}

file_sha256() {
  local line
  line=$(sha256sum -- "$1")
  printf '%s' "${line%% *}"
}

check_file_hash() {
  local path=$1
  local expected_hash=$2
  local actual
  if [[ ! -f "$path" ]]; then
    record_evidence_failure "missing frozen input: $path"
    return
  fi
  actual=$(file_sha256 "$path")
  if [[ "$actual" != "$expected_hash" ]]; then
    record_evidence_failure "$path changed: expected sha256 $expected_hash, got $actual"
  fi
}

missing_files=()
for path in \
  "$rows" \
  "$environment" \
  "$pinned_environment" \
  "$expected" \
  "$population_validator" \
  "$results_validator" \
  "$strict_artifact_validator" \
  "$here/static-population-check.json" \
  "$built_binary" \
  "$built_binary_ledger" \
  "$here/source-status.before" \
  "$here/source-status.before.rc" \
  "$here/source-status.before.stderr" \
  "$here/source-status.after" \
  "$here/source-status.after.rc" \
  "$here/source-status.after.stderr" \
  "$here/execution-complete.txt" \
  "$here/phase-status.tsv" \
  "$here/frozen-input-checks.tsv" \
  "$here/payload-input-checks.tsv" \
  "$here/round-cleanliness.tsv" \
  "$here/population.tsv" \
  "$here/round1-eligible.tsv" \
  "$here/artifacts.tsv" \
  "$here/artifact-hashes.tsv" \
  "$here/artifact-hashes.stderr" \
  "$here/artifact-inventory-wrapper.log" \
  "$here/strict-validation-wrapper.log" \
  "$here/resource-guard.tsv" \
  "$here/disk-budget.tsv" \
  "$here/results/invocations.tsv"; do
  [[ -f "$path" ]] || missing_files+=("$path")
done
if [[ ${#missing_files[@]} -ne 0 ]]; then
  jq -n --arg missing "${missing_files[*]}" \
    '{schema:2,ok:false,failure_reasons:["missing evidence file(s): " + $missing]}'
  exit 1
fi

built_binary_ok=true
built_binary_sha256=''
if [[ ! -f "$built_binary" || -L "$built_binary" || ! -x "$built_binary" ||
      $(stat -Lc '%h' "$built_binary" 2>/dev/null || printf 0) != 1 ]]; then
  built_binary_ok=false
  record_evidence_failure "retained release Hermit is not one executable unaliased regular file"
elif ! built_binary_sha256=$(file_sha256 "$built_binary") ||
     [[ ! "$built_binary_sha256" =~ ^[0-9a-f]{64}$ ]]; then
  built_binary_ok=false
  record_evidence_failure "retained release Hermit cannot be hashed"
fi
if [[ $(wc -l <"$built_binary_ledger") -ne 2 ]] ||
   [[ $(sed -n '1p' "$built_binary_ledger") != $'sha256\tpath' ]] ||
   ! IFS=$'\t' read -r ledger_binary_sha256 ledger_binary_path ledger_binary_extra \
     < <(sed -n '2p' "$built_binary_ledger") ||
   [[ -n ${ledger_binary_extra:-} ||
      ! ${ledger_binary_sha256:-} =~ ^[0-9a-f]{64}$ ||
      ${ledger_binary_path:-} != split/target/release/hermit ||
      ${ledger_binary_sha256:-} != "$built_binary_sha256" ]]; then
  built_binary_ok=false
  record_evidence_failure "built-binary.tsv does not exactly bind the retained release Hermit"
fi

check_file_hash "$expected" "$expected_cells_sha256"
check_file_hash "$population_validator" "$population_validator_sha256"
check_file_hash "$results_validator" "$results_validator_sha256"
check_file_hash "$strict_artifact_validator" "$strict_artifact_validator_sha256"
check_file_hash "$source_root/ci/compat-envelope/cells.json" "$cells_sha256"
check_file_hash "$source_root/ci/expected-e2e-plan.json" "$plan_sha256"
check_file_hash "$source_root/ci/hermetic/image.digest" "$image_file_sha256"

required_environment_lines=(
  "source_sha=$source_sha"
  "source_tree=$source_tree"
  "source_status=clean"
  "image_digest=$image"
  "machine_shortname=$machine"
  "kvm_type=character special file"
  "kvm_mode=666"
  "pinned_environment_rc=0"
  "backend=kvm"
  "mode=verify"
  "log_level=info"
  "relaxations=none"
  "retained_log_max_bytes=1073741824"
  "invocation_log_max_bytes=1073741824"
  "capture_file_max_bytes=1073741824"
  "service_log_max_bytes=1073741824"
  "process_file_limit_bytes=1073742000"
  "service_log_path=/home/newton/work/dev-hermit/ignored/validate/hermit-kvm-ratchet-90-086d41a2-run6.log"
  "host_rlimit_fsize_soft_bytes=1073742000"
  "host_rlimit_fsize_hard_bytes=1073742000"
  "campaign_budget_bytes=137438953472"
  "filesystem_reserve_bytes=137438953472"
  "prior_campaign_allocated_bytes=4688396288"
  "prior_campaign_results_allocated_bytes=93057024"
  "retained_log_truncation_marker_bytes=176"
  "retained_log_single_max_with_marker_bytes=1073742000"
  "retained_log_theoretical_count=1190"
  "retained_log_theoretical_ceiling_bytes=1277752980000"
  "campaign_budget_policy=early-global-invalid-stop-not-full-theoretical-retention"
  "resource_guard_policy=nominal-one-second-polling-process-group-stop-preserve-prefix-invalidate-campaign"
  "resource_guard_scan_latency=synchronous-filesystem-scan-not-a-one-second-latency-guarantee"
  "cpu_timeout_multiplier=1"
  "wall_timeout_multiplier=1"
  "round1_cell_count=119"
  "round1_probe_disabled=102"
  "round1_include_manual=17"
  "continuation_rule=exactly-one-attempt-1-canonical-pass"
  "deadline_prior_outer_invocations=120"
  "deadline_prior_attempts=130"
  "deadline_prior_elapsed_seconds=1169"
  "deadline_max_outer_invocations=357"
  "deadline_max_attempts=595"
  "deadline_retry_aware_projection_seconds=5350"
  "deadline_hard_attempt_exposure_seconds=33915"
  "deadline_policy=operational-stop-for-investigation"
  "invocation_backstop_seconds=600"
  "invocation_retry_aware_modeled_seconds=248"
  "invocation_backstop_policy=infrastructure-only"
  "campaign_child_deadline_seconds=7200"
  "payload_sha256=$payload_sha256"
  "freeze_manifest_sha256=$freeze_manifest_sha256"
  "runtime_input_source=sealed-memfd"
  "reviewed_input_trust=external-launch-and-freeze-digests-plus-argv-capsule-and-post-boundary-sealed-runtime-memfds"
  "external_measurement_trust=with-proxy,systemd,python-stdlib,validate-lock,bash,jq,git,pinned-checkout-and-toolchain"
  "harness_jobs=1"
)
for line in "${required_environment_lines[@]}"; do
  if ! grep -Fqx -- "$line" "$environment"; then
    record_environment_failure "missing environment line: $line"
  fi
done
for key in initial_campaign_allocated_bytes initial_filesystem_free_bytes; do
  count=$(grep -c "^${key}=" "$environment" || true)
  value=$(sed -n "s/^${key}=//p" "$environment")
  if [[ $count -ne 1 || ! "$value" =~ ^[0-9]+$ ]]; then
    record_environment_failure "environment.log has no unique numeric $key"
  fi
done
environment_initial_allocated_bytes=$(sed -n 's/^initial_campaign_allocated_bytes=//p' "$environment")
environment_initial_free_bytes=$(sed -n 's/^initial_filesystem_free_bytes=//p' "$environment")

resource_guard_ok=true
if ! awk -F '\t' '
  NR == 1 {
    good = ($0 == "timestamp_epoch_seconds\tscope\treason\tobserved_bytes\tlimit_bytes\tpath")
    next
  }
  { rows++ }
  END { exit (good && rows == 0) ? 0 : 1 }
' "$here/resource-guard.tsv"; then
  resource_guard_ok=false
  record_evidence_failure "resource guard recorded a cap/reserve/probe trip"
fi

disk_budget_ok=true
if ! awk -F '\t' \
    -v budget=137438953472 -v reserve=137438953472 -v service_cap=1073741824 \
    -v initial_allocated="$environment_initial_allocated_bytes" \
    -v initial_free="$environment_initial_free_bytes" '
  NR == 1 {
    good = ($0 == "phase\tallocated_bytes\tlogical_bytes\tfilesystem_free_bytes\tservice_log_bytes\tcampaign_budget_bytes\tfilesystem_reserve_bytes\tservice_log_max_bytes\tguard_hits\tstatus")
    next
  }
  {
    rows++
    seen[$1]++
    if (($1 != "initial" && $1 != "final") ||
        $2 !~ /^[0-9]+$/ || $3 !~ /^[0-9]+$/ || $4 !~ /^[0-9]+$/ ||
        $5 !~ /^[0-9]+$/ || $6 != budget || $7 != reserve ||
        $8 != service_cap || $9 != 0 || $10 != "clear" ||
        $2 > budget || $3 > budget || $4 < reserve || $5 >= service_cap) good = 0
    if ($1 == "initial" && ($2 != initial_allocated || $4 != initial_free)) good = 0
  }
  END {
    if (rows != 2 || length(seen) != 2 || seen["initial"] != 1 || seen["final"] != 1) good = 0
    exit good ? 0 : 1
  }
' "$here/disk-budget.tsv"; then
  disk_budget_ok=false
  record_evidence_failure "disk-budget.tsv does not prove both 128 GiB bounds and a bounded service log"
fi

kernel_lines=$(grep -c '^kernel_version=' "$environment" || true)
kernel=$(sed -n 's/^kernel_version=//p' "$environment")
if [[ $kernel_lines -ne 1 || -z "$kernel" || "$kernel" == *$'\n'* ]]; then
  record_environment_failure "environment.log must contain exactly one nonempty kernel_version"
  kernel=INVALID
fi

if ! grep -Eq \
  '^(ldd \(GNU libc\) 2\.42|ld\.so \(GNU libc\) stable release version 2\.42\.)$' \
  "$pinned_environment"; then
  record_environment_failure "the pinned environment did not report exactly glibc 2.42"
fi
if ! grep -Fqx -- 'container_kvm_type=character special file' "$pinned_environment"; then
  record_environment_failure "the pinned environment did not expose /dev/kvm as a character device"
fi
if ! grep -Fqx -- 'container_kvm_mode=666' "$pinned_environment"; then
  record_environment_failure "the pinned environment did not expose read-write /dev/kvm"
fi
if ! grep -Fqx -- 'container_rlimit_fsize_soft_bytes=1073742000' "$pinned_environment" ||
   ! grep -Fqx -- 'container_rlimit_fsize_hard_bytes=1073742000' "$pinned_environment"; then
  record_environment_failure "the pinned environment did not inherit the exact 1 GiB-plus-marker RLIMIT_FSIZE"
fi
if [[ -s "$here/source-status.before" || -s "$here/source-status.before.stderr" ||
      $(cat "$here/source-status.before.rc") != 0 ||
      -s "$here/source-status.after" || -s "$here/source-status.after.stderr" ||
      $(cat "$here/source-status.after.rc") != 0 ]]; then
  record_environment_failure "the source checkout was not clean for the complete campaign"
fi
if [[ -s "$here/artifact-hashes.stderr" ]]; then
  record_evidence_failure "artifact inventory construction reported traversal or hashing errors"
fi
private_summary_residue=(
  "$source_root"/.hermit-verify-summary-*
  "$source_root"/.hermit-backend-engagement-summary-*
)
if [[ ${#private_summary_residue[@]} -ne 0 ]]; then
  record_environment_failure "unpreserved private Hermit summary residue remains in the source root"
fi

if source_image=$(git -C "$source_root" show "$source_sha:ci/hermetic/image.digest" 2>/dev/null); then
  source_image=${source_image//$'\n'/}
  [[ "$source_image" == "$image" ]] ||
    record_environment_failure "the source commit does not pin the expected image"
else
  record_environment_failure "the measured source commit is unavailable"
fi
observed_tree=$(git -C "$source_root" rev-parse "$source_sha^{tree}" 2>/dev/null || true)
[[ "$observed_tree" == "$source_tree" ]] ||
  record_environment_failure "the source commit does not have tree $source_tree"
observed_head=$(git -C "$source_root" rev-parse 'HEAD^{commit}' 2>/dev/null || true)
observed_head_tree=$(git -C "$source_root" rev-parse 'HEAD^{tree}' 2>/dev/null || true)
[[ "$observed_head" == "$source_sha" && "$observed_head_tree" == "$source_tree" ]] ||
  record_environment_failure "the validation checkout moved away from the exact source head/tree"
observed_submodule_hash=$(git -C "$source_root" submodule status --recursive | sha256sum | awk '{print $1}')
[[ "$observed_submodule_hash" == "$submodule_status_sha256" ]] ||
  record_environment_failure "submodule status is not the frozen recursive state"

population_summary_file="$scratch/population-summary.json"
if jq --arg operation summary \
    --slurpfile expected "$expected" \
    --slurpfile plan "$source_root/ci/expected-e2e-plan.json" \
    -f "$population_validator" \
    "$source_root/ci/compat-envelope/cells.json" >"$population_summary_file" &&
   jq -e '.ok == true' "$population_summary_file" >/dev/null; then
  population_ok=true
else
  population_ok=false
  record_evidence_failure "the source population no longer satisfies the frozen 340/221/119 and 102/13/4 contract"
fi

population_digest() {
  local operation=$1
  jq -cS --arg operation "$operation" \
    --slurpfile expected "$expected" \
    --slurpfile plan "$source_root/ci/expected-e2e-plan.json" \
    -f "$population_validator" \
    "$source_root/ci/compat-envelope/cells.json" |
    LC_ALL=C sort |
    sha256sum |
    awk '{print $1}'
}

digest_checks_ok=true
for spec in \
  "denominator:$denominator_sha256" \
  "overlap:$overlap_sha256" \
  "complement-ptrace:$complement_ptrace_sha256" \
  "complement-kvm:$complement_kvm_sha256" \
  "classified-complement:$classified_complement_sha256" \
  "frozen:$classified_complement_sha256"; do
  operation=${spec%%:*}
  expected_digest=${spec#*:}
  actual_digest=$(population_digest "$operation")
  if [[ "$actual_digest" != "$expected_digest" ]]; then
    digest_checks_ok=false
    record_evidence_failure "$operation digest changed: expected $expected_digest, got $actual_digest"
  fi
done

if ! jq -e \
  --arg source_sha "$source_sha" \
  --arg source_tree "$source_tree" \
  --arg cells_sha256 "$cells_sha256" \
  --arg plan_sha256 "$plan_sha256" \
  --arg image_file_sha256 "$image_file_sha256" \
  --arg submodule_status_sha256 "$submodule_status_sha256" \
  --arg expected_cells_sha256 "$expected_cells_sha256" \
  --arg population_validator_sha256 "$population_validator_sha256" \
  --arg results_validator_sha256 "$results_validator_sha256" \
  --arg evidence_validator_sha256 "$(file_sha256 "$evidence_validator")" \
  --arg strict_artifact_validator_sha256 "$strict_artifact_validator_sha256" \
  --arg denominator_sha256 "$denominator_sha256" \
  --arg overlap_sha256 "$overlap_sha256" \
  --arg complement_ptrace_sha256 "$complement_ptrace_sha256" \
  --arg complement_kvm_sha256 "$complement_kvm_sha256" \
  --arg classified_complement_sha256 "$classified_complement_sha256" '
    .ok == true and
    .source_sha == $source_sha and
    .source_tree == $source_tree and
    .source_clean == true and
    .hashes.cells == $cells_sha256 and
    .hashes.expected_plan == $plan_sha256 and
    .hashes.image_file == $image_file_sha256 and
    .hashes.submodule_status == $submodule_status_sha256 and
    .hashes.expected_cells == $expected_cells_sha256 and
    .hashes.population_validator == $population_validator_sha256 and
    .hashes.results_validator == $results_validator_sha256 and
    .hashes.evidence_validator == $evidence_validator_sha256 and
    .hashes.strict_artifact_validator == $strict_artifact_validator_sha256 and
    .digests.denominator == $denominator_sha256 and
    .digests.overlap == $overlap_sha256 and
    .digests.complement_ptrace == $complement_ptrace_sha256 and
    .digests.complement_kvm == $complement_kvm_sha256 and
    .digests.classified_complement == $classified_complement_sha256
  ' "$here/static-population-check.json" >/dev/null; then
  record_evidence_failure "static-population-check.json is missing, stale, or inconsistent"
fi

row_summary_file="$scratch/row-summary.json"
if jq -s \
    --arg operation validate \
    --slurpfile expected "$expected" \
    --arg source_sha "$source_sha" \
    --arg machine "$machine" \
    --arg kernel "$kernel" \
    --arg campaign_prefix "$campaign_prefix" \
    --arg test '' \
    --argjson run_index 0 \
    -f "$results_validator" \
    "$rows" >"$row_summary_file"; then
  row_results_ok=$(jq -r '.ok' "$row_summary_file")
else
  jq -n '{schema:2,ok:false,failure_reasons:["result JSON or frozen population is unreadable"]}'
  exit 1
fi
if [[ "$row_results_ok" != true ]]; then
  record_evidence_failure "result rows do not satisfy the adaptive typed-evidence contract"
fi
row_binary_sha256=$(jq -r '
  (.binary_sha256 | select(type == "array" and length == 1) | .[0]) // "INVALID"
' "$row_summary_file")
if [[ "$row_binary_sha256" != "$built_binary_sha256" ]]; then
  built_binary_ok=false
  record_evidence_failure "result rows are not bound to the retained release Hermit binary"
fi

round1_eligible=$(jq -r '.round1_eligible_count' "$row_summary_file")
expected_invocations=$(jq -r '.expected_invocation_count' "$row_summary_file")
expected_continuations=$((round1_eligible * 2))

read_completion_value() {
  local key=$1
  local path=$2
  local count value
  count=$(grep -c "^${key}=" "$path" || true)
  [[ $count -eq 1 ]] || return 1
  value=$(sed -n "s/^${key}=//p" "$path")
  [[ -n "$value" && "$value" != *$'\n'* ]] || return 1
  printf '%s' "$value"
}

for spec in \
  'round1_invocations:119' \
  "round1_eligible:$round1_eligible" \
  "continuation_invocations:$expected_continuations" \
  "expected_invocations:$expected_invocations" \
  "recorded_invocations:$expected_invocations" \
  'driver_infrastructure_faults:0' \
  'resource_guard_hits:0' \
  'evidence_complete:true'; do
  key=${spec%%:*}
  wanted=${spec#*:}
  got=$(read_completion_value "$key" "$here/execution-complete.txt" || true)
  if [[ "$got" != "$wanted" ]]; then
    record_evidence_failure "execution-complete $key: expected $wanted, got ${got:-MISSING}"
  fi
done

if ! awk -F '\t' '
  NR == 1 {
    good = ($0 == "phase\trc\telapsed_seconds")
    next
  }
  {
    rows++
    seen[$1]++
    if (($1 != "fetch" && $1 != "build" && $1 != "manifest_validate" && $1 != "preparation") ||
        $2 != "0" || $3 !~ /^[0-9]+$/) good = 0
  }
  END {
    if (rows != 4 || length(seen) != 4 ||
        seen["fetch"] != 1 || seen["build"] != 1 ||
        seen["manifest_validate"] != 1 || seen["preparation"] != 1) good = 0
    exit good ? 0 : 1
  }
' "$here/phase-status.tsv"; then
  record_evidence_failure "phase-status.tsv does not record one successful fetch, build, manifest validation, and preparation phase"
fi

if ! awk -F '\t' \
    -v expected_cells="$expected_cells_sha256" \
    -v population="$population_validator_sha256" \
    -v results="$results_validator_sha256" \
    -v evidence="$(file_sha256 "$evidence_validator")" \
    -v strict_artifacts="$strict_artifact_validator_sha256" '
  NR == 1 {
    good = ($0 == "phase\texpected_cells_sha256\tpopulation_validator_sha256\tresults_validator_sha256\tevidence_validator_sha256\tstrict_artifact_validator_sha256")
    next
  }
  {
    rows++
    seen[$1]++
    if (($1 != "round-1" && $1 != "round-2" && $1 != "round-3" && $1 != "final") ||
        $2 != expected_cells || $3 != population || $4 != results ||
        $5 != evidence || $6 != strict_artifacts) good = 0
  }
  END {
    if (rows != 4 || length(seen) != 4 ||
        seen["round-1"] != 1 || seen["round-2"] != 1 ||
        seen["round-3"] != 1 || seen["final"] != 1) good = 0
    exit good ? 0 : 1
  }
' "$here/frozen-input-checks.tsv"; then
  record_evidence_failure "frozen-input-checks.tsv does not bind all runtime subordinate hashes before each round and final validation"
fi

if ! awk -F '\t' -v payload="$payload_sha256" '
  NR == 1 {
    good = ($0 == "phase\texpected_sha256\tobserved_sha256")
    next
  }
  {
    rows++
    seen[$1]++
    if (($1 != "startup" && $1 != "round-1" && $1 != "round-2" &&
         $1 != "round-3" && $1 != "final") ||
        $2 != payload || $3 != payload) good = 0
  }
  END {
    if (rows != 5 || length(seen) != 5 ||
        seen["startup"] != 1 || seen["round-1"] != 1 ||
        seen["round-2"] != 1 || seen["round-3"] != 1 ||
        seen["final"] != 1) good = 0
    exit good ? 0 : 1
  }
' "$here/payload-input-checks.tsv"; then
  record_evidence_failure "payload-input-checks.tsv does not bind the launcher-reviewed payload at startup, every round, and final validation"
fi

if ! awk -F '\t' \
    -v source_sha="$source_sha" -v source_tree="$source_tree" '
  NR == 1 {
    good = ($0 == "phase\tsource_sha\tsource_tree\tstatus")
    next
  }
  {
    rows++
    seen[$1]++
    if (($1 != "round-1" && $1 != "round-2" && $1 != "round-3") ||
        $2 != source_sha || $3 != source_tree || $4 != "clean") good = 0
  }
  END {
    if (rows != 3 || length(seen) != 3 ||
        seen["round-1"] != 1 || seen["round-2"] != 1 || seen["round-3"] != 1) good = 0
    exit good ? 0 : 1
  }
' "$here/round-cleanliness.tsv"; then
  record_evidence_failure "round-cleanliness.tsv does not prove a clean exact source before all three rounds"
fi

{
  printf 'test\tselector\trepetition\n'
  jq -r '.[] | [.test,.selector,"1"] | @tsv' "$expected"
  for expected_repetition in 2 3; do
    jq -r --arg repetition "$expected_repetition" --slurpfile summary "$row_summary_file" '
      ($summary[0].round1_eligible_ids) as $eligible |
      .[] as $cell | select(($eligible | index($cell.test)) != null) |
      [$cell.test,$cell.selector,$repetition] | @tsv
    ' "$expected"
  done
} >"$scratch/expected-invocations.tsv"

declare -A expected_selector=()
declare -A invocation_result_file=()
declare -A invocation_seen=()
: >"$scratch/expected-result-dirs.txt.unsorted"
while IFS=$'\t' read -r test_id selector repetition; do
  [[ "$test_id" == test ]] && continue
  expected_selector["$test_id|$repetition"]=$selector
done <"$scratch/expected-invocations.tsv"

invocations_ok=true
invocation_rows=0
exec 3<"$here/results/invocations.tsv"
invocation_header=''
IFS= read -r invocation_header <&3 || true
if [[ "$invocation_header" != $'test\tselector\trepetition\trc\telapsed_seconds\tresult_rows\tstrict_single_pass\tleaked_summary_count\tresult_dir' ]]; then
  invocations_ok=false
  record_evidence_failure "results/invocations.tsv has the wrong header"
fi
while IFS=$'\t' read -r \
  test_id selector repetition rc elapsed result_rows strict_single_pass leaked_summary_count result_dir extra <&3; do
  invocation_rows=$((invocation_rows + 1))
  key="$test_id|$repetition"
  if [[ -n "$extra" || ! -v "expected_selector[$key]" ]]; then
    invocations_ok=false
    record_evidence_failure "unexpected invocation row: $test_id repetition $repetition"
    continue
  fi
  if [[ -v "invocation_seen[$key]" ]]; then
    invocations_ok=false
    record_evidence_failure "duplicate invocation row: $test_id repetition $repetition"
    continue
  fi
  invocation_seen["$key"]=1
  if [[ "$selector" != "${expected_selector[$key]}" ]]; then
    invocations_ok=false
    record_evidence_failure "$test_id repetition $repetition uses selector $selector"
  fi
  slug=${test_id//\//-}
  expected_result_dir="$here/results/$slug/repetition-$repetition"
  printf '%s\n' "$expected_result_dir" >>"$scratch/expected-result-dirs.txt.unsorted"
  result_file="$expected_result_dir/results.jsonl"
  invocation_result_file["$key"]=$result_file
  if [[ "$result_dir" != "$expected_result_dir" || ! -f "$result_file" || -L "$result_file" || ! -s "$result_file" ]]; then
    invocations_ok=false
    record_evidence_failure "$test_id repetition $repetition has missing or misplaced results.jsonl"
    continue
  fi
  actual_rows=$(jq -s 'length' "$result_file" 2>/dev/null || printf INVALID)
  if [[ "$result_rows" != "$actual_rows" || ! "$result_rows" =~ ^[1-9][0-9]*$ ]]; then
    invocations_ok=false
    record_evidence_failure "$test_id repetition $repetition result-row count does not match"
  fi
  signal_timeout_leak_capacity=0
  signal_timeout_attempts="$scratch/signal-timeout-attempts-$invocation_rows.txt"
  if jq -sr -r '
      .[] |
      select(.attempts[0].timed_out == true and
             .attempts[0].status == null and
             (.attempts[0].signal == 9 or .attempts[0].signal == 15)) |
      .attempt
    ' "$result_file" >"$signal_timeout_attempts" 2>/dev/null; then
    while IFS= read -r signal_timeout_attempt; do
      retained_log_state=$(awk -F '\t' \
        -v test="$test_id" -v run="$repetition" -v attempt="$signal_timeout_attempt" '
          NR > 1 && $1 == test && $2 == run && $3 == attempt {
            matches++
            if ($10 != "-" && $11 != "-") full++
          }
          END { printf "%d:%d", matches + 0, full + 0 }
        ' "$here/artifacts.tsv")
      if [[ "$retained_log_state" == 1:1 ]]; then
        signal_timeout_leak_capacity=$((signal_timeout_leak_capacity + 1))
      fi
    done <"$signal_timeout_attempts"
  else
    signal_timeout_leak_capacity=INVALID
  fi
  expected_harness_rc=$(jq -sr '
    sort_by(.attempt) |
    if length > 0 and .[-1].outcome == "PASS" then "0" else "1" end
  ' "$result_file" 2>/dev/null || printf INVALID)
  if [[ "$rc" != "$expected_harness_rc" || ! "$elapsed" =~ ^[0-9]+$ ]]; then
    invocations_ok=false
    record_evidence_failure "$test_id repetition $repetition harness rc does not match the terminal attempt, or elapsed time is invalid"
  fi
  leak_manifest="$result_dir/leaked-summaries.tsv"
  if [[ ! "$leaked_summary_count" =~ ^[0-9]+$ || ! -f "$leak_manifest" ]]; then
    invocations_ok=false
    record_evidence_failure "$test_id repetition $repetition has no valid leaked-summary ledger"
  else
    observed_leaks=$(awk 'END { print (NR > 0 ? NR - 1 : 0) }' "$leak_manifest")
    if [[ "$observed_leaks" != "$leaked_summary_count" ]]; then
      invocations_ok=false
      record_evidence_failure "$test_id repetition $repetition leaked-summary count does not match"
    fi
    if ! awk -F '\t' -v root="$result_dir/leaked-private-summaries/" '
      NR == 1 {
        good = ($0 == "kind\tpath\tmtime_epoch_seconds\tsize_bytes\tsha256")
        next
      }
      {
        if ($1 != "verify" ||
            $2 !~ /^results\/[^/]+\/repetition-[123]\/leaked-private-summaries\/\.hermit-verify-summary-[^/]+$/ ||
            $3 !~ /^[0-9]+$/ || $4 !~ /^[0-9]+$/ ||
            $5 !~ /^[0-9a-f]{64}$/) good = 0
      }
      END { exit good ? 0 : 1 }
    ' "$leak_manifest"; then
      invocations_ok=false
      record_evidence_failure "$test_id repetition $repetition leaked-summary ledger is malformed"
    fi
    while IFS=$'\t' read -r leak_kind leak_relative leak_mtime leak_size leak_hash; do
      [[ "$leak_kind" == kind ]] && continue
      leak_path="$here/$leak_relative"
      expected_leak_prefix="${result_dir#"$here/"}/leaked-private-summaries/"
      if [[ "$leak_relative" != "$expected_leak_prefix"* ||
            ! -f "$leak_path" || -L "$leak_path" ||
            $(stat -Lc '%h' "$leak_path") != 1 ||
            $(stat -Lc '%Y' "$leak_path") != "$leak_mtime" ||
            $(stat -Lc '%s' "$leak_path") != "$leak_size" ||
            $(file_sha256 "$leak_path") != "$leak_hash" ]]; then
        invocations_ok=false
        record_evidence_failure "$test_id repetition $repetition leaked summary does not match its ledger"
      fi
    done <"$leak_manifest"
    actual_leaks=$(find "$result_dir/leaked-private-summaries" -mindepth 1 -maxdepth 1 -type f | wc -l)
    if [[ "$actual_leaks" != "$leaked_summary_count" ||
          -n $(find "$result_dir/leaked-private-summaries" -mindepth 1 -maxdepth 1 ! -type f -print -quit) ]]; then
      invocations_ok=false
      record_evidence_failure "$test_id repetition $repetition has unledgered leaked-summary residue"
    fi
    if [[ ! "$signal_timeout_leak_capacity" =~ ^[0-9]+$ ||
          "$leaked_summary_count" -gt "$signal_timeout_leak_capacity" ]]; then
      invocations_ok=false
      record_evidence_failure "$test_id repetition $repetition leaked more KVM verify summaries than signal-killed two-log attempts can produce"
    fi
  fi
  if jq -s -e \
      --arg operation eligibility \
      --slurpfile expected "$expected" \
      --arg source_sha "$source_sha" \
      --arg machine "$machine" \
      --arg kernel "$kernel" \
      --arg campaign_prefix "$campaign_prefix" \
      --arg test "$test_id" \
      --argjson run_index "$repetition" \
      -f "$results_validator" \
      "$result_file" >/dev/null; then
    expected_strict=yes
  else
    expected_strict=no
  fi
  if [[ "$strict_single_pass" != "$expected_strict" ||
        ( "$expected_strict" == yes && "$leaked_summary_count" != 0 ) ]]; then
    invocations_ok=false
    record_evidence_failure "$test_id repetition $repetition strict-pass classification is inconsistent"
  fi
done
exec 3<&-

if [[ $invocation_rows -ne $expected_invocations ||
      ${#invocation_seen[@]} -ne $expected_invocations ]]; then
  invocations_ok=false
  record_evidence_failure "invocation table has $invocation_rows rows and ${#invocation_seen[@]} unique identities; expected $expected_invocations"
fi
for key in "${!expected_selector[@]}"; do
  if [[ ! -v "invocation_seen[$key]" ]]; then
    invocations_ok=false
    record_evidence_failure "invocation table is missing $key"
  fi
done
LC_ALL=C sort -u "$scratch/expected-result-dirs.txt.unsorted" >"$scratch/expected-result-dirs.txt"
find "$here/results" -mindepth 2 -maxdepth 2 -type d -name 'repetition-*' -print |
  LC_ALL=C sort -u >"$scratch/actual-result-dirs.txt"
if ! cmp -s "$scratch/expected-result-dirs.txt" "$scratch/actual-result-dirs.txt"; then
  invocations_ok=false
  record_evidence_failure "result-directory set does not exactly match the adaptive invocation ledger"
fi

sed '1d' "$here/results/invocations.tsv" |
  awk -F '\t' '{print $1 "\t" $2 "\t" $3}' >"$scratch/observed-invocations.tsv"
if ! cmp -s "$scratch/expected-invocations.tsv" <(
    { printf 'test\tselector\trepetition\n'; cat "$scratch/observed-invocations.tsv"; }
  ); then
  invocations_ok=false
  record_evidence_failure "invocations are not in frozen round-1 then adaptive round-2/3 order"
fi

producer_artifacts_ok=true
if python3 - "$here/results/invocations.tsv" \
    >"$scratch/producer-artifacts.stdout" \
    2>"$scratch/producer-artifacts.stderr" <<'PY'
import csv
import json
import os
import sys
import xml.etree.ElementTree as ET
from pathlib import Path


def fail(message: str) -> None:
    raise RuntimeError(message)


def is_nonnegative_integer(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def require_regular(path: Path, label: str, *, nonempty: bool = False) -> None:
    if not path.is_file() or path.is_symlink():
        fail(f"{label} is absent, non-regular, or a symlink: {path}")
    if nonempty and path.stat().st_size == 0:
        fail(f"{label} is empty: {path}")


ledger = Path(sys.argv[1])
with ledger.open(newline="") as source:
    records = list(csv.DictReader(source, delimiter="\t"))
for record in records:
    result_dir = Path(record["result_dir"])
    result_file = result_dir / "results.jsonl"
    invocation_log = result_dir / "invocation.log"
    summary_file = result_dir / "summary.json"
    junit_file = result_dir / "junit.xml"
    require_regular(invocation_log, "invocation.log")
    require_regular(summary_file, "summary.json", nonempty=True)
    require_regular(junit_file, "junit.xml", nonempty=True)
    with result_file.open() as source:
        rows = [json.loads(line) for line in source if line]
    if not rows:
        fail(f"results.jsonl has no rows: {result_file}")
    rows.sort(key=lambda row: row["attempt"])
    if any(row["outcome"] == "PASS" for row in rows):
        selected_outcome = "PASS"
    elif any(row["outcome"] == "FAIL" for row in rows):
        selected_outcome = "FAIL"
    elif any(row["outcome"] == "ERROR" for row in rows):
        selected_outcome = "ERROR"
    else:
        fail(f"result rows have no executable terminal outcome: {result_file}")
    selected = next(
        row for row in reversed(rows) if row["outcome"] == selected_outcome
    )
    cpu_values = [row.get("cpu_usage_usec") for row in rows]
    if not all(value is None or is_nonnegative_integer(value) for value in cpu_values):
        fail(f"result rows have invalid CPU accounting: {result_file}")
    expected_cpu_usage = (
        None if any(value is None for value in cpu_values) else sum(cpu_values)
    )
    expected_summary = {
        "schema": 1,
        "cells": 1,
        "passed": 1 if selected_outcome == "PASS" else 0,
        "failed": 1 if selected_outcome == "FAIL" else 0,
        "errors": 1 if selected_outcome == "ERROR" else 0,
        "host_inapplicable": 0,
        "cell_cpu_usage_usec": expected_cpu_usage,
        "host_inapplicable_cells": [],
    }
    if json.loads(summary_file.read_text()) != expected_summary:
        fail(f"summary.json does not match its terminal row/counts: {summary_file}")

    root = ET.parse(junit_file).getroot()
    expected_suite = {
        "name": "hermit-e2e",
        "tests": "1",
        "failures": "1" if selected_outcome == "FAIL" else "0",
        "errors": "1" if selected_outcome == "ERROR" else "0",
        "skipped": "0",
    }
    if root.tag != "testsuite" or root.attrib != expected_suite:
        fail(f"JUnit suite counts are inconsistent: {junit_file}")
    cases = list(root)
    duration = selected.get("duration_ms")
    if not is_nonnegative_integer(duration):
        fail(f"terminal duration is not a nonnegative integer: {result_file}")
    expected_case = {
        "classname": selected["category"],
        "name": f"{selected['test']}/{selected['mode']}/{selected['backend']}",
        "time": f"{duration / 1000.0:.3f}",
    }
    if len(cases) != 1 or cases[0].tag != "testcase" or cases[0].attrib != expected_case:
        fail(f"JUnit testcase identity is inconsistent: {junit_file}")
    children = list(cases[0])
    expected_child = "failure" if selected_outcome == "FAIL" else (
        "error" if selected_outcome == "ERROR" else None
    )
    if expected_child is None:
        if children:
            fail(f"passing JUnit testcase carries a failure child: {junit_file}")
    elif (
        len(children) != 1
        or children[0].tag != expected_child
        or children[0].attrib
        or (children[0].text or "") != selected["reason"]
    ):
        fail(f"JUnit terminal disposition is inconsistent: {junit_file}")

    for row in rows:
        attempts = row.get("attempts")
        if not isinstance(attempts, list) or len(attempts) != 1:
            fail(f"row does not have exactly one execution attempt: {result_file}")
        artifact_dir = row.get("artifact_dir")
        if not isinstance(artifact_dir, str) or not artifact_dir.startswith("/results/runs/"):
            fail(f"row has an invalid artifact_dir: {result_file}")
        host_artifact = result_dir / artifact_dir.removeprefix("/results/")
        captures = host_artifact / "captures"
        for stream in ("stdout", "stderr"):
            capture = captures / f"verify-1.{stream}"
            require_regular(capture, f"verify capture {stream}")
            value = attempts[0].get(stream)
            if not isinstance(value, str) or capture.read_bytes() != value.encode():
                fail(f"verify capture {stream} bytes differ from the result row: {capture}")
PY
then
  :
else
  producer_artifacts_ok=false
  record_evidence_failure "mandatory invocation/JUnit/summary/capture artifacts are missing or inconsistent: $(tail -n 1 "$scratch/producer-artifacts.stderr")"
fi

: >"$scratch/rebuilt-all-results.jsonl"
while IFS=$'\t' read -r test_id selector repetition rc elapsed result_rows strict_single_pass leaked_summary_count result_dir; do
  [[ "$test_id" == test ]] && continue
  result_file="$result_dir/results.jsonl"
  [[ -f "$result_file" ]] && cat -- "$result_file" >>"$scratch/rebuilt-all-results.jsonl"
done <"$here/results/invocations.tsv"
aggregate_ok=true
if ! cmp -s "$scratch/rebuilt-all-results.jsonl" "$rows"; then
  aggregate_ok=false
  record_evidence_failure "all-results.jsonl is not the exact ordered concatenation of every invocation result file"
fi

jq -r '.[] | [.test,.selector] | @tsv' "$expected" >"$scratch/expected-population.tsv.body"
{ printf 'test\tselector\n'; cat "$scratch/expected-population.tsv.body"; } >"$scratch/expected-population.tsv"
population_tsv_ok=true
if ! cmp -s "$scratch/expected-population.tsv" "$here/population.tsv"; then
  population_tsv_ok=false
  record_evidence_failure "population.tsv does not exactly match the frozen 119-cell population and selectors"
fi

{
  printf 'test\tselector\n'
  jq -r --slurpfile summary "$row_summary_file" '
    ($summary[0].round1_eligible_ids) as $eligible |
    .[] as $cell | select(($eligible | index($cell.test)) != null) |
    [$cell.test,$cell.selector] | @tsv
  ' "$expected"
} >"$scratch/expected-round1-eligible.tsv"
round1_eligible_tsv_ok=true
if ! cmp -s "$scratch/expected-round1-eligible.tsv" "$here/round1-eligible.tsv"; then
  round1_eligible_tsv_ok=false
  record_evidence_failure "round1-eligible.tsv does not exactly match validator-recomputed strict round-1 cells"
fi

declare -A row_artifact_dir=()
declare -A row_report_hash=()
declare -A row_report_file=()
declare -A row_report_verdict=()
declare -A row_report_no_result_kind=()
declare -A row_report_left_count=()
declare -A row_report_right_count=()
declare -A row_attempt_status=()
declare -A row_attempt_signal=()
declare -A timeout_row=()
declare -A pre_attempt_timeout_row=()
declare -A executed_timeout_without_report_row=()
row_lookup_count=0
while IFS=$'\t' read -r lane category test_id mode backend run_index attempt run_id; do
  key="$lane|$category|$test_id|$mode|$backend|$run_index|$attempt|$run_id"
  timeout_row["$key"]=1
done < <(jq -r '
  .timeout_attempts[] |
  [.lane,.category,.test,.mode,.backend,(.run_index|tostring),(.attempt|tostring),.run_id] | @tsv
' "$row_summary_file")
while IFS=$'\t' read -r lane category test_id mode backend run_index attempt run_id; do
  key="$lane|$category|$test_id|$mode|$backend|$run_index|$attempt|$run_id"
  pre_attempt_timeout_row["$key"]=1
done < <(jq -r '
  .pre_attempt_timeout_attempts[] |
  [.lane,.category,.test,.mode,.backend,(.run_index|tostring),(.attempt|tostring),.run_id] | @tsv
' "$row_summary_file")
while IFS=$'\t' read -r lane category test_id mode backend run_index attempt run_id; do
  key="$lane|$category|$test_id|$mode|$backend|$run_index|$attempt|$run_id"
  executed_timeout_without_report_row["$key"]=1
done < <(jq -r '
  .executed_timeout_without_report_attempts[] |
  [.lane,.category,.test,.mode,.backend,(.run_index|tostring),(.attempt|tostring),.run_id] | @tsv
' "$row_summary_file")
while IFS=$'\t' read -r lane category test_id mode backend run_index attempt run_id artifact_dir report_hash report_base64 report_verdict report_no_result_kind report_left_count report_right_count attempt_status attempt_signal; do
  key="$lane|$category|$test_id|$mode|$backend|$run_index|$attempt|$run_id"
  row_artifact_dir["$key"]=$artifact_dir
  row_report_hash["$key"]=$report_hash
  row_report_verdict["$key"]=$report_verdict
  row_report_no_result_kind["$key"]=$report_no_result_kind
  row_report_left_count["$key"]=$report_left_count
  row_report_right_count["$key"]=$report_right_count
  row_attempt_status["$key"]=$attempt_status
  row_attempt_signal["$key"]=$attempt_signal
  if [[ "$report_base64" == - ]]; then
    row_report_file["$key"]=-
  else
    embedded_report="$scratch/embedded-report-$row_lookup_count.json"
    if ! printf '%s' "$report_base64" | base64 --decode >"$embedded_report"; then
      record_evidence_failure "$key has undecodable embedded verification report bytes"
    fi
    row_report_file["$key"]=$embedded_report
  fi
  row_lookup_count=$((row_lookup_count + 1))
done < <(jq -r '
  [
    .lane,
    .category,
    .test,
    .mode,
    .backend,
    (.run_index | tostring),
    (.attempt | tostring),
    .run_id,
    .artifact_dir,
    (.attempts[0].verification_report_sha256 // "-"),
    (if .attempts[0].verification_report == null
     then "-" else (.attempts[0].verification_report | @base64) end),
    (if .attempts[0].verification_report == null
     then "NONE"
     else (try (.attempts[0].verification_report | fromjson | .verdict) catch "INVALID")
     end),
    (if .attempts[0].verification_report == null
     then "NONE"
     else (try ((.attempts[0].verification_report | fromjson |
       .no_result_reason.kind) // "NONE") catch "INVALID")
     end),
    (if .attempts[0].verification_report == null
     then "-"
     else (try ((.attempts[0].verification_report | fromjson |
       .compared_log_messages.left) // "-") catch "-")
     end),
    (if .attempts[0].verification_report == null
     then "-"
     else (try ((.attempts[0].verification_report | fromjson |
       .compared_log_messages.right) // "-") catch "-")
     end),
    (if .attempts[0].status == null then "NONE"
     else (.attempts[0].status | tostring) end),
    (if .attempts[0].signal == null then "NONE"
     else (.attempts[0].signal | tostring) end)
  ] | @tsv
' "$rows")

artifacts_ok=true
artifact_failures=0
artifact_rows=0
declare -A artifact_seen=()
: >"$scratch/expected-artifact-dirs.txt.unsorted"

record_artifact_failure() {
  artifacts_ok=false
  artifact_failures=$((artifact_failures + 1))
  if [[ $artifact_failures -le 30 ]]; then
    record_evidence_failure "$1"
  fi
}

validate_optional_timeout_logs() {
  local key=$1
  local log_dir=$2
  local run1_log=$3
  local run2_log=$4
  local require_run1=$5
  local expected_log_count=0 log_entry
  local -a log_entries=()
  if [[ "$run1_log" != - &&
        ( ${run1_log##*/} != run1_log_* || ${run1_log%/*} != "$log_dir" ||
          ! -f "$run1_log" || -L "$run1_log" ) ]]; then
    record_artifact_failure "$key optional timeout run1 log is invalid"
  elif [[ "$run1_log" != - ]]; then
    expected_log_count=$((expected_log_count + 1))
    if [[ $(stat -Lc '%s' "$run1_log") -gt 1073741824 ]]; then
      record_artifact_failure "$key optional timeout run1 log exceeds the 1 GiB retained-log guard"
    fi
  fi
  if [[ "$run2_log" != - &&
        ( ${run2_log##*/} != run2_log_* || ${run2_log%/*} != "$log_dir" ||
          ! -f "$run2_log" || -L "$run2_log" ) ]]; then
    record_artifact_failure "$key optional timeout run2 log is invalid"
  elif [[ "$run2_log" != - ]]; then
    expected_log_count=$((expected_log_count + 1))
    if [[ $(stat -Lc '%s' "$run2_log") -gt 1073741824 ]]; then
      record_artifact_failure "$key optional timeout run2 log exceeds the 1 GiB retained-log guard"
    fi
  fi
  if [[ "$run1_log" != - && "$run1_log" == "$run2_log" ]]; then
    record_artifact_failure "$key optional timeout logs alias the same path"
  fi
  if [[ "$run1_log" == - && "$run2_log" != - ]]; then
    record_artifact_failure "$key impossible run2-without-run1 timeout log sequence"
  fi
  if [[ "$require_run1" == yes && "$run1_log" == - ]]; then
    record_artifact_failure "$key first-run-rejected timeout omitted its completed run1 log"
  fi
  if [[ ! -d "$log_dir" || -L "$log_dir" ]]; then
    record_artifact_failure "$key optional timeout log path is not a regular directory"
  else
      shopt -s nullglob dotglob
      log_entries=("$log_dir"/*)
      shopt -u nullglob dotglob
      if [[ ${#log_entries[@]} -ne $expected_log_count ]]; then
        record_artifact_failure "$key optional timeout log directory has unledgered entries"
      fi
      for log_entry in "${log_entries[@]}"; do
        if [[ "$log_entry" != "$run1_log" && "$log_entry" != "$run2_log" ]]; then
          record_artifact_failure "$key optional timeout retained an unexpected log entry"
        fi
      done
  fi
}

exec 4<"$here/artifacts.tsv"
artifact_header=''
IFS= read -r artifact_header <&4 || true
if [[ "$artifact_header" != $'test\trun_index\tattempt\tresults_jsonl\tresults_sha256\tartifact_dir\tartifact_state\tverification_report\tverification_report_sha256\trun1_log\trun2_log\treport_staging_file' ]]; then
  record_artifact_failure "artifacts.tsv has the wrong header"
fi
while IFS=$'\t' read -r \
  test_id run_index attempt results_jsonl results_hash artifact_dir artifact_state report_file report_hash run1_log run2_log report_staging_file extra <&4; do
  unset expected_artifact_state row_expected_artifact_state staging_path staging_basename
  artifact_rows=$((artifact_rows + 1))
  category=${test_id%%/*}
  run_id="$campaign_prefix-${test_id//\//-}-repetition-$run_index"
  key="portable|$category|$test_id|verify|kvm|$run_index|$attempt|$run_id"
  invocation_key="$test_id|$run_index"
  if [[ -n "$extra" || ! -v "row_artifact_dir[$key]" ||
        ! -v "invocation_result_file[$invocation_key]" ]]; then
    record_artifact_failure "artifacts.tsv has an unexpected row: $key"
    continue
  fi
  if [[ -v "artifact_seen[$key]" ]]; then
    record_artifact_failure "artifacts.tsv duplicates $key"
    continue
  fi
  artifact_seen["$key"]=1
  expected_results=${invocation_result_file[$invocation_key]}
  if [[ "$results_jsonl" != "$expected_results" || ! -f "$results_jsonl" || -L "$results_jsonl" ]]; then
    record_artifact_failure "$key has an invalid results.jsonl path"
  elif [[ "$(file_sha256 "$results_jsonl")" != "$results_hash" ]]; then
    record_artifact_failure "$key results.jsonl hash does not match artifacts.tsv"
  fi
  container_artifact=${row_artifact_dir[$key]}
  result_root=${expected_results%/results.jsonl}
  if [[ "$container_artifact" == /results/runs/* ]]; then
    expected_artifact="$result_root${container_artifact#/results}"
  else
    expected_artifact=INVALID
    record_artifact_failure "$key has an invalid container artifact_dir"
  fi
  if [[ "$artifact_dir" != "$container_artifact" ]]; then
    record_artifact_failure "$key artifact path does not match its result row"
  fi
  if [[ -v "pre_attempt_timeout_row[$key]" ]]; then
    row_expected_artifact_state=pre-attempt-not-run
  elif [[ -v "executed_timeout_without_report_row[$key]" ]]; then
    row_expected_artifact_state=executed-no-report
  else
    row_expected_artifact_state=executed-report
  fi
  printf '%s\n' "$expected_artifact" >>"$scratch/expected-artifact-dirs.txt.unsorted"
  if [[ ! -d "$expected_artifact" || -L "$expected_artifact" ]]; then
    record_artifact_failure "$key artifact directory is absent or a symlink"
  else
    for required_artifact_directory in \
      home xdg-config tmp fixtures recording captures verify-logs workdir; do
      required_artifact_path="$expected_artifact/$required_artifact_directory"
      if [[ ! -d "$required_artifact_path" || -L "$required_artifact_path" ]]; then
        record_artifact_failure "$key is missing producer directory $required_artifact_directory"
      fi
    done
    shopt -s nullglob dotglob
    artifact_root_entries=("$expected_artifact"/*)
    report_staging_candidates=("$expected_artifact"/.tmp*)
    shopt -u nullglob dotglob
    for artifact_root_entry in "${artifact_root_entries[@]}"; do
      artifact_root_basename=${artifact_root_entry##*/}
      case "$artifact_root_basename" in
        home | xdg-config | tmp | fixtures | recording | captures | verify-logs | workdir)
          [[ -d "$artifact_root_entry" && ! -L "$artifact_root_entry" ]] ||
            record_artifact_failure "$key producer directory has the wrong type: $artifact_root_basename"
          ;;
        verify-1.json)
          [[ -f "$artifact_root_entry" && ! -L "$artifact_root_entry" ]] ||
            record_artifact_failure "$key report path has the wrong type"
          ;;
        .tmp*) ;;
        *) record_artifact_failure "$key retained an unexpected artifact-root entry: $artifact_root_basename" ;;
      esac
    done
    if [[ ${#report_staging_candidates[@]} -eq 0 ]]; then
      [[ "$report_staging_file" == - ]] ||
        record_artifact_failure "$key claims an absent atomic report staging file"
    elif [[ ${#report_staging_candidates[@]} -eq 1 ]]; then
      staging_path=${report_staging_candidates[0]}
      staging_basename=${staging_path##*/}
      if [[ "$report_staging_file" != "$staging_path" ||
            ! "$staging_basename" =~ ^\.tmp[A-Za-z0-9]{6}$ ||
            ! -f "$staging_path" || -L "$staging_path" ||
            $(stat -Lc '%h' "$staging_path" 2>/dev/null || printf 0) != 1 ]]; then
        record_artifact_failure "$key atomic report staging file is malformed or unbound"
      fi
      if [[ ! -v "timeout_row[$key]" ||
            "${row_attempt_status[$key]}" != NONE ||
            ( "${row_attempt_signal[$key]}" != 9 &&
              "${row_attempt_signal[$key]}" != 15 ) ||
            ( "$row_expected_artifact_state" != executed-no-report &&
              ! ( "${row_report_verdict[$key]}" == no_result &&
                  "${row_report_no_result_kind[$key]}" == not_run ) ) ]]; then
        record_artifact_failure "$key retained an atomic report staging file outside a signal-killed absent/pending publish"
      fi
    else
      record_artifact_failure "$key retained multiple atomic report staging files"
    fi
  fi
  case "$artifact_state" in
    pre-attempt-not-run | executed-report | executed-no-report)
      expected_artifact_state=$artifact_state
      ;;
    *)
      expected_artifact_state=INVALID
      record_artifact_failure "$key has an unknown artifact state: $artifact_state"
      ;;
  esac
  if [[ "$expected_artifact_state" != "$row_expected_artifact_state" ]]; then
    record_artifact_failure "$key artifact state does not match its typed result row"
  fi
  embedded_report=${row_report_file[$key]}
  if [[ "$row_expected_artifact_state" == executed-no-report ]]; then
    if [[ "$embedded_report" != - || "${row_report_hash[$key]}" != - ||
          "$report_hash" != - ]]; then
      record_artifact_failure "$key no-report timeout contradicts its null embedded report/hash"
    fi
  elif [[ "$embedded_report" == - || ! -s "$embedded_report" ||
          $(file_sha256 "$embedded_report") != "${row_report_hash[$key]}" ||
          "$report_hash" != "${row_report_hash[$key]}" ]]; then
    record_artifact_failure "$key embedded verification report hash is inconsistent"
  fi
  expected_report="$expected_artifact/verify-1.json"
  log_dir="$expected_artifact/verify-logs/verify-1"
  if [[ "$row_expected_artifact_state" == pre-attempt-not-run ]]; then
    if [[ "$report_file" != - || "$run1_log" != - || "$run2_log" != - ||
          -e "$expected_report" ]]; then
      record_artifact_failure "$key pre-attempt timeout claims or retains a disk report/log"
    fi
    if [[ ! -d "$log_dir" || -L "$log_dir" ]]; then
      record_artifact_failure "$key pre-attempt timeout is missing its producer-created verify-log directory"
    elif [[ -n $(find "$log_dir" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
      record_artifact_failure "$key pre-attempt timeout retained unexpected verify-log entries"
    fi
    continue
  fi

  if [[ "$row_expected_artifact_state" == executed-no-report ]]; then
    if [[ "$report_file" != - || "$report_hash" != - ||
          "$run1_log" != - || "$run2_log" != - || -e "$expected_report" ]]; then
      record_artifact_failure "$key no-report timeout retained an impossible final report/log"
    fi
    if [[ ! -d "$log_dir" || -L "$log_dir" ]]; then
      record_artifact_failure "$key no-report timeout is missing its producer-created verify-log directory"
    elif [[ -n $(find "$log_dir" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
      record_artifact_failure "$key no-report timeout retained unexpected verify-log entries"
    fi
    continue
  fi

  if [[ "$report_file" != "$expected_report" ]]; then
    record_artifact_failure "$key verification report path does not match its result row"
  fi
  if [[ ! -f "$report_file" || -L "$report_file" || ! -s "$report_file" ]]; then
    record_artifact_failure "$key verify-1.json is not a nonempty regular file"
  else
    actual_report_hash=$(file_sha256 "$report_file")
    if ! cmp -s "$report_file" "$embedded_report" ||
       [[
          "$actual_report_hash" != "${row_report_hash[$key]}" ||
          "$report_hash" != "${row_report_hash[$key]}" ]]; then
      record_artifact_failure "$key verify report content or hash differs from the embedded result evidence"
    fi
  fi
  if [[ "${row_report_verdict[$key]}" == no_result &&
        -v "timeout_row[$key]" ]]; then
    if [[ "${row_report_no_result_kind[$key]}" == first_run_rejected ]]; then
      require_timeout_run1=yes
    else
      require_timeout_run1=no
    fi
    validate_optional_timeout_logs \
      "$key" "$log_dir" "$run1_log" "$run2_log" "$require_timeout_run1"
    if [[ "${row_report_no_result_kind[$key]}" == first_run_rejected &&
          "${row_attempt_status[$key]}" == 125 &&
          "${row_attempt_signal[$key]}" == NONE &&
          "$run2_log" != - ]]; then
      record_artifact_failure "$key post-exit first-run-rejected timeout retained an impossible run2 log"
    fi
    continue
  fi
  shopt -s nullglob dotglob
  log_entries=("$log_dir"/*)
  shopt -u nullglob dotglob
  if [[ ! -d "$log_dir" || -L "$log_dir" ||
        ${run1_log##*/} != run1_log_* ||
        ${run1_log%/*} != "$log_dir" ||
        ! -f "$run1_log" || -L "$run1_log" ]]; then
    record_artifact_failure "$key does not retain one regular run1 log"
  elif [[ $(stat -Lc '%s' "$run1_log") -gt 1073741824 ]]; then
    record_artifact_failure "$key run1 log exceeds the 1 GiB retained-log guard"
  elif [[ "${row_report_verdict[$key]}" == no_result ]]; then
    if [[ "$run2_log" != - || ${#log_entries[@]} -ne 1 ||
          "${log_entries[0]}" != "$run1_log" ]]; then
      record_artifact_failure "$key non-timeout no_result log directory is not exactly {run1}"
    fi
  elif [[ ( "${row_report_left_count[$key]}" != - &&
             "${row_report_left_count[$key]}" -gt 0 &&
             ! -s "$run1_log" ) ||
          ${#log_entries[@]} -ne 2 ||
          "$run1_log" == "$run2_log" ||
          ${run2_log##*/} != run2_log_* ||
          ${run2_log%/*} != "$log_dir" ||
          ! -f "$run2_log" || -L "$run2_log" ||
          ( "${row_report_right_count[$key]}" != - &&
            "${row_report_right_count[$key]}" -gt 0 &&
            ! -s "$run2_log" ) ||
          ! ( ( "${log_entries[0]}" == "$run1_log" && "${log_entries[1]}" == "$run2_log" ) ||
              ( "${log_entries[0]}" == "$run2_log" && "${log_entries[1]}" == "$run1_log" ) ) ]]; then
    record_artifact_failure "$key matched/diverged report lacks its exact run1/run2 logs or required nonempty canonical evidence"
  fi
  if [[ "$run2_log" != - && -f "$run2_log" && ! -L "$run2_log" ]] &&
     [[ $(stat -Lc '%s' "$run2_log") -gt 1073741824 ]]; then
    record_artifact_failure "$key run2 log exceeds the 1 GiB retained-log guard"
  fi
done
exec 4<&-

if [[ $artifact_rows -ne $row_lookup_count ||
      ${#artifact_seen[@]} -ne $row_lookup_count ]]; then
  record_artifact_failure "artifacts.tsv has $artifact_rows rows and ${#artifact_seen[@]} unique keys; aggregate has $row_lookup_count rows"
fi
for key in "${!row_artifact_dir[@]}"; do
  [[ -v "artifact_seen[$key]" ]] || record_artifact_failure "artifacts.tsv is missing $key"
done
if [[ $artifact_failures -gt 30 ]]; then
  record_evidence_failure "$((artifact_failures - 30)) additional artifact failures omitted"
fi

LC_ALL=C sort -u "$scratch/expected-artifact-dirs.txt.unsorted" >"$scratch/expected-artifact-dirs.txt"
find "$here/results" -mindepth 5 -maxdepth 5 -type d -path '*/runs/*/*' -print |
  LC_ALL=C sort -u >"$scratch/actual-artifact-dirs.txt"
if ! cmp -s "$scratch/expected-artifact-dirs.txt" "$scratch/actual-artifact-dirs.txt"; then
  record_artifact_failure "artifact directory set does not exactly match result-row artifact_dir values"
fi

tree_node_errors="$scratch/result-tree-node-errors.txt"
tree_symlinks="$scratch/result-tree-symlinks.txt"
tree_special_nodes="$scratch/result-tree-special-nodes.txt"
: >"$tree_node_errors"
if ! find "$here/results" -type l -print >"$tree_symlinks" 2>>"$tree_node_errors"; then
  record_artifact_failure "cannot traverse retained result tree for symlinks"
elif [[ -s "$tree_symlinks" ]]; then
  record_artifact_failure "retained result tree contains a symlink"
fi
if ! find "$here/results" ! -type d ! -type f ! -type l -print \
    >"$tree_special_nodes" 2>>"$tree_node_errors"; then
  record_artifact_failure "cannot traverse retained result tree for special nodes"
elif [[ -s "$tree_special_nodes" ]]; then
  record_artifact_failure "retained result tree contains a non-directory/non-regular node"
fi
tree_hardlinks="$scratch/result-tree-hardlinks.txt"
if ! find "$here/results" -type f -links +1 -print >"$tree_hardlinks" \
    2>>"$tree_node_errors"; then
  record_artifact_failure "cannot traverse retained result tree for hardlinks"
elif [[ -s "$tree_hardlinks" ]]; then
  record_artifact_failure "retained result tree contains a hardlink alias"
fi
artifact_hash_inventory_ok=true
if ! awk -F '\t' '
  NR == 1 { good = ($0 == "sha256\tpath"); next }
  {
    rows++
    if (NF != 2 || $1 !~ /^[0-9a-f]{64}$/ ||
        $2 !~ /^results\// || $2 ~ /(^|\/)\.\.($|\/)/ || seen[$2]++) good = 0
  }
  END { if (rows == 0) good = 0; exit good ? 0 : 1 }
' "$here/artifact-hashes.tsv"; then
  artifact_hash_inventory_ok=false
  record_artifact_failure "artifact-hashes.tsv has malformed, duplicate, or unsafe entries"
fi
artifact_paths="$scratch/artifact-paths.bin"
artifact_paths_sorted="$scratch/artifact-paths-sorted.bin"
: >"$scratch/artifact-hash-errors.txt"
if ! find "$here/results" -type f -print0 \
    >"$artifact_paths" 2>>"$scratch/artifact-hash-errors.txt"; then
  artifact_hash_inventory_ok=false
  record_artifact_failure "cannot completely traverse the retained result tree"
fi
if ! LC_ALL=C sort -z "$artifact_paths" >"$artifact_paths_sorted" \
    2>>"$scratch/artifact-hash-errors.txt"; then
  artifact_hash_inventory_ok=false
  record_artifact_failure "cannot sort the complete retained result file set"
fi
printf 'sha256\tpath\n' >"$scratch/artifact-hashes.tsv"
while IFS= read -r -d '' path; do
  relative=${path#"$here/"}
  if [[ "$relative" == *$'\t'* || "$relative" == *$'\n'* ]]; then
    artifact_hash_inventory_ok=false
    record_artifact_failure "retained artifact path cannot be represented safely in the inventory"
    continue
  fi
  if hash_line=$(sha256sum -- "$path" 2>>"$scratch/artifact-hash-errors.txt"); then
    hash=${hash_line%% *}
  else
    hash=INVALID
    artifact_hash_inventory_ok=false
    record_artifact_failure "cannot hash retained artifact: $relative"
  fi
  if [[ ! "$hash" =~ ^[0-9a-f]{64}$ ]]; then
    artifact_hash_inventory_ok=false
    record_artifact_failure "retained artifact hash is malformed: $relative"
  fi
  printf '%s\t%s\n' "$hash" "$relative" >>"$scratch/artifact-hashes.tsv"
done <"$artifact_paths_sorted"
if ! cmp -s "$scratch/artifact-hashes.tsv" "$here/artifact-hashes.tsv"; then
  artifact_hash_inventory_ok=false
  record_artifact_failure "artifact-hashes.tsv does not match the complete retained artifact tree"
fi

if [[ ${#environment_failures[@]} -eq 0 ]]; then
  environment_ok=true
else
  environment_ok=false
fi
if [[ ${#evidence_failures[@]} -eq 0 ]]; then
  evidence_ok=true
else
  evidence_ok=false
fi
all_failures=("${environment_failures[@]}" "${evidence_failures[@]}")
if [[ ${#all_failures[@]} -eq 0 ]]; then
  failure_detail_text=''
else
  failure_detail_text=$(IFS=$'\n'; printf '%s' "${all_failures[*]}")
fi

summary=$(jq \
  --slurpfile population "$population_summary_file" \
  --argjson environment_ok "$environment_ok" \
  --argjson population_ok "$population_ok" \
  --argjson digest_checks_ok "$digest_checks_ok" \
  --argjson invocations_ok "$invocations_ok" \
  --argjson aggregate_ok "$aggregate_ok" \
  --argjson population_tsv_ok "$population_tsv_ok" \
  --argjson round1_eligible_tsv_ok "$round1_eligible_tsv_ok" \
  --argjson resource_guard_ok "$resource_guard_ok" \
  --argjson disk_budget_ok "$disk_budget_ok" \
  --argjson built_binary_ok "$built_binary_ok" \
  --argjson producer_artifacts_ok "$producer_artifacts_ok" \
  --argjson artifacts_ok "$artifacts_ok" \
  --argjson artifact_hash_inventory_ok "$artifact_hash_inventory_ok" \
  --argjson evidence_ok "$evidence_ok" \
  --argjson retained_artifact_rows "$artifact_rows" \
  --arg failure_detail_text "$failure_detail_text" '
    .checks.environment = $environment_ok
    | .checks.source_population = $population_ok
    | .checks.population_digests = $digest_checks_ok
    | .checks.invocation_ledger = $invocations_ok
    | .checks.aggregate_exact = $aggregate_ok
    | .checks.population_ledger = $population_tsv_ok
    | .checks.round1_eligible_ledger = $round1_eligible_tsv_ok
    | .checks.resource_guard = $resource_guard_ok
    | .checks.disk_budget = $disk_budget_ok
    | .checks.retained_binary = $built_binary_ok
    | .checks.mandatory_producer_artifacts = $producer_artifacts_ok
    | .checks.retained_artifacts = $artifacts_ok
    | .checks.artifact_hash_inventory = $artifact_hash_inventory_ok
    | .checks.complete_evidence = $evidence_ok
    | .population = $population[0]
    | .retained_artifact_rows = $retained_artifact_rows
    | .provisional_qualification_scope = "row-only same-backend KVM canonical L2 repeatability; not ptrace-vs-KVM parity"
    | .projected_overlap_scope = "selected-set identity overlap only; not ptrace-vs-KVM output or log parity"
    | .failure_reasons = (
        [.checks | to_entries[] | select(.value != true) | .key]
        + ($failure_detail_text | split("\n") | map(select(length > 0)))
      )
    | .ok = (.checks | all(.[]; . == true))
    | if .ok then
        .qualified_cell_count = .provisional_qualified_cell_count
        | .qualified_ids = .provisional_qualified_ids
        | .qualified_fraction = .provisional_qualified_fraction
        | .projected_overlap_numerator = .provisional_projected_overlap_numerator
        | .projected_overlap_denominator = .provisional_projected_overlap_denominator
        | .target_new_cells_required = .provisional_target_new_cells_required
        | .target_90_percent_reached = .provisional_target_90_percent_reached
        | .qualification_scope = "authoritative same-backend KVM canonical L2 repeatability after full evidence validation"
      else
        del(
          .qualified_cell_count,
          .qualified_ids,
          .qualified_fraction,
          .projected_overlap_numerator,
          .projected_overlap_denominator,
          .target_new_cells_required,
          .target_90_percent_reached,
          .qualification_scope
        )
      end
  ' "$row_summary_file")

printf '%s\n' "$summary"
jq -e '.ok == true' >/dev/null <<<"$summary"

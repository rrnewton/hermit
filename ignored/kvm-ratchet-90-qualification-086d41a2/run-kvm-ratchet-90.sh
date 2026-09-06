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
campaign_dir_source=${KVM_RATCHET_CAMPAIGN_DIR:-$(dirname -- "$payload_source")}
script_dir=$(CDPATH= cd -- "$campaign_dir_source" && pwd -P) || {
  printf 'KVM ratchet campaign: cannot resolve payload directory\n' >&2
  exit 2
}
readonly script_dir
unset campaign_dir_source
readonly payload_path=${KVM_RATCHET_PAYLOAD_PATH:-"$script_dir/${payload_source##*/}"}
readonly freeze_manifest_path=${KVM_RATCHET_FREEZE_MANIFEST_PATH:-"$script_dir/FROZEN_SHA256SUMS"}
readonly expected_cells_path=${KVM_RATCHET_EXPECTED_CELLS_PATH:-"$script_dir/expected-cells.json"}
readonly population_validator_path=${KVM_RATCHET_POPULATION_VALIDATOR_PATH:-"$script_dir/validate-population.jq"}
readonly results_validator_path=${KVM_RATCHET_RESULTS_VALIDATOR_PATH:-"$script_dir/validate-results.jq"}
readonly evidence_validator_path=${KVM_RATCHET_EVIDENCE_VALIDATOR_PATH:-"$script_dir/validate-evidence.sh"}
readonly strict_artifact_validator_path=${KVM_RATCHET_STRICT_ARTIFACT_VALIDATOR_PATH:-"$script_dir/validate-strict-invocation-artifacts.sh"}
readonly payload_expected_sha256=${KVM_RATCHET_PAYLOAD_SHA256:-}
payload_startup_sha256=
case ${1:-} in
  --static-check | --self-test-invocation-gate | --self-test-resource-guard | --self-test-resource-watchdog | --self-test-monitor-stop | --self-test-initial-disk-snapshot) ;;
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
readonly campaign_prefix=kvm-ratchet-90-086d41a2-run6
readonly expected_image=localhost/hermit-hermetic-validate@sha256:e38c3b2d5cd8a17ed2f99b4a24dd76c9ee63329cdc8c9723e4650ab085fae985
readonly retained_log_max_bytes=1073741824
readonly invocation_log_max_bytes=1073741824
readonly capture_file_max_bytes=1073741824
readonly service_log_max_bytes=1073741824
readonly process_file_limit_bytes=1073742000
readonly expected_service_log_path=/home/newton/work/dev-hermit/ignored/validate/hermit-kvm-ratchet-90-086d41a2-run6.log
readonly service_log_path=${KVM_RATCHET_SERVICE_LOG_PATH:-$expected_service_log_path}
readonly campaign_budget_bytes=137438953472
readonly filesystem_reserve_bytes=137438953472
readonly prior_campaign_allocated_bytes=4688396288
readonly prior_campaign_results_allocated_bytes=93057024
readonly cpu_timeout_multiplier=1
readonly wall_timeout_multiplier=1
readonly campaign_child_deadline_seconds=7200
readonly cells_sha256=84c5dfa8aebe71b65396134e09986d9e88bf8dbabc18b7fd2e08e28c73b4aa71
readonly plan_sha256=bc48a92f8d3981619bb9211eee12c42b5cfd2af202080293556c0f57c5824e3f
readonly image_file_sha256=761e73803bbea4333b9c316234a9d9f86ac9982ea111737293c7be59cfb0ee93
readonly submodule_status_sha256=134f1b5725890ecccc3dd6764cd49bc3b0f75aa87db31f687a9c2b3e821d1428
readonly expected_cells_sha256=ce945b0e134dc6ca0b7089d3c439d00672d322a94f5efc8730bf18cb59fa4e72
readonly population_validator_sha256=e9769e1f46b04118d242fda5cd6f66f893065cd3163b410c90877ac43b0cd484
readonly results_validator_sha256=2f06da4497ea1dfd19c144b39b54a5db54f912b6d3f871f21e27fa781635753c
readonly evidence_validator_sha256=51d84298cb45d7e6ddec2cc9453db1147112aec7c6a24acf5f8342c9bd61a1ad
readonly strict_artifact_validator_sha256=b1d3a0ab1e686160f7da617f871a68bb93780b6463e0c57ec105350be43a465a
readonly denominator_sha256=dabeb9601aeee07c40dde62763eecc97c5184be70c0ff3fe3a90d3deddfdf723
readonly overlap_sha256=ba32abd030a58b52f4cd73cf4b6dd327c7437ab1c1324f37a37e8cbce29b8727
readonly complement_ptrace_sha256=5e90830fec51df5d99c072e0a121ea997634ef2a230a9b79833e667fe9bc1f64
readonly complement_kvm_sha256=57e02a11097863186b0055a06ecd9ebb9888e024d644ec9b7b855f66590454ab
readonly classified_complement_sha256=85d180b33c937b3fcc533d6a98f7ef58a9ad76afd8fe5c09bbeb46218deb91aa

fail() {
  printf 'KVM ratchet campaign: %s\n' "$*" >&2
  exit 2
}

allocated_bytes_for() {
  local path=$1 output rc
  output=$(du -s -B1 -- "$path" 2>/dev/null)
  rc=$?
  [[ $rc -eq 0 && "$output" =~ ^[0-9]+$'\t' ]] || return 1
  printf '%s' "${output%%$'\t'*}"
}

logical_bytes_for() {
  local path=$1 output rc
  output=$(du -s -B1 --apparent-size -- "$path" 2>/dev/null)
  rc=$?
  [[ $rc -eq 0 && "$output" =~ ^[0-9]+$'\t' ]] || return 1
  printf '%s' "${output%%$'\t'*}"
}

filesystem_free_bytes_for() {
  local path=$1 output rc
  output=$(df -PB1 -- "$path" 2>/dev/null)
  rc=$?
  [[ $rc -eq 0 ]] || return 1
  output=$(awk 'NR == 2 { print $4 }' <<<"$output")
  [[ "$output" =~ ^[0-9]+$ ]] || return 1
  printf '%s' "$output"
}

record_resource_guard_hit() {
  local guard_root=$1 scope=$2 reason=$3 observed=$4 limit=$5 path=$6
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$(date +%s)" "$scope" "$reason" "$observed" "$limit" "$path" \
    >>"$guard_root/resource-guard.tsv" || return 1
  : >"$guard_root/.resource-guard-tripped" || return 1
  /usr/bin/sync -f \
    "$guard_root/resource-guard.tsv" "$guard_root/.resource-guard-tripped" || return 1
}

resource_guard_once() {
  local guard_root=$1 scope=$2 scan_root=$3 budget=$4 reserve=$5 invocation_cap=$6 capture_cap=$7 retained_cap=$8 service_path=$9 service_cap=${10}
  local allocated logical free offender find_rc size
  [[ ! -e "$guard_root/.resource-guard-tripped" ]] || return 1
  allocated=$(allocated_bytes_for "$guard_root") || {
    record_resource_guard_hit "$guard_root" "$scope" probe-failure 0 "$budget" "$guard_root"
    return 1
  }
  logical=$(logical_bytes_for "$guard_root") || {
    record_resource_guard_hit "$guard_root" "$scope" probe-failure 0 "$budget" "$guard_root"
    return 1
  }
  free=$(filesystem_free_bytes_for "$guard_root") || {
    record_resource_guard_hit "$guard_root" "$scope" probe-failure 0 "$reserve" "$guard_root"
    return 1
  }
  if (( allocated > budget )); then
    record_resource_guard_hit "$guard_root" "$scope" campaign-budget "$allocated" "$budget" "$guard_root"
    return 1
  fi
  if (( logical > budget )); then
    record_resource_guard_hit "$guard_root" "$scope" campaign-logical-budget "$logical" "$budget" "$guard_root"
    return 1
  fi
  if (( free < reserve )); then
    record_resource_guard_hit "$guard_root" "$scope" filesystem-reserve "$free" "$reserve" "$guard_root"
    return 1
  fi
  if [[ -e "$service_path" ]]; then
    if [[ ! -f "$service_path" || -L "$service_path" ||
          $(stat -Lc '%h' "$service_path" 2>/dev/null || printf 0) != 1 ]]; then
      record_resource_guard_hit "$guard_root" "$scope" service-log-type 0 "$service_cap" "$service_path"
      return 1
    fi
    size=$(stat -Lc '%s' "$service_path" 2>/dev/null || printf INVALID)
    if [[ ! "$size" =~ ^[0-9]+$ ]]; then
      record_resource_guard_hit "$guard_root" "$scope" probe-failure 0 "$service_cap" "$service_path"
      return 1
    elif (( size >= service_cap )); then
      record_resource_guard_hit "$guard_root" "$scope" service-log-cap "$size" "$service_cap" "$service_path"
      return 1
    fi
  fi
  offender=$(find "$scan_root" -type f -name invocation.log -size +"${invocation_cap}"c -print -quit 2>/dev/null)
  find_rc=$?
  if [[ $find_rc -ne 0 ]]; then
    record_resource_guard_hit "$guard_root" "$scope" probe-failure 0 "$invocation_cap" "$scan_root"
    return 1
  elif [[ -n "$offender" ]]; then
    size=$(stat -Lc '%s' "$offender" 2>/dev/null || printf 0)
    record_resource_guard_hit "$guard_root" "$scope" invocation-log-cap "$size" "$invocation_cap" "$offender"
    return 1
  fi
  offender=$(find "$scan_root" -type f \
    \( -path '*/captures/verify-*.stdout' -o -path '*/captures/verify-*.stderr' \) \
    -size +"${capture_cap}"c -print -quit 2>/dev/null)
  find_rc=$?
  if [[ $find_rc -ne 0 ]]; then
    record_resource_guard_hit "$guard_root" "$scope" probe-failure 0 "$capture_cap" "$scan_root"
    return 1
  elif [[ -n "$offender" ]]; then
    size=$(stat -Lc '%s' "$offender" 2>/dev/null || printf 0)
    record_resource_guard_hit "$guard_root" "$scope" capture-file-cap "$size" "$capture_cap" "$offender"
    return 1
  fi
  offender=$(find "$scan_root" -type f \
    \( -name 'run1_log_*' -o -name 'run2_log_*' \) \
    -size +"${retained_cap}"c -print -quit 2>/dev/null)
  find_rc=$?
  if [[ $find_rc -ne 0 ]]; then
    record_resource_guard_hit "$guard_root" "$scope" probe-failure 0 "$retained_cap" "$scan_root"
    return 1
  elif [[ -n "$offender" ]]; then
    size=$(stat -Lc '%s' "$offender" 2>/dev/null || printf 0)
    record_resource_guard_hit "$guard_root" "$scope" retained-log-cap "$size" "$retained_cap" "$offender"
    return 1
  fi
  return 0
}

run_with_resource_watchdog() {
  local guard_root=$1 scope=$2 scan_root=$3 output_path=$4
  local budget=$5 reserve=$6 invocation_cap=$7 capture_cap=$8 retained_cap=$9 service_path=${10} service_cap=${11}
  shift 11
  local child_pid rc waited
  [[ ! -e "$guard_root/.resource-guard-tripped" ]] || return 125
  setsid "$@" >"$output_path" 2>&1 &
  child_pid=$!
  while kill -0 "$child_pid" 2>/dev/null; do
    if ! resource_guard_once \
        "$guard_root" "$scope" "$scan_root" "$budget" "$reserve" \
        "$invocation_cap" "$capture_cap" "$retained_cap" \
        "$service_path" "$service_cap"; then
      kill -TERM -- "-$child_pid" 2>/dev/null || true
      waited=0
      while kill -0 "$child_pid" 2>/dev/null && (( waited < 10 )); do
        sleep 1
        waited=$((waited + 1))
      done
      kill -KILL -- "-$child_pid" 2>/dev/null || true
      wait "$child_pid" 2>/dev/null || true
      return 125
    fi
    sleep 1
  done
  wait "$child_pid"
  rc=$?
  if ! resource_guard_once \
      "$guard_root" "$scope" "$scan_root" "$budget" "$reserve" \
      "$invocation_cap" "$capture_cap" "$retained_cap" \
      "$service_path" "$service_cap"; then
    return 125
  fi
  if [[ $rc -eq 153 ]]; then
    size=$(stat -Lc '%s' "$output_path" 2>/dev/null || printf 0)
    record_resource_guard_hit \
      "$guard_root" "$scope" process-file-limit "$size" \
      "$process_file_limit_bytes" "$output_path"
    return 125
  fi
  return "$rc"
}

resource_guard_hit_count() {
  awk 'END { print (NR > 0 ? NR - 1 : 0) }' "$1/resource-guard.tsv"
}

service_log_bytes_for() {
  local metadata type links size
  metadata=$(stat -Lc '%F|%h|%s' "$service_log_path" 2>/dev/null) || return 1
  IFS='|' read -r type links size <<<"$metadata"
  [[ "$type" == "regular file" && "$links" == 1 && "$size" =~ ^[0-9]+$ ]] ||
    return 1
  printf '%s' "$size"
}

capture_disk_budget_snapshot() {
  local guard_root=$1
  disk_snapshot_allocated=$(allocated_bytes_for "$guard_root") || return 1
  disk_snapshot_logical=$(logical_bytes_for "$guard_root") || return 1
  disk_snapshot_free=$(filesystem_free_bytes_for "$guard_root") || return 1
  disk_snapshot_service_size=$(service_log_bytes_for) || return 1
  disk_snapshot_hits=$(resource_guard_hit_count "$guard_root") || return 1
}

append_disk_budget_snapshot() {
  local guard_root=$1 phase=$2 status=$3
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$phase" "$disk_snapshot_allocated" "$disk_snapshot_logical" \
    "$disk_snapshot_free" "$disk_snapshot_service_size" \
    "$campaign_budget_bytes" "$filesystem_reserve_bytes" \
    "$service_log_max_bytes" "$disk_snapshot_hits" "$status" \
    >>"$guard_root/disk-budget.tsv"
}

record_disk_budget_snapshot() {
  local guard_root=$1 phase=$2 status=$3
  capture_disk_budget_snapshot "$guard_root" || return 1
  append_disk_budget_snapshot "$guard_root" "$phase" "$status"
}

run_campaign_command() {
  local scope=$1 scan_root=$2 output_path=$3
  shift 3
  [[ ! -e "$campaign/.resource-guard-tripped" ]] || return 125
  run_with_resource_watchdog \
    "$campaign" "$scope" "$scan_root" "$output_path" \
    "$campaign_budget_bytes" "$filesystem_reserve_bytes" \
    "$invocation_log_max_bytes" "$capture_file_max_bytes" \
    "$retained_log_max_bytes" "$service_log_path" "$service_log_max_bytes" "$@"
}

campaign_resource_guard_monitor() {
  while [[ ! -e "$campaign/.resource-guard-tripped" ]]; do
    sleep 1
    resource_guard_once \
      "$campaign" campaign-lifetime "$campaign" "$campaign_budget_bytes" \
      "$filesystem_reserve_bytes" "$invocation_log_max_bytes" \
      "$capture_file_max_bytes" "$retained_log_max_bytes" \
      "$service_log_path" "$service_log_max_bytes" || return 0
  done
}

stop_campaign_resource_guard_monitor() {
  if [[ -n ${campaign_guard_pid:-} ]]; then
    kill -TERM "$campaign_guard_pid" 2>/dev/null || true
    wait "$campaign_guard_pid" 2>/dev/null || true
    campaign_guard_pid=
  fi
}

require_resource_guard_clear() {
  [[ ! -e "$campaign/.resource-guard-tripped" ]] ||
    fail "resource guard tripped during $1; preserved evidence is incomplete"
}

file_sha256() {
  local line
  line=$(sha256sum -- "$1") || fail "cannot hash $1"
  printf '%s' "${line%% *}"
}

build_artifact_inventory_only() {
  local evidence_root=$1
  local results_root="$evidence_root/results"
  local paths sorted special path relative hash_line hash
  paths=$(mktemp "$evidence_root/.artifact-paths.XXXXXX") || return 1
  sorted=$(mktemp "$evidence_root/.artifact-paths-sorted.XXXXXX") || {
    rm -f -- "$paths"
    return 1
  }
  special=$(mktemp "$evidence_root/.artifact-special-nodes.XXXXXX") || {
    rm -f -- "$paths" "$sorted"
    return 1
  }
  : >"$evidence_root/artifact-hashes.stderr"
  if ! find "$results_root" ! -type d ! -type f -print \
      >"$special" 2>"$evidence_root/artifact-hashes.stderr"; then
    rm -f -- "$paths" "$sorted" "$special"
    return 1
  fi
  if [[ -s "$special" ]]; then
    printf 'retained results contain symlinks or special nodes:\n' \
      >>"$evidence_root/artifact-hashes.stderr"
    sed -n '1,20p' "$special" >>"$evidence_root/artifact-hashes.stderr"
    rm -f -- "$paths" "$sorted" "$special"
    return 1
  fi
  if ! find "$results_root" -type f -print0 \
      >"$paths" 2>>"$evidence_root/artifact-hashes.stderr" ||
     ! LC_ALL=C sort -z "$paths" >"$sorted" \
      2>>"$evidence_root/artifact-hashes.stderr"; then
    rm -f -- "$paths" "$sorted" "$special"
    return 1
  fi
  printf 'sha256\tpath\n' >"$evidence_root/artifact-hashes.tsv"
  while IFS= read -r -d '' path; do
    relative=${path#"$evidence_root/"}
    if [[ "$relative" == *$'\t'* || "$relative" == *$'\n'* ]]; then
      rm -f -- "$paths" "$sorted" "$special"
      return 1
    fi
    hash_line=$(sha256sum -- "$path" \
      2>>"$evidence_root/artifact-hashes.stderr") || {
      rm -f -- "$paths" "$sorted" "$special"
      return 1
    }
    hash=${hash_line%% *}
    [[ "$hash" =~ ^[0-9a-f]{64}$ ]] || {
      rm -f -- "$paths" "$sorted" "$special"
      return 1
    }
    printf '%s\t%s\n' "$hash" "$relative" \
      >>"$evidence_root/artifact-hashes.tsv"
  done <"$sorted"
  rm -f -- "$paths" "$sorted" "$special"
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
  require_sha256 "$expected_cells_path" "$expected_cells_sha256"
  require_sha256 "$population_validator_path" "$population_validator_sha256"
  require_sha256 "$results_validator_path" "$results_validator_sha256"
  require_sha256 "$evidence_validator_path" "$evidence_validator_sha256"
  require_sha256 "$strict_artifact_validator_path" "$strict_artifact_validator_sha256"
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
    --slurpfile expected "$expected_cells_path" \
    --slurpfile plan "$root/ci/expected-e2e-plan.json" \
    -f "$population_validator_path" \
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
  require_sha256 "$expected_cells_path" "$expected_cells_sha256"
  require_sha256 "$population_validator_path" "$population_validator_sha256"
  require_sha256 "$results_validator_path" "$results_validator_sha256"
  require_sha256 "$evidence_validator_path" "$evidence_validator_sha256"
  require_sha256 "$strict_artifact_validator_path" "$strict_artifact_validator_sha256"

  submodule_status_hash=$(git -C "$root" submodule status --recursive | sha256sum | awk '{print $1}')
  [[ "$submodule_status_hash" == "$submodule_status_sha256" ]] ||
    fail "submodule status changed: expected $submodule_status_sha256, got $submodule_status_hash"

  IFS= read -r image <"$root/ci/hermetic/image.digest"
  [[ "$image" == "$expected_image" ]] || fail "pinned image content changed"

  population_summary=$(jq --arg operation summary \
    --slurpfile expected "$expected_cells_path" \
    --slurpfile plan "$root/ci/expected-e2e-plan.json" \
    -f "$population_validator_path" \
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
  local leaked_summaries=$6
  [[ "$row_strict" == yes && "$artifacts_strict" == yes &&
     "$actual_rc" == 0 && "$expected_rc" == 0 && "$actual_rc" == "$expected_rc" &&
     "$infrastructure_fault_delta" == 0 && "$leaked_summaries" == 0 ]]
}

if [[ ${1:-} == --self-test-invocation-gate ]]; then
  [[ $# -eq 7 ]] || fail "--self-test-invocation-gate requires ROW_OK ARTIFACTS_OK ACTUAL_RC EXPECTED_RC INFRA_FAULT_DELTA LEAKED_SUMMARIES"
  if strict_invocation_eligible "$2" "$3" "$4" "$5" "$6" "$7"; then
    exit 0
  fi
  exit 1
fi

if [[ ${1:-} == --self-test-resource-guard ]]; then
  [[ $# -eq 10 ]] || fail "--self-test-resource-guard requires ROOT SCAN_ROOT BUDGET RESERVE INVOCATION_CAP CAPTURE_CAP RETAINED_CAP SERVICE_LOG SERVICE_LOG_CAP"
  resource_guard_once "$2" fixture "$3" "$4" "$5" "$6" "$7" "$8" "$9" "${10}"
  exit $?
fi

if [[ ${1:-} == --self-test-resource-watchdog ]]; then
  [[ $# -ge 13 && ${12} == -- ]] ||
    fail "--self-test-resource-watchdog requires ROOT SCAN_ROOT OUTPUT BUDGET RESERVE INVOCATION_CAP CAPTURE_CAP RETAINED_CAP SERVICE_LOG SERVICE_LOG_CAP -- COMMAND..."
  guard_root=$2
  scan_root=$3
  guard_output=$4
  guard_budget=$5
  guard_reserve=$6
  guard_invocation_cap=$7
  guard_capture_cap=$8
  guard_retained_cap=$9
  guard_service_log=${10}
  guard_service_log_cap=${11}
  shift 12
  run_with_resource_watchdog \
    "$guard_root" fixture "$scan_root" "$guard_output" \
    "$guard_budget" "$guard_reserve" "$guard_invocation_cap" \
    "$guard_capture_cap" "$guard_retained_cap" \
    "$guard_service_log" "$guard_service_log_cap" "$@"
  exit $?
fi

if [[ ${1:-} == --self-test-monitor-stop ]]; then
  [[ $# -eq 1 ]] || fail "--self-test-monitor-stop takes no additional arguments"
  (while :; do sleep 1; done) &
  campaign_guard_pid=$!
  stopped_pid=$campaign_guard_pid
  stop_campaign_resource_guard_monitor
  ! kill -0 "$stopped_pid" 2>/dev/null
  exit $?
fi

if [[ ${1:-} == --self-test-initial-disk-snapshot ]]; then
  [[ $# -eq 2 ]] ||
    fail "--self-test-initial-disk-snapshot requires FIXTURE_ROOT"
  fixture_root=$2
  probe_calls="$fixture_root/probe-calls"
  : >"$probe_calls"
  probe_sequence() {
    local metric=$1 first=$2 second=$3 count
    count=$(grep -c -x "$metric" "$probe_calls" 2>/dev/null || true)
    printf '%s\n' "$metric" >>"$probe_calls"
    if [[ $count -eq 0 ]]; then printf '%s' "$first"; else printf '%s' "$second"; fi
  }
  allocated_bytes_for() { probe_sequence allocated 4096 8192; }
  logical_bytes_for() { probe_sequence logical 2048 16384; }
  filesystem_free_bytes_for() { probe_sequence free 1099511627776 549755813888; }
  service_log_bytes_for() { probe_sequence service 123 456; }
  resource_guard_hit_count() { probe_sequence hits 0 7; }
  printf 'phase\tallocated_bytes\tlogical_bytes\tfilesystem_free_bytes\tservice_log_bytes\tcampaign_budget_bytes\tfilesystem_reserve_bytes\tservice_log_max_bytes\tguard_hits\tstatus\n' \
    >"$fixture_root/disk-budget.tsv"
  capture_disk_budget_snapshot "$fixture_root" || exit 1
  initial_campaign_allocated_bytes=$disk_snapshot_allocated
  initial_filesystem_free_bytes=$disk_snapshot_free
  append_disk_budget_snapshot "$fixture_root" initial clear || exit 1
  {
    printf 'initial_campaign_allocated_bytes=%s\n' "$initial_campaign_allocated_bytes"
    printf 'initial_filesystem_free_bytes=%s\n' "$initial_filesystem_free_bytes"
  } >"$fixture_root/environment.log"
  exit 0
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
[[ ${KVM_RATCHET_INPUT_SOURCE:-} == sealed-memfd ]] ||
  fail "dynamic campaign requires sealed memfd runtime inputs"
[[ ${KVM_RATCHET_FREEZE_MANIFEST_SHA256:-} =~ ^[0-9a-f]{64}$ ]] ||
  fail "dynamic campaign requires the captured freeze-manifest digest"
[[ "$service_log_path" == "$expected_service_log_path" ]] ||
  fail "dynamic campaign service log path is not the attributed unit log"
for runtime_input_path in \
  "$payload_path" \
  "$freeze_manifest_path" \
  "$expected_cells_path" \
  "$population_validator_path" \
  "$results_validator_path" \
  "$evidence_validator_path" \
  "$strict_artifact_validator_path"; do
  [[ "$runtime_input_path" =~ ^/proc/self/fd/[1-9][0-9]{2,}$ ]] ||
    fail "dynamic runtime input is not a reserved high-numbered inherited fd: $runtime_input_path"
done

if [[ ${1:-} == --internal-build-artifact-inventory ]]; then
  [[ $# -eq 2 ]] || fail "--internal-build-artifact-inventory requires EVIDENCE_ROOT"
  [[ "$2" == "$campaign" ]] || fail "artifact inventory target is not the campaign evidence root"
  build_artifact_inventory_only "$2" ||
    fail "cannot build the complete retained artifact inventory"
  exit 0
fi
[[ $# -eq 0 ]] || fail "usage: $0 [--static-check]"

read -r host_rlimit_fsize_soft host_rlimit_fsize_hard host_rlimit_fsize_units \
  < <(awk '$1 == "Max" && $2 == "file" && $3 == "size" { print $4, $5, $6 }' \
      /proc/self/limits)
[[ "$host_rlimit_fsize_soft" == "$process_file_limit_bytes" &&
   "$host_rlimit_fsize_hard" == "$process_file_limit_bytes" &&
   "$host_rlimit_fsize_units" == bytes ]] ||
  fail "worker RLIMIT_FSIZE is not the exact 1 GiB plus 176-byte marker hard per-file cap"

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

mapfile -t all_tests < <(jq -r '.[].test' "$expected_cells_path")
declare -A selector_for=()
while IFS=$'\t' read -r test_id selector; do
  selector_for["$test_id"]=$selector
done < <(jq -r '.[] | [.test,.selector] | @tsv' "$expected_cells_path")
[[ ${#all_tests[@]} -eq 119 && ${#selector_for[@]} -eq 119 ]] ||
  fail "frozen population did not load as 119 unique tests"

qualification_started=$(date +%s)
mkdir -p "$campaign" "$cargo_home" "$out/target"
printf 'timestamp_epoch_seconds\tscope\treason\tobserved_bytes\tlimit_bytes\tpath\n' \
  >"$campaign/resource-guard.tsv"
printf 'phase\tallocated_bytes\tlogical_bytes\tfilesystem_free_bytes\tservice_log_bytes\tcampaign_budget_bytes\tfilesystem_reserve_bytes\tservice_log_max_bytes\tguard_hits\tstatus\n' \
  >"$campaign/disk-budget.tsv"
disk_final_recorded=no
campaign_guard_pid=
finalize_disk_budget_on_exit() {
  local exit_rc=$?
  stop_campaign_resource_guard_monitor
  if [[ ${disk_final_recorded:-no} != yes && -d "$campaign" ]]; then
    final_status=aborted
    [[ -e "$campaign/.resource-guard-tripped" ]] && final_status=tripped
    record_disk_budget_snapshot "$campaign" final "$final_status" || true
  fi
  return "$exit_rc"
}
trap finalize_disk_budget_on_exit EXIT
capture_disk_budget_snapshot "$campaign" ||
  fail "cannot capture initial campaign disk budget"
initial_campaign_allocated_bytes=$disk_snapshot_allocated
initial_filesystem_free_bytes=$disk_snapshot_free
append_disk_budget_snapshot "$campaign" initial clear ||
  fail "cannot record initial campaign disk budget"
resource_guard_once \
  "$campaign" startup "$campaign" "$campaign_budget_bytes" \
  "$filesystem_reserve_bytes" "$invocation_log_max_bytes" \
  "$capture_file_max_bytes" "$retained_log_max_bytes" \
  "$service_log_path" "$service_log_max_bytes" ||
  fail "resource guard refused campaign startup"
campaign_resource_guard_monitor &
campaign_guard_pid=$!
printf '%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$campaign/.qualification-started"
printf 'phase\texpected_cells_sha256\tpopulation_validator_sha256\tresults_validator_sha256\tevidence_validator_sha256\tstrict_artifact_validator_sha256\n' \
  >"$campaign/frozen-input-checks.tsv"
printf 'phase\texpected_sha256\tobserved_sha256\nstartup\t%s\t%s\n' \
  "$payload_expected_sha256" "$payload_startup_sha256" \
  >"$campaign/payload-input-checks.tsv"
printf '%s\n' "$static_check_output" >"$campaign/static-population-check.json"
{
  printf 'test\tselector\n'
  jq -r '.[] | [.test,.selector] | @tsv' "$expected_cells_path"
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
  printf 'invocation_log_max_bytes=%s\n' "$invocation_log_max_bytes"
  printf 'capture_file_max_bytes=%s\n' "$capture_file_max_bytes"
  printf 'service_log_max_bytes=%s\n' "$service_log_max_bytes"
  printf 'process_file_limit_bytes=%s\n' "$process_file_limit_bytes"
  printf 'service_log_path=%s\n' "$service_log_path"
  printf 'host_rlimit_fsize_soft_bytes=%s\n' "$host_rlimit_fsize_soft"
  printf 'host_rlimit_fsize_hard_bytes=%s\n' "$host_rlimit_fsize_hard"
  printf 'campaign_budget_bytes=%s\n' "$campaign_budget_bytes"
  printf 'filesystem_reserve_bytes=%s\n' "$filesystem_reserve_bytes"
  printf 'initial_campaign_allocated_bytes=%s\n' "$initial_campaign_allocated_bytes"
  printf 'initial_filesystem_free_bytes=%s\n' "$initial_filesystem_free_bytes"
  printf 'prior_campaign_allocated_bytes=%s\n' "$prior_campaign_allocated_bytes"
  printf 'prior_campaign_results_allocated_bytes=%s\n' "$prior_campaign_results_allocated_bytes"
  printf 'retained_log_truncation_marker_bytes=176\n'
  printf 'retained_log_single_max_with_marker_bytes=1073742000\n'
  printf 'retained_log_theoretical_count=1190\n'
  printf 'retained_log_theoretical_ceiling_bytes=1277752980000\n'
  printf 'campaign_budget_policy=early-global-invalid-stop-not-full-theoretical-retention\n'
  printf 'resource_guard_policy=nominal-one-second-polling-process-group-stop-preserve-prefix-invalidate-campaign\n'
  printf 'resource_guard_scan_latency=synchronous-filesystem-scan-not-a-one-second-latency-guarantee\n'
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
  printf 'freeze_manifest_sha256=%s\n' "${KVM_RATCHET_FREEZE_MANIFEST_SHA256:-}"
  printf 'runtime_input_source=sealed-memfd\n'
  printf 'reviewed_input_trust=external-launch-and-freeze-digests-plus-argv-capsule-and-post-boundary-sealed-runtime-memfds\n'
  printf 'external_measurement_trust=with-proxy,systemd,python-stdlib,validate-lock,bash,jq,git,pinned-checkout-and-toolchain\n'
  printf 'harness_jobs=1\n'
  git -C "$root" submodule status --recursive
} >"$campaign/environment.log"
require_resource_guard_clear environment-evidence

environment_started=$(date +%s)
run_campaign_command pinned-environment "$campaign" "$campaign/pinned-environment.log" \
  "$root/ci/hermetic/run-in-pinned-root.sh" \
  --src "$root" --out "$out" -- \
  bash -c '
    set -euo pipefail
    /src/ci/hermetic/assert-no-network.sh
    printf "container_kernel=%s\n" "$(uname -r)"
    printf "container_kvm_type=%s\n" "$(stat -Lc %F /dev/kvm)"
    printf "container_kvm_mode=%s\n" "$(stat -Lc %a /dev/kvm)"
    read -r rlimit_soft rlimit_hard rlimit_units \
      < <(awk '\''$1 == "Max" && $2 == "file" && $3 == "size" { print $4, $5, $6 }'\'' \
          /proc/self/limits)
    printf "container_rlimit_fsize_soft_bytes=%s\n" "$rlimit_soft"
    printf "container_rlimit_fsize_hard_bytes=%s\n" "$rlimit_hard"
    [[ "$rlimit_soft" == 1073742000 && "$rlimit_hard" == 1073742000 &&
       "$rlimit_units" == bytes ]]
    [[ -c /dev/kvm && -r /dev/kvm && -w /dev/kvm ]]
    /lib64/ld-linux-x86-64.so.2 --version
  '
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
run_campaign_command fetch "$campaign" "$campaign/fetch.log" \
  /bin/bash -c '
    set -euo pipefail
    root=$1
    cargo_home=$2
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
  ' bash "$root" "$cargo_home"
fetch_rc=$?
fetch_finished=$(date +%s)
printf 'fetch\t%s\t%s\n' "$fetch_rc" "$((fetch_finished - fetch_started))" \
  >>"$campaign/phase-status.tsv"
[[ $fetch_rc -eq 0 ]] || fail "locked fetch failed; see $campaign/fetch.log"

build_started=$(date +%s)
run_campaign_command build "$campaign" "$campaign/build.log" \
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
  '
build_rc=$?
build_finished=$(date +%s)
printf 'build\t%s\t%s\n' "$build_rc" "$((build_finished - build_started))" \
  >>"$campaign/phase-status.tsv"
[[ $build_rc -eq 0 ]] || fail "lean exact-cell build failed; see $campaign/build.log"
[[ -x "$out/target/debug/test-harness" ]] || fail "test-harness was not built"
[[ -x "$out/target/debug/hermit-manifest-plan" ]] || fail "manifest planner was not built"
[[ -x "$out/target/release/hermit" ]] || fail "release Hermit was not built"
[[ -f "$out/target/release/hermit" && ! -L "$out/target/release/hermit" ]] ||
  fail "release Hermit is not a regular non-symlink file"
[[ $(stat -Lc '%h' "$out/target/release/hermit") == 1 ]] ||
  fail "release Hermit has an unexpected hardlink alias"
built_binary_sha256=$(file_sha256 "$out/target/release/hermit")
[[ "$built_binary_sha256" =~ ^[0-9a-f]{64}$ ]] ||
  fail "release Hermit hash is malformed"
printf 'sha256\tpath\n%s\tsplit/target/release/hermit\n' \
  "$built_binary_sha256" >"$campaign/built-binary.tsv"

manifest_validate_started=$(date +%s)
run_campaign_command manifest-validate "$campaign" "$campaign/manifest-validate.log" \
  "$root/ci/hermetic/run-in-pinned-root.sh" \
  --src "$root" --out "$out" --src-rw --cargo-home "$cargo_home" -- \
  bash -c '
    set -euo pipefail
    /src/ci/hermetic/assert-no-network.sh
    target/debug/test-harness validate
  '
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
run_campaign_command preparation "$campaign" "$campaign/preparation-wrapper.log" \
  /usr/bin/env \
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
    ' bash "${all_tests[@]}"
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
      .hermit-backend-engagement-summary-*)
        kind=backend-engagement
        infrastructure_faults=$((infrastructure_faults + 1))
        ;;
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
  run_campaign_command "round-$repetition:$test_id" "$result_dir" \
    "$result_dir/invocation.log" /usr/bin/env \
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
      ' bash "$selector" "$test_id"
  rc=$?
  finished=$(date +%s)
  preserve_leaked_summaries "$result_dir"
  if [[ -e "$campaign/.resource-guard-tripped" ]]; then
    result_rows=0
    if [[ -f "$result_dir/results.jsonl" ]]; then
      result_rows=$(jq -s 'length' "$result_dir/results.jsonl" 2>/dev/null || printf INVALID)
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\tno\t%s\t%s\n' \
      "$test_id" "$selector" "$repetition" "$rc" "$((finished - started))" \
      "$result_rows" "$leaked_summary_count" "$result_dir" \
      >>"$campaign/results/invocations.tsv"
    fail "resource guard stopped $test_id repetition $repetition after preserving partial evidence"
  fi
  result_rows=0
  invocation_strict=no
  if [[ -f "$result_dir/results.jsonl" ]] &&
     jq -s -e 'length > 0 and all(.[]; type == "object")' \
       "$result_dir/results.jsonl" >/dev/null 2>&1; then
    result_rows=$(jq -s 'length' "$result_dir/results.jsonl")
    if jq -s -e \
        --arg operation eligibility \
        --slurpfile expected "$expected_cells_path" \
        --arg source_sha "$expected_sha" \
        --arg machine "$machine" \
        --arg kernel "$kernel" \
        --arg campaign_prefix "$campaign_prefix" \
        --arg test "$test_id" \
        --argjson run_index "$repetition" \
        -f "$results_validator_path" \
        "$result_dir/results.jsonl" >/dev/null; then
      row_strict=yes
      if bash "$strict_artifact_validator_path" \
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
      "$((infrastructure_faults - infrastructure_faults_before))" \
      "$leaked_summary_count"; then
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
require_resource_guard_clear round-1-complete

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
require_resource_guard_clear round-2-complete
verify_frozen_inputs round-3
assert_source_clean round-3
for test_id in "${round1_eligible_tests[@]}"; do
  run_invocation "$test_id" "${selector_for[$test_id]}" 3
done
require_resource_guard_clear round-3-complete

resource_guard_once \
  "$campaign" final-aggregation "$campaign" "$campaign_budget_bytes" \
  "$filesystem_reserve_bytes" "$invocation_log_max_bytes" \
  "$capture_file_max_bytes" "$retained_log_max_bytes" \
  "$service_log_path" "$service_log_max_bytes" ||
  fail "resource guard stopped final evidence aggregation"
: >"$campaign/all-results.jsonl"
printf 'test\trun_index\tattempt\tresults_jsonl\tresults_sha256\tartifact_dir\tartifact_state\tverification_report\tverification_report_sha256\trun1_log\trun2_log\treport_staging_file\n' \
  >"$campaign/artifacts.tsv"
artifact_extract=$(mktemp "$campaign/.artifact-extract.XXXXXX")
while IFS=$'\t' read -r test_id selector repetition rc _elapsed result_rows _strict_single_pass _leaked_count result_dir; do
  [[ "$test_id" == test ]] && continue
  result_file="$result_dir/results.jsonl"
  if [[ ! -f "$result_file" ]]; then
    infrastructure_faults=$((infrastructure_faults + 1))
    continue
  fi
  if ! cat -- "$result_file" >>"$campaign/all-results.jsonl"; then
    record_resource_guard_hit \
      "$campaign" final-aggregation aggregation-write-failure 0 \
      "$process_file_limit_bytes" "$campaign/all-results.jsonl"
    fail "cannot append a complete invocation result during final aggregation"
  fi
  require_resource_guard_clear final-aggregation
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
       end),
      (if .attempts[0].status == null then "NONE"
       else (.attempts[0].status | tostring) end),
      (if .attempts[0].signal == null then "NONE"
       else (.attempts[0].signal | tostring) end)
    ] | @tsv' \
      "$result_file" >"$artifact_extract"; then
    infrastructure_faults=$((infrastructure_faults + 1))
    continue
  fi
  while IFS=$'\t' read -r row_test row_run_index attempt artifact_container report_hash artifact_state row_result report_verdict report_no_result_kind row_status row_signal; do
    if [[ "$artifact_container" == /results/runs/* ]]; then
      artifact_dir="$result_dir/${artifact_container#/results/}"
    else
      artifact_dir="$result_dir/INVALID_ARTIFACT_DIR"
      infrastructure_faults=$((infrastructure_faults + 1))
    fi
    log_dir="$artifact_dir/verify-logs/verify-1"
    report_staging_file=-
    shopt -s nullglob dotglob
    report_staging_candidates=("$artifact_dir"/.tmp*)
    shopt -u nullglob dotglob
    if [[ ${#report_staging_candidates[@]} -eq 1 ]]; then
      report_staging_file=${report_staging_candidates[0]}
      report_staging_basename=${report_staging_file##*/}
      if [[ ! "$report_staging_basename" =~ ^\.tmp[A-Za-z0-9]{6}$ ||
            ! -f "$report_staging_file" || -L "$report_staging_file" ||
            $(stat -Lc '%h' "$report_staging_file" 2>/dev/null || printf 0) != 1 ||
            "$row_result" != timeout || "$row_status" != NONE ||
            ( "$row_signal" != 9 && "$row_signal" != 15 ) ||
            ( "$artifact_state" != executed-no-report &&
              ! ( "$report_verdict" == no_result &&
                  "$report_no_result_kind" == not_run ) ) ]]; then
        infrastructure_faults=$((infrastructure_faults + 1))
      fi
    elif [[ ${#report_staging_candidates[@]} -gt 1 ]]; then
      report_staging_file=INVALID_MULTIPLE
      infrastructure_faults=$((infrastructure_faults + 1))
    fi
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
              if [[ "$report_no_result_kind" == first_run_rejected ]]; then
                if [[ "$row_status" == 125 && "$row_signal" == NONE ]]; then
                  [[ "$run2_log" == - && ${#log_entries[@]} -eq 1 ]] ||
                    infrastructure_faults=$((infrastructure_faults + 1))
                elif [[ "$row_status" == NONE &&
                        ( "$row_signal" == 9 || "$row_signal" == 15 ) ]]; then
                  [[ ${#log_entries[@]} -eq 1 || ${#log_entries[@]} -eq 2 ]] ||
                    infrastructure_faults=$((infrastructure_faults + 1))
                else
                  infrastructure_faults=$((infrastructure_faults + 1))
                fi
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
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$row_test" "$row_run_index" "$attempt" "$result_file" "$results_hash" \
      "$artifact_container" "$artifact_state" "$report_file" "$report_hash" \
      "$run1_log" "$run2_log" "$report_staging_file" \
      >>"$campaign/artifacts.tsv"
  done <"$artifact_extract"
done <"$campaign/results/invocations.tsv"
require_resource_guard_clear final-artifact-ledger
rm -f -- "$artifact_extract"

run_campaign_command final-hashing "$campaign" \
  "$campaign/artifact-inventory-wrapper.log" \
  /bin/bash "$payload_path" --internal-build-artifact-inventory "$campaign"
artifact_inventory_rc=$?
if [[ $artifact_inventory_rc -ne 0 ]]; then
  infrastructure_faults=$((infrastructure_faults + 1))
fi

resource_guard_once \
  "$campaign" final-hash-complete "$campaign" "$campaign_budget_bytes" \
  "$filesystem_reserve_bytes" "$invocation_log_max_bytes" \
  "$capture_file_max_bytes" "$retained_log_max_bytes" \
  "$service_log_path" "$service_log_max_bytes" ||
  fail "resource guard stopped final artifact hashing"

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
resource_guard_hits=$(resource_guard_hit_count "$campaign" || printf INVALID)
if [[ ! "$resource_guard_hits" =~ ^[0-9]+$ || "$resource_guard_hits" != 0 ]]; then
  infrastructure_faults=$((infrastructure_faults + 1))
fi
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
  printf 'resource_guard_hits=%s\n' "$resource_guard_hits"
  printf 'evidence_complete=%s\n' "$evidence_complete"
} >"$campaign/execution-complete.txt"

resource_guard_once \
  "$campaign" pre-validation "$campaign" "$campaign_budget_bytes" \
  "$filesystem_reserve_bytes" "$invocation_log_max_bytes" \
  "$capture_file_max_bytes" "$retained_log_max_bytes" \
  "$service_log_path" "$service_log_max_bytes" ||
  fail "resource guard stopped before final validation"
stop_campaign_resource_guard_monitor
record_disk_budget_snapshot "$campaign" final clear ||
  fail "cannot record final campaign disk budget"
disk_final_recorded=yes
validator_started=$(date +%s)
run_campaign_command final-validation "$campaign" \
  "$campaign/strict-validation-wrapper.log" \
  /bin/bash -c '
    KVM_QUALIFICATION_CAMPAIGN=$1 bash "$2" >"$3" 2>"$4"
  ' bash "$campaign" "$evidence_validator_path" \
  "$campaign/strict-validation.json" "$campaign/strict-validation.stderr"
validator_rc=$?
resource_guard_once \
  "$campaign" post-validation "$campaign" "$campaign_budget_bytes" \
  "$filesystem_reserve_bytes" "$invocation_log_max_bytes" \
  "$capture_file_max_bytes" "$retained_log_max_bytes" \
  "$service_log_path" "$service_log_max_bytes" || validator_rc=125
if [[ -e "$campaign/.resource-guard-tripped" ]]; then
  if [[ -e "$campaign/strict-validation.json" ]]; then
    mv -- "$campaign/strict-validation.json" \
      "$campaign/strict-validation.provisional-after-resource-trip.json"
  fi
  printf '%s\n' \
    '{"schema":2,"ok":false,"checks":{"resource_guard":false},"failure_reasons":["resource guard tripped during final validation; any provisional qualification is non-authoritative"]}' \
    >"$campaign/strict-validation.json"
  validator_rc=125
fi
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

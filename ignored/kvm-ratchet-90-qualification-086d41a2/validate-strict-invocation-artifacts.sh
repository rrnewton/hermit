#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
shopt -s nullglob dotglob

[[ $# -eq 2 ]] || {
  printf 'usage: %s RESULT_DIR RESULTS_JSONL\n' "$0" >&2
  exit 2
}
result_dir=$1
result_file=$2

fail() {
  printf 'strict invocation artifact check: %s\n' "$*" >&2
  exit 1
}

[[ -d "$result_dir" && ! -L "$result_dir" ]] || fail "result directory is absent or a symlink"
[[ -f "$result_file" && ! -L "$result_file" && -s "$result_file" ]] ||
  fail "results JSONL is absent, empty, or a symlink"
jq -s -e 'length == 1 and .[0].attempt == 1 and .[0].outcome == "PASS"' \
  "$result_file" >/dev/null || fail "result is not exactly one attempt-1 PASS row"

test_id=$(jq -r '.test' "$result_file")
run_id=$(jq -r '.run_id' "$result_file")
artifact_container=$(jq -r '.artifact_dir' "$result_file")
report_hash=$(jq -r '.attempts[0].verification_report_sha256' "$result_file")
slug=${test_id//\//-}
expected_container="/results/runs/$run_id/$slug-verify-kvm"
[[ "$artifact_container" == "$expected_container" ]] ||
  fail "artifact_dir does not match the first-attempt row identity"

artifact_dir="$result_dir/${artifact_container#/results/}"
[[ -d "$artifact_dir" && ! -L "$artifact_dir" ]] || fail "artifact directory is absent or a symlink"
[[ -z $(find "$artifact_dir" -type l -print -quit) ]] || fail "artifact tree contains a symlink"
artifact_root_entries=("$artifact_dir"/*)
[[ ${#artifact_root_entries[@]} -eq 9 ]] || fail "artifact root has an unexpected entry count"
for required_directory in home xdg-config tmp fixtures recording captures verify-logs workdir; do
  [[ -d "$artifact_dir/$required_directory" && ! -L "$artifact_dir/$required_directory" ]] ||
    fail "artifact root is missing producer directory $required_directory"
done
for artifact_root_entry in "${artifact_root_entries[@]}"; do
  case ${artifact_root_entry##*/} in
    home | xdg-config | tmp | fixtures | recording | captures | verify-logs | workdir)
      [[ -d "$artifact_root_entry" && ! -L "$artifact_root_entry" ]] ||
        fail "artifact root producer directory has the wrong type"
      ;;
    verify-1.json) ;;
    *) fail "strict PASS artifact root retained an unexpected entry" ;;
  esac
done
report_file="$artifact_dir/verify-1.json"
[[ -f "$report_file" && ! -L "$report_file" && -s "$report_file" ]] ||
  fail "verify-1.json is absent, empty, or a symlink"
[[ $(stat -Lc '%h' "$report_file") == 1 ]] || fail "verify-1.json is hardlinked"
hash_line=$(sha256sum -- "$report_file")
[[ ${hash_line%% *} == "$report_hash" ]] || fail "verify-1.json hash differs from the result row"
cmp -s "$report_file" <(jq -rj '.attempts[0].verification_report' "$result_file") ||
  fail "verify-1.json bytes differ from the embedded report"

log_dir="$artifact_dir/verify-logs/verify-1"
[[ -d "$log_dir" && ! -L "$log_dir" ]] || fail "retained-log directory is absent or a symlink"
log_entries=("$log_dir"/*)
run1_candidates=("$log_dir"/run1_log_*)
run2_candidates=("$log_dir"/run2_log_*)
[[ ${#log_entries[@]} -eq 2 &&
   ${#run1_candidates[@]} -eq 1 && ${#run2_candidates[@]} -eq 1 ]] ||
  fail "retained-log directory is not exactly {run1,run2}"
run1_log=${run1_candidates[0]}
run2_log=${run2_candidates[0]}
[[ -f "$run1_log" && ! -L "$run1_log" && -s "$run1_log" ]] || fail "run1 log is invalid"
[[ -f "$run2_log" && ! -L "$run2_log" && -s "$run2_log" ]] || fail "run2 log is invalid"
[[ $(stat -Lc '%h' "$run1_log") == 1 && $(stat -Lc '%h' "$run2_log") == 1 ]] ||
  fail "retained logs are hardlinked"
[[ ! "$run1_log" -ef "$run2_log" ]] || fail "retained logs alias the same inode"

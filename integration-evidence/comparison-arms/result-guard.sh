#!/usr/bin/env bash

# Return one of: pass, measured-nonpass, infrastructure-error.  Only strict
# canonical BitwiseInfoV1 matches are passes.  Product-attributed divergence or
# crash rows and explicitly typed timeout/OOM rows are measured nonpasses.  A
# readable row carrying an infrastructure/prerequisite/no-result failure is
# deliberately not a measurement.
classify_result_row() {
  jq -ser '
    def positive_integer:
      type == "number" and . > 0 and . == floor;
    def canonical_info_comparison($report):
      ($report | type) == "object"
      and $report.infrastructure_error == null
      and ($report.comparison | type) == "object"
      and $report.comparison.strictness == "canonical"
      and $report.comparison.display_name == "BitwiseInfoV1"
      and $report.comparison.compare_logs == true
      and $report.comparison.compare_io_buffers == true
      and $report.comparison.log_scope == "info"
      and $report.comparison.record_envelope == "all_records_v1"
      and $report.comparison.virtualize_time == true
      and $report.comparison.strip_lines == false
      and $report.comparison.canonicalize_addresses == true
      and $report.comparison.full_trace == true
      and $report.comparison.exact_remainder == true
      and $report.comparison.stripped_prefixes == ["real-wall-clock-prefix/v1"]
      and $report.comparison.canonicalizations == ["host-address-to-first-appearance-ordinal/v1"]
      and $report.comparison.ignore_lines == false
      and $report.comparison.skip_commit == false
      and $report.comparison.skip_detlog == false
      and ($report.compared_log_messages | type) == "object"
      and ($report.compared_log_messages.left | positive_integer)
      and ($report.compared_log_messages.right | positive_integer);
    if length != 1 then
      "infrastructure-error"
    else
      .[0] as $row
      | (try ($row.attempts[0].verification_report | fromjson) catch null) as $report
      | ((($row.attempts // []) | length) == 1) as $one_attempt
      | ((($row.relaxations // []) | length) == 0) as $no_relaxations
      | if $one_attempt and $no_relaxations
          and $row.outcome == "PASS"
          and $row.result == "pass"
          and $row.failure_class == null
          and $row.error_kind == null
          and $row.attempts[0].outcome == "PASS"
          and $row.attempts[0].error_kind == null
          and $row.attempts[0].status == 0
          and $row.attempts[0].signal == null
          and $row.attempts[0].timed_out == false
          and $report.verified == true
          and $report.bitwise_parity == true
          and $report.verdict == "matched"
          and canonical_info_comparison($report)
        then "pass"
        elif $one_attempt and $no_relaxations
          and $row.outcome == "FAIL"
          and $row.result == "determinism-failure"
          and $row.failure_class == "product_failure"
          and $row.error_kind == null
          and $row.attempts[0].outcome == "FAIL"
          and $row.attempts[0].error_kind == null
          and ($row.attempts[0].status | type) == "number"
          and $row.attempts[0].status != 0
          and $row.attempts[0].signal == null
          and $row.attempts[0].timed_out == false
          and $report.verified == false
          and $report.bitwise_parity == false
          and $report.verdict == "diverged"
          and canonical_info_comparison($report)
        then "measured-nonpass"
        elif $one_attempt and $no_relaxations
          and $row.outcome == "FAIL"
          and $row.result == "crash-error"
          and $row.failure_class == "product_failure"
          and $row.error_kind == null
          and $row.attempts[0].outcome == "FAIL"
          and $row.attempts[0].error_kind == null
          and $row.attempts[0].timed_out == false
          and ((($row.attempts[0].status | type) == "number" and $row.attempts[0].status != 0)
            or (($row.attempts[0].signal | type) == "number" and $row.attempts[0].signal != 0))
          and $report.verified == false
          and $report.bitwise_parity == false
          and $report.verdict == "no_result"
          and $report.infrastructure_error == null
        then "measured-nonpass"
        elif $one_attempt and $no_relaxations
          and $row.outcome == "ERROR"
          and $row.result == "timeout"
          and $row.failure_class == "no_result"
          and ($row.error_kind == "cpu-timeout" or $row.error_kind == "wall-timeout")
          and $row.attempts[0].outcome == "ERROR"
          and $row.attempts[0].error_kind == $row.error_kind
          and $row.attempts[0].timed_out == true
          and $report.verified == false
          and $report.bitwise_parity == false
          and $report.verdict == "no_result"
          and $report.infrastructure_error == null
          and $report.comparison == null
          and $report.compared_log_messages == null
        then "measured-nonpass"
        elif $one_attempt and $no_relaxations
          and $row.outcome == "ERROR"
          and $row.result == "oom"
          and $row.failure_class == "no_result"
          and $row.error_kind == "oom"
          and $row.attempts[0].outcome == "ERROR"
          and $row.attempts[0].error_kind == "oom"
          and $row.attempts[0].timed_out == false
          and ((($row.attempts[0].status | type) == "number" and $row.attempts[0].status != 0)
            or (($row.attempts[0].signal | type) == "number" and $row.attempts[0].signal != 0))
          and $report.verified == false
          and $report.bitwise_parity == false
          and $report.verdict == "no_result"
          and $report.infrastructure_error == null
        then "measured-nonpass"
        else "infrastructure-error"
        end
    end
  ' "$1"
}

invocation_verdict() {
  local command_rc=$1 result_class=$2 rows=$3
  [[ $rows -eq 1 ]] || return 2
  case "$result_class:$command_rc" in
    pass:0) return 0 ;;
    measured-nonpass:1) return 1 ;;
    *) return 2 ;;
  esac
}

run_result_guard_self_tests() {
  local canonical_report divergent_report no_result_report pass_row row actual verdict_rc
  canonical_report=$(jq -cn '{
    verified:true, bitwise_parity:true, verdict:"matched", infrastructure_error:null,
    comparison:{strictness:"canonical",display_name:"BitwiseInfoV1",compare_logs:true,
      compare_io_buffers:true,log_scope:"info",record_envelope:"all_records_v1",
      virtualize_time:true,strip_lines:false,canonicalize_addresses:true,
      full_trace:true,exact_remainder:true,
      stripped_prefixes:["real-wall-clock-prefix/v1"],
      canonicalizations:["host-address-to-first-appearance-ordinal/v1"],
      ignore_lines:false,skip_commit:false,skip_detlog:false},
    compared_log_messages:{left:1,right:1}}')
  divergent_report=$(jq -cn --argjson base "$canonical_report" \
    '$base | .verified=false | .bitwise_parity=false | .verdict="diverged"')
  no_result_report=$(jq -cn '{verified:false,bitwise_parity:false,verdict:"no_result",
    infrastructure_error:null,comparison:null,compared_log_messages:null}')
  pass_row=$(jq -cn --arg report "$canonical_report" '{
    outcome:"PASS",result:"pass",failure_class:null,error_kind:null,relaxations:[],
    attempts:[{index:"1",outcome:"PASS",error_kind:null,status:0,signal:null,
      timed_out:false,verification_report:$report}]}')

  actual=$(classify_result_row <(printf '%s\n' "$pass_row"))
  guard_eq strict-pass-control "$actual" pass
  invocation_verdict 0 "$actual" 1 || die "strict PASS/rc0 positive control failed"

  row=$(jq -cn --argjson base "$pass_row" --arg report "$divergent_report" \
    '$base | .outcome="FAIL" | .result="determinism-failure"
     | .failure_class="product_failure" | .attempts[0].outcome="FAIL"
     | .attempts[0].status=1 | .attempts[0].verification_report=$report')
  actual=$(classify_result_row <(printf '%s\n' "$row"))
  guard_eq deterministic-nonpass-control "$actual" measured-nonpass
  invocation_verdict 1 "$actual" 1
  verdict_rc=$?
  [[ $verdict_rc -eq 1 ]] || die "determinism failure/rc1 control failed"

  row=$(jq -cn --argjson base "$pass_row" --arg report "$no_result_report" '
    $base | .outcome="ERROR" | .result="timeout" | .failure_class="no_result"
    | .error_kind="cpu-timeout" | .attempts[0].outcome="ERROR"
    | .attempts[0].error_kind="cpu-timeout" | .attempts[0].status=null
    | .attempts[0].signal=15 | .attempts[0].timed_out=true
    | .attempts[0].verification_report=$report')
  actual=$(classify_result_row <(printf '%s\n' "$row"))
  guard_eq typed-timeout-control "$actual" measured-nonpass
  invocation_verdict 1 "$actual" 1
  verdict_rc=$?
  [[ $verdict_rc -eq 1 ]] || die "typed timeout/rc1 control failed"

  row=$(jq -cn --argjson base "$pass_row" --arg report "$no_result_report" '
    $base | .outcome="ERROR" | .result="oom" | .failure_class="no_result"
    | .error_kind="oom" | .attempts[0].outcome="ERROR"
    | .attempts[0].error_kind="oom" | .attempts[0].status=null
    | .attempts[0].signal=9 | .attempts[0].timed_out=false
    | .attempts[0].verification_report=$report')
  actual=$(classify_result_row <(printf '%s\n' "$row"))
  guard_eq typed-oom-control "$actual" measured-nonpass

  for mutation in \
    bitwise-pass \
    zero-info-count \
    noncanonical-pass \
    infrastructure-error \
    prerequisite-error \
    incomplete-verification \
    untyped-error; do
    case "$mutation" in
      bitwise-pass)
        row=$(jq -cn --argjson base "$pass_row" '
          $base | .attempts[0].verification_report=(
            (.attempts[0].verification_report | fromjson)
            | .bitwise_parity=false | tojson)') ;;
      zero-info-count)
        row=$(jq -cn --argjson base "$pass_row" '
          $base | .attempts[0].verification_report=(
            (.attempts[0].verification_report | fromjson)
            | .compared_log_messages.left=0 | tojson)') ;;
      noncanonical-pass)
        row=$(jq -cn --argjson base "$pass_row" '
          $base | .attempts[0].verification_report=(
            (.attempts[0].verification_report | fromjson)
            | .comparison.strictness="stripped" | tojson)') ;;
      infrastructure-error)
        row=$(jq -cn --argjson base "$pass_row" --arg report "$no_result_report" '
          $base | .outcome="ERROR" | .result=null
          | .failure_class="understood_infrastructure_failure"
          | .error_kind="infrastructure" | .attempts[0].outcome="ERROR"
          | .attempts[0].error_kind="infrastructure" | .attempts[0].status=1
          | .attempts[0].verification_report=$report') ;;
      prerequisite-error)
        row=$(jq -cn --argjson base "$pass_row" --arg report "$no_result_report" '
          $base | .outcome="ERROR" | .result=null
          | .failure_class="understood_prerequisite_failure"
          | .error_kind="backend-unavailable" | .attempts[0].outcome="ERROR"
          | .attempts[0].error_kind="backend-unavailable" | .attempts[0].status=1
          | .attempts[0].verification_report=$report') ;;
      incomplete-verification)
        row=$(jq -cn --argjson base "$pass_row" --arg report "$no_result_report" '
          $base | .outcome="ERROR" | .result=null | .failure_class="no_result"
          | .error_kind="incomplete-verification-evidence"
          | .attempts[0].outcome="ERROR"
          | .attempts[0].error_kind="incomplete-verification-evidence"
          | .attempts[0].status=1 | .attempts[0].verification_report=$report') ;;
      untyped-error)
        row=$(jq -cn --argjson base "$pass_row" --arg report "$no_result_report" '
          $base | .outcome="ERROR" | .result=null | .failure_class="no_result"
          | .error_kind=null | .attempts[0].outcome="ERROR"
          | .attempts[0].status=1 | .attempts[0].verification_report=$report') ;;
    esac
    actual=$(classify_result_row <(printf '%s\n' "$row"))
    guard_eq "$mutation-classification" "$actual" infrastructure-error
    invocation_verdict 1 "$actual" 1
    verdict_rc=$?
    [[ $verdict_rc -eq 2 ]] || die "$mutation could produce a complete measurement"
    printf '%s\trejected-as-measurement-infrastructure\n' "$mutation"
  done

  for mutation in "1 pass 1" "0 measured-nonpass 1" "0 pass 2" "2 measured-nonpass 1"; do
    read -r mutation_rc mutation_class mutation_rows <<<"$mutation"
    invocation_verdict "$mutation_rc" "$mutation_class" "$mutation_rows"
    verdict_rc=$?
    [[ $verdict_rc -eq 2 ]] || die "result/exit consistency mutation was not rejected: $mutation"
    printf '%s\trejected\n' "$mutation"
  done
}

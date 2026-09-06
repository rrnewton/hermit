def report:
  try (.attempts[0].verification_report | fromjson) catch null;

def hex64:
  try ((type == "string") and test("^[0-9a-f]{64}$")) catch false;

def nonnegative_integer:
  type == "number" and . >= 0 and . == floor;

def positive_integer:
  type == "number" and . > 0 and . == floor;

def expected_run_id($prefix):
  $prefix + "-" + (.test | gsub("/"; "-")) +
  "-repetition-" + (.run_index | tostring);

def expected_artifact_dir($prefix):
  "/results/runs/" + expected_run_id($prefix) + "/" +
  (.test | gsub("/"; "-")) + "-verify-kvm" +
  (if .attempt == 1 then "" else "-attempt-" + (.attempt | tostring) end);

def provenance_ok($sha; $machine; $kernel):
  try (
    .schema == 4 and
    .hermit_sha == $sha and
    .source_tree_dirty == false and
    .binary_build_sha == ($sha[0:12]) and
    (.binary_sha256 | hex64) and
    (.test_sha256 | hex64) and
    .machine_shortname == $machine and
    .kernel_version == $kernel and
    .host_capabilities.kvm.present == true
  ) catch false;

def classification_ok($cells):
  . as $row |
  ([$cells[]
    | select(
        .lane == $row.lane and
        .category == $row.category and
        .test == $row.test and
        .mode == $row.mode and
        .backend == $row.backend
      )]) as $matches |
  try (
    ($matches | length) == 1 and
    $row.classification ==
      (if $matches[0].selector == "--probe-disabled"
       then "disabled"
       elif $matches[0].selector == "--include-manual"
       then "required"
       else "INVALID"
       end)
  ) catch false;

def fixed_command_args_ok:
  . as $row |
  $row.artifact_dir as $artifact |
  {
    "E2E_FIXTURE_DIR": ($artifact + "/fixtures"),
    "E2E_TMPDIR": "/tmp/hermit-e2e",
    "HERMIT_E2E_SCHEDULED_JOBS": "1",
    "HOME": ($artifact + "/home"),
    "LC_ALL": "C",
    "TZ": "UTC",
    "XDG_CONFIG_HOME": ($artifact + "/xdg-config")
  } as $expected_env |
  [
    "--log", "info", "run",
    "--base-env=minimal",
    "--backend", "kvm",
    "--strict", "--verify-strict", "--verify",
    "--verify-json", ($artifact + "/verify-1.json"),
    "--keep-logs",
    "--verify-log-dir", ($artifact + "/verify-logs/verify-1"),
    "--mount=type=tmpfs,target=/test",
    "--workdir", "/test",
    "--env", "LC_ALL=C",
    "--env", "TZ=UTC",
    "--env", ("HOME=" + $artifact + "/home"),
    "--env", ("XDG_CONFIG_HOME=" + $artifact + "/xdg-config"),
    "--env", "E2E_TMPDIR=/test",
    "--env", ("E2E_FIXTURE_DIR=" + $artifact + "/fixtures"),
    "--env", "HERMIT_E2E_SCHEDULED_JOBS=1"
  ] as $expected_pre_delimiter |
  $row.cwd == "/src" and
  $row.env == $expected_env and
  ($row.guest_argv | type) == "array" and
  ($row.guest_argv | length) > 0 and
  $row.argv ==
    (["/src/target/release/hermit"] + $expected_pre_delimiter +
     ["--"] + $row.guest_argv);

def row_disposition_fields_present:
  has("outcome") and
  has("result") and
  has("failure_class") and
  has("error_kind") and
  has("reason");

def attempt_disposition_fields_present:
  (.attempts[0] | has("outcome")) and
  (.attempts[0] | has("error_kind")) and
  (.attempts[0] | has("status")) and
  (.attempts[0] | has("signal")) and
  (.attempts[0] | has("timed_out")) and
  (.attempts[0] | has("reason"));

def report_disposition_fields_present:
  (report) as $report |
  ($report | has("infrastructure_error")) and
  ($report | has("verified")) and
  ($report | has("bitwise_parity")) and
  ($report | has("verdict")) and
  ($report | has("comparison")) and
  ($report | has("compared_log_messages")) and
  ($report | has("guest_exit_code")) and
  ($report | has("guest_signal"));

def row_divergence_fields_present:
  has("first_divergent_scheduler_turn") and
  has("first_divergent_virtual_nanoseconds") and
  has("first_divergent_record") and
  has("first_divergent_syscall") and
  has("first_divergent_left_message") and
  has("first_divergent_right_message");

def attempt_divergence_fields_present:
  (.attempts[0] | has("first_divergent_scheduler_turn")) and
  (.attempts[0] | has("first_divergent_virtual_nanoseconds")) and
  (.attempts[0] | has("first_divergent_record")) and
  (.attempts[0] | has("first_divergent_syscall")) and
  (.attempts[0] | has("first_divergent_left_message")) and
  (.attempts[0] | has("first_divergent_right_message"));

def report_divergence_fields_present:
  (report) as $report |
  ($report | has("first_divergent_scheduler_turn")) and
  ($report | has("first_divergent_virtual_nanoseconds")) and
  ($report | has("first_divergent_record")) and
  ($report | has("first_divergent_syscall")) and
  ($report | has("first_divergent_left_message")) and
  ($report | has("first_divergent_right_message"));

def report_divergence_fields_typed:
  (report) as $report |
  (($report.first_divergent_scheduler_turn == null) or
   ($report.first_divergent_scheduler_turn | nonnegative_integer)) and
  (($report.first_divergent_virtual_nanoseconds == null) or
   ($report.first_divergent_virtual_nanoseconds | nonnegative_integer)) and
  (($report.first_divergent_record == null) or
   ($report.first_divergent_record | positive_integer)) and
  (($report.first_divergent_syscall == null) or
   ($report.first_divergent_syscall | nonnegative_integer)) and
  (($report.first_divergent_left_message == null) or
   ($report.first_divergent_left_message | type) == "string") and
  (($report.first_divergent_right_message == null) or
   ($report.first_divergent_right_message | type) == "string");

def report_divergence_relationships_ok:
  (report) as $report |
  (if $report.first_divergent_record == null
   then ($report.first_divergent_left_message == null and
         $report.first_divergent_right_message == null)
   else (((($report.first_divergent_left_message | type) == "string" and
           ($report.first_divergent_left_message | length) > 0) or
          (($report.first_divergent_right_message | type) == "string" and
           ($report.first_divergent_right_message | length) > 0)))
   end) and
  (if $report.first_divergent_virtual_nanoseconds == null
   then true
   else $report.first_divergent_scheduler_turn != null
   end);

def row_has_no_divergence:
  row_divergence_fields_present and
  .first_divergent_scheduler_turn == null and
  .first_divergent_virtual_nanoseconds == null and
  .first_divergent_record == null and
  .first_divergent_syscall == null and
  .first_divergent_left_message == null and
  .first_divergent_right_message == null;

def report_has_no_divergence:
  (report) as $report |
  report_divergence_fields_present and
  $report.first_divergent_scheduler_turn == null and
  $report.first_divergent_virtual_nanoseconds == null and
  $report.first_divergent_record == null and
  $report.first_divergent_syscall == null and
  $report.first_divergent_left_message == null and
  $report.first_divergent_right_message == null;

def attempt_has_no_divergence:
  attempt_divergence_fields_present and
  .attempts[0].first_divergent_scheduler_turn == null and
  .attempts[0].first_divergent_virtual_nanoseconds == null and
  .attempts[0].first_divergent_record == null and
  .attempts[0].first_divergent_syscall == null and
  .attempts[0].first_divergent_left_message == null and
  .attempts[0].first_divergent_right_message == null;

def row_attempt_report_divergence_matches:
  . as $row |
  .attempts[0] as $attempt |
  (report) as $report |
  row_divergence_fields_present and
  attempt_divergence_fields_present and
  report_divergence_fields_present and
  report_divergence_fields_typed and
  report_divergence_relationships_ok and
  $row.first_divergent_scheduler_turn == $report.first_divergent_scheduler_turn and
  $attempt.first_divergent_scheduler_turn == $report.first_divergent_scheduler_turn and
  $row.first_divergent_virtual_nanoseconds == $report.first_divergent_virtual_nanoseconds and
  $attempt.first_divergent_virtual_nanoseconds == $report.first_divergent_virtual_nanoseconds and
  $row.first_divergent_record == $report.first_divergent_record and
  $attempt.first_divergent_record == $report.first_divergent_record and
  $row.first_divergent_syscall == $report.first_divergent_syscall and
  $attempt.first_divergent_syscall == $report.first_divergent_syscall and
  $row.first_divergent_left_message == $report.first_divergent_left_message and
  $attempt.first_divergent_left_message == $report.first_divergent_left_message and
  $row.first_divergent_right_message == $report.first_divergent_right_message and
  $attempt.first_divergent_right_message == $report.first_divergent_right_message;

def row_attempt_have_no_divergence:
  row_has_no_divergence and attempt_has_no_divergence;

def row_attempt_report_have_no_divergence:
  row_attempt_have_no_divergence and report_has_no_divergence and
  row_attempt_report_divergence_matches;

def attempt_has_nonzero_disposition:
  ((.attempts[0].status | positive_integer) and
   .attempts[0].signal == null)
  or
  (.attempts[0].status == null and
   (.attempts[0].signal | positive_integer));

def attempt_has_completed_disposition:
  ((.attempts[0].status | nonnegative_integer) and
   .attempts[0].signal == null)
  or
  (.attempts[0].status == null and
   (.attempts[0].signal | positive_integer));

def attempt_has_runner_timeout_signal:
  .attempts[0].status == null and
  (.attempts[0].signal == 9 or .attempts[0].signal == 15);

def report_has_completed_guest_disposition:
  (report) as $report |
  (($report.guest_exit_code | nonnegative_integer) and
   $report.guest_signal == null)
  or
  ($report.guest_exit_code == null and
   ($report.guest_signal | positive_integer));

def executed_resource_evidence_ok:
  has("duration_ms") and
  has("cpu_usage_usec") and
  (.duration_ms | nonnegative_integer) and
  (.cpu_usage_usec | nonnegative_integer) and
  (.attempts[0] | has("duration_ms")) and
  (.attempts[0] | has("cpu_usage_usec")) and
    (.attempts[0].duration_ms | nonnegative_integer) and
    (.attempts[0].cpu_usage_usec | nonnegative_integer) and
    .duration_ms >= .attempts[0].duration_ms and
    .cpu_usage_usec >= .attempts[0].cpu_usage_usec;

def expected_timeout_reason($before_attempt):
  (if .error_kind == "wall-timeout"
   then "cell exceeded " + (.execution_wall_timeout_seconds | tostring) +
        " wall s backstop (" + (.execution_cpu_timeout_seconds | tostring) +
        " s CPU budget)"
   else "cell exceeded " + (.execution_cpu_timeout_seconds | tostring) + " CPU s"
   end) +
  (if $before_attempt then " before attempt 1 started" else "" end);

def runtime_stats_ok:
  try (
    type == "object" and
    ([keys[] |
      select(. != "scheduler_turns" and
             . != "virtual_nanoseconds" and
             . != "syscalls")] | length) == 0 and
    has("scheduler_turns") and
    has("virtual_nanoseconds") and
    (.scheduler_turns | nonnegative_integer) and
    (.virtual_nanoseconds | nonnegative_integer) and
    ((has("syscalls") | not) or (.syscalls | nonnegative_integer))
  ) catch false;

def verification_runtime_ok:
  try (
    type == "object" and
    length > 0 and
    ([keys[] | select(. != "run1" and . != "run2")] | length) == 0 and
    ((has("run1") | not) or (.run1 | runtime_stats_ok)) and
    ((has("run2") | not) or (.run2 | runtime_stats_ok))
  ) catch false;

def fixed_verify_auxiliary_evidence_ok:
  try (
    has("runtime") and
    has("execution_path") and
    has("diversity") and
    .execution_path == null and
    .diversity == null and
    (.attempts[0] | has("runtime")) and
    (.attempts[0] | has("observation_sha256")) and
    (.attempts[0] | has("sabre_path_evidence")) and
    (.attempts[0] | has("sabre_path_evidence_sha256")) and
    (.attempts[0].observation_sha256 | hex64) and
    .attempts[0].sabre_path_evidence == null and
    .attempts[0].sabre_path_evidence_sha256 == null and
    .runtime == .attempts[0].runtime and
    (if .attempts[0].verification_report == null
     then .runtime == null
     else ((report) | has("dbt_counted_branches") | not) and
          (if ((report) | has("runtime"))
           then ((report).runtime | verification_runtime_ok) and
                .runtime == (report).runtime
           else .runtime == null
           end)
     end)
  ) catch false;

def verification_report_serialization_ok:
  if .attempts[0].verification_report == null then true
  elif (.attempts[0].timed_out == true and
        .attempts[0].status == null and
        .attempts[0].signal == null)
  then (([.attempts[0].verification_report | scan("\n")] | length) == 0)
  else (.attempts[0].verification_report | endswith("\n")) and
       (([.attempts[0].verification_report | scan("\n")] | length) == 1)
  end;

def invocation_ok($prefix):
  try (
    (.attempt | type) == "number" and
    .attempt >= 1 and .attempt == (.attempt | floor) and
    (.run_index == 1 or .run_index == 2 or .run_index == 3) and
    .run_id == expected_run_id($prefix) and
    .artifact_dir == expected_artifact_dir($prefix) and
    .lane == "portable" and
    .category == (.test | split("/")[0]) and
    .mode == "verify" and
    .backend == "kvm" and
    .log_level == "info" and
    .relaxations == [] and
    (.argv | type == "array" and length > 0) and
    (.guest_argv | type == "array" and length > 0) and
    (.env | type == "object" and length > 0) and
    (.cwd | type == "string" and length > 0) and
    (.shell_command | type == "string" and length > 0) and
    .effective_args == .argv[1:] and
    fixed_command_args_ok and
    (.effective_args | index("--strict") != null) and
    (.effective_args | index("--verify") != null) and
    (.effective_args | index("--verify-strict") != null) and
    (.effective_args | index("--mount=type=tmpfs,target=/test") != null) and
    (.effective_args | index("/test") != null) and
    (.execution_cpu_timeout_seconds | positive_integer) and
    (.execution_wall_timeout_seconds | positive_integer) and
    (.timeout_seconds | positive_integer) and
    .execution_cpu_timeout_seconds == 22 and
    .execution_wall_timeout_seconds == 57 and
    .timeout_seconds == 57 and
    (.attempts | length == 1) and
    .attempts[0].index == "1" and
    .attempts[0].argv == .argv and
    .attempts[0].guest_argv == .guest_argv and
    .attempts[0].env == .env and
    .attempts[0].cwd == .cwd and
    .attempts[0].shell_command == .shell_command and
    fixed_verify_auxiliary_evidence_ok and
    verification_report_serialization_ok and
    (.attempts[0] | has("verification_report")) and
    (.attempts[0] | has("verification_report_sha256")) and
    (
      ((.attempts[0].verification_report | type) == "string" and
       (.attempts[0].verification_report | length) > 0 and
       (.attempts[0].verification_report_sha256 | hex64) and
       (report != null))
      or
      (.attempts[0].verification_report == null and
       .attempts[0].verification_report_sha256 == null)
    )
  ) catch false;

def canonical_comparison_structure_ok:
  try (
    (report) as $report |
    report_disposition_fields_present and
    $report.infrastructure_error == null and
    $report.comparison.strictness == "canonical" and
    $report.comparison.display_name == "BitwiseInfoV1" and
    $report.comparison.compare_logs == true and
    $report.comparison.compare_io_buffers == true and
    $report.comparison.log_scope == "info" and
    $report.comparison.record_envelope == "all_records_v1" and
    $report.comparison.virtualize_time == true and
    $report.comparison.strip_lines == false and
    $report.comparison.canonicalize_addresses == true and
    $report.comparison.full_trace == true and
    $report.comparison.exact_remainder == true and
    $report.comparison.stripped_prefixes == ["real-wall-clock-prefix/v1"] and
    $report.comparison.canonicalizations == ["host-address-to-first-appearance-ordinal/v1"] and
    $report.comparison.ignore_lines == false and
    $report.comparison.skip_commit == false and
    $report.comparison.skip_detlog == false and
    ($report.compared_log_messages.left | nonnegative_integer) and
    ($report.compared_log_messages.right | nonnegative_integer)
  ) catch false;

def canonical_comparison_ok:
  canonical_comparison_structure_ok and
  ((report).compared_log_messages.left | positive_integer) and
  ((report).compared_log_messages.right | positive_integer);

def canonical_match_counts_equal:
  (report).compared_log_messages.left ==
    (report).compared_log_messages.right;

def timeout_divergence_coordinates_match_counts:
  (report) as $report |
  if $report.verdict != "diverged" then true
  elif ($report.compared_log_messages.left == 0 and
        $report.compared_log_messages.right == 0)
  then row_attempt_report_have_no_divergence
  elif ($report.compared_log_messages.left == 0 and
        $report.compared_log_messages.right > 0)
  then ($report.first_divergent_record | positive_integer) and
       $report.first_divergent_left_message == null and
       (($report.first_divergent_right_message | type) == "string" and
        ($report.first_divergent_right_message | length) > 0)
  elif ($report.compared_log_messages.left > 0 and
        $report.compared_log_messages.right == 0)
  then ($report.first_divergent_record | positive_integer) and
       (($report.first_divergent_left_message | type) == "string" and
        ($report.first_divergent_left_message | length) > 0) and
       $report.first_divergent_right_message == null
  else true
  end;

def canonical_pass_ok:
  try (
    (report) as $report |
    row_disposition_fields_present and
    attempt_disposition_fields_present and
    report_disposition_fields_present and
    .outcome == "PASS" and
    .result == "pass" and
    .failure_class == null and
    .error_kind == null and
    .reason == null and
    .attempts[0].outcome == "PASS" and
    .attempts[0].error_kind == null and
    .attempts[0].status == 0 and
    .attempts[0].signal == null and
    .attempts[0].timed_out == false and
    .attempts[0].reason == null and
    executed_resource_evidence_ok and
    row_attempt_report_have_no_divergence and
    canonical_comparison_ok and
    canonical_match_counts_equal and
    $report.verified == true and
    $report.bitwise_parity == true and
    $report.verdict == "matched" and
    ($report | has("no_result_reason") | not) and
    $report.guest_exit_code == 0 and
    $report.guest_signal == null
  ) catch false;

def strict_pass_ok:
  .attempt == 1 and canonical_pass_ok;

def typed_first_run_rejected_report_ok:
  try (
    (report) as $report |
    report_disposition_fields_present and
    row_attempt_report_have_no_divergence and
    $report.infrastructure_error == null and
    $report.verified == false and
    $report.bitwise_parity == false and
    $report.verdict == "no_result" and
    ($report.no_result_reason | type) == "object" and
    ($report.no_result_reason | has("kind")) and
    ($report.no_result_reason | has("exit_code")) and
    ($report.no_result_reason | has("signal")) and
    ($report.no_result_reason | has("stdout_bytes")) and
    ($report.no_result_reason | has("stderr_bytes")) and
    $report.no_result_reason.kind == "first_run_rejected" and
    $report.comparison == null and
    $report.compared_log_messages == null and
    ($report.no_result_reason.stdout_bytes | nonnegative_integer) and
    ($report.no_result_reason.stderr_bytes | nonnegative_integer) and
    (
      (($report.no_result_reason.exit_code | positive_integer) and
       $report.no_result_reason.signal == null and
       $report.guest_exit_code == $report.no_result_reason.exit_code and
       $report.guest_signal == null)
      or
      ($report.no_result_reason.exit_code == null and
       ($report.no_result_reason.signal | positive_integer) and
       $report.guest_exit_code == null and
       $report.guest_signal == $report.no_result_reason.signal)
    )
  ) catch false;

def canonical_product_failure_ok:
  try (
    (report) as $report |
    row_disposition_fields_present and
    attempt_disposition_fields_present and
    report_disposition_fields_present and
    .outcome == "FAIL" and
    .failure_class == "product_failure" and
    .error_kind == null and
    (.reason | type == "string" and length > 0) and
    .attempts[0].outcome == "FAIL" and
    .attempts[0].error_kind == null and
    .attempts[0].timed_out == false and
    .attempts[0].reason == .reason and
    .attempts[0].status == 1 and
    .attempts[0].signal == null and
    executed_resource_evidence_ok and
    row_attempt_report_divergence_matches and
    canonical_comparison_ok and
    .result == "determinism-failure" and
    $report.verdict == "diverged" and
    $report.verified == false and
    $report.bitwise_parity == false and
    ($report | has("no_result_reason") | not) and
    report_has_completed_guest_disposition
  ) catch false;

def typed_first_run_rejected_ok:
  try (
    (report) as $report |
    row_disposition_fields_present and
    attempt_disposition_fields_present and
    report_disposition_fields_present and
    .outcome == "FAIL" and
    .result == "crash-error" and
    .failure_class == "product_failure" and
    .error_kind == null and
    (.reason | type == "string" and length > 0) and
    .attempts[0].outcome == "FAIL" and
    .attempts[0].error_kind == null and
    .attempts[0].timed_out == false and
    .attempts[0].reason == .reason and
    .attempts[0].status == 125 and
    .attempts[0].signal == null and
    executed_resource_evidence_ok and
    typed_first_run_rejected_report_ok
  ) catch false;

def timeout_common_ok:
  try (
    row_disposition_fields_present and
    attempt_disposition_fields_present and
    row_divergence_fields_present and
    attempt_divergence_fields_present and
    (.outcome == "FAIL" or .outcome == "ERROR") and
    .attempts[0].outcome == .outcome and
    .result == "timeout" and
    .failure_class == "no_result" and
    (.error_kind == "wall-timeout" or .error_kind == "cpu-timeout") and
    .attempts[0].timed_out == true and
    .attempts[0].error_kind == .error_kind and
    (.reason | type == "string" and length > 0) and
    .attempts[0].reason == .reason and
    (.duration_ms | nonnegative_integer) and
    (.attempts[0].duration_ms | nonnegative_integer) and
    .duration_ms >= .attempts[0].duration_ms and
    (.attempts[0] | has("cpu_usage_usec")) and
    (.attempts[0] | has("observation_sha256")) and
    (.attempts[0] | has("runtime")) and
    (.attempts[0] | has("stdout")) and
    (.attempts[0] | has("stderr")) and
    (.attempts[0].stdout | type) == "string" and
    (.attempts[0].stderr | type) == "string" and
    has("cpu_usage_usec") and
    has("runtime")
  ) catch false;

def timeout_cpu_usage_ok:
  (.attempts[0].cpu_usage_usec | nonnegative_integer) and
  (.cpu_usage_usec | nonnegative_integer) and
  .cpu_usage_usec >= .attempts[0].cpu_usage_usec and
  (if .error_kind == "cpu-timeout"
   then .attempts[0].cpu_usage_usec >=
        (.execution_cpu_timeout_seconds * 1000000)
   else true
   end);

def typed_not_run_report_ok:
  try (
    (report) as $report |
    report_disposition_fields_present and
    row_attempt_report_have_no_divergence and
    $report.infrastructure_error == null and
    $report.verified == false and
    $report.bitwise_parity == false and
    $report.verdict == "no_result" and
    $report.no_result_reason == {"kind":"not_run"} and
    $report.comparison == null and
    $report.compared_log_messages == null and
    $report.guest_exit_code == null and
    $report.guest_signal == null
  ) catch false;

def typed_timeout_canonical_report_ok:
  try (
    (report) as $report |
    canonical_comparison_structure_ok and
    row_attempt_report_divergence_matches and
    timeout_divergence_coordinates_match_counts and
    $report.infrastructure_error == null and
    ($report | has("no_result_reason") | not) and
    (
      ($report.verdict == "matched" and
       $report.verified == true and
       $report.bitwise_parity ==
         (($report.compared_log_messages.left > 0) and
          ($report.compared_log_messages.right > 0)) and
       canonical_match_counts_equal and
       row_attempt_report_have_no_divergence and
       $report.guest_exit_code == 0 and
       $report.guest_signal == null)
      or
      ($report.verdict == "diverged" and
       $report.verified == false and
       $report.bitwise_parity == false and
       report_has_completed_guest_disposition)
    ) and
    (if canonical_comparison_ok
     then .outcome == "FAIL"
     else .outcome == "ERROR"
     end)
  ) catch false;

def timeout_report_disposition_correlation_ok:
  (report) as $report |
  attempt_has_runner_timeout_signal or
  (
    .attempts[0].signal == null and
    (
      ($report.verdict == "matched" and .attempts[0].status == 0) or
      ($report.verdict == "diverged" and .attempts[0].status == 1) or
      ($report.verdict == "no_result" and
       .attempts[0].status == 125 and
       $report.no_result_reason.kind == "first_run_rejected")
    )
  );

def typed_executed_timeout_with_report_ok:
  try (
    timeout_common_ok and
    attempt_has_completed_disposition and
    timeout_cpu_usage_ok and
    timeout_report_disposition_correlation_ok and
    .reason == expected_timeout_reason(false) and
    ((.attempts[0].verification_report | type) == "string") and
    (.attempts[0].verification_report_sha256 | hex64) and
    (
      typed_timeout_canonical_report_ok or
      (typed_first_run_rejected_report_ok and .outcome == "ERROR") or
      (typed_not_run_report_ok and .outcome == "ERROR")
    )
  ) catch false;

def typed_executed_timeout_without_report_ok:
  try (
    timeout_common_ok and
    .outcome == "ERROR" and
    attempt_has_runner_timeout_signal and
    timeout_cpu_usage_ok and
    .reason == expected_timeout_reason(false) and
    .attempts[0].verification_report == null and
    .attempts[0].verification_report_sha256 == null and
    row_attempt_have_no_divergence and
    .attempts[0].runtime == null and
    .runtime == null
  ) catch false;

def typed_pre_attempt_timeout_ok:
  try (
    timeout_common_ok and
    .outcome == "ERROR" and
    .error_kind == "wall-timeout" and
    .attempts[0].status == null and
    .attempts[0].signal == null and
    .attempts[0].cpu_usage_usec == null and
    .attempts[0].runtime == null and
    .attempts[0].stdout == "" and
    .attempts[0].stderr == "" and
    .cpu_usage_usec == null and
    .runtime == null and
    .reason == expected_timeout_reason(true) and
    typed_not_run_report_ok
  ) catch false;

def typed_timeout_ok:
  typed_executed_timeout_with_report_ok or
  typed_executed_timeout_without_report_ok or
  typed_pre_attempt_timeout_ok;

def typed_killed_timeout_ok:
  typed_timeout_ok and
  (.attempts[0].signal | positive_integer);

def typed_post_exit_cpu_timeout_ok:
  typed_executed_timeout_with_report_ok and
  .error_kind == "cpu-timeout" and
  .attempts[0].signal == null and
  (.attempts[0].status == 0 or
   .attempts[0].status == 1 or
   .attempts[0].status == 125);

def typed_disposition_ok:
  canonical_pass_ok or canonical_product_failure_ok or
  typed_first_run_rejected_ok or typed_timeout_ok;

def row_evidence_ok($sha; $machine; $kernel; $prefix; $cells):
  provenance_ok($sha; $machine; $kernel) and
  classification_ok($cells) and
  invocation_ok($prefix) and
  typed_disposition_ok;

def invocation_identity:
  {test, run_index};

def attempt_identity:
  {lane, category, test, mode, backend, run_index, attempt, run_id};

if $operation == "eligibility" then
  ($expected[0]) as $cells |
  length == 1 and
  .[0].test == $test and
  .[0].run_index == $run_index and
  (. [0] | row_evidence_ok($source_sha; $machine; $kernel; $campaign_prefix; $cells)) and
  (. [0] | strict_pass_ok)
elif $operation == "validate" then
  ($expected[0]) as $cells |
  . as $rows |
  [
    $cells[].test as $test_id |
    ($rows | map(select(.test == $test_id and .run_index == 1))) as $cell_rows |
    select(
      ($cell_rows | length) == 1 and
      ($cell_rows[0] | row_evidence_ok($source_sha; $machine; $kernel; $campaign_prefix; $cells)) and
      ($cell_rows[0] | strict_pass_ok)
    ) |
    $test_id
  ] | sort as $round1_eligible_ids |
  (
    [$cells[] | {test, run_index: 1}] +
    [$round1_eligible_ids[] as $test_id |
      range(2; 4) as $run_index |
      {test: $test_id, run_index: $run_index}]
    | sort_by(.test, .run_index)
  ) as $expected_invocations |
  ([$rows[] | invocation_identity] | unique | sort_by(.test, .run_index)) as $observed_invocations |
  ([$rows[] | attempt_identity] | sort_by(.test, .run_index, .attempt)) as $observed_attempts |
  ([
    $observed_attempts
    | group_by([.lane, .category, .test, .mode, .backend, .run_index, .attempt, .run_id])[]
    | select(length != 1)
    | {identity: .[0], rows: length}
  ]) as $duplicate_attempts |
  ([
    $observed_invocations[] as $invocation |
    ($rows | map(select(
      .test == $invocation.test and .run_index == $invocation.run_index
    ))) as $group |
    select(
      ($group | map(.attempt) | sort) != [range(1; ($group | length) + 1)]
    ) |
    {
      test: $invocation.test,
      run_index: $invocation.run_index,
      attempts: ($group | map(.attempt) | sort)
    }
  ]) as $noncontiguous_retries |
  ([
    $observed_invocations[] as $invocation |
    ($rows
      | map(select(.test == $invocation.test and .run_index == $invocation.run_index))
      | sort_by(.attempt)) as $group |
    select(
      if ($group[0].outcome // "INVALID") == "PASS"
      then (($group | length) != 1 or $group[0].attempt != 1)
      else (($group | length) != 2 or ($group | map(.attempt)) != [1, 2])
      end
    ) |
    {
      test: $invocation.test,
      run_index: $invocation.run_index,
      first_outcome: ($group[0].outcome // null),
      attempts: ($group | map(.attempt))
    }
  ]) as $incomplete_retry_groups |
  ([
    $rows[]
    | select((row_evidence_ok($source_sha; $machine; $kernel; $campaign_prefix; $cells)) | not)
    | {
        test,
        run_index,
        attempt,
        outcome,
        result,
        failure_class,
        error_kind,
        failed_checks: [
          if (provenance_ok($source_sha; $machine; $kernel) | not)
            then "provenance" else empty end,
          if (classification_ok($cells) | not)
            then "source-bound classification" else empty end,
          if (invocation_ok($campaign_prefix) | not)
            then "invocation" else empty end,
          if (typed_disposition_ok | not)
            then "typed product disposition" else empty end
        ]
      }
  ]) as $violations |
  ([
    $round1_eligible_ids[] as $test_id |
    ($rows | map(select(.test == $test_id))) as $cell_rows |
    select(
      ($cell_rows | length) == 3 and
      (($cell_rows | map(.run_index) | sort) == [1, 2, 3]) and
      all($cell_rows[];
        row_evidence_ok($source_sha; $machine; $kernel; $campaign_prefix; $cells) and
        strict_pass_ok)
    ) |
    $test_id
  ] | sort) as $qualified_ids |
  {
    schema: 2,
    source_sha: $source_sha,
    expected_cells: ($cells | length),
    observed_cells: ($rows | map(.test) | unique | length),
    round1_cells: ($rows | map(select(.run_index == 1) | .test) | unique | length),
    round1_eligible_count: ($round1_eligible_ids | length),
    round1_eligible_ids: $round1_eligible_ids,
    expected_invocation_count: ($expected_invocations | length),
    observed_invocation_count: ($observed_invocations | length),
    observed_result_rows: ($rows | length),
    retry_rows: ($rows | map(select(.attempt > 1)) | length),
    strict_pass_rows: ($rows | map(select(strict_pass_ok)) | length),
    canonical_product_failure_rows: ($rows | map(select(canonical_product_failure_ok)) | length),
    crash_error_rows: ($rows | map(select(typed_first_run_rejected_ok)) | length),
    timeout_rows: ($rows | map(select(typed_timeout_ok)) | length),
    killed_timeout_rows: ($rows | map(select(typed_killed_timeout_ok)) | length),
    post_exit_cpu_timeout_rows: ($rows | map(select(typed_post_exit_cpu_timeout_ok)) | length),
    pre_attempt_timeout_rows: ($rows | map(select(typed_pre_attempt_timeout_ok)) | length),
    executed_timeout_without_report_rows:
      ($rows | map(select(typed_executed_timeout_without_report_ok)) | length),
    timeout_attempts:
      ($rows | map(select(typed_timeout_ok) | attempt_identity)),
    pre_attempt_timeout_attempts:
      ($rows | map(select(typed_pre_attempt_timeout_ok) | attempt_identity)),
    executed_timeout_without_report_attempts:
      ($rows | map(select(typed_executed_timeout_without_report_ok) | attempt_identity)),
    provisional_qualified_cell_count: ($qualified_ids | length),
    provisional_qualified_ids: $qualified_ids,
    provisional_qualified_fraction: (($qualified_ids | length | tostring) + "/119"),
    provisional_projected_overlap_numerator: (221 + ($qualified_ids | length)),
    provisional_projected_overlap_denominator: 340,
    provisional_target_new_cells_required: 85,
    provisional_target_90_percent_reached: (($qualified_ids | length) >= 85),
    binary_sha256: ($rows | map(.binary_sha256) | unique),
    kernel_versions: ($rows | map(.kernel_version) | unique),
    checks: {
      expected_population_shape:
        (($cells | type) == "array" and
         ($cells | length) == 119 and
         ($cells | map([.lane,.category,.test,.mode,.backend]) | unique | length) == 119 and
         ($cells | map(select(.classification == "unselected-no-imported-evidence" and .selector == "--probe-disabled")) | length) == 102 and
         ($cells | map(select((.classification == "enabled-red-determinism-failure" or .classification == "enabled-red-timeout") and .selector == "--include-manual")) | length) == 17),
      exact_adaptive_invocations: ($observed_invocations == $expected_invocations),
      no_duplicate_attempt_rows: (($duplicate_attempts | length) == 0),
      contiguous_retry_attempts: (($noncontiguous_retries | length) == 0),
      complete_retry_policy: (($incomplete_retry_groups | length) == 0),
      independent_run_ids:
        (($rows | group_by([.test,.run_index]) | map(.[0].run_id) | unique | length) ==
         ($expected_invocations | length)),
      one_binary: (($rows | map(.binary_sha256) | unique | length) == 1),
      stable_test_binaries:
        all(($rows | group_by(.test))[]; (map(.test_sha256) | unique | length) == 1),
      every_row_typed_and_auditable: (($violations | length) == 0)
    },
    missing_invocations: ($expected_invocations - $observed_invocations),
    unexpected_invocations: ($observed_invocations - $expected_invocations),
    duplicate_attempt_rows: $duplicate_attempts,
    noncontiguous_retries: $noncontiguous_retries,
    incomplete_retry_groups: $incomplete_retry_groups,
    violations: $violations
  } |
  .ok = (.checks | all(.[]; . == true))
else
  error("unsupported operation: " + $operation)
end

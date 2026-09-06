def identity:
  {lane, category, test, mode, backend};

def identity_without_backend:
  {lane, category, test, mode};

def classified_identity:
  {lane, category, test, mode, backend, classification, selector};

. as $manifest |
($expected[0]) as $frozen |
($plan[0]) as $selected_plan |
([$manifest.cells[]
  | select(.mode == "verify" and .backend == "ptrace" and
           .enabled == true and .status == "green")
  | identity]
 | sort_by(.lane,.category,.test,.mode,.backend)) as $ptrace_raw |
($ptrace_raw | unique) as $ptrace |
([$manifest.cells[]
  | select(.mode == "verify" and .backend == "kvm" and
           .enabled == true and .status == "green")
  | identity]
 | sort_by(.lane,.category,.test,.mode,.backend)) as $kvm_raw |
($kvm_raw | unique) as $kvm |
([$selected_plan.cells[]
  | select(.mode == "verify" and .backend == "ptrace")
  | identity]
 | sort_by(.lane,.category,.test,.mode,.backend)) as $plan_ptrace_raw |
($plan_ptrace_raw | unique) as $plan_ptrace |
([$selected_plan.cells[]
  | select(.mode == "verify" and .backend == "kvm")
  | identity]
 | sort_by(.lane,.category,.test,.mode,.backend)) as $plan_kvm_raw |
($plan_kvm_raw | unique) as $plan_kvm |
([$ptrace[] | identity_without_backend]
 | sort_by(.lane,.category,.test,.mode)) as $ptrace_no_backend |
([$kvm[] | identity_without_backend]
 | sort_by(.lane,.category,.test,.mode)) as $kvm_no_backend |
([$ptrace_no_backend[] as $candidate
  | select(($kvm_no_backend | index($candidate)) != null)
  | $candidate + {backend:"ptrace"}]
 | sort_by(.lane,.category,.test,.mode,.backend)) as $overlap_ptrace |
([$ptrace_no_backend[] as $candidate
  | select(($kvm_no_backend | index($candidate)) == null)
  | $candidate + {backend:"ptrace"}]
 | sort_by(.lane,.category,.test,.mode,.backend)) as $complement_ptrace |
([$complement_ptrace[] | .backend = "kvm"]
 | sort_by(.lane,.category,.test,.mode,.backend)) as $complement_kvm |
([$complement_kvm[] as $candidate
  | $manifest.cells[]
  | select(
      .lane == $candidate.lane and
      .category == $candidate.category and
      .test == $candidate.test and
      .mode == $candidate.mode and
      .backend == "kvm"
    )
  | {
      lane,
      category,
      test,
      mode,
      backend,
      classification:
        (if .enabled == false and .status == "not-applicable" and
            .measurement == "never-measured"
         then "unselected-no-imported-evidence"
         elif .enabled == true and .status == "red" and
              .ci_disabled_reason.result == "determinism-failure"
         then "enabled-red-determinism-failure"
         elif .enabled == true and .status == "red" and
              .ci_disabled_reason.result == "timeout"
         then "enabled-red-timeout"
         else "INVALID"
         end),
      selector:
        (if .enabled == false then "--probe-disabled" else "--include-manual" end)
    }]
 | sort_by(.lane,.category,.test,.mode,.backend)) as $classified |
($frozen | map(classified_identity) |
 sort_by(.lane,.category,.test,.mode,.backend)) as $frozen_sorted |

if $operation == "denominator" then
  $ptrace[]
elif $operation == "overlap" then
  $overlap_ptrace[]
elif $operation == "complement-ptrace" then
  $complement_ptrace[]
elif $operation == "complement-kvm" then
  $complement_kvm[]
elif $operation == "classified-complement" then
  $classified[]
elif $operation == "frozen" then
  $frozen_sorted[]
elif $operation == "summary" then
  {
    schema: 1,
    denominator_definition: "all selected Green ptrace/verify identities across all lanes",
    ptrace_green: ($ptrace | length),
    ptrace_green_portable: ($ptrace | map(select(.lane == "portable")) | length),
    kvm_green_total: ($kvm | length),
    kvm_green_overlap: ($overlap_ptrace | length),
    kvm_green_overlap_portable: ($overlap_ptrace | map(select(.lane == "portable")) | length),
    complement: ($complement_ptrace | length),
    complement_portable: ($complement_ptrace | map(select(.lane == "portable")) | length),
    unselected_no_imported_evidence:
      ($classified | map(select(.classification == "unselected-no-imported-evidence")) | length),
    enabled_red_determinism_failure:
      ($classified | map(select(.classification == "enabled-red-determinism-failure")) | length),
    enabled_red_timeout:
      ($classified | map(select(.classification == "enabled-red-timeout")) | length),
    enabled_red_total:
      ($classified | map(select(.selector == "--include-manual")) | length),
    expected_plan_ptrace: ($plan_ptrace | length),
    expected_plan_kvm: ($plan_kvm | length),
    checks: {
      denominator_340: (($ptrace | length) == 340),
      selected_kvm_222: (($kvm | length) == 222),
      overlap_221: (($overlap_ptrace | length) == 221),
      complement_119: (($complement_ptrace | length) == 119),
      complement_all_portable:
        (($complement_ptrace | map(select(.lane == "portable")) | length) == 119),
      typed_partition_102_13_4:
        (($classified | length) == 119 and
         ($classified | map(select(.classification == "unselected-no-imported-evidence")) | length) == 102 and
         ($classified | map(select(.classification == "enabled-red-determinism-failure")) | length) == 13 and
         ($classified | map(select(.classification == "enabled-red-timeout")) | length) == 4 and
         ($classified | map(select(.classification == "INVALID")) | length) == 0),
      selectors_match_classification:
        all($classified[];
          if .classification == "unselected-no-imported-evidence"
          then .selector == "--probe-disabled"
          else .selector == "--include-manual"
          end),
      frozen_population_exact: ($classified == $frozen_sorted),
      frozen_no_duplicates:
        (($frozen | map([.lane,.category,.test,.mode,.backend]) | unique | length) == 119),
      manifest_selected_no_duplicates:
        (($ptrace_raw | length) == ($ptrace | length) and
         ($kvm_raw | length) == ($kvm | length)),
      expected_plan_no_duplicates:
        (($plan_ptrace_raw | length) == ($plan_ptrace | length) and
         ($plan_kvm_raw | length) == ($plan_kvm | length)),
      expected_plan_ptrace_exact: ($plan_ptrace == $ptrace),
      expected_plan_kvm_exact: ($plan_kvm == $kvm)
    }
  } |
  .ok = (.checks | all(.[]; . == true))
else
  error("unsupported operation: " + $operation)
end

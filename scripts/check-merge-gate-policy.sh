#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Pin the workflow wiring around the tested trinary predicate. The exhaustive
# state test proves the predicate; this lint proves every gate leg uses it.
set -euo pipefail

ROOT_DIR=${1:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}
WORKFLOW="$ROOT_DIR/.github/workflows/merge-gate.yml"
PRODUCER_AUTHORITY_REF=070b504dce00f701e96d3bb04fb5928a2d488d32
RECEIPT_VERIFIER_SHA256=1b0792415134afed7066ee70e1bc35319a204c5192cac69d33a8ca96b2f01082
QUALIFYING_RECEIPT_SHA256=e0c1ec31c69fd2070f1b07957e721e9992143349b9083a2980b3a0a8582bc498
PRODUCER_DEFINITION_SHA256=2deef4cead55fafcc1db2664e03680ca6b6045ee88aac16fb80b0ee1b266ee0f

fail() {
    echo "check-merge-gate-policy.sh: $*" >&2
    exit 1
}

[[ -f $WORKFLOW ]] || fail "missing $WORKFLOW"
grep -Fq 'actions: write' "$WORKFLOW" || fail "NO_RESULT must be able to re-dispatch and cancel"
grep -Fq 'ref=4b78d727f35bc8612ac460a6e270dda5f5df304c' "$WORKFLOW" ||
    fail "gate must pin the parent authority commit"
grep -Fq '2f1c61d5ec9d98b9697317fd9e66b705161defb69b808d23e6d83384e1e2a1e8' "$WORKFLOW" ||
    fail "gate must content-pin the check-status authority"
grep -Fq '"$CHECK_OUTCOME_AUTHORITY"' "$WORKFLOW" ||
    fail "gate must call the parent check-status authority"
[[ $(grep -Fc -- '--select-latest-run' "$WORKFLOW") -eq 3 ]] ||
    fail "portable, privileged, and demo selectors must use the exact-head/latest authority"
grep -Fq 'job_status=missing' "$WORKFLOW" ||
    fail "a missing portable job must start as NO_RESULT"
grep -Fq 'priv_status=missing' "$WORKFLOW" ||
    fail "a missing privileged job must start as NO_RESULT"
if grep -Fq 'job_status=$run_status' "$WORKFLOW"; then
    fail "workflow success must not stand in for a missing authoritative job"
fi
grep -Fq '[ "$job_found" != true ] && [ "$run_state" = FAILED ]' "$WORKFLOW" ||
    fail "a complete workflow failure must remain a failure fallback"
grep -Fq '[ "$priv_job_found" != true ] && [ "$priv_run_state" = FAILED ]' "$WORKFLOW" ||
    fail "a complete privileged workflow failure must remain a failure fallback"
grep -Fq 'portable_cancellation_absence "$run" "$jobs" "$regular_job" "$head_sha"' "$WORKFLOW" ||
    fail "portable aggregate failure must use the exact cancellation-derived absence proof"
grep -Fq 'portable_state=NO_RESULT' "$WORKFLOW" ||
    fail "proved cancellation-derived portable absence must downgrade only to NO_RESULT"
grep -Fq 'agent-utils/py/ci_hub_check_outcome.py' "$ROOT_DIR/scripts/classify-required-check.sh" ||
    fail "local shell adapter must delegate to the parent status authority"
grep -Fq 'from ci_hub_check_outcome import' "$ROOT_DIR/scripts/pr_status.py" ||
    fail "PR rollup must import the parent-authority adapter"
grep -Fq '"rrnewton/hermit": ("merge-gate-v4",)' "$ROOT_DIR/scripts/pr_status.py" ||
    fail "Hermit PR rollup must read the live versioned gate context"
grep -Fq 'agent-utils/py/ci_hub_check_outcome.py" --annotate-rollups' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "lander rollup must call the parent-authority adapter"
grep -Fq '[[ $REPO == rrnewton/hermit ]] && GATE_CONTEXT=merge-gate-v4' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "lander rollup must use Hermit's live versioned gate context"
grep -Fq 'latest_named($r; $gate_context)' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "lander rollup must select the repository-specific gate context"
grep -Fq -- '--select-latest-rollup --head-sha "$MAIN_FULL_SHA"' "$ROOT_DIR/scripts/pr-dag-health.sh" ||
    fail "main-health rollup must select the latest check at the exact head"
[[ ! -e $ROOT_DIR/scripts/check_outcome.jq ]] ||
    fail "duplicate jq status classifier must not exist"
[[ ! -e $ROOT_DIR/scripts/check_status_outcome.py ]] ||
    fail "duplicate Hermit status adapter must not exist"
[[ $(grep -Fc "REGISTRY_REF: $PRODUCER_AUTHORITY_REF" "$WORKFLOW") -eq 2 ]] ||
    fail "both gate legs must pin the producer registries to the parent authority commit"
[[ $(grep -Fc "VERIFIER_REF: $PRODUCER_AUTHORITY_REF" "$WORKFLOW") -eq 2 ]] ||
    fail "both gate legs must pin the receipt verifier to the parent authority commit"
[[ $(grep -Fc "$RECEIPT_VERIFIER_SHA256" "$WORKFLOW") -eq 2 ]] ||
    fail "both gate legs must content-pin the parent receipt verifier"
[[ $(grep -Fc "$QUALIFYING_RECEIPT_SHA256" "$WORKFLOW") -eq 2 ]] ||
    fail "both gate legs must content-pin the qualifying-receipt registry"
[[ $(grep -Fc "$PRODUCER_DEFINITION_SHA256" "$WORKFLOW") -eq 2 ]] ||
    fail "both gate legs must content-pin the producer-definition registry"
grep -Fq '"$RECEIPT_VERIFIER"' "$WORKFLOW" ||
    fail "local alternate leg must call the parent receipt verifier"
if grep -Eq 'scripts/(check|verify)-local-validation' "$WORKFLOW"; then
    fail "gate must not call a PR-local validation-evidence verifier"
fi
grep -Fq 'NO_RESULT)' "$WORKFLOW" || fail "gate must handle NO_RESULT explicitly"
grep -Fq 'dispatch_no_result' "$WORKFLOW" || fail "NO_RESULT must re-dispatch"
grep -Fq 'queue_hosted_retry "$demo_status" demo-hot-path-rerun' "$WORKFLOW" ||
    fail "demo NO_RESULT must rerun the selected pull-request run"
grep -Fq 'queued | in_progress | waiting | requested | pending)' "$WORKFLOW" ||
    fail "active NO_RESULT runs must wait for workflow_run completion, not rerun"
grep -Fq 'label_event()' "$WORKFLOW" ||
    fail "label events must be recognized before hosted retry dispatch"
grep -Fq 'dispatch_budget_spent()' "$WORKFLOW" ||
    fail "hosted retries must consult the exact-head dispatch budget"
grep -Fq 'Could not read the dispatch budget' "$WORKFLOW" ||
    fail "an unreadable dispatch budget must retain NO_RESULT without dispatch"
grep -Fq 'actions/runs/${run_id}/rerun' "$WORKFLOW" ||
    fail "demo recovery must use the selected run ID"
if grep -Fq 'queue_dispatch demo-hot-path.yml' "$WORKFLOW"; then
    fail "workflow_dispatch demo runs are ineligible and must not be queued"
fi
grep -Fq 'cancel_no_result_gate' "$WORKFLOW" || fail "NO_RESULT must not exit red or green"
grep -Fq '/force-cancel' "$WORKFLOW" || fail "if: always() gate requires force-cancel for NO_RESULT"
grep -Fq 'GATE_RUN_ID' "$WORKFLOW" || fail "self-cancellation must identify the exact gate run"
if grep -Eq 'success[[:space:]]*\|[[:space:]]*skipped|success[[:space:]]+or[[:space:]]+skipped' "$WORKFLOW"; then
    fail "skipped must never satisfy a required check"
fi

# Both brackets below execute code extracted from the workflow itself, so they
# share one temporary workspace and one cleanup trap. A second `trap ... EXIT`
# would silently replace the first and leak the other bracket's scratch files.
tmpdir=$(mktemp -d)
trap 'rm -rf -- "$tmpdir"' EXIT

# Execute the workflow's embedded classifier itself, rather than a test copy.
# The markers are part of the policy contract so a workflow edit cannot silently
# leave these adversarial fixtures exercising stale helper code.
classifier_fixture="$tmpdir/cancellation-classifier.sh"
awk '
    /# BEGIN portable-cancellation-absence-classifier/ { capture=1; next }
    /# END portable-cancellation-absence-classifier/ { capture=0 }
    capture { sub(/^          /, ""); print }
' "$WORKFLOW" >"$classifier_fixture"
grep -Fq 'portable_cancellation_absence()' "$classifier_fixture" ||
    fail "could not extract the embedded cancellation-derived absence classifier"
# shellcheck source=/dev/null
source "$classifier_fixture"

REPO=rrnewton/hermit
head_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
aggregate='{"id":2,"name":"Regular tests (GitHub-managed portable)","status":"completed","conclusion":"failure"}'
cancelled_run='{"id":1,"head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","status":"completed","conclusion":"cancelled","name":"CI (GitHub-managed portable)","path":".github/workflows/ci-portable.yml"}'
stale_run='{"id":1,"head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","status":"completed","conclusion":"cancelled","name":"CI (GitHub-managed portable)","path":".github/workflows/ci-portable.yml"}'
cancel_only_jobs='{"total_count":3,"jobs":[{"id":2,"name":"Regular tests (GitHub-managed portable)","status":"completed","conclusion":"failure"},{"id":3,"name":"test: strict-compat","status":"completed","conclusion":"cancelled"},{"id":4,"name":"test: unit","status":"completed","conclusion":"success"}]}'
leaf_failure_jobs='{"total_count":4,"jobs":[{"id":2,"name":"Regular tests (GitHub-managed portable)","status":"completed","conclusion":"failure"},{"id":3,"name":"test: strict-compat","status":"completed","conclusion":"cancelled"},{"id":4,"name":"test: unit","status":"completed","conclusion":"success"},{"id":5,"name":"test: integration","status":"completed","conclusion":"failure"}]}'
no_cancel_jobs='{"total_count":3,"jobs":[{"id":2,"name":"Regular tests (GitHub-managed portable)","status":"completed","conclusion":"failure"},{"id":3,"name":"test: strict-compat","status":"completed","conclusion":"success"},{"id":4,"name":"test: unit","status":"completed","conclusion":"skipped"}]}'

gh() {
    [[ $1 == api && $2 == repos/rrnewton/hermit/actions/jobs/2 ]] || return 1
    case ${CANCELLATION_FIXTURE:-missing} in
        cancel-only)
            printf '%s\n' '{"id":2,"name":"Regular tests (GitHub-managed portable)","status":"completed","conclusion":"failure","steps":[{"name":"Set up job","status":"completed","conclusion":"success"},{"name":"Require every portable DAG job to succeed or be deselected","status":"completed","conclusion":"failure"},{"name":"Complete job","status":"completed","conclusion":"success"}]}'
            ;;
        aggregate-failure)
            printf '%s\n' '{"id":2,"name":"Regular tests (GitHub-managed portable)","status":"completed","conclusion":"failure","steps":[{"name":"Verify completeness","status":"completed","conclusion":"failure"},{"name":"Require every portable DAG job to succeed or be deselected","status":"completed","conclusion":"failure"},{"name":"Complete job","status":"completed","conclusion":"success"}]}'
            ;;
        *) return 1 ;;
    esac
}

run_cancellation_case() {
    local expected=$1 label=$2 run=$3 jobs=$4 fixture=$5 rc=0
    CANCELLATION_FIXTURE=$fixture \
        portable_cancellation_absence "$run" "$jobs" "$aggregate" "$head_sha" || rc=$?
    [[ $rc -eq $expected ]] ||
        fail "$label expected classifier rc=$expected, got rc=$rc"
}

run_cancellation_case 0 "planted cancellation-only aggregate absence" \
    "$cancelled_run" "$cancel_only_jobs" cancel-only
run_cancellation_case 1 "genuine leaf failure" \
    "$cancelled_run" "$leaf_failure_jobs" cancel-only
run_cancellation_case 1 "independent aggregate failure" \
    "$cancelled_run" "$cancel_only_jobs" aggregate-failure
run_cancellation_case 1 "stale exact-head evidence" \
    "$stale_run" "$cancel_only_jobs" cancel-only
run_cancellation_case 1 "no cancelled leaf" \
    "$cancelled_run" "$no_cancel_jobs" cancel-only
run_cancellation_case 1 "missing aggregate detail" \
    "$cancelled_run" "$cancel_only_jobs" missing

# Exercise the real workflow function bodies against an inert `gh` function.
# This is intentionally incapable of dispatching a workflow: queue_dispatch only
# appends to a temporary TSV, and the stub refuses every API shape except the
# read-only dispatch-budget query. Sed stops at the workflow function's own
# ten-space closing brace; nested braces are more deeply indented.
functions_file="$tmpdir/dispatch-functions.sh"
for function_name in queue_dispatch label_event dispatch_budget_spent queue_hosted_retry; do
    body=$(sed -n "/^          ${function_name}()/,/^          }$/p" "$WORKFLOW")
    [[ -n $body ]] || fail "could not extract $function_name from workflow"
    sed 's/^          //' <<<"$body" >>"$functions_file"
done
# shellcheck source=/dev/null
source "$functions_file"
MAX_HEAD_DISPATCHES=$(sed -n 's/^          MAX_HEAD_DISPATCHES=//p' "$WORKFLOW")
[[ $MAX_HEAD_DISPATCHES =~ ^[0-9]+$ ]] || fail "dispatch limit must be a numeric literal"

GH_STUB_MODE=count
GH_STUB_COUNT=0
GH_EXPECTED_WORKFLOW=ci-portable.yml
GH_EXPECTED_SHA=0123456789abcdef0123456789abcdef01234567
GH_CALLS_FILE="$tmpdir/gh-calls"
gh() {
    [[ ${1:-} == api ]] || fail "inert gh stub refused non-API operation: $*"
    [[ ${2:-} == "repos/${REPO}/actions/runs?event=workflow_dispatch&head_sha=${GH_EXPECTED_SHA}&per_page=100" ]] ||
        fail "inert gh stub refused query not bound to the expected repo and exact head: $*"
    [[ ${3:-} == --jq ]] || fail "inert gh stub requires the workflow-path selector: $*"
    [[ ${4:-} == "[.workflow_runs[] | select(.path == \".github/workflows/${GH_EXPECTED_WORKFLOW}\")] | length" ]] ||
        fail "inert gh stub refused query not bound to the expected workflow path: $*"
    printf 'api\n' >>"$GH_CALLS_FILE"
    case "$GH_STUB_MODE" in
        count) printf '%s\n' "$GH_STUB_COUNT" ;;
        fail) return 1 ;;
        malformed) printf '%s\n' not-a-count ;;
        *) fail "unknown gh stub mode: $GH_STUB_MODE" ;;
    esac
}
sleep() { :; }

dispatch_file="$tmpdir/dispatch.tsv"
REPO=rrnewton/hermit
case_log="$tmpdir/case.log"
cases_run=0
run_retry_case() {
    local name=$1 event=$2 action=$3 status=$4 workflow=$5 stub_mode=$6 prior=$7 expected=$8 expected_api_calls=$9
    : >"$dispatch_file"
    : >"$case_log"
    : >"$GH_CALLS_FILE"
    EVENT_NAME=$event
    PR_ACTION=$action
    GH_STUB_MODE=$stub_mode
    GH_STUB_COUNT=$prior
    GH_EXPECTED_WORKFLOW=$workflow
    no_result=0
    queue_hosted_retry "$status" "$workflow" branch rrnewton/hermit 123 \
        "$GH_EXPECTED_SHA" >"$case_log" 2>&1
    local actual api_calls
    actual=$(wc -l <"$dispatch_file")
    api_calls=$(wc -l <"$GH_CALLS_FILE")
    [[ $actual -eq $expected ]] ||
        fail "$name: expected $expected inert dispatch record(s), got $actual: $(cat "$case_log")"
    [[ $api_calls -eq $expected_api_calls ]] ||
        fail "$name: expected $expected_api_calls budget API call(s), got $api_calls: $(cat "$case_log")"
    [[ $no_result -eq 1 ]] || fail "$name: every retry path must retain NO_RESULT"
    cases_run=$((cases_run + 1))
}

run_retry_case labeled-suppressed pull_request labeled missing ci-portable.yml count 0 0 0
run_retry_case unlabeled-suppressed pull_request unlabeled missing ci-portable.yml count 0 0 0
run_retry_case synchronize-dispatches pull_request synchronize missing ci-portable.yml count 0 1 1
run_retry_case opened-dispatches pull_request opened missing ci-portable.yml count 1 1 1
run_retry_case budget-at-limit workflow_dispatch '' missing ci-portable.yml count 2 0 1
run_retry_case budget-over-limit workflow_dispatch '' missing ci-portable.yml count 4 0 1
run_retry_case budget-api-failure workflow_dispatch '' missing ci-portable.yml fail 0 0 3
run_retry_case budget-malformed workflow_dispatch '' missing ci-portable.yml malformed 0 0 1
run_retry_case active-queued workflow_dispatch '' queued ci-portable.yml fail 0 0 0
run_retry_case active-in-progress workflow_dispatch '' in_progress ci-portable.yml fail 0 0 0
run_retry_case demo-rerun-not-budgeted workflow_dispatch '' missing demo-hot-path-rerun count 99 1 0
run_retry_case demo-label-suppressed pull_request labeled missing demo-hot-path-rerun count 99 0 0

echo "check-merge-gate-policy.sh: OK - registry pins 2/2; cancellation-only positive 1/1; leaf/aggregate/stale/absence negatives 5/5; PASSED/FAILED/NO_RESULT wiring plus ${cases_run}/12 dispatch brackets enforced"

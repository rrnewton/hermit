#!/usr/bin/env bash
# Two-way fixtures for the trusted-base review/budget linter.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly LINT="$SCRIPT_DIR/core-review-protocol-lint.sh"
readonly HEAD_SHA=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
readonly OLD_SHA=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
readonly AUTHOR_LOGIN=rrnewton
readonly TEST_OWNER_LOGIN=rrnewton

readonly FULL_BODY='[impl agent, CODEX] [author-agent, testbox]

## Summary
Adds a thing.

## Determinism
Deterministic because reasons and an informal proof.

## Linux Semantics
Matches the kernel behavior described here.

## Validation
Focused tests passed.

## Human Review Required
Trigger 4: core DetCore scheduling change.'

readonly KVM_BODY="${FULL_BODY}

## Relationship to gVisor
No gVisor analog applies."

readonly CODEX_REVIEW="[adversarial-reviewer agent, CODEX] [codex-reviewer, devbig998]

PASS at exact head ${HEAD_SHA}. Independent review found no blocker."
readonly CLAUDE_REVIEW="[adversarial-reviewer agent, claude-opus] [claude-reviewer, devbig997]

APPROVE at exact head ${HEAD_SHA}. Independent review found no blocker."
readonly SELF_REVIEW="[adversarial-reviewer agent, CODEX] [author-agent, devbig999]

PASS at exact head ${HEAD_SHA}."
readonly EARLIER_AUTHOR_REVIEW="[adversarial-reviewer agent, CODEX] [earlier-author, devbig998]

PASS at exact head ${HEAD_SHA}."
readonly STALE_REVIEW="[adversarial-reviewer agent, CODEX] [codex-reviewer, devbig998]

PASS at stale head ${OLD_SHA}."
readonly BLOCK_REVIEW="[adversarial-reviewer agent, CODEX] [codex-reviewer, devbig998]

REQUEST CHANGES at exact head ${HEAD_SHA}."
readonly MALFORMED_REVIEW="[adversarial-reviewer agent, CODEX] [codex-reviewer@devbig998]

PASS at exact head ${HEAD_SHA}."
readonly OWNER_EXCEPTION="[Human]

TIMEOUT-CAP-EXCEPTION: APPROVED
Exact head: ${HEAD_SHA}
JUSTIFICATION: The validated workload intentionally doubled after adding a required full-corpus lane.
EVIDENCE: Same-host measurements show unchanged per-test cost and exactly twice the selected test population."
readonly WEAK_OWNER_EXCEPTION="[Human]

TIMEOUT-CAP-EXCEPTION: APPROVED
Exact head: ${HEAD_SHA}
JUSTIFICATION: CI timed out.
EVIDENCE: green"

readonly ORDINARY_DIFF='diff --git a/docs/example.md b/docs/example.md
--- a/docs/example.md
+++ b/docs/example.md
@@ -1 +1 @@
-old
+new'
readonly POLICY_SELF_EDIT='diff --git a/scripts/core-review-protocol-lint.sh b/scripts/core-review-protocol-lint.sh
--- a/scripts/core-review-protocol-lint.sh
+++ b/scripts/core-review-protocol-lint.sh
@@ -1 +1 @@
-require_independent_review
+exit 0 # permit timeout increases without review'
readonly POLICY_TEST_SELF_EDIT='diff --git a/scripts/core-review-protocol-lint-test.sh b/scripts/core-review-protocol-lint-test.sh
--- a/scripts/core-review-protocol-lint-test.sh
+++ b/scripts/core-review-protocol-lint-test.sh
@@ -1 +1 @@
-run_case "timeout increase blocks" 1
+run_case "timeout increase blocks" 0'
readonly TIMEOUT_INCREASE='diff --git a/.github/workflows/ci-portable.yml b/.github/workflows/ci-portable.yml
--- a/.github/workflows/ci-portable.yml
+++ b/.github/workflows/ci-portable.yml
@@ -1 +1 @@
-    timeout-minutes: 30
+    timeout-minutes: 240'
readonly CAP_INCREASE='diff --git a/ci/dag/portable.json b/ci/dag/portable.json
--- a/ci/dag/portable.json
+++ b/ci/dag/portable.json
@@ -1 +1 @@
-    "cpu_budget_seconds": 300,
+    "cpu_budget_seconds": 2100,'
readonly PARALLELISM_INCREASE='diff --git a/ci/run-dag.sh b/ci/run-dag.sh
--- a/ci/run-dag.sh
+++ b/ci/run-dag.sh
@@ -1 +1 @@
-exec safe-ci-dag-runner --jobs 8
+exec safe-ci-dag-runner --jobs 64'
readonly OFF_TIMED_PATH='diff --git a/ci/dag/portable.json b/ci/dag/portable.json
--- a/ci/dag/portable.json
+++ b/ci/dag/portable.json
@@ -10 +10,0 @@
-    {"group":"test","job":"slow-node","timeout":300}'
readonly NON_BLOCKING='diff --git a/.github/workflows/ci-portable.yml b/.github/workflows/ci-portable.yml
--- a/.github/workflows/ci-portable.yml
+++ b/.github/workflows/ci-portable.yml
@@ -1 +1 @@
-      continue-on-error: false
+      continue-on-error: true'
readonly BASELINE_BUMP='diff --git a/ci/performance-baseline.json b/ci/performance-baseline.json
--- a/ci/performance-baseline.json
+++ b/ci/performance-baseline.json
@@ -1 +1 @@
-  "expected_seconds": 100,
+  "expected_seconds": 700,'

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

comments_json() {
    jq -cn '$ARGS.positional | map({body:., user:{login:"rrnewton"}, author_association:"OWNER"})' --args "$@"
}

pass=0
fail=0

# run_case NAME EXPECTED LABELS BODY IS_KVM COMMENTS_JSON REVIEWS_JSON DIFF
#          [HEAD_COMMIT_MESSAGE] [PRIOR_COMMIT_MESSAGE]
run_case() {
    local name=$1 expected=$2 labels=$3 pr_body=$4 is_kvm=$5 comments=$6 reviews=$7 diff=$8
    local commit_message=${9:-}
    local prior_commit_message=${10:-}
    local actual=0
    printf '%s\n' "$comments" >"$tmp/comments.json"
    printf '%s\n' "$reviews" >"$tmp/reviews.json"
    printf '%s\n' "$diff" >"$tmp/pr.diff"
    if [[ -n $commit_message ]]; then
        :
    else
        commit_message=$'Test candidate\n\n[impl agent, CODEX] [author-agent, devbig999]'
    fi
    if [[ -n $prior_commit_message ]]; then
        jq -cn --arg prior_sha "$OLD_SHA" --arg prior "$prior_commit_message" \
            --arg head_sha "$HEAD_SHA" --arg head "$commit_message" \
            '[{sha:$prior_sha,message:$prior},{sha:$head_sha,message:$head}]' \
            >"$tmp/commit-messages.json"
    else
        jq -cn --arg sha "$HEAD_SHA" --arg message "$commit_message" \
            '[{sha:$sha,message:$message}]' >"$tmp/commit-messages.json"
    fi
    PR_LABELS="$labels" PR_BODY="$pr_body" PR_IS_KVM="$is_kvm" PR_NUMBER=test \
        PR_HEAD_SHA="$HEAD_SHA" PR_AUTHOR_LOGIN="$AUTHOR_LOGIN" OWNER_LOGIN="$TEST_OWNER_LOGIN" \
        PR_COMMENTS_FILE="$tmp/comments.json" PR_REVIEWS_FILE="$tmp/reviews.json" \
        PR_DIFF_FILE="$tmp/pr.diff" PR_COMMIT_MESSAGES_FILE="$tmp/commit-messages.json" \
        bash "$LINT" >/dev/null 2>&1 || actual=$?
    if [[ $actual -eq $expected ]]; then
        echo "ok   - ${name} (exit ${actual})"
        pass=$((pass + 1))
    else
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"
        fail=$((fail + 1))
    fi
}

empty='[]'
codex=$(comments_json "$CODEX_REVIEW")
dual=$(comments_json "$CODEX_REVIEW" "$CLAUDE_REVIEW")
formal_codex=$(jq -cn --arg body "$CODEX_REVIEW" --arg sha "$HEAD_SHA" \
    '[{body:$body, user:{login:"rrnewton"}, author_association:"OWNER", state:"APPROVED", commit_id:$sha}]')
unauthorized_codex=$(jq -cn --arg body "$CODEX_REVIEW" \
    '[{body:$body, user:{login:"stranger"}, author_association:"NONE"}]')

# Mandatory review and exact-head/identity binding.
run_case "unlabeled PR without review blocks" 1 "" "$FULL_BODY" false "$empty" "$empty" "$ORDINARY_DIFF"
run_case "ordinary PR with independent exact-head review passes" 0 "" "$FULL_BODY" false "$codex" "$empty" "$ORDINARY_DIFF"
run_case "formal exact-head approval satisfies mandatory review" 0 "" "$FULL_BODY" false "$empty" "$formal_codex" "$ORDINARY_DIFF"
run_case "untrusted commenter cannot manufacture approval" 1 "" "$FULL_BODY" false "$unauthorized_codex" "$empty" "$ORDINARY_DIFF"
run_case "malformed reviewer team tag cannot manufacture approval" 1 "" "$FULL_BODY" false \
    "$(comments_json "$MALFORMED_REVIEW")" "$empty" "$ORDINARY_DIFF"
run_case "malformed exact-head author trailer blocks" 1 "" "$FULL_BODY" false "$codex" "$empty" "$ORDINARY_DIFF" \
    'Test candidate\n\n[impl agent, CODEX] [author-agent, testbox]'
run_case "malformed historical author trailer blocks" 1 "" "$FULL_BODY" false "$codex" "$empty" "$ORDINARY_DIFF" \
    $'Test candidate\n\n[impl agent, CODEX] [author-agent, devbig999]' \
    'Historical candidate\n\n[impl agent, CODEX] [author-agent@devbig999]'
run_case "self-review does not satisfy independence" 1 "" "$FULL_BODY" false "$(comments_json "$SELF_REVIEW")" "$empty" "$ORDINARY_DIFF"
run_case "reviewer disjoint from every commit author passes" 0 "" "$FULL_BODY" false "$codex" "$empty" "$ORDINARY_DIFF" \
    $'Head candidate\n\n[impl agent, CLAUDE] [head-author, devbig999]' \
    $'Earlier candidate\n\n[impl agent, CODEX] [earlier-author, devbig998]'
run_case "earlier commit author cannot approve aggregate" 1 "" "$FULL_BODY" false \
    "$(comments_json "$EARLIER_AUTHOR_REVIEW")" "$empty" "$ORDINARY_DIFF" \
    $'Head candidate\n\n[impl agent, CLAUDE] [head-author, devbig999]' \
    $'Earlier candidate\n\n[impl agent, CODEX] [earlier-author, devbig998]'
run_case "stale-head review does not count" 1 "" "$FULL_BODY" false "$(comments_json "$STALE_REVIEW")" "$empty" "$ORDINARY_DIFF"
run_case "request-changes verdict does not authorize landing" 1 "" "$FULL_BODY" false "$(comments_json "$BLOCK_REVIEW")" "$empty" "$ORDINARY_DIFF"
run_case "triggered PR with only Codex review blocks" 1 post-facto-human-review "$FULL_BODY" false "$codex" "$empty" "$ORDINARY_DIFF"
run_case "triggered PR with both independent families passes" 0 post-facto-human-review "$FULL_BODY" false "$dual" "$empty" "$ORDINARY_DIFF"

# Body contracts remain enforced for every PR.
run_case "missing Linux Semantics blocks" 1 "" "${FULL_BODY/Linux Semantics/Other}" false "$codex" "$empty" "$ORDINARY_DIFF"
run_case "KVM PR missing gVisor section blocks" 1 "" "$FULL_BODY" true "$codex" "$empty" "$ORDINARY_DIFF"
run_case "KVM PR with gVisor section passes" 0 "" "$KVM_BODY" true "$codex" "$empty" "$ORDINARY_DIFF"

# The candidate may edit policy, but the base-owned executable still decides.
run_case "candidate policy self-edit cannot remove mandatory review" 1 "" "$FULL_BODY" false "$empty" "$empty" "$POLICY_SELF_EDIT"
run_case "reviewed policy edit still requires owner exception" 1 "" "$FULL_BODY" false "$codex" "$empty" "$POLICY_SELF_EDIT"
run_case "reviewed test self-edit still requires owner exception" 1 "" "$FULL_BODY" false "$codex" "$empty" "$POLICY_TEST_SELF_EDIT"
run_case "owner exact-head exception permits policy edit" 0 "" "$FULL_BODY" false \
    "$(comments_json "$CODEX_REVIEW" "$OWNER_EXCEPTION")" "$empty" "$POLICY_SELF_EDIT"

# All six accepted evasions are now explicit negative brackets.
run_case "direct timeout increase is default-rejected" 1 "" "$FULL_BODY" false "$codex" "$empty" "$TIMEOUT_INCREASE"
run_case "wider budget cap is default-rejected" 1 "" "$FULL_BODY" false "$codex" "$empty" "$CAP_INCREASE"
run_case "higher parallelism is default-rejected" 1 "" "$FULL_BODY" false "$codex" "$empty" "$PARALLELISM_INCREASE"
run_case "moving node off timed path is default-rejected" 1 "" "$FULL_BODY" false "$codex" "$empty" "$OFF_TIMED_PATH"
run_case "marking step non-blocking is default-rejected" 1 "" "$FULL_BODY" false "$codex" "$empty" "$NON_BLOCKING"
run_case "recorded baseline bump is default-rejected" 1 "" "$FULL_BODY" false "$codex" "$empty" "$BASELINE_BUMP"

# Owner exceptions are comments/reviews, never candidate-editable PR prose.
run_case "owner exact-head exception with evidence permits cap change" 0 "" "$FULL_BODY" false \
    "$(comments_json "$CODEX_REVIEW" "$OWNER_EXCEPTION")" "$empty" "$TIMEOUT_INCREASE"
run_case "weak owner exception is refused" 1 "" "$FULL_BODY" false \
    "$(comments_json "$CODEX_REVIEW" "$WEAK_OWNER_EXCEPTION")" "$empty" "$TIMEOUT_INCREASE"
run_case "candidate prose cannot mint owner approval" 1 "" "${FULL_BODY}

${OWNER_EXCEPTION}" false "$codex" "$empty" "$TIMEOUT_INCREASE"
run_case "stale-head owner exception is refused" 1 "" "$FULL_BODY" false \
    "$(comments_json "$CODEX_REVIEW" "${OWNER_EXCEPTION//$HEAD_SHA/$OLD_SHA}")" "$empty" "$TIMEOUT_INCREASE"

echo
echo "core-review-protocol-lint self-test: ${pass} passed, ${fail} failed."
[[ $fail -eq 0 ]]

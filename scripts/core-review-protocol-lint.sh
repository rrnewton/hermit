#!/usr/bin/env bash
# Enforce review and validation-budget policy for one PR.
#
# This script is executed from the trusted base branch by merge-gate.yml. The
# candidate head is data only: the workflow supplies its exact SHA, GitHub
# comments/reviews, and a base-to-head textual diff. Never execute the
# candidate's copy of this script.

set -euo pipefail

pr="${PR_NUMBER:-unknown}"
labels="${PR_LABELS-}"
body="${PR_BODY-}"
is_kvm="${PR_IS_KVM:-false}"
head_sha="${PR_HEAD_SHA-}"
author_login="${PR_AUTHOR_LOGIN-}"
owner_login="${OWNER_LOGIN:-rrnewton}"
comments_file="${PR_COMMENTS_FILE-}"
reviews_file="${PR_REVIEWS_FILE-}"
diff_file="${PR_DIFF_FILE-}"
commit_messages_file="${PR_COMMIT_MESSAGES_FILE-}"

die() {
    echo "::error::PR #${pr}: $*" >&2
    exit 2
}

[[ $head_sha =~ ^[0-9a-f]{40}$ ]] || die "PR_HEAD_SHA must be an exact 40-hex commit."
[[ -n $author_login ]] || die "PR_AUTHOR_LOGIN is required."
for input in "$comments_file" "$reviews_file" "$diff_file" "$commit_messages_file"; do
    [[ -n $input && -f $input ]] || die "review inputs and PR_DIFF_FILE must name readable files."
done
jq -e 'type == "array"' "$comments_file" >/dev/null || die "PR_COMMENTS_FILE is not a JSON array."
jq -e 'type == "array"' "$reviews_file" >/dev/null || die "PR_REVIEWS_FILE is not a JSON array."
jq -e '
    type == "array" and length > 0
    and all(
        type == "object"
        and (.sha | type == "string" and test("^[0-9a-f]{40}$"))
        and (.message | type == "string")
    )
' "$commit_messages_file" >/dev/null || die "PR_COMMIT_MESSAGES_FILE is not a nonempty commit-message array."

errors=0
fail() {
    echo "::error::PR #${pr}: $*"
    errors=$((errors + 1))
}

has_label() {
    printf '%s\n' "$labels" | grep -Fxq -- "$1"
}

has_section() {
    local section=$1
    printf '%s\n' "$body" | grep -Eiq \
        "^[[:space:]]*(#{1,6}[[:space:]]*${section}|\*\*[[:space:]]*${section}|${section}[[:space:]]*:)"
}

team_tag() {
    sed -nE '1s/^\[[^]]+\][[:space:]]+\[([^],]+),[[:space:]]*devbig[0-9]+\]$/\1/p'
}

head_author_identity=
declare -A author_identities=()
declare -A seen_commit_shas=()
commit_count=0
head_count=0
while IFS= read -r item; do
    commit_sha=$(jq -r '.sha' <<< "$item")
    commit_message=$(jq -r '.message' <<< "$item")
    commit_tag=$(awk 'NF { line=$0 } END { print line }' <<< "$commit_message")
    if [[ -n ${seen_commit_shas[$commit_sha]+x} ]]; then
        fail "commit-message input repeats ${commit_sha}."
        continue
    fi
    seen_commit_shas[$commit_sha]=1
    commit_count=$((commit_count + 1))

    commit_identity=
    if [[ $commit_tag == \[Human\] ]]; then
        commit_identity="human:${author_login,,}"
    elif grep -Eq '^\[(impl agent|coordinator),[^]]+\][[:space:]]+\[[a-z0-9][a-z0-9-]*,[[:space:]]+devbig[0-9]+\]$' <<< "$commit_tag"; then
        commit_team=$(printf '%s\n' "$commit_tag" | team_tag)
        commit_identity="agent:${commit_team,,}"
    else
        fail "commit ${commit_sha} must end with a standalone role tag and comma-delimited team/machine tag."
    fi

    if [[ -n $commit_identity ]]; then
        author_identities["$commit_identity"]=1
    fi

    if [[ $commit_sha == "$head_sha" ]]; then
        head_count=$((head_count + 1))
        head_author_identity=$commit_identity
    fi
done < <(jq -c '.[]' "$commit_messages_file")

((commit_count > 0)) || fail "base-to-head commit range is empty."
((head_count == 1)) || fail "base-to-head commit range must contain exact head ${head_sha} once."
[[ -n $head_author_identity ]] || fail "exact-head author identity is unavailable."

positive_verdict() {
    local review_body=$1 verdict
    verdict=$(printf '%s\n' "$review_body" | tail -n +2 | awk 'NF { print; exit }')
    grep -Eiq '^(PASS|APPROVE(D)?|ADVERSARIAL REVIEW CLEARED|CLEARED)([[:space:]:—-]|$)' <<< "$verdict"
}

review_count=0
codex_review=0
claude_review=0
consider_review() {
    local review_body=$1 reviewer_login=$2 reviewer_association=$3 exact_by_api=$4
    local first_line reviewer_team reviewer_identity family=

    [[ $reviewer_association =~ ^(OWNER|MEMBER|COLLABORATOR)$ ]] || return 0
    first_line=$(printf '%s\n' "$review_body" | sed -n '1p')
    grep -Eiq '^\[adversarial-reviewer agent,[^]]+\][[:space:]]+\[[a-z0-9][a-z0-9-]*,[[:space:]]+devbig[0-9]+\]$' <<< "$first_line" || return 0
    reviewer_team=$(printf '%s\n' "$first_line" | team_tag)
    [[ -n $reviewer_team ]] || return 0
    if [[ -n $reviewer_login && ${reviewer_login,,} != "${author_login,,}" ]]; then
        reviewer_identity="github:${reviewer_login,,}"
    else
        # Agents currently share the repository owner's GitHub credential. In
        # that case the mandatory role+team provenance tag is the independently
        # checkable fleet identity and must differ from the author's tag.
        reviewer_identity="agent:${reviewer_team,,}"
    fi
    [[ -z ${author_identities[$reviewer_identity]+x} ]] || return 0
    positive_verdict "$review_body" || return 0
    if [[ $exact_by_api != true ]]; then
        grep -Fq "$head_sha" <<< "$review_body" || return 0
    fi

    review_count=$((review_count + 1))
    if grep -Eiq '(codex|gpt-[0-9])' <<< "$first_line"; then
        family=codex
        codex_review=$((codex_review + 1))
    elif grep -Eiq '(claude|opus|sonnet|haiku)' <<< "$first_line"; then
        family=claude
        claude_review=$((claude_review + 1))
    fi
    echo "PR #${pr}: accepted independent ${family:-unclassified} review from ${reviewer_team} at ${head_sha}."
}

while IFS= read -r item; do
    comment_body=$(jq -r '.body // ""' <<< "$item")
    comment_login=$(jq -r '.user.login // ""' <<< "$item")
    comment_association=$(jq -r '.author_association // ""' <<< "$item")
    consider_review "$comment_body" "$comment_login" "$comment_association" false
done < <(jq -c '.[]' "$comments_file")

while IFS= read -r item; do
    [[ $(jq -r '.state // ""' <<< "$item") == APPROVED ]] || continue
    [[ $(jq -r '.commit_id // ""' <<< "$item") == "$head_sha" ]] || continue
    review_body=$(jq -r '.body // ""' <<< "$item")
    review_login=$(jq -r '.user.login // ""' <<< "$item")
    review_association=$(jq -r '.author_association // ""' <<< "$item")
    consider_review "$review_body" "$review_login" "$review_association" true
done < <(jq -c '.[]' "$reviews_file")

((review_count > 0)) || fail "no independent positive adversarial verdict bound to exact head ${head_sha}."

# Triggered core changes retain the stronger dual-family requirement. Labels
# route the review but are caches; the role-tagged exact-head verdicts above are
# the approval authority.
if has_label post-facto-human-review; then
    ((codex_review > 0)) || fail "post-facto-human-review requires an independent Codex-family exact-head approval."
    ((claude_review > 0)) || fail "post-facto-human-review requires an independent Claude-family exact-head approval."
fi

for section in "Summary" "Determinism" "Linux Semantics" "Validation"; do
    has_section "$section" || fail "PR body is missing the required \"${section}\" section."
done
if has_label post-facto-human-review; then
    has_section "Human Review Required" \
        || fail "post-facto-human-review PR is missing the required \"Human Review Required\" section."
fi
if [[ $is_kvm == true ]]; then
    has_section "Relationship to gVisor" \
        || fail "KVM change is missing the required \"Relationship to gVisor\" section."
fi

# Conservative trusted-base classifier. It does not try to prove that a
# threshold change is benign: any validation-control change in these classes
# is rejected unless the owner supplies a bound exception below.
sensitive_classes=$(
    awk '
        function control_path(p) {
            return p ~ /^(\.github\/workflows\/|ci\/|scripts\/|validate\.sh$|Makefile$)/
        }
        function review_guard(p) {
            return p ~ /^scripts\/core-review-protocol-lint(-test)?\.sh$/
        }
        /^diff --git a\// {
            path=$4
            sub(/^b\//, "", path)
            next
        }
        /^(---|\+\+\+) / { next }
        /^[+-]/ {
            if (!control_path(path)) next
            line=tolower(substr($0, 2))
            if (review_guard(path))
                print "review-policy"
            if (line ~ /(^|[^a-z])(timeout|timeouts|budget|budgets|cap|caps|limit|limits)([^a-z]|$)|resource_caps|max_(wall|cpu|seconds|duration)/)
                print "timeout-or-cap"
            if (line ~ /(^|[^a-z])(parallel|parallelism|concurrency|jobs|workers|threads)([^a-z]|$)|max-parallel|--jobs|-[jJ][0-9]/)
                print "parallelism"
            if (line ~ /continue-on-error|allow[_-]?failure|non[_-]?blocking|optional|allowed-to-fail|set \+e|\|\| true/)
                print "non-blocking"
            if (path ~ /(baseline|benchmark|history)/ || line ~ /baseline|p95|p99|median.*seconds|expected.*seconds|recorded.*cost/)
                print "baseline"
            if ($0 ~ /^-/ && path ~ /^ci\/dag\/.*\.json$/)
                print "timed-path-membership"
            if ($0 ~ /^-/ && path ~ /(validate|test_harness|safe-ci-dag-runner)/ && line ~ /(node|step|timed|blocking|required|run)/)
                print "timed-path-membership"
        }
    ' "$diff_file" | LC_ALL=C sort -u
)

owner_exception_body() {
    local approval_body=$1
    [[ $(printf '%s\n' "$approval_body" | sed -n '1p') == \[Human\]* ]] || return 1
    grep -Fq "$head_sha" <<< "$approval_body" || return 1
    grep -Eq '^TIMEOUT-CAP-EXCEPTION:[[:space:]]+APPROVED[[:space:]]*$' <<< "$approval_body" || return 1
    grep -Eq '^JUSTIFICATION:[[:space:]].{20,}$' <<< "$approval_body" || return 1
    grep -Eq '^EVIDENCE:[[:space:]].{20,}$' <<< "$approval_body" || return 1
}

owner_exception=false
if [[ -n $sensitive_classes ]]; then
    while IFS= read -r item; do
        [[ $(jq -r '.user.login // ""' <<< "$item") == "$owner_login" ]] || continue
        [[ $(jq -r '.author_association // ""' <<< "$item") == OWNER ]] || continue
        approval_body=$(jq -r '.body // ""' <<< "$item")
        if owner_exception_body "$approval_body"; then
            owner_exception=true
            break
        fi
    done < <(jq -c '.[]' "$comments_file")

    if [[ $owner_exception != true ]]; then
        while IFS= read -r item; do
            [[ $(jq -r '.user.login // ""' <<< "$item") == "$owner_login" ]] || continue
            [[ $(jq -r '.author_association // ""' <<< "$item") == OWNER ]] || continue
            [[ $(jq -r '.state // ""' <<< "$item") == APPROVED ]] || continue
            [[ $(jq -r '.commit_id // ""' <<< "$item") == "$head_sha" ]] || continue
            approval_body=$(jq -r '.body // ""' <<< "$item")
            if owner_exception_body "$approval_body"; then
                owner_exception=true
                break
            fi
        done < <(jq -c '.[]' "$reviews_file")
    fi

    if [[ $owner_exception != true ]]; then
        fail "validation-control change is default-rejected (${sensitive_classes//$'\n'/, }); require an owner-authored exact-head TIMEOUT-CAP-EXCEPTION with substantive JUSTIFICATION and EVIDENCE."
    else
        echo "PR #${pr}: owner exception is bound to ${head_sha} for: ${sensitive_classes//$'\n'/, }."
    fi
fi

if ((errors > 0)); then
    echo "::error::PR #${pr}: review/budget protocol NOT satisfied (${errors} problem(s)); blocking landing."
    exit 1
fi

echo "PR #${pr}: trusted-base review/budget protocol satisfied."

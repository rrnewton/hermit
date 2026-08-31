#!/usr/bin/env bash
#
# Mechanical gate: any change that TOUCHES a runnable demo (demos/**) must carry
# at least one ADVERSARIAL GREEN-DEMO ATTESTATION verifying the demo still runs
# GREEN -- independent of post-facto-human-review. Motivated by a change that
# flipped demos/05-qemu-boot.py flags (--no-rcb-time) and broke the
# demo without anyone running it green.
#
# The attestation is a commit-message trailer produced by an adversarial reviewer
# who actually ran the demo:
#
#   Demo-Green-Review: reviewer=<agent-id> demo=<demos/path|all> result=GREEN evidence=<url|path|sha>
#
# result must be GREEN; reviewer and evidence must be non-empty. The reviewer
# should be a DIFFERENT agent than the implementer (independence) -- see policy.
#
# Modes:
#   --range <BASE>..<HEAD>              # CI / lander: scan a commit range
#   --staged --message-file <FILE>     # commit-msg hook: staged diff + this message
#   --commit <SHA>                     # a single commit
# Override (implementer's pre-review commit; CI still blocks the merge):
#   HERMIT_DEMO_REVIEW_OVERRIDE=1
#
# Scope: demos/** EXCEPT *.md and demos/**/ignored/ (docs and scratch cannot
# change a demo's runtime green-ness). Widen by editing demo_touched() below.

set -uo pipefail

POLICY="demos/ADVERSARIAL-REVIEW-POLICY.md"
# Clean help for the safe probe: stdout, exit 0, no leaked raw comment header.
help() {
    cat <<'EOF'
check-demo-review.sh — require an adversarial green-demo attestation for any demos/** change

USAGE:
  scripts/check-demo-review.sh --range <BASE>..<HEAD>          scan a commit range (CI / lander)
  scripts/check-demo-review.sh --staged --message-file <FILE>  staged diff + this message (commit-msg hook)
  scripts/check-demo-review.sh --commit <SHA>                  a single commit
  scripts/check-demo-review.sh -h|--help                       show this help and exit (no side effects)

Passes (exit 0) when no runnable demo is touched, or a valid trailer is present:
  Demo-Green-Review: reviewer=<agent> demo=<demos/path|all> result=GREEN evidence=<url|path|sha>
HERMIT_DEMO_REVIEW_OVERRIDE=1 allows a LOCAL commit (never --range). Policy: demos/ADVERSARIAL-REVIEW-POLICY.md.
EOF
    exit 0
}

# Usage error: message to stderr, nonzero exit (kept distinct from --help).
usage() { sed -n '2,30p' "$0" >&2; exit 2; }

# Is a changed path a runnable demo file that requires attestation?
demo_touched() {
    case "$1" in
        demos/*/ignored/* | */demos/*/ignored/*) return 1 ;;
        *.md) return 1 ;;
        demos/* | */demos/*) return 0 ;;
        *) return 1 ;;
    esac
}

any_demo_touched() {  # stdin: NUL- or newline-separated paths
    local f hit=1
    while IFS= read -r f; do
        [ -z "$f" ] && continue
        if demo_touched "$f"; then printf '%s\n' "$f"; hit=0; fi
    done
    return $hit
}

has_attestation() {  # $1 = text blob to search for a valid trailer
    local line
    while IFS= read -r line; do
        printf '%s\n' "$line" | grep -qE '^[[:space:]]*Demo-Green-Review:' || continue
        printf '%s\n' "$line" | grep -qE '(^|[[:space:]])reviewer=[^[:space:]]+([[:space:]]|$)' || continue
        printf '%s\n' "$line" | grep -qE '(^|[[:space:]])demo=(all|demos/[^[:space:]]+)([[:space:]]|$)' || continue
        printf '%s\n' "$line" | grep -qE '(^|[[:space:]])result=GREEN([[:space:]]|$)' || continue
        printf '%s\n' "$line" | grep -qE '(^|[[:space:]])evidence=[^[:space:]]+([[:space:]]|$)' || continue
        return 0
    done <<<"$1"
    return 1
}

git_refused() {
    echo "demo-review gate: REFUSED: git could not inspect $1" >&2
    exit 2
}

mode="" ; range="" ; msgfile="" ; commit=""
while (($#)); do
    case "$1" in
        --range) mode=range; range="$2"; shift 2 ;;
        --staged) mode=staged; shift ;;
        --message-file) msgfile="$2"; shift 2 ;;
        --commit) mode=commit; commit="$2"; shift 2 ;;
        -h|--help) help ;;
        *) echo "unknown arg: $1" >&2; usage ;;
    esac
done

case "$mode" in
    range)
        changed=$(git diff --name-only "$range" 2>/dev/null) || git_refused "range $range"
        commits=$(git rev-list "$range" 2>/dev/null) || git_refused "range $range"
        messages=""
        latest_demo_commit=""
        for revision in $commits; do
            paths=$(git diff-tree --no-commit-id --name-only -r "$revision" 2>/dev/null) \
                || git_refused "commit $revision"
            if printf '%s\n' "$paths" | any_demo_touched >/dev/null; then
                latest_demo_commit="$revision"
                break
            fi
        done
        if [ -n "$latest_demo_commit" ]; then
            for revision in $commits; do
                message=$(git log -1 --format='%B' "$revision" 2>/dev/null) \
                    || git_refused "commit $revision"
                messages="${messages}${message}"$'\n'
                [ "$revision" = "$latest_demo_commit" ] && break
            done
        fi
        where="range $range" ;;
    staged)
        changed=$(git diff --cached --name-only 2>/dev/null) || git_refused "staged change"
        messages=$([ -n "$msgfile" ] && cat "$msgfile" 2>/dev/null || true)
        where="staged change" ;;
    commit)
        changed=$(git show --name-only --format= "$commit" 2>/dev/null) \
            || git_refused "commit $commit"
        messages=$(git log -1 --format='%B' "$commit" 2>/dev/null) \
            || git_refused "commit $commit"
        where="commit $commit" ;;
    *) usage ;;
esac

touched=$(printf '%s\n' "$changed" | any_demo_touched) || {
    echo "demo-review gate: no runnable demo files touched in $where -- OK."
    exit 0
}

if has_attestation "$messages"; then
    echo "demo-review gate: green-demo attestation present for $where -- OK."
    echo "  touched:"; printf '    %s\n' $touched
    exit 0
fi

{
    echo "----------------------------------------------------------------------"
    echo "DEMO-REVIEW GATE: $where touches a runnable demo but has NO adversarial"
    echo "green-demo attestation:"
    printf '    %s\n' $touched
    echo
    echo "Any demos/** change must be verified GREEN by an adversarial reviewer who"
    echo "actually ran the demo, then recorded a commit-message trailer:"
    echo
    echo "  Demo-Green-Review: reviewer=<agent> demo=<demos/path|all> result=GREEN evidence=<url|path|sha>"
    echo
    echo "Policy: ${POLICY}"
} >&2

if [ "${HERMIT_DEMO_REVIEW_OVERRIDE:-}" = "1" ] && [ "$mode" != "range" ]; then
    echo "HERMIT_DEMO_REVIEW_OVERRIDE=1 -- allowing this LOCAL commit; the green-demo" >&2
    echo "review is still REQUIRED and CI/lander will BLOCK the merge until the" >&2
    echo "Demo-Green-Review attestation exists in the PR." >&2
    exit 0
fi
[ "$mode" = "range" ] && echo "(CI/lander gate is authoritative -- no local override applies here.)" >&2
echo "----------------------------------------------------------------------" >&2
exit 1

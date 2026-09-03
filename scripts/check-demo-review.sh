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
#   Demo-Green-Review: reviewer=<agent-id> demo=<demos/path[,demos/path...]|all> result=GREEN evidence=<url|path|sha>
#
# result must be GREEN; reviewer and evidence must be non-empty. The reviewer
# must be a DIFFERENT agent than the role=impl disclosure (independence), and a
# GREEN claim must not contradict the commit body's reported demo result.
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

Passes (exit 0) when no runnable demo is touched, or valid trailers cover every path:
  Demo-Green-Review: reviewer=<agent> demo=<demos/path[,demos/path...]|all> result=GREEN evidence=<url|path|sha>
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

# Emit the implementer identities carried by canonical disclosure tags. Git's
# author is the repository owner for agent-authored commits, so it cannot
# establish whether reviewer=<agent-id> is independent.
implementer_identities() {  # $1 = commit message
    local line tag identity
    while IFS= read -r line; do
        [[ "$line" =~ ^\[([^]]+)\] ]] || continue
        tag="${BASH_REMATCH[1]}"
        [[ ",$tag," =~ ,[[:space:]]*role=impl[[:space:]]*, ]] || continue
        identity=$(printf '%s\n' "$tag" | cut -d, -f2 | sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//')
        [ -n "$identity" ] && printf '%s\n' "$identity"
    done <<<"$1" | sort -u
}

# Does a demo= claim include the numbered demo reported by a mechanical result
# line such as "demo 5 wired: FIRST RUN SAVED, PARTIAL"?
claim_covers_demo_number() {  # $1 = comma-separated demo= value, $2 = number
    local claims="$1" wanted="$2" value number
    while IFS= read -r value; do
        [ "$value" = all ] && return 0
        if [[ "$value" =~ ^demos/0*([0-9]+)([^0-9]|$) ]]; then
            number="${BASH_REMATCH[1]}"
            [ "$((10#$number))" = "$((10#$wanted))" ] && return 0
        fi
    done < <(printf '%s\n' "$claims" | tr ',' '\n')
    return 1
}

# A body's mechanical result lines may include deliberate negative controls as
# well as the real run. A claim is contradicted when it covers a numbered demo
# for which the body reports a non-green result and no green result. This makes
# a forced-failure check followed by a successful real run valid, while a body
# that reports only PARTIAL or FAILURE cannot claim GREEN.
claim_contradicts_body() {  # $1 = commit message, $2 = demo= value
    local text="$1" claims="$2" line number result
    declare -A green=() nongreen=()
    while IFS= read -r line; do
        if [[ "${line,,}" =~ ^[[:space:]]*demo[[:space:]]*0*([0-9]+)[^:]*:[[:space:]]*(first[[:space:]]+run[[:space:]]+saved,[[:space:]]*)?([a-z_-]+) ]]; then
            number="${BASH_REMATCH[1]}"
            result="${BASH_REMATCH[3]}"
            case "$result" in
                green|pass|success) green[$((10#$number))]=1 ;;
                partial|failure|failed|red|skip|skipped|incomplete|no_result|no-result|error)
                    nongreen[$((10#$number))]=1 ;;
            esac
        fi
    done <<<"$text"
    for number in "${!nongreen[@]}"; do
        [ "${green[$number]:-0}" = 1 ] && continue
        claim_covers_demo_number "$claims" "$number" && return 0
    done
    return 1
}

# Emit every demo path named by a valid trailer, one per line. A trailer may
# cover several exact paths with a comma-separated demo= value; `all` and
# directory coverage retain their documented meanings. Invalid trailers are
# reported and withheld while later valid trailers remain usable.
attested_demos() {  # $1 = text blob, $2 = report rejected trailers (optional)
    local text="$1" report="${2:-}" line reviewer demos implementer self_review
    while IFS= read -r line; do
        printf '%s\n' "$line" | grep -qE '^[[:space:]]*Demo-Green-Review:' || continue
        printf '%s\n' "$line" | grep -qE '(^|[[:space:]])reviewer=[^[:space:]]+([[:space:]]|$)' || continue
        printf '%s\n' "$line" | grep -qE '(^|[[:space:]])demo=(all|demos/[^[:space:]]+)([[:space:]]|$)' || continue
        printf '%s\n' "$line" | grep -qE '(^|[[:space:]])result=GREEN([[:space:]]|$)' || continue
        printf '%s\n' "$line" | grep -qE '(^|[[:space:]])evidence=[^[:space:]]+([[:space:]]|$)' || continue
        reviewer=$(printf '%s\n' "$line" | grep -oE '(^|[[:space:]])reviewer=[^[:space:]]+' | sed -E 's/^[[:space:]]*reviewer=//')
        demos=$(printf '%s\n' "$line" | grep -oE '(^|[[:space:]])demo=[^[:space:]]+' | sed -E 's/^[[:space:]]*demo=//')
        self_review=0
        while IFS= read -r implementer; do
            if [ "$reviewer" = "$implementer" ]; then
                self_review=1
                break
            fi
        done < <(implementer_identities "$text")
        if [ "$self_review" = 1 ]; then
            if [ "$report" = report ]; then
                echo "demo-review gate: REFUSED trailer: reviewer=$reviewer is also the role=impl identity" >&2
            fi
            continue
        fi
        if claim_contradicts_body "$text" "$demos"; then
            if [ "$report" = report ]; then
                echo "demo-review gate: REFUSED trailer: demo=$demos result=GREEN contradicts the body's reported result" >&2
            fi
            continue
        fi
        printf '%s\n' "$demos" | tr ',' '\n'
    done <<<"$text"
}

# Does one attested demo= value cover a touched path?
#   all                        -> covers everything
#   demos/05-qemu-boot.py      -> covers exactly that file
#   demos/qemu-busybox         -> covers files beneath that directory
# A bare filename is deliberately not accepted: the trailer must name the path
# as it appears in the diff.
demo_value_covers() {  # $1 = demo= value, $2 = touched path
    local value="${1%/}" path="$2"
    [ "$value" = all ] && return 0
    [ "$value" = "$path" ] && return 0
    case "$path" in "$value"/*) return 0 ;; esac
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

# Name each rejected claim once. Coverage below may inspect the same commit for
# several touched paths, but repeating one refusal per path hides the useful
# reason in noise.
if [ "$mode" = range ]; then
    for candidate in $commits; do
        candidate_msg=$(git log -1 --format='%B' "$candidate" 2>/dev/null) \
            || git_refused "commit $candidate"
        attested_demos "$candidate_msg" report >/dev/null
    done
else
    attested_demos "$messages" report >/dev/null
fi

# A review applies only to the paths it names, and it must be at or after the
# last change to each path. One old `demo=all` trailer must not bless later
# edits, and a trailer for demos 1-7 must not bless Demo 8.
uncovered=""
stale=""

if [ "$mode" = range ]; then
    for path in $touched; do
        last_change=$(git log --format='%H' -1 "$range" -- "$path" 2>/dev/null) \
            || git_refused "last change to $path in range $range"
        if [ -z "$last_change" ]; then
            uncovered="$uncovered $path"
            continue
        fi
        covered=0
        stale_only=0
        for candidate in $commits; do
            candidate_msg=$(git log -1 --format='%B' "$candidate" 2>/dev/null) \
                || git_refused "commit $candidate"
            value_covers=0
            for value in $(attested_demos "$candidate_msg"); do
                if demo_value_covers "$value" "$path"; then
                    value_covers=1
                    break
                fi
            done
            [ "$value_covers" = 1 ] || continue
            if git merge-base --is-ancestor "$last_change" "$candidate" 2>/dev/null; then
                covered=1
                break
            fi
            stale_only=1
        done
        if [ "$covered" = 1 ]; then
            continue
        elif [ "$stale_only" = 1 ]; then
            stale="$stale $path"
        else
            uncovered="$uncovered $path"
        fi
    done
else
    for path in $touched; do
        covered=0
        for value in $(attested_demos "$messages"); do
            if demo_value_covers "$value" "$path"; then
                covered=1
                break
            fi
        done
        [ "$covered" = 1 ] || uncovered="$uncovered $path"
    done
fi

if [ -z "$uncovered" ] && [ -z "$stale" ]; then
    echo "demo-review gate: green-demo attestation covers every touched demo in $where -- OK."
    echo "  touched:"; printf '    %s\n' $touched
    exit 0
fi

{
    echo "----------------------------------------------------------------------"
    echo "DEMO-REVIEW GATE: $where touches runnable demos that are not covered by"
    echo "a valid adversarial green-demo attestation."
    echo
    if [ -n "$uncovered" ]; then
        echo "  NO attestation names these touched demos:"
        printf '    %s\n' $uncovered
        echo
    fi
    if [ -n "$stale" ]; then
        echo "  An attestation names these, but it was recorded BEFORE the last"
        echo "  commit that changed them, so it attests to superseded content."
        echo "  Re-run the demo at the current head and record a new trailer:"
        printf '    %s\n' $stale
        echo
    fi
    echo "  all touched demo files:"
    printf '    %s\n' $touched
    echo
    echo "Any demos/** change must be verified GREEN by an adversarial reviewer who"
    echo "actually ran the demo, then recorded a commit-message trailer:"
    echo
    echo "  Demo-Green-Review: reviewer=<agent> demo=<demos/path[,demos/path...]|all> result=GREEN evidence=<url|path|sha>"
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

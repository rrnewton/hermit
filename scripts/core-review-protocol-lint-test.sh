#!/usr/bin/env bash
# Self-test for core-review-protocol-lint.sh.
#
# Feeds the linter a set of fixture PRs (labels + body + KVM flag) and asserts
# the exit status. Run locally or in CI:
#
#     scripts/core-review-protocol-lint-test.sh
#
# Exits 0 when every case matches its expected status, 1 otherwise.

set -euo pipefail

# Same rule as the check-status checkers: if the pinned review-label contract
# cannot be consulted, no case here is evaluable. Declare it and exit 0 so
# `make lint-checks` still passes and ci/lint-checks-node.sh classifies the run
# no_result (exit 75) instead of fail. A nonzero exit could never be reported as
# no_result -- classify_run lets a real failure outrank any marker.
_probe_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
_auth_rc=0
_authority_dir=$("$_probe_root/scripts/authority-available.sh" "$_probe_root/scripts/review_contract_adapter.py") || _auth_rc=$?
# ⚠️ ONLY EXIT 3 MAY SKIP, AND AN EARLIER VERSION OF THIS GUARD GOT IT WRONG.
# It treated ANY nonzero from the helper as "unavailable", so a TAMPERED
# authority -- fetched successfully, wrong bytes, AuthorityIntegrityError, exit
# 1 -- was laundered into a no-result skip. That is the most dangerous possible
# reading: the one failure mode the content pin exists to catch, silently
# reported as "could not evaluate". Measured 2026-09-04: checker exited 0 with a
# marker against a deliberately corrupted authority.
if [ "$_auth_rc" -eq 3 ]; then
    echo "NO-RESULT-CASE: core-review-protocol-lint-test.sh: the pinned review-label contract could not be consulted; no case in this checker was evaluated"
    exit 0
elif [ "$_auth_rc" -ne 0 ]; then
    echo "core-review-protocol-lint-test.sh: obtaining the pinned review-label contract failed with exit $_auth_rc; that is not an outage and is not being skipped" >&2
    exit "$_auth_rc"
fi
# ⚠️ EXPORTING THIS IS THE POINT, not tidiness. Every later adapter process in
# this checker now reads the authority from disk instead of fetching it again,
# so a 504 arriving after the check above cannot turn a case into a nonzero
# exit. Probing alone left exactly that window open; see
# scripts/authority-available.sh.
export DEV_HERMIT_PARENT="$_authority_dir"
trap 'rm -rf "$_authority_dir"' EXIT


SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly LINT="$SCRIPT_DIR/core-review-protocol-lint.sh"
readonly CONTRACT_ADAPTER="$SCRIPT_DIR/review_contract_adapter.py"

# A complete, valid non-KVM PR body containing every required section.
readonly FULL_BODY='## Summary
Adds a thing.

## Determinism
Deterministic because reasons and an informal proof.

## Linux Semantics
Matches the kernel behavior described here.

## Validation
`cargo test -p hermit-detcore` passed at L2 (ptrace).

## Human Review Required
Trigger 4: core DetCore scheduling change.'

# The label set for a fully reviewed + approved PR (round 1).
readonly FULL_LABELS='post-facto-human-review
adversarial-review-codex1
adversarial-review-claude1
passed-review-codex
passed-review-claude'

pass=0
fail=0

# run_case NAME EXPECTED_EXIT LABELS BODY IS_KVM
run_case() {
    local name=$1 expected=$2 labels=$3 body=$4 is_kvm=${5:-false}
    local actual=0
    PR_LABELS="$labels" PR_BODY="$body" PR_IS_KVM="$is_kvm" PR_NUMBER="test" \
        bash "$LINT" >/dev/null 2>&1 || actual=$?
    if [ "$actual" -eq "$expected" ]; then
        echo "ok   - ${name} (exit ${actual})"
        pass=$((pass + 1))
    else
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"
        fail=$((fail + 1))
    fi
}

# --- Not applicable: no post-facto-human-review label always passes. ----------
run_case "unlabeled PR passes even with empty body" 0 \
    $'random-label\nlocally-validated' ""
run_case "unlabeled PR passes even missing everything the protocol wants" 0 \
    "" ""

# --- Happy paths -------------------------------------------------------------
run_case "labeled + full labels + all sections (non-KVM) passes" 0 \
    "$FULL_LABELS" "$FULL_BODY"

run_case "later review round (round 2 labels) still passes" 0 \
    $'post-facto-human-review\nadversarial-review-codex2\nadversarial-review-claude3\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

run_case "KVM PR with Relationship to gVisor section passes" 0 \
    "$FULL_LABELS" "${FULL_BODY}"$'\n\n## Relationship to gVisor\nN/A: no gVisor analog.' \
    true

run_case "bold-style headings are accepted" 0 \
    "$FULL_LABELS" \
    $'**Summary** foo\n**Determinism** bar\n**Linux Semantics** baz\n**Validation** qux\n**Human Review Required** trigger 4'

# --- Missing review labels blocks --------------------------------------------
run_case "missing adversarial-review-codex blocks" 1 \
    $'post-facto-human-review\nadversarial-review-claude1\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

run_case "missing adversarial-review-claude blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

run_case "missing passed-review-codex blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\nadversarial-review-claude1\npassed-review-claude' \
    "$FULL_BODY"

run_case "missing passed-review-claude blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\nadversarial-review-claude1\npassed-review-codex' \
    "$FULL_BODY"

run_case "adversarial review present but not approved blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\nadversarial-review-claude1' \
    "$FULL_BODY"

run_case "round label out of range (round 5) does not count, blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex5\nadversarial-review-claude5\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

run_case "non-label review-round-codex does not count, blocks" 1 \
    $'post-facto-human-review\nreview-round-codex\nadversarial-review-claude1\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

# Every numbered label accepted by the writer's shared contract must also pass
# this reader when the other families use their first accepted label.
contract_output=$(python3 "$CONTRACT_ADAPTER" --format lint-records)
declare -a contract_families=()
declare -A contract_approval=()
declare -A contract_rounds=()
contract_post_facto=
while IFS=$'\t' read -r first second third; do
    if [ "$first" = post-facto ]; then
        contract_post_facto=$second
        continue
    fi
    contract_families+=("$first")
    contract_approval[$first]=$second
    contract_rounds[$first]=$third
done <<<"$contract_output"

for family in "${contract_families[@]}"; do
    IFS=, read -r -a family_rounds <<<"${contract_rounds[$family]}"
    for candidate in "${family_rounds[@]}"; do
        labels=$contract_post_facto
        for fixture_family in "${contract_families[@]}"; do
            IFS=, read -r first_round _ <<<"${contract_rounds[$fixture_family]}"
            if [ "$fixture_family" = "$family" ]; then
                labels+=$'\n'"$candidate"
            else
                labels+=$'\n'"$first_round"
            fi
            labels+=$'\n'"${contract_approval[$fixture_family]}"
        done
        run_case "accepted label ${candidate} passes through the reader" 0 \
            "$labels" "$FULL_BODY"
    done
done

diagnostic_status=0
diagnostic=$(PR_LABELS=$'post-facto-human-review\nreview-round-codex\nadversarial-review-claude1\npassed-review-codex\npassed-review-claude' \
    PR_BODY="$FULL_BODY" PR_NUMBER=test bash "$LINT" 2>&1) || diagnostic_status=$?
if [ "$diagnostic_status" -eq 1 ] \
    && [[ $diagnostic == *"adversarial-review-codex1, adversarial-review-codex2, adversarial-review-codex3, adversarial-review-codex4"* ]] \
    && [[ $diagnostic != *"review-round-codex"* ]]; then
    echo "ok   - missing-round diagnostic names exact accepted alternatives"
    pass=$((pass + 1))
else
    echo "FAIL - missing-round diagnostic did not name exact accepted alternatives"
    fail=$((fail + 1))
fi

# --- Missing body sections blocks --------------------------------------------
run_case "missing Summary section blocks" 1 \
    "$FULL_LABELS" \
    $'## Determinism\nx\n## Linux Semantics\ny\n## Validation\nz\n## Human Review Required\nt'

run_case "missing Linux Semantics section blocks" 1 \
    "$FULL_LABELS" \
    $'## Summary\nx\n## Determinism\ny\n## Validation\nz\n## Human Review Required\nt'

run_case "missing Human Review Required section blocks" 1 \
    "$FULL_LABELS" \
    $'## Summary\nx\n## Determinism\ny\n## Linux Semantics\nl\n## Validation\nz'

run_case "empty body blocks a labeled PR" 1 \
    "$FULL_LABELS" ""

# --- KVM-specific section -----------------------------------------------------
run_case "KVM PR without Relationship to gVisor section blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" true

run_case "non-KVM PR does not require Relationship to gVisor" 0 \
    "$FULL_LABELS" "$FULL_BODY" false

# --- Prose must not satisfy a section ----------------------------------------
run_case "prose mention of a section keyword does not satisfy it" 1 \
    "$FULL_LABELS" \
    $'## Summary\nIn summary, this changes determinism and validation broadly.\n## Determinism\nd\n## Validation\nv\n## Human Review Required\nt'

# --- UNSET vs EMPTY: the defect this gate had, in both directions -----------
# run_case cannot express "unset", because it always assigns the variables.
# These cases invoke the lint directly with the variable removed from the
# environment, which is the whole point: the previous `${PR_LABELS-}` made the
# unset case indistinguishable from an empty one and returned a PASS having
# checked nothing.
run_unset_case() { # NAME EXPECTED_EXIT UNSET_VAR [ENV...]
    local name=$1 expected=$2 unset_var=$3; shift 3
    local actual=0
    env -u "$unset_var" PR_NUMBER=test "$@" bash "$LINT" >/dev/null 2>&1 || actual=$?
    if [ "$actual" -eq "$expected" ]; then
        echo "ok   - ${name} (exit ${actual})"; pass=$((pass + 1))
    else
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"; fail=$((fail + 1))
    fi
}

run_unset_case "PR_LABELS unset REFUSES (was a silent pass)" 2 PR_LABELS PR_BODY=""
run_case "PR_LABELS empty is a PASS and is not the unset case" 0 "" ""
run_unset_case "PR_BODY unset REFUSES when the protocol applies" 2 PR_BODY \
    "PR_LABELS=$FULL_LABELS"
run_unset_case "PR_BODY unset is IGNORED when the protocol does not apply" 0 PR_BODY \
    "PR_LABELS=category:detcore"

# The genuinely-empty label set must still take the not-applicable path, so the
# refusal above cannot be mistaken for "any missing label now blocks".
run_case "empty labels + empty body still passes (not applicable)" 0 "" ""

# --- A failed predicate is not a clean no-match -------------------------------
#
# The linter's grep predicates accept only grep's 0 (match) and 1 (no match).
# Every other observable shell status must become the linter's internal-error 2.
# The fake python3 supplies the already-validated contract records so these
# checks isolate the predicate status rather than depending on network access.
run_predicate_status_case() { # NAME MODE RAW_STATUS OPERATION
    local name=$1 mode=$2 raw_status=$3 operation=$4
    local actual=0 output

    # These functions are exported and invoked by the child Bash, which the
    # static check cannot see from this process.
    # shellcheck disable=SC2317
    python3() {
        printf '%s\n' \
            $'post-facto\tpost-facto-human-review' \
            $'codex\tpassed-review-codex\tadversarial-review-codex1,adversarial-review-codex2,adversarial-review-codex3,adversarial-review-codex4' \
            $'claude\tpassed-review-claude\tadversarial-review-claude1,adversarial-review-claude2,adversarial-review-claude3,adversarial-review-claude4'
    }
    # shellcheck disable=SC2317
    grep() {
        while IFS= read -r _; do :; done
        case "$PREDICATE_FAILURE_MODE" in
            status) return "$PREDICATE_FAILURE_STATUS" ;;
            section-status)
                if [ "$1" = -Eiq ]; then
                    return "$PREDICATE_FAILURE_STATUS"
                fi
                return 0
                ;;
            not-executable) command / "$@" ;;
            not-found) command /tmp/core-review-protocol-lint-no-such-command "$@" ;;
        esac
    }
    export -f python3 grep

    output=$(PREDICATE_FAILURE_MODE="$mode" PREDICATE_FAILURE_STATUS="$raw_status" \
        PR_LABELS=post-facto-human-review PR_BODY='' PR_IS_KVM=false PR_NUMBER=test \
        bash "$LINT" 2>&1) || actual=$?
    unset -f python3 grep

    if [ "$actual" -eq 2 ] \
        && [[ $output == *"${operation} could not decide (exit ${raw_status})"* ]]; then
        echo "ok   - ${name} (predicate exit ${raw_status} -> linter exit 2)"
        pass=$((pass + 1))
    else
        echo "FAIL - ${name}: predicate exit ${raw_status}, linter exit ${actual}"
        printf '%s\n' "$output"
        fail=$((fail + 1))
    fi
}

run_predicate_status_case \
    "command found but not executable refuses" not-executable 126 "label lookup"
run_predicate_status_case \
    "command not found refuses" not-found 127 "label lookup"

predicate_status_failures=0
for predicate_mode in status section-status; do
    if [ "$predicate_mode" = status ]; then
        operation="label lookup"
    else
        operation="PR-body section lookup"
    fi
    for ((predicate_status = 2; predicate_status <= 255; predicate_status++)); do
        before_fail=$fail
        run_predicate_status_case \
            "${operation} status ${predicate_status} is not a clean no-match" \
            "$predicate_mode" "$predicate_status" "$operation" >/dev/null
        if [ "$fail" -ne "$before_fail" ]; then
            predicate_status_failures=$((predicate_status_failures + 1))
        fi
    done
done
if [ "$predicate_status_failures" -eq 0 ]; then
    echo "ok   - label and section statuses 2..255 refuse as linter exit 2"
else
    echo "FAIL - ${predicate_status_failures} label/section statuses in 2..255 did not refuse"
fi

# Prove that an unreviewed local or fetched parent contract cannot become the
# authority silently. A changed local file falls back to the reviewed bytes;
# changed fetched bytes are refused by the content pin.
ROOT_DIR="$SCRIPT_DIR/.." python3 - <<'PY'
import hashlib
import os
from pathlib import Path
import sys
import tempfile

root = Path(os.environ["ROOT_DIR"])
sys.path.insert(0, str(root / "scripts"))
import review_contract_adapter as adapter

source = adapter._verified_source()
assert hashlib.sha256(source).hexdigest() == adapter.AUTHORITY_SHA256

with tempfile.TemporaryDirectory(prefix="review-contract-adapter-") as tmp:
    parent = Path(tmp)
    authority = parent / adapter.AUTHORITY_RELATIVE_PATH
    authority.parent.mkdir(parents=True)
    authority.write_text("raise RuntimeError('unreviewed local contract executed')\n")
    os.environ["DEV_HERMIT_PARENT"] = str(parent)
    adapter._fetch_pinned_source = lambda: source
    assert adapter._verified_source() == source

    adapter._fetch_pinned_source = lambda: source + b"# changed\n"
    try:
        adapter._verified_source()
    except RuntimeError as error:
        assert "digest mismatch" in str(error)
    else:
        raise AssertionError("changed fetched review contract passed its content pin")
PY
echo "ok   - review-contract adapter accepts only content-pinned authority bytes"
pass=$((pass + 1))

echo
echo "core-review-protocol-lint self-test: ${pass} passed, ${fail} failed."
[ "$fail" -eq 0 ]

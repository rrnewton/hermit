#!/usr/bin/env bash
#
# CI node entrypoint for `make lint-checks`.
#
# WHY THIS WRAPPER EXISTS: the target's checkers need initialized submodules --
# ci/verify-submodules.sh inspects them directly, and run-reverie-pin-check.sh and
# check-nested-lockfiles.rs both run under $(SUBMODULE_PROXY) against the pinned
# trees. A freshly created linked worktree has NO submodules initialized, so
# verify-submodules.sh reports a leading '-' inventory and exits 1.
#
# That exit is a SETUP condition, not a product failure, and letting it land as a
# `fail` is the third-state bug: an abort that reads like a failure. scripts/
# validate.rs:5362 reserves exit 75 (EX_TEMPFAIL) as "the only nonzero code that is
# not a product failure"; outcome_is_no_result() classifies it `no_result` and
# outcome_is_failure() excludes it. ci/run-with-reverie-dbt-budget.sh already uses
# 75 for its pin-mismatch refusal, so this follows the established spelling rather
# than inventing one. Every other nonzero exit from the target stays loud.
#
# The distinction is load-bearing in practice, not hypothetical: an agent hit this
# exact abort in a fresh worktree on 2026-08-25 and had to reason it out by hand to
# call it environment rather than a red.
set -euo pipefail

# Classify a `git submodule status` inventory. Each entry is marked: leading '-'
# uninitialized, '+' a revision other than the pin, 'U' a merge conflict.
#
# ONLY '-' IS A SETUP CONDITION. '+' and 'U' are real drift and must fall through
# to verify-submodules.sh and be reported as an ordinary failure -- classifying
# those as no_result would silence exactly the drift the checker exists to catch.
# Reads the inventory on stdin; echoes "ok", "empty", or "uninitialized <n>".
classify_inventory() {
    local inventory uninitialized
    inventory="$(cat)"
    if [ -z "${inventory//[[:space:]]/}" ]; then
        echo 'empty'
        return 0
    fi
    uninitialized="$(printf '%s\n' "$inventory" | grep -c '^-' || true)"
    if [ "${uninitialized:-0}" -ne 0 ]; then
        echo "uninitialized ${uninitialized}"
        return 0
    fi
    echo 'ok'
}

self_test() {
    local got failures=0
    check_case() {
        local name="$1" want="$2" input="$3"
        got="$(printf '%s' "$input" | classify_inventory)"
        if [ "$got" != "$want" ]; then
            echo "FAIL: ${name}: expected '${want}', got '${got}'" >&2
            failures=$((failures + 1))
        fi
    }

    check_case 'clean inventory'   'ok'               ' abc123 agent-utils (v1)
 def456 third-party/rr (5.9.0)'
    check_case 'uninitialized'     'uninitialized 1'  '-def456 third-party/rr'
    check_case 'two uninitialized' 'uninitialized 2'  '-abc123 agent-utils
-def456 third-party/rr'
    check_case 'empty inventory'   'empty'            ''
    check_case 'whitespace only'   'empty'            '   '
    # The negative arm, and the one that matters most: drift and conflicts are
    # NOT setup conditions and must not be reported as no_result.
    check_case 'drift is not setup'    'ok' '+def456 third-party/rr (5.9.0-1)'
    check_case 'conflict is not setup' 'ok' 'Udef456 third-party/rr'
    check_case 'mixed drift+clean'     'ok' ' abc123 agent-utils
+def456 third-party/rr'
    # A '-' anywhere in the inventory still wins, including alongside drift.
    check_case 'mixed uninit+drift' 'uninitialized 1' '+abc123 agent-utils
-def456 third-party/rr'

    if [ "$failures" -ne 0 ]; then
        echo "lint-checks-node --self-test: ${failures} case(s) failed" >&2
        return 1
    fi
    echo 'PASS: lint-checks-node classifies uninitialized as no_result, and drift/conflict as failure'
}

if [ "${1:-}" = '--self-test' ]; then
    self_test
    exit $?
fi

cd "$(dirname "$0")/.."

verdict="$(git submodule status 2>/dev/null | classify_inventory)"
case "$verdict" in
    empty)
        echo 'lint-checks: NO RESULT -- `git submodule status` produced no inventory;' >&2
        echo '  cannot establish whether the submodule precondition holds.' >&2
        exit 75
        ;;
    uninitialized*)
        echo "lint-checks: NO RESULT -- ${verdict#uninitialized } submodule(s) not initialized." >&2
        echo '  This is a SETUP condition, not a lint failure: the checkers in this' >&2
        echo '  target read the pinned submodule trees. Run `make checkout-all` (or' >&2
        echo '  `git submodule update --init`) in this checkout and re-run the node.' >&2
        git submodule status | sed 's/^/    /' >&2
        exit 75
        ;;
esac

# ⚠️ THE PARENT-ADAPTER PRECONDITION IS CLASSIFIED AFTER THE TARGET RUNS, NOT BEFORE.
# An earlier version exited 75 HERE, before `make lint-checks`, and so skipped every
# checker in the target -- 17 of them -- for a precondition that affects one arm of
# one case in one of them. It shipped saying "Every other checker in this target is
# unaffected and would have run", which was false: none of them ran. That is worse
# than the false main-red it was fixing, and it landed one day after this node was
# created precisely so those checkers would be gated by construction.
#
# So: run the whole target. scripts/test_validate_stop_paths.py now skips only the
# arm it cannot evaluate, passes everything else, and announces the skip on stderr
# with a machine-readable prefix. If the target SUCCEEDED but something announced
# itself unevaluated, the run as a whole is a no_result: everything that could be
# checked was checked and passed, and something could not be checked.
#
# Any real failure still propagates unchanged -- a nonzero from make is a failure,
# never a no_result, because a no_result must not be able to swallow a red.
#
# ⚠️ WHY 75 AND NOT A NEW CODE. scripts/validate.rs recognises exactly one no-result
# value -- NO_RESULT_EXIT_CODE = 75, matched by outcome_is_no_result() and excluded
# by outcome_is_failure() -- so any other number is classified a FAILURE and would
# reintroduce the false main-red. The code space has one slot. (The general rule
# about not collapsing two conditions into one code is stated in
# ci-hub/bin/gh-merge-verified in the DEV-HERMIT PARENT repository; this repository
# has no ci-hub/ directory, so that path does not resolve from here.)
NO_RESULT_MARKER='NO-RESULT-CASE:'
node_out=$(mktemp) || exit 1
trap 'rm -f "$node_out"' EXIT
set +e
make lint-checks 2>&1 | tee "$node_out"
make_rc=${PIPESTATUS[0]}
set -e
if [ "$make_rc" -ne 0 ]; then
    exit "$make_rc"
fi
if grep -q "$NO_RESULT_MARKER" "$node_out"; then
    echo "lint-checks: NO RESULT -- the target PASSED, and at least one case could not be" >&2
    echo '  evaluated from this checkout. Every checker ran; the unevaluable cases are' >&2
    echo '  listed above, each on a line beginning with the marker below.' >&2
    grep "$NO_RESULT_MARKER" "$node_out" | sed 's/^/    /' >&2
    exit 75
fi
exit 0

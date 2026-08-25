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

# SECOND SETUP PRECONDITION, same class and same spelling as the submodule check
# above: scripts/test_validate_stop_paths.py's canonical-adapter contract exercises
# the REAL dev-hermit parent adapter, found by walking ROOT.parents for
# ci-hub/ledger/validate_rows.py. From the canonical layout
# ~/work/dev-hermit/hermit that resolves; from a detached worktree under /tmp --
# which is the layout agents are TOLD to land from, so it is the COMMON case --
# the ancestors are only /tmp and /, and the contract cannot be tested at all.
#
# ⚠️ AND THE COST OF GETTING THIS WRONG IS ASYMMETRIC. Reported as a failure it
# manufactures a MAIN-RED, which is a standing P0 here, from the DEFAULT working
# layout -- so the false reading was the common one and the true one the exception.
# Measured 2026-08-25 on clean main from /tmp: a bare StopIteration with no message
# and no path, surfaced by make as `Error 1`. Telling a precondition from a finding
# required reading the test's source.
#
# Checked HERE rather than only inside the test because make maps ANY nonzero recipe
# status to its own error -- `exit 75` from a recipe becomes `make: *** Error 75`,
# rc=2 -- so a no_result cannot be expressed through the make layer. The node is the
# outermost place that can still say 75 and have validate.rs classify it no_result.
if [ ! -f ../ci-hub/ledger/validate_rows.py ] \
   && ! git rev-parse --show-superproject-working-tree 2>/dev/null | grep -q .; then
    parent_found=
    probe=$PWD
    while [ "$probe" != "/" ]; do
        probe=$(dirname "$probe")
        if [ -f "$probe/ci-hub/ledger/validate_rows.py" ]; then parent_found=$probe; break; fi
    done
    if [ -z "$parent_found" ]; then
        echo 'lint-checks: NO RESULT -- the dev-hermit parent adapter is not reachable.' >&2
        echo '  ci-hub/ledger/validate_rows.py is on no ancestor of' >&2
        echo "    $PWD" >&2
        echo '  This is a SETUP condition, not a lint failure: the canonical-adapter' >&2
        echo '  contract in scripts/test_validate_stop_paths.py exercises the REAL parent' >&2
        echo '  adapter, and there is none to exercise here. Every other checker in this' >&2
        echo '  target is unaffected and would have run.' >&2
        echo '  Run the node from a checkout nested under the dev-hermit parent' >&2
        echo '  (canonically ~/work/dev-hermit/hermit), not from a detached worktree.' >&2
        exit 75
    fi
fi

exec make lint-checks

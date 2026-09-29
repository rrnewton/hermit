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

_node_lib="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/node-run-classification.sh"
# shellcheck source=ci/node-run-classification.sh
. "$_node_lib"

# ---- Cases the hosted-portable lane may declare out of scope ----------------
#
# ⚠️ A CLOSED SET OF ONE, KEYED BY AN ID, NOT A PATTERN THE CALLER SUPPLIES.
# The accept arm of the canonical adapter contract in
# scripts/test_validate_stop_paths.py exercises the REAL ledger adapter,
# ci-hub/ledger/validate_rows.py, which lives in the dev-hermit PARENT
# repository. A GitHub-hosted checkout has no parent, so that one arm cannot be
# evaluated there and the whole target reports a no_result. Measured on hosted
# run https://github.com/rrnewton/hermit/actions/runs/36532203200 at main
# 6be37a833df8: every other checker in the target passed, and the node exited
# 75, which made the hosted checks job exit 75.
#
# The hosted-portable DAG node alone passes --hosted-out-of-scope with this id.
# The local check.lint_checks node never does, so local validation inside the
# parent still EVALUATES the arm, and outside the parent still reports 75.
#
# A declared case is NOT a pass. When it is the only unevaluable case, the node
# prints a NOT-EVALUATED-ON-HOSTED line for it, appends the same statement to
# the GitHub job summary, and exits 0 for the checkers that did run. Any other
# unevaluable case still makes the node exit 75, and a real failure still
# outranks everything (classify_run decides that before any of this runs).
hosted_out_of_scope_prefix() {
    # Echoes the exact column-0 marker prefix the producer emits for case $1;
    # returns 1 for an id outside the closed set.
    case "$1" in
        canonical-adapter-accept-arm)
            printf '%s\n' "${NO_RESULT_MARKER} canonical adapter contract, accept arm: "
            ;;
        *)
            return 1
            ;;
    esac
}

classify_declared_no_result() {
    # $1 = the declared case's marker prefix, $2 = the target's combined output.
    # Echoes "declared" only when the output carries at least one column-0 marker
    # and EVERY column-0 marker begins with that prefix; otherwise "undeclared".
    # The column-0 test is the same anchor classify_run applies.
    local prefix="$1" out="$2"
    awk -v marker="$NO_RESULT_MARKER" -v prefix="$prefix" '
        index($0, marker) == 1 {
            total++
            if (index($0, prefix) == 1) declared++
        }
        END {
            if (total > 0 && declared == total) print "declared"
            else print "undeclared"
        }' "$out"
}

report_not_evaluated_on_hosted() {
    # $1 = declared case id, $2 = the target's combined output. Prints one
    # NOT-EVALUATED-ON-HOSTED line per declared marker on stderr and appends the
    # same lines to $GITHUB_STEP_SUMMARY. Returns 1 when the summary cannot be
    # written, or when this runs under GitHub Actions with no summary file: the
    # caller then reports NO RESULT, because a declaration nobody can see in the
    # summary is not the declaration the hosted lane makes.
    local id="$1" out="$2" reasons
    reasons="$(grep "^${NO_RESULT_MARKER}" "$out" | sed "s/^${NO_RESULT_MARKER} //")"
    {
        echo "lint-checks: every checker that could run PASSED. The case below was NOT EVALUATED:"
        echo "  the hosted-portable lane declares it out of scope, and it is not counted as a pass."
        echo "  Local validation evaluates it in node check.lint_checks, from a checkout nested"
        echo "  under the dev-hermit parent repository."
        printf '%s\n' "$reasons" | sed "s/^/NOT-EVALUATED-ON-HOSTED: ${id}: /"
    } >&2
    if [ -z "${GITHUB_STEP_SUMMARY:-}" ]; then
        if [ "${GITHUB_ACTIONS:-}" = true ]; then
            echo 'lint-checks: GITHUB_ACTIONS is true but GITHUB_STEP_SUMMARY is unset; the job summary cannot name the case' >&2
            return 1
        fi
        return 0
    fi
    {
        echo
        echo "### lint-checks: 1 case NOT EVALUATED on hosted (not counted as a pass)"
        echo
        echo "The hosted-portable lane declares \`${id}\` out of scope. Every other checker in"
        echo "\`make lint-checks\` ran and passed. Local validation evaluates this case in node"
        echo "\`check.lint_checks\`, from a checkout nested under the dev-hermit parent repository."
        echo
        printf '%s\n' "$reasons" | sed "s/^/- NOT-EVALUATED-ON-HOSTED: ${id}: /"
    } >> "$GITHUB_STEP_SUMMARY" || {
        echo "lint-checks: cannot append to GITHUB_STEP_SUMMARY (${GITHUB_STEP_SUMMARY})" >&2
        return 1
    }
}

finish_node() {
    # $1 = make's exit code, $2 = the target's combined output, $3 = the declared
    # hosted out-of-scope id, or empty. Sets node_exit; prints the report.
    local make_rc="$1" out="$2" declared_id="$3" verdict prefix
    verdict="$(classify_run "$make_rc" "$out")"
    case "$verdict" in
        fail*)
            node_exit="${verdict#fail }"
            return 0
            ;;
        pass)
            node_exit=0
            return 0
            ;;
    esac
    if [ -n "$declared_id" ]; then
        if ! prefix="$(hosted_out_of_scope_prefix "$declared_id")"; then
            echo "lint-checks: unknown hosted out-of-scope case '${declared_id}'" >&2
            node_exit=2
            return 0
        fi
        if [ "$(classify_declared_no_result "$prefix" "$out")" = declared ]; then
            if report_not_evaluated_on_hosted "$declared_id" "$out"; then
                node_exit=0
                return 0
            fi
            echo 'lint-checks: the out-of-scope declaration could not be made visible; reporting NO RESULT' >&2
        else
            echo "lint-checks: only '${declared_id}' is declared out of scope, and another case could not be evaluated" >&2
        fi
    fi
    echo "lint-checks: NO RESULT -- the target PASSED, and at least one case could not be" >&2
    echo '  evaluated from this checkout. Every checker ran; the unevaluable cases are' >&2
    echo '  listed above, each on a line beginning with the marker below.' >&2
    grep "^${NO_RESULT_MARKER}" "$out" | sed 's/^/    /' >&2
    node_exit=75
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

    # ---- classify_run: the pass / no_result / fail split -------------------
    #
    # This branch decides what the NODE REPORTS, and until now it was the one
    # thing in this target not covered by anything. Each case below is planted so
    # that it FAILS if the classification regresses -- in particular the first two
    # fail under the unanchored `grep -q "$NO_RESULT_MARKER"` this replaced, and
    # the last two fail if a no_result is ever allowed to outrank a real failure.
    local tmp
    tmp="$(mktemp)" || return 1
    check_run() {
        local name="$1" want="$2" rc="$3" body="$4"
        printf '%s\n' "$body" > "$tmp"
        got="$(classify_run "$rc" "$tmp")"
        if [ "$got" != "$want" ]; then
            echo "FAIL: ${name}: expected '${want}', got '${got}'" >&2
            failures=$((failures + 1))
        fi
    }

    # ⚠️ THE CASE THAT MUST NOT MATCH. A checker that SUCCEEDS while echoing a line
    # that merely mentions the marker -- scanning a source file, quoting the
    # convention, or testing this very feature -- must stay a pass. Unanchored,
    # this returns no_result and silently converts a green run into an invisible
    # one.
    check_run 'marker quoted mid-line is not a no_result' 'pass' 0 \
        "shellcheck: scanning ci/lint-checks-node.sh for NO-RESULT-CASE: handling
lint-checks: OK"
    check_run 'marker indented is not a no_result' 'pass' 0 \
        "    NO-RESULT-CASE: quoted inside an indented block"
    # ⚠️ AND THE CASE THAT MUST MATCH, so the anchor cannot be "fixed" by making it
    # never fire. This is the exact shape the producer emits: column 0, on its own
    # line, with make's recipe echo above it.
    check_run 'genuine emission at column 0 is a no_result' 'no_result' 0 \
        "python3 scripts/example-producer-not-a-real-checker.py
NO-RESULT-CASE: canonical adapter contract, accept arm: no parent adapter
PARTIAL: every evaluable assertion passed"
    check_run 'clean run is a pass' 'pass' 0 "lint-checks: everything passed"
    # ⚠️ A NO_RESULT MUST NEVER SWALLOW A RED. Both arms: a plain failure, and a
    # failure that ALSO carries a genuine marker. The second is the one that
    # matters -- if the order of these two tests is ever inverted, a real red is
    # reported as an invisible no_result, which is the defect this entire marker
    # channel is downstream of.
    check_run 'a failure is a failure' 'fail 2' 2 "make: *** [lint-checks] Error 1"
    check_run 'a failure outranks a genuine marker' 'fail 2' 2 \
        "NO-RESULT-CASE: canonical adapter contract, accept arm: no parent adapter
make: *** [lint-checks] Error 1"
    check_run 'a failure outranks it whatever the code' 'fail 75' 75 \
        "NO-RESULT-CASE: something unevaluable"

    # ---- the hosted out-of-scope declaration ---------------------------------
    #
    # Each case FAILS if the declaration widens: an undeclared marker must keep
    # the node at 75, a failure must keep its code, and a declared case must be
    # named as NOT EVALUATED rather than disappear into a silent 0.
    local id='canonical-adapter-accept-arm' prefix summary errs
    summary="$(mktemp)" || return 1
    errs="$(mktemp)" || return 1
    if prefix="$(hosted_out_of_scope_prefix "$id")"; then
        if [ "$prefix" != 'NO-RESULT-CASE: canonical adapter contract, accept arm: ' ]; then
            echo "FAIL: declared prefix drifted from the producer's marker: '${prefix}'" >&2
            failures=$((failures + 1))
        fi
    else
        echo "FAIL: '${id}' is not in the closed set" >&2
        failures=$((failures + 1))
        prefix='unreachable'
    fi
    # The producer must still emit the exact text the prefix matches. Drift
    # there fails safe (the hosted node returns to 75), but it should be loud here.
    local producer
    producer="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)/scripts/test_validate_stop_paths.py"
    if ! grep -qF 'unevaluated.append(f"canonical adapter contract, accept arm: {exc}")' "$producer" \
        || ! grep -qF 'f"{NO_RESULT_MARKER} {item}"' "$producer"; then
        echo "FAIL: ${producer} no longer emits the declared case's marker text" >&2
        failures=$((failures + 1))
    fi
    if hosted_out_of_scope_prefix 'canonical-adapter' >/dev/null \
        || hosted_out_of_scope_prefix '' >/dev/null; then
        echo 'FAIL: an id outside the closed set was accepted' >&2
        failures=$((failures + 1))
    fi
    check_declared() {
        local name="$1" want="$2" body="$3"
        printf '%s\n' "$body" > "$tmp"
        got="$(classify_declared_no_result "$prefix" "$tmp")"
        if [ "$got" != "$want" ]; then
            echo "FAIL: ${name}: expected '${want}', got '${got}'" >&2
            failures=$((failures + 1))
        fi
    }
    check_declared 'only the declared case' 'declared' \
        "NO-RESULT-CASE: canonical adapter contract, accept arm: no parent adapter
PARTIAL: every evaluable assertion passed"
    check_declared 'declared plus another case' 'undeclared' \
        "NO-RESULT-CASE: canonical adapter contract, accept arm: no parent adapter
NO-RESULT-CASE: check-status authority unreachable"
    check_declared 'another case alone' 'undeclared' \
        "NO-RESULT-CASE: check-status authority unreachable"
    check_declared 'the refuse arm is not the accept arm' 'undeclared' \
        "NO-RESULT-CASE: canonical adapter contract, refuse arm: no parent adapter"
    check_declared 'a quoted declared prefix does not cover a real marker' 'undeclared' \
        "  NO-RESULT-CASE: canonical adapter contract, accept arm: quoted
NO-RESULT-CASE: check-status authority unreachable"
    check_declared 'an undeclared marker quoting the declared one mid-line' 'undeclared' \
        "NO-RESULT-CASE: wrapper saw NO-RESULT-CASE: canonical adapter contract, accept arm: x"
    check_declared 'no marker at all' 'undeclared' 'lint-checks: everything passed'

    check_finish() {
        local name="$1" want_exit="$2" want_lines="$3" rc="$4" declared="$5" body="$6"
        printf '%s\n' "$body" > "$tmp"
        : > "$summary"
        node_exit=''
        GITHUB_STEP_SUMMARY="$summary" finish_node "$rc" "$tmp" "$declared" 2> "$errs"
        local lines
        lines="$(grep -c '^NOT-EVALUATED-ON-HOSTED: ' "$errs" || true)"
        lines="${lines}/$(grep -c '^- NOT-EVALUATED-ON-HOSTED: ' "$summary" || true)"
        if [ "$node_exit" != "$want_exit" ] || [ "$lines" != "$want_lines" ]; then
            echo "FAIL: ${name}: expected exit ${want_exit} with ${want_lines} output/summary lines, got exit ${node_exit} with ${lines}" >&2
            failures=$((failures + 1))
        fi
    }
    local declared_only='NO-RESULT-CASE: canonical adapter contract, accept arm: no parent adapter'
    check_finish 'declared case alone is named, not silently passed' 0 '1/1' 0 "$id" "$declared_only"
    if ! grep -q "^NOT-EVALUATED-ON-HOSTED: ${id}: canonical adapter contract, accept arm: no parent adapter\$" "$errs" \
        || ! grep -q 'not counted as a pass' "$errs" \
        || ! grep -q 'NOT EVALUATED on hosted (not counted as a pass)' "$summary"; then
        echo 'FAIL: the declared case is not named as not evaluated and not a pass' >&2
        failures=$((failures + 1))
    fi
    check_finish 'without the declaration it stays a no_result' 75 '0/0' 0 '' "$declared_only"
    check_finish 'an undeclared case stays a no_result' 75 '0/0' 0 "$id" \
        "${declared_only}
NO-RESULT-CASE: check-status authority unreachable"
    check_finish 'a failure outranks the declaration' 2 '0/0' 2 "$id" "$declared_only"
    check_finish 'a clean run needs no declaration' 0 '0/0' 0 "$id" 'lint-checks: everything passed'
    # ⚠️ A SUMMARY NOBODY CAN WRITE IS NOT A DECLARATION. An unwritable summary
    # path, or GitHub Actions without one, must fall back to 75.
    printf '%s\n' "$declared_only" > "$tmp"
    node_exit=''
    GITHUB_STEP_SUMMARY="${summary}.missing-dir/summary" finish_node 0 "$tmp" "$id" 2> "$errs"
    if [ "$node_exit" != 75 ]; then
        echo "FAIL: an unwritable job summary must report 75, got ${node_exit}" >&2
        failures=$((failures + 1))
    fi
    node_exit=''
    GITHUB_ACTIONS=true GITHUB_STEP_SUMMARY='' finish_node 0 "$tmp" "$id" 2> "$errs"
    if [ "$node_exit" != 75 ]; then
        echo "FAIL: GitHub Actions without a job summary must report 75, got ${node_exit}" >&2
        failures=$((failures + 1))
    fi

    # ---- end to end through the real entry point -----------------------------
    #
    # The cases above call finish_node directly. These run this script itself,
    # with stub `git` and `make` first on PATH, so argument parsing, the make
    # argv and the exit status are covered as the DAG node runs them.
    local stubs self out want_args
    self="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/$(basename -- "${BASH_SOURCE[0]}")"
    stubs="$(mktemp -d)" || return 1
    out="${stubs}/out"
    cat > "${stubs}/git" <<'STUB'
#!/usr/bin/env bash
if [ "${1:-}" = submodule ] && [ "${2:-}" = status ]; then
    echo ' abc123 agent-utils'
    exit 0
fi
echo "unexpected git invocation: $*" >&2
exit 2
STUB
    cat > "${stubs}/make" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" > "$STUB_MAKE_ARGS"
printf '%s\n' "$STUB_MAKE_OUTPUT"
exit "$STUB_MAKE_RC"
STUB
    chmod +x "${stubs}/git" "${stubs}/make"
    run_entry() {
        local name="$1" want_exit="$2" want_lines="$3" make_rc="$4" make_output="$5"
        shift 5
        local rc lines
        : > "$summary"
        env -u VALIDATE_REVERIE_PIN_BASE_REF -u MAKEFLAGS -u MFLAGS -u MAKEOVERRIDES \
            -u GITHUB_ACTIONS PATH="${stubs}:${PATH}" GITHUB_STEP_SUMMARY="$summary" \
            STUB_MAKE_ARGS="${stubs}/args" STUB_MAKE_OUTPUT="$make_output" \
            STUB_MAKE_RC="$make_rc" "$self" "$@" > "$out" 2>&1 && rc=0 || rc=$?
        lines="$(grep -c '^NOT-EVALUATED-ON-HOSTED: ' "$out" || true)"
        lines="${lines}/$(grep -c '^- NOT-EVALUATED-ON-HOSTED: ' "$summary" || true)"
        if [ "$rc" != "$want_exit" ] || [ "$lines" != "$want_lines" ]; then
            echo "FAIL: entry point: ${name}: expected exit ${want_exit} with ${want_lines} output/summary lines, got exit ${rc} with ${lines}" >&2
            sed 's/^/    /' "$out" >&2
            failures=$((failures + 1))
        fi
    }
    local declared_flag=(--hosted-out-of-scope "$id")
    local floor='0123456789abcdef0123456789abcdef01234567'
    run_entry 'hosted declaration names the case and exits 0' 0 '1/1' 0 "$declared_only" "${declared_flag[@]}"
    run_entry 'the local command still reports 75' 75 '0/0' 0 "$declared_only"
    run_entry 'hosted declaration never hides a failure' 1 '0/0' 1 "$declared_only" "${declared_flag[@]}"
    run_entry 'hosted declaration never hides another case' 75 '0/0' 0 \
        "${declared_only}
NO-RESULT-CASE: check-status authority unreachable" "${declared_flag[@]}"
    run_entry 'hosted declaration composes with the admitted pin floor' 0 '1/1' 0 "$declared_only" \
        "${declared_flag[@]}" --reverie-pin-base-ref "$floor"
    want_args="lint-checks VALIDATE_REVERIE_PIN_BASE_REF=${floor}"
    if [ "$(cat "${stubs}/args")" != "$want_args" ]; then
        echo "FAIL: entry point: make argv was '$(cat "${stubs}/args")', expected '${want_args}'" >&2
        failures=$((failures + 1))
    fi
    run_entry 'an unknown case id is a usage error' 2 '0/0' 0 "$declared_only" --hosted-out-of-scope other
    run_entry 'a repeated declaration is a usage error' 2 '0/0' 0 "$declared_only" \
        "${declared_flag[@]}" "${declared_flag[@]}"
    rm -rf "$stubs"
    rm -f "$tmp" "$summary" "$errs"

    if [ "$failures" -ne 0 ]; then
        echo "lint-checks-node --self-test: ${failures} case(s) failed" >&2
        return 1
    fi
    echo 'PASS: lint-checks-node classifies uninitialized as no_result, drift/conflict as failure,'
    echo '      a quoted marker as pass, a column-0 marker as no_result, and never lets a marker'
    echo '      outrank a real failure; the hosted declaration covers only its one case, names it'
    echo '      NOT EVALUATED in the output and job summary, and falls back to 75 when it cannot'
}

if [ "${1:-}" = '--self-test' ]; then
    if [ "$#" -ne 1 ]; then
        echo 'lint-checks-node: --self-test accepts no other argument' >&2
        exit 2
    fi
    self_test
    exit $?
fi

# --reverie-pin-base-ref carries only a SHA already selected by the driver. It
# neither grants admission nor changes the ordinary standalone checker policy.
# --hosted-out-of-scope names one case from the closed set above; only the
# hosted-portable DAG node passes it. Each option may appear at most once, and
# neither has an environment-variable form.
usage() {
    echo 'usage: ci/lint-checks-node.sh [--hosted-out-of-scope canonical-adapter-accept-arm] [--reverie-pin-base-ref FULL_SHA]' >&2
    exit 2
}
pin_args=()
declared_out_of_scope=''
while [ "$#" -ne 0 ]; do
    case "$1" in
        --reverie-pin-base-ref)
            if [ "$#" -lt 2 ] || [ "${#pin_args[@]}" -ne 0 ] || ! [[ "$2" =~ ^[0-9a-f]{40}$ ]]; then
                usage
            fi
            pin_args=("VALIDATE_REVERIE_PIN_BASE_REF=$2")
            shift 2
            ;;
        --hosted-out-of-scope)
            if [ "$#" -lt 2 ] || [ -n "$declared_out_of_scope" ] || ! hosted_out_of_scope_prefix "$2" >/dev/null; then
                usage
            fi
            declared_out_of_scope="$2"
            shift 2
            ;;
        *)
            usage
            ;;
    esac
done
if [[ ${VALIDATE_REVERIE_PIN_BASE_REF+x} || ${MAKEFLAGS:-}${MFLAGS:-}${MAKEOVERRIDES:-} == *VALIDATE_REVERIE_PIN_BASE_REF* ]]; then
    echo 'lint-checks-node: an ambient admission floor is not authority' >&2
    exit 2
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

node_out=$(mktemp) || exit 1
trap 'rm -f "$node_out"' EXIT
set +e
make lint-checks "${pin_args[@]}" 2>&1 | tee "$node_out"
pipeline_status=("${PIPESTATUS[@]}")
set -e
make_rc=${pipeline_status[0]}
tee_rc=${pipeline_status[1]}

# The capture file is the input to classify_run. A tee failure can leave it
# empty or partial even though marker text reached the terminal, which would
# otherwise turn an unevaluable run into pass. Any capture failure is a real
# node failure; preserve make's status when both commands fail.
if [ "$tee_rc" -ne 0 ]; then
    echo "lint-checks: output capture failed with exit ${tee_rc}" >&2
    if [ "$make_rc" -eq 0 ]; then
        make_rc=$tee_rc
    fi
fi
node_exit=''
finish_node "$make_rc" "$node_out" "$declared_out_of_scope"
exit "$node_exit"

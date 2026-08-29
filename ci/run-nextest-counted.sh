#!/usr/bin/env bash

set -uo pipefail

function write_structured_test_counts {
    local executed=$1 filtered=$2 path=${DAGRUN_TEST_COUNTS_PATH:-} tmp
    [[ -n $path ]] || return 0
    tmp="${path}.tmp.$$"
    umask 077
    if ! printf '{"schema":1,"executed_tests":%s,"filtered_tests":%s}\n' \
        "$executed" "$filtered" >"$tmp"; then
        printf 'run-nextest-counted: cannot write structured test counts to %s\n' "$tmp" >&2
        return 2
    fi
    if ! mv -f -- "$tmp" "$path"; then
        rm -f -- "$tmp"
        printf 'run-nextest-counted: cannot publish structured test counts to %s\n' "$path" >&2
        return 2
    fi
}

function emit_libtest_count {
    local log=$1 status=${2:-0} expected=${NEXTEST_EXPECTED_EXECUTED:-}
    local line finished='' initial='' passed=0 failed=0
    local exec_failed=0 timed_out=0 skipped=0 summary='' categories='' completed=0 executed=0
    local matches=0
    local header_re='Summary.*\][[:space:]]+([0-9]+)(/([0-9]+))?[[:space:]]+tests?[[:space:]]+run:[[:space:]]*(.*)$'
    local categories_re='^([0-9]+)[[:space:]]+passed(,[[:space:]]+([0-9]+)[[:space:]]+failed)?(,[[:space:]]+([0-9]+)[[:space:]]+exec[[:space:]]+failed)?(,[[:space:]]+([0-9]+)[[:space:]]+timed[[:space:]]+out)?,[[:space:]]+([0-9]+)[[:space:]]+skipped$'
    while IFS= read -r line; do
        if [[ $line =~ $header_re ]]; then
            finished=${BASH_REMATCH[1]}
            initial=${BASH_REMATCH[3]:-${BASH_REMATCH[1]}}
            summary=${BASH_REMATCH[4]}
            ((matches += 1))
        fi
    done <"$log"

    if ((matches != 1)); then
        printf 'run-nextest-counted: expected exactly one final nextest Summary, found %s\n' "$matches" >&2
        return 2
    fi

    # Passed and failed annotations are strict subsets of their leading count.
    # Remove only nextest's documented annotations; any other parenthesized text
    # leaves the grammar unmatched and the count UNKNOWN.
    categories=$summary
    while [[ $categories =~ [[:space:]]+\([0-9]+[[:space:]]+(slow|flaky|leaky)(,[[:space:]]*[0-9]+[[:space:]]+(slow|flaky|leaky))*\) ]]; do
        categories=${categories/"${BASH_REMATCH[0]}"/}
    done
    while [[ $categories =~ [[:space:]]+\([0-9]+[[:space:]]+due[[:space:]]+to[[:space:]]+being[[:space:]]+leaky\) ]]; do
        categories=${categories/"${BASH_REMATCH[0]}"/}
    done
    # Older nextest releases omitted the trailing zero category. Supplying that
    # exact zero preserves their meaning; a nonzero or unknown trailing category
    # still remains in the string and is refused by the anchored grammar below.
    if [[ ! $categories =~ ,[[:space:]]+[0-9]+[[:space:]]+skipped$ ]]; then
        categories+=', 0 skipped'
    fi
    if [[ ! $categories =~ $categories_re ]]; then
        printf 'run-nextest-counted: cannot derive executed tests from nextest summary: %s\n' \
            "$summary" >&2
        return 2
    fi
    passed=${BASH_REMATCH[1]}
    failed=${BASH_REMATCH[3]:-0}
    exec_failed=${BASH_REMATCH[5]:-0}
    timed_out=${BASH_REMATCH[7]:-0}
    skipped=${BASH_REMATCH[8]}
    completed=$((passed + failed + exec_failed + timed_out))
    executed=$((passed + failed + timed_out))

    if ((initial < finished || completed != finished)); then
        printf 'run-nextest-counted: nextest summary arithmetic disagrees: %s/%s finished, categories total %s\n' \
            "$finished" "$initial" "$completed" >&2
        return 2
    fi
    if ((status == 0 && (finished != initial || failed != 0 || exec_failed != 0 || timed_out != 0))); then
        printf 'run-nextest-counted: successful nextest status disagrees with summary: %s\n' \
            "$summary" >&2
        return 2
    fi
    if [[ -n $expected ]]; then
        if [[ ! $expected =~ ^[0-9]+$ ]]; then
            printf 'run-nextest-counted: NEXTEST_EXPECTED_EXECUTED must be a nonnegative integer, got %s\n' \
                "$expected" >&2
            return 2
        fi
        if ((executed != expected)); then
            printf 'run-nextest-counted: expected %s tests to execute, saw %s; refusing because the selected set changed\n' \
                "$expected" "$executed" >&2
            return 2
        fi
    fi

    # The human lines below remain useful in logs, but receipt-bearing dagrun
    # clients consume this exact file instead. A command that merely prints a
    # libtest-looking banner therefore cannot manufacture an executed-test
    # count.
    write_structured_test_counts "$executed" "$skipped" || return $?

    # Preserve the canonical libtest spelling for human-facing logs and older
    # dagrun clients that have not required the structured count file.
    if ((status == 0)); then
        printf 'running %s tests\n' "$executed"
        printf 'test result: ok. %s passed; 0 failed; 0 ignored; %s filtered out\n' \
            "$passed" "$skipped"
    else
        # Do not rewrite a failure as `test result: ok`. The runner recognizes
        # `running N tests` independently of the process exit and retains both
        # facts on the failed StepOutcome. Preserve the complete nextest outcome
        # text because it may include `exec failed`, timeout, or other categories
        # in addition to ordinary test failures.
        printf 'running %s tests\n' "$executed"
        printf 'test result: FAILED. nextest: %s; %s filtered out\n' \
            "$summary" "$skipped"
    fi
}

function run_nextest {
    local summary_log=$1 status count_status=0
    shift

    set +e
    cargo nextest run --color never "$@" 2>&1 | tee "$summary_log"
    status=${PIPESTATUS[0]}
    set -e

    emit_libtest_count "$summary_log" "$status" || count_status=$?
    if ((status != 0)); then
        return "$status"
    fi
    return "$count_status"
}

function self_test {
    local scratch got expected status=0
    scratch=$(mktemp -d)
    trap 'rm -rf "$scratch"' RETURN

    printf 'Summary [  20.921s] 8 tests run: 8 passed, 7 skipped\n' >"$scratch/with-skips"
    got=$(emit_libtest_count "$scratch/with-skips" 0)
    expected=$'running 8 tests\ntest result: ok. 8 passed; 0 failed; 0 ignored; 7 filtered out'
    [[ $got == "$expected" ]] || return 1

    rm -f "$scratch/counts.json"
    got=$(DAGRUN_TEST_COUNTS_PATH="$scratch/counts.json" \
        emit_libtest_count "$scratch/with-skips" 0)
    [[ $got == "$expected" ]] || return 1
    [[ $(<"$scratch/counts.json") == \
        '{"schema":1,"executed_tests":8,"filtered_tests":7}' ]] || return 1

    got=$(NEXTEST_EXPECTED_EXECUTED=8 emit_libtest_count "$scratch/with-skips" 0)
    [[ $got == "$expected" ]] || return 1
    status=0
    NEXTEST_EXPECTED_EXECUTED=9 emit_libtest_count "$scratch/with-skips" 0 \
        >"$scratch/wrong-count.stdout" 2>"$scratch/wrong-count.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    [[ $(<"$scratch/wrong-count.stderr") == \
        'run-nextest-counted: expected 9 tests to execute, saw 8; refusing because the selected set changed' ]] || return 1
    status=0
    NEXTEST_EXPECTED_EXECUTED=unknown emit_libtest_count "$scratch/with-skips" 0 \
        >/dev/null 2>"$scratch/invalid-count.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    [[ $(<"$scratch/invalid-count.stderr") == \
        'run-nextest-counted: NEXTEST_EXPECTED_EXECUTED must be a nonnegative integer, got unknown' ]] || return 1

    printf 'Summary [   0.003s] 1 test run: 1 passed\n' >"$scratch/no-skips"
    got=$(emit_libtest_count "$scratch/no-skips" 0)
    expected=$'running 1 tests\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out'
    [[ $got == "$expected" ]] || return 1

    printf 'Summary [   0.103s] 23 tests run: 22 passed, 1 failed, 5 skipped\n' \
        >"$scratch/failed"
    got=$(emit_libtest_count "$scratch/failed" 100)
    expected=$'running 23 tests\ntest result: FAILED. nextest: 22 passed, 1 failed, 5 skipped; 5 filtered out'
    [[ $got == "$expected" ]] || return 1

    printf 'Summary [  97.472s] 35 tests run: 22 passed, 1 failed, 12 exec failed, 26 skipped\n' \
        >"$scratch/exec-failed"
    got=$(emit_libtest_count "$scratch/exec-failed" 100)
    expected=$'running 23 tests\ntest result: FAILED. nextest: 22 passed, 1 failed, 12 exec failed, 26 skipped; 26 filtered out'
    [[ $got == "$expected" ]] || return 1

    printf 'Summary [   1.000s] 8/10 tests run: 5 passed (1 slow, 1 flaky, 1 leaky), 1 failed, 1 exec failed, 1 timed out, 2 skipped\n' \
        >"$scratch/partial"
    got=$(emit_libtest_count "$scratch/partial" 100)
    expected=$'running 7 tests\ntest result: FAILED. nextest: 5 passed (1 slow, 1 flaky, 1 leaky), 1 failed, 1 exec failed, 1 timed out, 2 skipped; 2 filtered out'
    [[ $got == "$expected" ]] || return 1

    printf 'Summary [   0.010s] 12 tests run: 0 passed, 12 exec failed, 0 skipped\n' \
        >"$scratch/all-exec-failed"
    got=$(emit_libtest_count "$scratch/all-exec-failed" 100)
    expected=$'running 0 tests\ntest result: FAILED. nextest: 0 passed, 12 exec failed, 0 skipped; 0 filtered out'
    [[ $got == "$expected" ]] || return 1

    # Exercise the real wrapper path, not only its parser: a failed nextest run
    # must emit the count and still return nextest's original nonzero status.
    function cargo {
        printf 'Summary [   0.010s] 12 tests run: 0 passed, 12 exec failed, 0 skipped\n'
        return 100
    }
    set +e
    got=$(run_nextest "$scratch/wrapper")
    status=$?
    set -e
    unset -f cargo
    [[ $status == 100 ]] || return 1
    [[ $got == *$'running 0 tests\ntest result: FAILED. nextest: 0 passed, 12 exec failed, 0 skipped; 0 filtered out' ]] || return 1
    [[ $got != *'test result: ok.'* ]] || return 1

    status=0
    printf 'not a nextest summary\n' >"$scratch/missing"
    emit_libtest_count "$scratch/missing" 0 >/dev/null 2>&1 || status=$?
    [[ $status == 2 ]] || return 1
    status=0
    printf 'Summary [   0.003s] 2 tests run: 1 passed\n' >"$scratch/mismatch"
    emit_libtest_count "$scratch/mismatch" 0 >/dev/null 2>&1 || status=$?
    [[ $status == 2 ]] || return 1
    status=0
    emit_libtest_count "$scratch/partial" 0 >/dev/null 2>&1 || status=$?
    [[ $status == 2 ]] || return 1
    status=0
    printf 'Summary [  15.750s] 8/10 tests run: 5 passed, 2 failed, 1 exec failed, 1 timed out, 2 skipped\n' \
        >"$scratch/bad-partial-arithmetic"
    emit_libtest_count "$scratch/bad-partial-arithmetic" 100 >/dev/null 2>&1 || status=$?
    [[ $status == 2 ]] || return 1
    status=0
    printf 'Summary [   1.000s] 1/2 tests run: 1 cancelled, 1 skipped\n' \
        >"$scratch/unknown-category"
    emit_libtest_count "$scratch/unknown-category" 100 >/dev/null 2>&1 || status=$?
    [[ $status == 2 ]] || return 1

    # These malformed annotations once matched the loop regex but not the text
    # removed by the loop, so parsing never advanced. Run both refusals under a
    # bound: exit 2 is required; timeout exit 124 is a test failure.
    export -f emit_libtest_count
    status=0
    printf 'Summary [   0.010s] 1 test run: 1 passed(1 slow), 0 skipped\n' \
        >"$scratch/pass-annotation-missing-space"
    timeout 2s bash -c 'emit_libtest_count "$1" 100' _ \
        "$scratch/pass-annotation-missing-space" >/dev/null 2>&1 || status=$?
    [[ $status == 2 ]] || return 1
    status=0
    printf 'Summary [   0.010s] 2 tests run: 1 passed, 1 failed(1 due to being leaky), 0 skipped\n' \
        >"$scratch/fail-annotation-missing-space"
    timeout 2s bash -c 'emit_libtest_count "$1" 100' _ \
        "$scratch/fail-annotation-missing-space" >/dev/null 2>&1 || status=$?
    [[ $status == 2 ]] || return 1
    export -n -f emit_libtest_count

    printf 'run-nextest-counted: self-test PASS (8 positive, 9 refusal)\n'
}

if [[ ${1:-} == --self-test ]]; then
    self_test
    exit
fi

summary_log=$(mktemp)
trap 'rm -f "$summary_log"' EXIT
run_nextest "$summary_log" "$@"

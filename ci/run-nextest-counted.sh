#!/usr/bin/env bash

set -uo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly SCRIPT_DIR
readonly RESULT_WRITER="$SCRIPT_DIR/nextest-test-results.rs"

function write_structured_test_results {
    local events=$1 executed=$2 filtered=$3 path=${DAGRUN_TEST_COUNTS_PATH:-}
    [[ -n $path ]] || return 0
    if [[ -z $events ]]; then
        printf 'run-nextest-counted: structured test results require nextest events\n' >&2
        return 2
    fi
    if ! "$RESULT_WRITER" "$events" "$executed" "$filtered" "$path"; then
        printf 'run-nextest-counted: cannot publish structured test results to %s\n' "$path" >&2
        return 2
    fi
}

function emit_libtest_count {
    local log=$1 status=${2:-0} events=${3:-} line finished='' initial='' passed=0 failed=0
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

    # The human lines below remain useful in logs, but receipt-bearing dagrun
    # clients consume this exact file instead. A command that merely prints a
    # libtest-looking banner therefore cannot manufacture an executed-test
    # count.
    write_structured_test_results "$events" "$executed" "$skipped" || return $?

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
    local summary_log=$1 events_log=$2 status count_status=0 stderr_fd tee_pid tee_status=0
    shift 2

    set +e
    exec {stderr_fd}> >(tee "$summary_log" >&2)
    tee_pid=$!
    NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 cargo nextest run --color never \
        --message-format libtest-json-plus --message-format-version 0.1 \
        "$@" >"$events_log" 2>&$stderr_fd
    status=$?
    exec {stderr_fd}>&-
    wait "$tee_pid" || tee_status=$?
    set -e

    if ((tee_status != 0)); then
        printf 'run-nextest-counted: cannot retain nextest human report (tee exited %s)\n' \
            "$tee_status" >&2
        return 2
    fi
    emit_libtest_count "$summary_log" "$status" "$events_log" || count_status=$?
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
    printf 'Summary [   0.300s] 2 tests run: 2 passed, 7 skipped\n' \
        >"$scratch/structured-summary"
    printf '%s\n' \
        '{"type":"suite","event":"started","test_count":2,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        '{"type":"test","event":"started","name":"suite::suite$passes"}' \
        '{"type":"test","event":"ok","name":"suite::suite$passes","exec_time":0.1}' \
        '{"type":"test","event":"started","name":"suite::suite$recovers"}' \
        '{"type":"test","event":"ok","name":"suite::suite$recovers#2","exec_time":0.2}' \
        '{"type":"suite","event":"ok","passed":2,"failed":0,"ignored":0,"measured":0,"filtered_out":7,"exec_time":0.3,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        >"$scratch/events.jsonl"
    got=$(DAGRUN_TEST_COUNTS_PATH="$scratch/counts.json" \
        emit_libtest_count "$scratch/structured-summary" 0 "$scratch/events.jsonl")
    expected=$'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 7 filtered out'
    [[ $got == "$expected" ]] || return 1
    python3 - "$scratch/counts.json" <<'PYEOF' || return 1
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
    assert report == {
    "schema": 2,
    "executed_tests": 2,
    "filtered_tests": 7,
    "results": [
        {"id": "suite$passes", "result": "pass", "attempts": 1},
        {"id": "suite$recovers", "result": "pass", "attempts": 2},
    ],
    }
PYEOF

    printf 'Summary [   0.200s] 1 test run: 0 passed, 1 failed, 3 skipped\n' \
        >"$scratch/structured-failed-summary"
    printf '%s\n' \
        '{"type":"suite","event":"started","test_count":1,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        '{"type":"test","event":"started","name":"suite::suite$fails"}' \
        '{"type":"test","event":"failed","name":"suite::suite$fails","exec_time":0.2}' \
        '{"type":"suite","event":"failed","passed":0,"failed":1,"ignored":0,"measured":0,"filtered_out":3,"exec_time":0.2,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        >"$scratch/failed-events.jsonl"
    DAGRUN_TEST_COUNTS_PATH="$scratch/failed-counts.json" \
        emit_libtest_count "$scratch/structured-failed-summary" 100 \
        "$scratch/failed-events.jsonl" >/dev/null || return 1
    python3 - "$scratch/failed-counts.json" <<'PYEOF' || return 1
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
assert report["results"] == [
    {"id": "suite$fails", "result": "fail", "attempts": 1},
]
PYEOF

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
        printf '%s\n' \
            '{"type":"suite","event":"started","test_count":0,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
            '{"type":"suite","event":"failed","passed":0,"failed":0,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.0,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}'
        printf 'Summary [   0.010s] 12 tests run: 0 passed, 12 exec failed, 0 skipped\n' >&2
        return 100
    }
    set +e
    got=$(run_nextest "$scratch/wrapper" "$scratch/wrapper-events")
    status=$?
    set -e
    unset -f cargo
    [[ $status == 100 ]] || return 1
    [[ $got == *$'running 0 tests\ntest result: FAILED. nextest: 0 passed, 12 exec failed, 0 skipped; 0 filtered out' ]] || return 1
    [[ $got != *'test result: ok.'* ]] || return 1

    printf '%s\n' \
        '{"type":"test","event":"future","name":"suite::suite$case"}' \
        >"$scratch/unknown-event.jsonl"
    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/refused.json" \
        emit_libtest_count "$scratch/with-skips" 0 "$scratch/unknown-event.jsonl" \
        >/dev/null 2>"$scratch/refusal" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'unsupported test event "future"' "$scratch/refusal" || return 1
    [[ ! -e $scratch/refused.json ]] || return 1

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

    printf 'run-nextest-counted: self-test PASS (8 positive, 7 refusal)\n'
}

if [[ ${1:-} == --self-test ]]; then
    self_test
    exit
fi

summary_log=$(mktemp)
events_log=$(mktemp)
trap 'rm -f "$summary_log" "$events_log"' EXIT
run_nextest "$summary_log" "$events_log" "$@"

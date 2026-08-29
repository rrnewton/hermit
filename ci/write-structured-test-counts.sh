#!/usr/bin/env bash

set -euo pipefail

function usage {
    echo "usage: $0 <executed-tests> <filtered-tests> [<id> <pass|fail> <attempts>]..." >&2
    exit 2
}

function json_string {
    local value=$1
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    value=${value//$'\n'/\\n}
    printf '"%s"' "$value"
}

function write_structured_test_results {
    local executed=$1 filtered=$2 path=${DAGRUN_TEST_COUNTS_PATH:-} tmp
    shift 2
    [[ $executed =~ ^[0-9]+$ && $filtered =~ ^[0-9]+$ ]] || usage
    (( $# % 3 == 0 && $# / 3 == executed )) || usage
    [[ -n $path ]] || return 0

    tmp="${path}.tmp.$$"
    umask 077
    if ! {
        printf '{"schema":2,"executed_tests":%s,"filtered_tests":%s,"results":[' \
            "$executed" "$filtered"
        local separator='' id result attempts
        declare -A seen=()
        while (( $# > 0 )); do
            id=$1
            result=$2
            attempts=$3
            shift 3
            [[ -n $id && $id != [[:space:]]* && $id != *[[:space:]] ]] || usage
            [[ -z ${seen["$id"]+present} ]] || usage
            seen["$id"]=1
            [[ $result == pass || $result == fail ]] || usage
            [[ $attempts =~ ^[1-9][0-9]*$ ]] || usage
            printf '%s{"id":' "$separator"
            json_string "$id"
            printf ',"result":"%s","attempts":%s}' "$result" "$attempts"
            separator=,
        done
        printf ']}\n'
    } >"$tmp"; then
        printf 'write-structured-test-counts: cannot write %s\n' "$tmp" >&2
        return 2
    fi
    if ! mv -f -- "$tmp" "$path"; then
        rm -f -- "$tmp"
        printf 'write-structured-test-counts: cannot publish %s\n' "$path" >&2
        return 2
    fi
}

function self_test {
    local scratch status=0
    scratch=$(mktemp -d)
    DAGRUN_TEST_COUNTS_PATH="$scratch/counts.json" \
        write_structured_test_results 2 2 \
            'suite$passes' pass 1 'suite$fails' fail 2
    [[ $(<"$scratch/counts.json") == \
        '{"schema":2,"executed_tests":2,"filtered_tests":2,"results":[{"id":"suite$passes","result":"pass","attempts":1},{"id":"suite$fails","result":"fail","attempts":2}]}' ]] || status=1
    (write_structured_test_results invalid 0) >/dev/null 2>&1 && status=1
    (write_structured_test_results 2 0 only-one pass 1) >/dev/null 2>&1 && status=1
    (DAGRUN_TEST_COUNTS_PATH="$scratch/invalid.json" \
        write_structured_test_results 1 0 ' leading' pass 1) \
        >/dev/null 2>&1 && status=1
    (DAGRUN_TEST_COUNTS_PATH="$scratch/invalid.json" \
        write_structured_test_results 1 0 'trailing ' pass 1) \
        >/dev/null 2>&1 && status=1
    (DAGRUN_TEST_COUNTS_PATH="$scratch/invalid.json" \
        write_structured_test_results 2 0 duplicate pass 1 duplicate fail 1) \
        >/dev/null 2>&1 && status=1
    rm -rf -- "$scratch"
    if ((status != 0)); then
        echo "write-structured-test-counts: self-test failed" >&2
        return "$status"
    fi
    echo "write-structured-test-counts: self-test passed"
}

if [[ ${1:-} == --self-test && $# -eq 1 ]]; then
    self_test
    exit
fi

(( $# >= 2 )) || usage
write_structured_test_results "$@"

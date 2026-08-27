#!/usr/bin/env bash

set -euo pipefail

function usage {
    echo "usage: $0 <executed-tests> <filtered-tests>" >&2
    exit 2
}

function write_structured_test_counts {
    local executed=$1 filtered=$2 path=${DAGRUN_TEST_COUNTS_PATH:-} tmp
    [[ $executed =~ ^[0-9]+$ && $filtered =~ ^[0-9]+$ ]] || usage
    [[ -n $path ]] || return 0

    tmp="${path}.tmp.$$"
    umask 077
    if ! printf '{"schema":1,"executed_tests":%s,"filtered_tests":%s}\n' \
        "$executed" "$filtered" >"$tmp"; then
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
        write_structured_test_counts 7 2
    [[ $(<"$scratch/counts.json") == \
        '{"schema":1,"executed_tests":7,"filtered_tests":2}' ]] || status=1
    (write_structured_test_counts invalid 0) >/dev/null 2>&1 && status=1
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

[[ $# -eq 2 ]] || usage
write_structured_test_counts "$1" "$2"

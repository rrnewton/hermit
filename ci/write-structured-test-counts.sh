#!/usr/bin/env bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly script_dir
readonly TEST_RESULTS_WRITER="$script_dir/test-results.rs"

function write_structured_test_results {
    local path=${DAGRUN_TEST_COUNTS_PATH:-}
    [[ -n $path ]] || return 0
    "$TEST_RESULTS_WRITER" write "$path" "$@"
}

function self_test {
    local scratch status=0
    scratch=$(mktemp -d)
    # shellcheck disable=SC2016 # `$` is part of the stable test identity.
    DAGRUN_TEST_COUNTS_PATH="$scratch/counts.json" \
        write_structured_test_results 2 2 \
            'suite$passes' pass 1 'suite$fails' fail 2
    # shellcheck disable=SC2016 # `$` is part of the expected JSON string.
    [[ $(<"$scratch/counts.json") == \
        '{"executed_tests":2,"filtered_tests":2,"results":[{"attempts":1,"id":"suite$passes","result":"pass"},{"attempts":2,"id":"suite$fails","result":"fail"}],"schema":2}' ]] || status=1
    [[ -z $(write_structured_test_results 1 0 anything at all) ]] || status=1
    (DAGRUN_TEST_COUNTS_PATH="$scratch/invalid.json" \
        write_structured_test_results invalid 0) >/dev/null 2>&1 && status=1
    (DAGRUN_TEST_COUNTS_PATH="$scratch/invalid.json" \
        write_structured_test_results 2 0 only-one pass 1) >/dev/null 2>&1 && status=1
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

write_structured_test_results "$@"

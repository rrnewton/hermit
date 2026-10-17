#!/usr/bin/env bash
# Run a conformance checker and publish its per-cell outcomes as the node's
# structured test results (DAGRUN_TEST_COUNTS_PATH), through
# ci/write-structured-test-counts.sh.
#
#   usage: conformance-test-counts.sh isolation ARTIFACTS -- CMD...
#          conformance-test-counts.sh syscall ARTIFACTS -- CMD...
#          conformance-test-counts.sh --self-test
#
# WHY: validate plans every test.* and *-test.* node as a test node, and a
# node counts as executed only through the structured result file it writes.
# The isolation matrix and the Reverie-layer syscall check wrote none, so from
# 2026-10-15 every full validation listed them as absent and none was
# canonically VALIDATED.
#
# isolation: tests/isolation/conformance.rs writes ARTIFACTS/results.tsv, one
#   `id<TAB>pass|fail` row per collected cell and per stale ledger row, judged
#   by its own expected-mismatch ledger.
# syscall: reverie-e2e-compare prints one `cell<TAB>backend<TAB>verdict<TAB>
#   signature<TAB>note` line per cell and backend (note UNEXPECTED fails), one
#   `STALE<TAB>...` line per stale ledger row, a `summary<TAB>backend<TAB>
#   verdict<TAB>count` line per verdict, and a closing `reverie e2e:` line.
#   The rows must add up to the summary counts.
#
# Output that cannot be read whole adds the failing row `<mode>/incomplete`;
# a checker that exits nonzero with no failing row adds `<mode>/checker-exit`.
# Either way the structured results never pass a node the checker failed. The
# exit status is the checker's, or the writer's when the checker passed.

set -uo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly script_dir

function usage {
    echo "usage: $0 isolation|syscall ARTIFACTS -- CMD... | --self-test" >&2
    exit 2
}

# rows_from_isolation RESULTS_TSV: prints `id<TAB>result` rows; status 1 if
# the file is missing or malformed.
function rows_from_isolation {
    local file=$1 line id result extra header_seen=false
    [[ -f $file ]] || return 1
    while IFS= read -r line || [[ -n $line ]]; do
        [[ $line == '#'* ]] && continue
        if ! $header_seen; then
            [[ $line == $'id\tresult' ]] || return 1
            header_seen=true
            continue
        fi
        IFS=$'\t' read -r id result extra <<<"$line"
        [[ -n $id && -z $extra && ( $result == pass || $result == fail ) ]] || return 1
        printf '%s\t%s\n' "$id" "$result"
    done <"$file"
    $header_seen
}

# rows_from_syscall STDOUT_FILE: prints `id<TAB>result` rows; status 1 if the
# closing line is missing or the rows do not add up to the summary counts.
function rows_from_syscall {
    local file=$1 line rows=0 summed=0 closed=false stale=0
    local -a f
    while IFS= read -r line || [[ -n $line ]]; do
        [[ $line == '    '* ]] && continue
        if [[ $line == 'reverie e2e: '* ]]; then
            closed=true
            continue
        fi
        IFS=$'\t' read -r -a f <<<"$line"
        if [[ ${f[0]:-} == summary && ${#f[@]} == 4 ]]; then
            [[ ${f[3]} =~ ^[0-9]+$ ]] || return 1
            summed=$((summed + f[3]))
        elif [[ ${f[0]:-} == STALE ]]; then
            stale=$((stale + 1))
            printf 'syscall-stale/%s/%s/%s/%d\tfail\n' "${f[2]:-}" "${f[1]:-}" "${f[3]:-}" "$stale"
        elif [[ $line == *$'\t'* ]]; then
            # The note is the last field and may be empty, so count tabs.
            local tabs=${line//[^$'\t']/}
            [[ ${#tabs} == 4 && -n ${f[0]:-} && -n ${f[1]:-} ]] || return 1
            rows=$((rows + 1))
            if [[ ${f[4]:-} == UNEXPECTED ]]; then
                printf 'syscall/%s/%s\tfail\n' "${f[0]}" "${f[1]}"
            else
                printf 'syscall/%s/%s\tpass\n' "${f[0]}" "${f[1]}"
            fi
        fi
    done <"$file"
    $closed && ((rows > 0 && rows == summed))
}

function main {
    local mode=${1:-} artifacts=${2:-}
    [[ ($mode == isolation || $mode == syscall) && -n $artifacts && ${3:-} == -- ]] || usage
    shift 3
    (($# > 0)) || usage

    local out status parsed parse_status=0
    out=$(mktemp) || exit 2
    "$@" | tee "$out"
    status=${PIPESTATUS[0]}

    if [[ $mode == isolation ]]; then
        parsed=$(rows_from_isolation "$artifacts/results.tsv") || parse_status=$?
    else
        parsed=$(rows_from_syscall "$out") || parse_status=$?
    fi
    rm -f -- "$out"

    local -a args=()
    local executed=0 failed=0 id result
    while IFS=$'\t' read -r id result; do
        [[ -n $id ]] || continue
        args+=("$id" "$result" 1)
        executed=$((executed + 1))
        [[ $result == fail ]] && failed=$((failed + 1))
    done <<<"$parsed"
    if ((parse_status != 0)); then
        echo "conformance-test-counts: $mode output is incomplete; recording $mode/incomplete as failed" >&2
        args+=("$mode/incomplete" fail 1)
        executed=$((executed + 1))
        failed=$((failed + 1))
        ((status != 0)) || status=1
    fi
    if ((status != 0 && failed == 0)); then
        args+=("$mode/checker-exit" fail 1)
        executed=$((executed + 1))
    fi

    local count_status=0
    "$script_dir/write-structured-test-counts.sh" "$executed" 0 "${args[@]}" || count_status=$?
    echo "conformance-test-counts: $mode: $executed result rows, $failed failed, checker exit $status" >&2
    if ((status == 0 && count_status != 0)); then
        status=$count_status
    fi
    exit "$status"
}

function self_test {
    local scratch status=0 got rc
    scratch=$(mktemp -d) || return 1
    # One case: CMD exit, expected exit, expected counts file.
    function check_case {
        local name=$1 expected_rc=$2 expected=$3
        shift 3
        rc=0
        DAGRUN_TEST_COUNTS_PATH="$scratch/$name.json" "$0" "$@" >/dev/null 2>&1 || rc=$?
        got=$(cat "$scratch/$name.json" 2>/dev/null)
        if [[ $rc != "$expected_rc" || $got != "$expected" ]]; then
            echo "conformance-test-counts: self-test $name: exit $rc (want $expected_rc), counts $got" >&2
            echo "  want $expected" >&2
            status=1
        fi
    }
    local iso=$scratch/iso
    mkdir -p "$iso"
    printf '# schema=backend-isolation-results-v1\nid\tresult\nisolation/a\tpass\nisolation/b\tpass\n' >"$iso/results.tsv"
    check_case iso-pass 0 \
        '{"executed_tests":2,"filtered_tests":0,"results":[{"attempts":1,"id":"isolation/a","result":"pass"},{"attempts":1,"id":"isolation/b","result":"pass"}],"schema":2}' \
        isolation "$iso" -- true
    printf '# schema=backend-isolation-results-v1\nid\tresult\nisolation/a\tfail\nisolation-ledger/line-3/kvm\tfail\n' >"$iso/results.tsv"
    check_case iso-fail 1 \
        '{"executed_tests":2,"filtered_tests":0,"results":[{"attempts":1,"id":"isolation/a","result":"fail"},{"attempts":1,"id":"isolation-ledger/line-3/kvm","result":"fail"}],"schema":2}' \
        isolation "$iso" -- false
    printf '# schema=backend-isolation-results-v1\nid\tresult\nisolation/a\tpass\n' >"$iso/results.tsv"
    check_case iso-exit-without-failing-row 2 \
        '{"executed_tests":2,"filtered_tests":0,"results":[{"attempts":1,"id":"isolation/a","result":"pass"},{"attempts":1,"id":"isolation/checker-exit","result":"fail"}],"schema":2}' \
        isolation "$iso" -- sh -c 'exit 2'
    rm -f -- "$iso/results.tsv"
    check_case iso-missing 1 \
        '{"executed_tests":1,"filtered_tests":0,"results":[{"attempts":1,"id":"isolation/incomplete","result":"fail"}],"schema":2}' \
        isolation "$iso" -- true
    printf 'id\tresult\nisolation/a\tmaybe\n' >"$iso/results.tsv"
    check_case iso-malformed 1 \
        '{"executed_tests":1,"filtered_tests":0,"results":[{"attempts":1,"id":"isolation/incomplete","result":"fail"}],"schema":2}' \
        isolation "$iso" -- true

    local sys=$scratch/sys.txt
    printf 'c1\tptrace\tmatch\t\t\n    c2 kvm: exit: 0 -> 1\nc2\tkvm\tmismatch\tsig\tEXPECTED #12\nsummary\tkvm\tmismatch\t1\nsummary\tptrace\tmatch\t1\nreverie e2e: 2 cells, 2 backends, 0 unexpected, 0 stale\n' >"$sys"
    check_case sys-pass 0 \
        '{"executed_tests":2,"filtered_tests":0,"results":[{"attempts":1,"id":"syscall/c1/ptrace","result":"pass"},{"attempts":1,"id":"syscall/c2/kvm","result":"pass"}],"schema":2}' \
        syscall "$scratch" -- cat "$sys"
    printf 'c1\tptrace\tmismatch\tsig\tUNEXPECTED\nSTALE\tc9\tkvm\tmismatch\tsig\t#7\nsummary\tptrace\tmismatch\t1\nreverie e2e: 1 cells, 1 backends, 1 unexpected, 1 stale\n' >"$sys"
    # shellcheck disable=SC2016 # $1 belongs to the inner shell.
    check_case sys-fail 1 \
        '{"executed_tests":2,"filtered_tests":0,"results":[{"attempts":1,"id":"syscall/c1/ptrace","result":"fail"},{"attempts":1,"id":"syscall-stale/kvm/c9/mismatch/1","result":"fail"}],"schema":2}' \
        syscall "$scratch" -- sh -c 'cat "$1"; exit 1' sh "$sys"
    printf 'c1\tptrace\tmatch\t\t\nsummary\tptrace\tmatch\t2\nreverie e2e: 1 cells, 1 backends, 0 unexpected, 0 stale\n' >"$sys"
    check_case sys-count-mismatch 1 \
        '{"executed_tests":2,"filtered_tests":0,"results":[{"attempts":1,"id":"syscall/c1/ptrace","result":"pass"},{"attempts":1,"id":"syscall/incomplete","result":"fail"}],"schema":2}' \
        syscall "$scratch" -- cat "$sys"
    printf 'c1\tptrace\tmatch\t\t\n' >"$sys"
    # shellcheck disable=SC2016 # $1 belongs to the inner shell.
    check_case sys-unclosed 101 \
        '{"executed_tests":2,"filtered_tests":0,"results":[{"attempts":1,"id":"syscall/c1/ptrace","result":"pass"},{"attempts":1,"id":"syscall/incomplete","result":"fail"}],"schema":2}' \
        syscall "$scratch" -- sh -c 'cat "$1"; exit 101' sh "$sys"
    check_case sys-empty 1 \
        '{"executed_tests":1,"filtered_tests":0,"results":[{"attempts":1,"id":"syscall/incomplete","result":"fail"}],"schema":2}' \
        syscall "$scratch" -- true

    rm -rf -- "$scratch"
    if ((status != 0)); then
        echo "conformance-test-counts: self-test FAILED" >&2
        return 1
    fi
    echo "conformance-test-counts: self-test PASS (isolation pass, fail, exit without failing row, missing, malformed; syscall pass, fail with stale, count mismatch, unclosed, empty)"
}

if [[ ${1:-} == --self-test ]]; then
    self_test
    exit
fi
main "$@"

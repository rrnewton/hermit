#!/usr/bin/env bash
# Verify every configured submodule without initializing or repairing it.

set -euo pipefail

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

check_submodules() {
    local git_command=${SUBMODULE_GIT:-git}
    local -a git_argv
    read -r -a git_argv <<<"$git_command"
    ((${#git_argv[@]} > 0)) || fail 'SUBMODULE_GIT is empty'

    [[ -r .gitmodules ]] || fail '.gitmodules is missing or unreadable'

    local expected_file status_file
    expected_file=$(mktemp)
    status_file=$(mktemp)
    trap 'rm -f -- "$expected_file" "$status_file"' RETURN

    if ! "${git_argv[@]}" config -f .gitmodules --get-regexp '^submodule\..*\.path$' \
        >"$expected_file"; then
        fail 'could not read the configured submodule paths from .gitmodules'
    fi

    local -a expected_paths=()
    local key path
    while read -r key path; do
        [[ -n $key && -n $path ]] || fail 'malformed submodule path entry in .gitmodules'
        expected_paths+=("$path")
    done <"$expected_file"
    ((${#expected_paths[@]} > 0)) || fail '.gitmodules declares no submodules'

    if ! "${git_argv[@]}" submodule status --recursive >"$status_file"; then
        fail 'git submodule status --recursive failed; no inventory was verified'
    fi
    [[ -s $status_file ]] || fail 'git submodule status --recursive returned an empty inventory'

    local line prefix suffix marker observed=0 bad=0
    while IFS= read -r line; do
        [[ ${#line} -ge 43 ]] || fail "malformed submodule status line: $line"
        prefix=${line:0:41}
        [[ $prefix =~ ^[\ +U-][0-9a-f]{40}$ && ${line:41:1} == ' ' ]] || \
            fail "malformed submodule status line: $line"
        suffix=${line:42}
        [[ -n $suffix ]] || fail "malformed submodule status line: $line"
        marker=${line:0:1}
        ((observed += 1))
        if [[ $marker != ' ' ]]; then
            ((bad += 1))
        fi
    done <"$status_file"

    local missing=0
    for path in "${expected_paths[@]}"; do
        if ! awk -v wanted="$path" '
            substr($0, 43, length(wanted)) == wanted &&
            (length($0) == 42 + length(wanted) || substr($0, 43 + length(wanted), 1) == " ") {
                found = 1
            }
            END { exit found ? 0 : 1 }
        ' "$status_file"; then
            printf 'error: configured submodule is absent from status output: %s\n' "$path" >&2
            ((missing += 1))
        fi
    done

    cat "$status_file"
    if ((bad > 0 || missing > 0)); then
        printf 'error: submodule verification refused: %d drifted/uninitialized/conflicted, %d configured path(s) absent\n' \
            "$bad" "$missing" >&2
        printf '%s\n' "  leading '-' = not initialized, '+' = a different revision, 'U' = merge conflict" >&2
        printf '%s\n' "  NOT repaired: run 'make checkout-all' explicitly to reset to the recorded pins." >&2
        return 1
    fi

    printf 'submodules OK -- %d configured path(s), %d status row(s), verified without repair\n' \
        "${#expected_paths[@]}" "$observed"
}

self_test() {
    local root self fake expected status output
    root=$(mktemp -d)
    self=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")
    fake=$root/fake-git
    expected=$root/expected
    status=$root/status
    output=$root/output
    trap 'rm -rf -- "$root"' RETURN

    touch "$root/.gitmodules"
    tee "$fake" >/dev/null <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
case "${1-} ${2-}" in
    'config -f')
        if [[ ${FAKE_CONFIG_RC:-0} != 0 ]]; then exit "$FAKE_CONFIG_RC"; fi
        cat "$FAKE_EXPECTED"
        ;;
    'submodule status')
        if [[ ${FAKE_STATUS_RC:-0} != 0 ]]; then exit "$FAKE_STATUS_RC"; fi
        cat "$FAKE_STATUS"
        ;;
    *)
        echo "unexpected fake-git argv: $*" >&2
        exit 97
        ;;
esac
FAKE
    chmod +x "$fake"

    printf '%s\n' \
        'submodule.agent-utils.path agent-utils' \
        'submodule.rr.path third-party/rr' >"$expected"
    printf ' %040d agent-utils (heads/main)\n %040d third-party/rr (v5.8)\n' 1 2 >"$status"

    run_check() {
        (cd "$root" && SUBMODULE_GIT="$fake" FAKE_EXPECTED="$expected" \
            FAKE_STATUS="$status" FAKE_CONFIG_RC="${FAKE_CONFIG_RC:-0}" \
            FAKE_STATUS_RC="${FAKE_STATUS_RC:-0}" "$self" --check) >"$output" 2>&1
    }
    expect_refusal() {
        local label=$1
        if run_check; then
            cat "$output" >&2
            fail "self-test $label unexpectedly passed"
        fi
    }

    run_check || { cat "$output" >&2; fail 'self-test clean inventory failed'; }
    grep -q 'verified without repair' "$output" || fail 'self-test clean success marker absent'

    FAKE_CONFIG_RC=17 expect_refusal 'config producer failure'
    FAKE_CONFIG_RC=0
    : >"$expected"
    expect_refusal 'empty configured population'
    printf '%s\n' \
        'submodule.agent-utils.path agent-utils' \
        'submodule.rr.path third-party/rr' >"$expected"

    FAKE_STATUS_RC=19 expect_refusal 'status producer failure'
    FAKE_STATUS_RC=0
    : >"$status"
    expect_refusal 'empty observed population'
    printf '%s\n' 'not-a-status-line' >"$status"
    expect_refusal 'malformed status'

    local marker before after
    for marker in '-' '+' 'U'; do
        printf '%s%040d agent-utils (heads/main)\n %040d third-party/rr (v5.8)\n' \
            "$marker" 1 2 >"$status"
        before=$(cksum <"$status")
        expect_refusal "status marker $marker"
        after=$(cksum <"$status")
        [[ $before == "$after" ]] || fail "self-test marker $marker mutated its observed input"
    done

    printf 'PASS: verify-submodules refuses failed, empty, malformed, -, +, and U inventories without mutation\n'
}

case "${1:---check}" in
    --check) check_submodules ;;
    --self-test) self_test ;;
    *) fail "usage: $0 [--check|--self-test]" ;;
esac

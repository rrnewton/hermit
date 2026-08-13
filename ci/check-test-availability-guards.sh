#!/usr/bin/env bash
# Refuse a `#[test]` that silently returns when a dependency is missing.
#
# libtest scores an early `return` from a test function as `ok`. So
#
#     #[test]
#     fn run_kvm_executes_dynamic_guest() {
#         if !Path::new("/dev/kvm").exists() { return; }
#         ...
#     }
#
# announces a PASS on every host without `/dev/kvm` while running no guest at
# all. Twenty-three tests under `hermit-cli/tests/` were doing exactly this;
# they now decide availability at build time and carry
# `#[cfg_attr(not(...), ignore = "SKIPPED: ...")]`, so an unavailable
# dependency reports IGNORED instead of PASSED.
#
# This check exists because that fix had no committed guard. Measured: removing
# one test's `cfg_attr` and restoring its early return left the suite green and
# dropped the ignored count 20 -> 19, with nothing failing. A manual table in a
# pull-request description is not a regression test.
#
# SCOPE, stated so it is not mistaken for more than it is. This refuses the
# availability probe written as a LITERAL absolute path -- the exact form that
# was removed and the exact form the mutation above reinstates. Four probes on
# a named constant remain in the tree
# (`cpuidle_determinism.rs`, `cppc_feedback_determinism.rs`,
# `thp_stats_determinism.rs`, and the two-binary probe at `cli.rs`); they
# predate this check, are the same defect class, and are NOT exempted here --
# they are simply not yet converted, because converting them means moving
# `/proc` and `/sys` probes to build time in three unrelated test files. They
# are named rather than allowlisted so that widening this check later is a
# matter of converting them, not of deleting an exemption.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT_DIR
readonly TEST_DIR="$ROOT_DIR/hermit-cli/tests"

# Emit "line:offending text" for each prohibited guard; exit 1 if any was found.
#
# Only `#[test]` bodies are scanned. A helper function may legitimately return
# early on a missing path -- the defect is specifically a TEST doing so, because
# only a test's early return is scored as a pass.
scan_file() {
    local path=$1
    awk '
        function flush(  probe) {
            if (buf == "") return
            probe = buf
            gsub(/[[:space:]]+/, " ", probe)
            while (match(probe, /if *! *Path::new\("\/[^"]*"\)\.(exists|is_file|is_dir)\(\)[^{]*\{[^{}]*return *;? *\}/)) {
                print fn_line ":" substr(probe, RSTART, RLENGTH)
                found = 1
                probe = substr(probe, RSTART + RLENGTH)
            }
            buf = ""
        }
        {
            line = $0
            sub(/\/\/.*$/, "", line)          # drop line comments
        }
        depth > 0 {
            buf = buf " " line
            n = gsub(/\{/, "{", line)
            m = gsub(/\}/, "}", line)
            depth += n - m
            if (depth <= 0) { flush(); depth = 0; pending = 0 }
            next
        }
        /^[[:space:]]*#\[test\][[:space:]]*$/ { pending = 1; next }
        pending && /^[[:space:]]*(pub[[:space:]]+)?(async[[:space:]]+)?fn[[:space:]]/ {
            fn_line = FNR
            buf = line
            depth = gsub(/\{/, "{", line) - gsub(/\}/, "}", line)
            if (depth <= 0) { flush(); depth = 0; pending = 0 }
            next
        }
        END { exit found ? 1 : 0 }
    ' "$path"
}

# Bracket the scanner in BOTH directions on fixtures, every run, before it is
# trusted against the tree. A check that cannot fail is worse than no check
# here, because the whole finding is about a guard that looked like coverage
# and was not.
self_test() {
    local fixture status
    fixture=$(mktemp)
    # shellcheck disable=SC2064
    trap "rm -f '$fixture'" RETURN

    # 1. The prohibited form MUST be flagged.
    cat >"$fixture" <<'PROHIBITED'
#[test]
fn guarded_by_an_early_return() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    panic!("never reached on a host without /dev/kvm");
}
PROHIBITED
    if scan_file "$fixture" >/dev/null; then
        echo "self-test: scanner MISSED the prohibited early-return guard" >&2
        return 1
    fi

    # 2. The build-time-gated replacement MUST NOT be flagged, and neither may
    #    an ordinary assertion on a path -- otherwise this check would refuse
    #    the very shape the fix introduced.
    cat >"$fixture" <<'ACCEPTED'
#[test]
#[cfg_attr(
    not(hermit_kvm_tests_available),
    ignore = "SKIPPED: requires readable and writable /dev/kvm"
)]
fn gated_at_build_time() {
    assert!(Path::new("/dev/kvm").exists(), "cfg said this was available");
    run_the_guest();
}
ACCEPTED
    status=0
    scan_file "$fixture" >/dev/null || status=$?
    if [[ $status -ne 0 ]]; then
        echo "self-test: scanner FLAGGED the build-time-gated form" >&2
        return 1
    fi

    # 3. The same early return in a NON-test helper MUST NOT be flagged: only a
    #    test's early return is scored as a pass. This is what keeps the check
    #    from degenerating into "no function may return early".
    cat >"$fixture" <<'HELPER'
fn locate_optional_tool() -> Option<PathBuf> {
    if !Path::new("/usr/bin/awk").exists() {
        return None;
    }
    Some(PathBuf::from("/usr/bin/awk"))
}
HELPER
    status=0
    scan_file "$fixture" >/dev/null || status=$?
    if [[ $status -ne 0 ]]; then
        echo "self-test: scanner FLAGGED an early return in a non-test helper" >&2
        return 1
    fi
}

check_tree() {
    local found=0 path hits
    for path in "$TEST_DIR"/*.rs; do
        [[ -e $path ]] || continue
        hits=$(scan_file "$path") || {
            found=1
            while IFS= read -r hit; do
                [[ -n $hit ]] || continue
                echo "${path#"$ROOT_DIR"/}:${hit}" >&2
            done <<<"$hits"
        }
    done
    return "$found"
}

self_test
if ! check_tree; then
    cat >&2 <<'REMEDY'

REFUSED: a #[test] above returns early when a dependency is missing.

libtest scores that as `ok`, so the test announces a pass having executed
nothing. Do not re-add the guard and do not delete the test. Decide
availability at build time instead:

  * add the probe to hermit-cli/build.rs so it emits a cfg
    (see `hermit_kvm_tests_available` and `hermit_test_<tool>_available`), and
  * annotate the test:

        #[test]
        #[cfg_attr(not(hermit_kvm_tests_available),
                   ignore = "SKIPPED: requires readable and writable /dev/kvm")]

An unavailable dependency then reports IGNORED, which is true, instead of
PASSED, which is not.
REMEDY
    exit 1
fi

echo "Test availability-guard check passed."

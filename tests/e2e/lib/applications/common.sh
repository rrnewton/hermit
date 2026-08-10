#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

APPLICATION_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly APPLICATION_DIR
REPO_ROOT=$(cd -- "$APPLICATION_DIR/../../../.." && pwd)
readonly REPO_ROOT
readonly HERMIT_BIN=${HERMIT_BIN:-"$REPO_ROOT/target/debug/hermit"}
readonly HERMIT_APPLICATION_TIMEOUT=${HERMIT_APPLICATION_TIMEOUT:-120}

function require_commands {
    local command

    for command in "$@"; do
        if ! command -v "$command" >/dev/null 2>&1; then
            printf 'required application-test command not found: %s\n' "$command" >&2
            return 1
        fi
    done

    if [[ ! -x $HERMIT_BIN ]]; then
        printf 'Hermit binary not found or not executable: %s\n' "$HERMIT_BIN" >&2
        return 1
    fi
}

function assert_native_nondeterminism {
    local label=$1
    local first=$2
    local second=$3

    if [[ $first == "$second" ]]; then
        printf '%s native probes unexpectedly matched:\n%s\n' "$label" "$first" >&2
        return 1
    fi
}

function run_hermit_verify {
    local label=$1
    shift

    local stdout_file stderr_file verdict_file status=0
    stdout_file=$(mktemp "${TMPDIR:-/tmp}/hermit-app-stdout.XXXXXX")
    stderr_file=$(mktemp "${TMPDIR:-/tmp}/hermit-app-stderr.XXXXXX")
    verdict_file=$(mktemp "${TMPDIR:-/tmp}/hermit-app-verdict.XXXXXX")
    rm -f -- "$verdict_file"

    # STRICT L2, AND THE CLAIM IS NOW SOURCED.
    #
    # This used to pass a bare `--verify` and then grep stderr for
    # "Success: deterministic. Determinism verified." under a comment asserting
    # "every application must exercise strict L2". Neither half held. Bare
    # `--verify` runs the STRIPPED comparison, whose own --verify-json reports
    # bitwise_parity:false -- it normalises numbers, addresses and tmp paths, so a
    # differing read() length, pointer argument or path is erased before comparison.
    # And that banner is printed BY such a run, so scraping it cannot tell a stripped
    # match from a bitwise one. Every application cell inherited an overstated tier.
    #
    # So: ask for the strict comparison, and read the TYPED verdict rather than a
    # human-readable banner. `bitwise_parity` is true only for a full-trace,
    # unstripped, unfiltered comparison, and only when that comparison actually
    # consumed log evidence -- two empty selections "match" under the strictest
    # possible spec, which is why the counts are part of the predicate.
    timeout "$HERMIT_APPLICATION_TIMEOUT" \
        "$HERMIT_BIN" --log=info run --no-virtualize-cpuid \
        --max-timeslice=disabled --base-env=minimal --strict \
        --verify --verify-strict "--verify-json=$verdict_file" -- \
        "$@" >"$stdout_file" 2>"$stderr_file" || status=$?

    # FOUR OUTCOMES, kept distinct. Collapsing them is how a non-result becomes a
    # pass: a run that never reached a comparison is not a run that compared and
    # agreed, and neither is a run whose guest never launched.
    local verdict_summary
    verdict_summary=$(
        python3 - "$verdict_file" <<'PYEOF'
import json, pathlib, sys

path = pathlib.Path(sys.argv[1])
if not path.exists() or not path.read_text().strip():
    print("NO_RESULT missing-or-empty-verify-json")
    raise SystemExit(0)
try:
    record = json.loads(path.read_text().strip())
except ValueError as error:
    print(f"NO_RESULT malformed-verify-json:{error}")
    raise SystemExit(0)
if not isinstance(record, dict):
    print("NO_RESULT verify-json-not-an-object")
    raise SystemExit(0)

verdict = record.get("verdict")
if verdict in (None, "no_result"):
    print("NO_RESULT verdict=no_result (no comparison was performed)")
    raise SystemExit(0)
if verdict != "matched" or not record.get("verified"):
    print(f"DIVERGED verdict={verdict}")
    raise SystemExit(0)

counts = record.get("compared_log_messages") or {}
left, right = counts.get("left"), counts.get("right")
comparison = record.get("comparison") or {}
if not record.get("bitwise_parity"):
    print(
        "NOT_STRICT matched but bitwise_parity=false"
        f" strictness={comparison.get('strictness')!r}"
    )
    raise SystemExit(0)
if not left or not right:
    print(f"NOT_STRICT bitwise_parity=true but compared_log_messages={left}|{right}")
    raise SystemExit(0)
print(f"STRICT_PASS strictness={comparison.get('strictness')} compared={left}|{right}")
PYEOF
    )

    local kind=${verdict_summary%% *}
    local detail=${verdict_summary#* }

    if [[ $kind != STRICT_PASS ]]; then
        case $kind in
            NO_RESULT)
                if ((status != 0)); then
                    printf '%s LAUNCH-REFUSAL: hermit exited %s without reaching a comparison (%s)\n' \
                        "$label" "$status" "$detail" >&2
                else
                    printf '%s NO-RESULT: %s\n' "$label" "$detail" >&2
                fi
                ;;
            DIVERGED)
                printf '%s COMPARISON-FAILURE: %s\n' "$label" "$detail" >&2 ;;
            NOT_STRICT)
                printf '%s NOT-STRICT-L2: %s\n' "$label" "$detail" >&2 ;;
            *)
                printf '%s UNCLASSIFIED verdict: %s\n' "$label" "$verdict_summary" >&2 ;;
        esac
        printf 'hermit exit status: %s\nstdout:\n' "$status" >&2
        cat "$stdout_file" >&2
        printf 'stderr:\n' >&2
        cat "$stderr_file" >&2
        rm -f -- "$stdout_file" "$stderr_file" "$verdict_file"
        return 1
    fi

    if ((status != 0)); then
        printf '%s reported strict L2 but hermit exited %s\n' "$label" "$status" >&2
        rm -f -- "$stdout_file" "$stderr_file" "$verdict_file"
        return "$status"
    fi

    printf '%s STRICT-L2: %s\n' "$label" "$detail" >&2
    cat "$stdout_file"
    rm -f -- "$stdout_file" "$stderr_file" "$verdict_file"
}

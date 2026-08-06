#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Regression guard for `validate.sh::resolve_selective_baseline`.
#
# WHAT THIS PROTECTS. Selective validation runs only the tests whose footprint
# changed since a "last known green" baseline. That baseline is therefore load
# bearing: inherit from a bad one and the lane silently runs a smaller suite
# while still reporting green.
#
# This branch used to pick the baseline with a two-field jq filter --
# `result == "pass" and commit != "unknown"` -- which cannot tell a receipt that
# executed 942 tests from one that executed ZERO. It now routes through
# `ci-hub/validate/anchor_select.py`, the one shared verifier that applies the
# whole qualifying-receipt predicate.
#
# THE CONTRACT BEING PINNED, and the only one that matters for safety:
#   exit 0 WITH an anchor  -> inherit that baseline (a smaller test set)
#   ANY other outcome      -> print nothing, so the caller runs the FULL lane.
# There must be no path on which a failure of the tool yields a smaller test
# set. Every non-zero exit, a null anchor, and a sha absent from this checkout
# are all asserted to fall back.
#
# WHY A STUBBED VERIFIER. The exit-code contract is glue, and glue is what
# rots when someone edits the function. Stubbing anchor_select lets each exit
# code be exercised deterministically and independently of the predicate, which
# is tested directly in ci-hub/validate/tests/. A live-CLI test could not reach
# most of these branches on demand.
#
# THE POSITIVE CASE IS NOT OPTIONAL. Two earlier revisions of this harness
# "passed" while producing <empty> for every case -- once because the stub was
# never written, once because it ran outside the git repo so the trailing
# `git cat-file -e` guard rejected even valid SHAs. Both were caught only
# because one case is REQUIRED to succeed. Do not delete it to make the suite
# faster.
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

failures=0
note() { printf '  %-44s -> %s\n' "$1" "$2"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; failures=$((failures + 1)); }

anchor_sha=$(git rev-parse --verify HEAD~1 2>/dev/null || true)
if [[ -z $anchor_sha ]]; then
    echo "check-selective-baseline-qualification: SKIP (no HEAD~1 in this checkout)"
    exit 0
fi

workdir=$(mktemp -d)
trap 'rm -rf -- "$workdir"' EXIT
stub_dir="$workdir/parent/ci-hub/validate"
mkdir -p "$stub_dir"
stub="$stub_dir/anchor_select.py"

# Extract the function from the REAL validate.sh rather than restating it, so
# this test cannot drift away from the code it guards.
fn="$workdir/fn.sh"
sed -n '/^function resolve_selective_baseline {/,/^}/p' validate.sh > "$fn"
if ! grep -q 'anchor_select.py' "$fn"; then
    fail "resolve_selective_baseline no longer routes through anchor_select.py -- the
      baseline is being inferred without the shared qualifying-receipt predicate"
    exit 1
fi

resolve() { # $1=exit $2=stdout ; echoes the resolved baseline, empty for full lane
    cat > "$stub" <<PY
import sys
sys.stdout.write('''$2''')
sys.exit($1)
PY
    bash -c "
        cd '$repo_root'
        SHALLOW_SELECT=0; SELECTIVE_BASELINE=''; HERMIT_LAST_GREEN_SHA=''
        DEV_HERMIT_PARENT='$workdir/parent'
        VALIDATION_LEDGER_FILE='$workdir/ledger.jsonl'
        . '$fn'
        resolve_selective_baseline
    " 2>/dev/null
}
: > "$workdir/ledger.jsonl"

# POSITIVE CONTROL -- must inherit, or every assertion below is vacuous.
got=$(resolve 0 "{\"anchor\":{\"sha\":\"$anchor_sha\"}}")
note "exit 0 + valid anchor" "${got:-<empty>}"
[[ $got == "$anchor_sha" ]] || fail "exit 0 with a valid anchor must inherit it; got '${got:-<empty>}'"

# Every non-zero exit means FULL LANE.
for code in 2 3 4 5; do
    got=$(resolve "$code" '{"anchor":null}')
    note "exit $code" "${got:-<empty: FULL LANE>}"
    [[ -z $got ]] || fail "exit $code must fall back to the full lane; got '$got'"
done

# Degenerate exit-0 shapes must also fall back.
got=$(resolve 0 '{"anchor":null}')
note "exit 0, anchor null" "${got:-<empty: FULL LANE>}"
[[ -z $got ]] || fail "a null anchor must fall back to the full lane; got '$got'"

got=$(resolve 0 '{"anchor":{"sha":"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"}}')
note "exit 0, sha absent from this checkout" "${got:-<empty: FULL LANE>}"
[[ -z $got ]] || fail "a sha that does not exist here must fall back; got '$got'"

# The two explicit operator overrides are deliberately NOT run through the
# predicate: a human naming a baseline is an instruction, not an inference.
# Pinned so the exemption stays a decision rather than an accident.
got=$(bash -c "cd '$repo_root'; SHALLOW_SELECT=0; SELECTIVE_BASELINE='$anchor_sha'
    HERMIT_LAST_GREEN_SHA=''; DEV_HERMIT_PARENT='$workdir/parent'
    VALIDATION_LEDGER_FILE='$workdir/ledger.jsonl'; . '$fn'; resolve_selective_baseline" 2>/dev/null)
note "explicit --selective-baseline (exempt)" "${got:-<empty>}"
[[ $got == "$anchor_sha" ]] || fail "an explicit baseline must still be honoured"

# A bare hermit checkout has no dev-hermit parent, so selection is disabled and
# the full suite runs: slower, never weaker.
got=$(bash -c "cd '$repo_root'; SHALLOW_SELECT=0; SELECTIVE_BASELINE=''
    HERMIT_LAST_GREEN_SHA=''; DEV_HERMIT_PARENT=''
    VALIDATION_LEDGER_FILE='$workdir/ledger.jsonl'; . '$fn'; resolve_selective_baseline" 2>/dev/null)
note "no dev-hermit parent (bare checkout)" "${got:-<empty: FULL LANE>}"
[[ -z $got ]] || fail "with no parent the baseline must be empty; got '$got'"

if ((failures > 0)); then
    printf 'check-selective-baseline-qualification: %d failure(s)\n' "$failures" >&2
    exit 1
fi
echo "check-selective-baseline-qualification: ok -- 9 cases; exit-0-with-anchor inherits, every other outcome runs the full lane"

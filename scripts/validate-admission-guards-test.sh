#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Mutation test for validate.sh's admission guards. It brackets BOTH sides of
# BOTH refusals plus the bare-clone escape and the discouraged override, by
# driving the real validate.sh with its documented test hooks and asserting the
# guard verdict (exit code + refusal text) without launching the box-consuming
# validation suite:
#
#   HERMIT_VALIDATE_ADMISSION_STATUS_CMD  stand in for `ci-hub validate-lock
#                                         status` (emit HELD:/not-HELD).
#   HERMIT_VALIDATE_REVERIE_PIN_CMD       stand in for scripts/check-reverie-pin.rs
#                                         (fresh vs stale pin, honours the flag).
#   HERMIT_VALIDATE_ADMISSION_SELFTEST=1  stop at the post-guard barrier so an
#                                         ADMITTED run terminates with exit 0 and
#                                         NO validation work / receipt.
#
# The six cases:
#   1 bare-clone-runs         no dev-hermit parent            -> RUN  (exit 0)
#   2 harnessed-unadmitted    dev-hermit parent, box not HELD -> REFUSE (exit 3, names ci-hub)
#   3 harnessed-admitted-runs dev-hermit parent, box HELD     -> RUN  (exit 0)
#   4 stale-pin-refused       admitted, pin check fails       -> REFUSE (exit 4, names docs/updating-reverie.md)
#   5 fresh-pin-runs          admitted, pin check passes      -> RUN  (exit 0)
#   6 stale-pin-override-runs admitted, stale pin + flag      -> RUN  (exit 0); --help documents flag as EXTREMELY DISCOURAGED

set -uo pipefail
export LC_ALL=C

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
VALIDATE="$REPO_ROOT/validate.sh"

readonly EXIT_UNHARNESSED=3
readonly EXIT_STALE_PIN=4

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/validate-admission-test.XXXXXX")"
trap 'rm -rf "$WORK_DIR"' EXIT

# A stand-in for check-reverie-pin.rs: FAILS by default (stale pin) but SUCCEEDS
# when handed --allow-stale-reverie-pin, exactly mirroring the real script's
# escape hatch. This is what makes case 6 exercise the override path rather than
# a blanket-passing stub.
PIN_STUB="$WORK_DIR/pin-stub.sh"
cat >"$PIN_STUB" <<'STUB'
#!/usr/bin/env bash
for arg in "$@"; do
    [[ $arg == --allow-stale-reverie-pin ]] && exit 0
done
exit 1
STUB
chmod +x "$PIN_STUB"

failures=0
pass() { printf '  ok  %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1" >&2; failures=$((failures + 1)); }

# Run validate.sh with a clean, guard-relevant environment. Lock-owner env is
# always cleared so admission is decided ONLY by the status stand-in; each case
# adds the hooks it needs. Returns exit code; stdout+stderr captured in $OUT.
OUT=""
run_validate() {
    local dir=$1; shift
    OUT="$(cd "$dir" && env \
        -u CI_HUB_VALIDATE_LOCK_OWNER_PID \
        -u CI_HUB_VALIDATE_LOCK_OWNER_FILE \
        "$@" ./validate.sh 2>&1)"
    return $?
}

# --- Case 1: a bare/standalone clone (no dev-hermit parent) RUNS -------------
# Copy validate.sh alone into a temp dir that has no .gitmodules ancestor, so
# find_dev_hermit_parent returns nothing and the harness guard must not fire.
BARE_DIR="$WORK_DIR/bare/hermit"
mkdir -p "$BARE_DIR"
cp "$VALIDATE" "$BARE_DIR/validate.sh"
rc=0
run_validate "$BARE_DIR" \
    HERMIT_VALIDATE_ADMISSION_SELFTEST=1 \
    HERMIT_VALIDATE_REVERIE_PIN_CMD=true \
    || rc=$?
if ((rc == 0)) && grep -q "admission guards PASSED" <<<"$OUT"; then
    pass "case1 bare-clone-runs (exit 0, reached barrier)"
else
    fail "case1 bare-clone-runs: expected exit 0 at barrier, got rc=$rc; out=$OUT"
fi

# --- Case 2: harnessed but box not HELD -> REFUSE exit 3, names ci-hub -------
rc=0
run_validate "$REPO_ROOT" \
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD='echo NOT-HELD' \
    || rc=$?
if ((rc == EXIT_UNHARNESSED)) \
    && grep -q "REFUSING TO RUN UNHARNESSED" <<<"$OUT" \
    && grep -q "ci-hub" <<<"$OUT"; then
    pass "case2 harnessed-unadmitted-refused (exit 3, names ci-hub)"
else
    fail "case2 harnessed-unadmitted-refused: expected exit 3 naming ci-hub, got rc=$rc; out=$OUT"
fi

# --- Case 3: harnessed AND box HELD -> RUN ----------------------------------
rc=0
run_validate "$REPO_ROOT" \
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD='echo HELD:agent=ci pid=1' \
    HERMIT_VALIDATE_REVERIE_PIN_CMD=true \
    HERMIT_VALIDATE_ADMISSION_SELFTEST=1 \
    || rc=$?
if ((rc == 0)) && grep -q "admission guards PASSED" <<<"$OUT"; then
    pass "case3 harnessed-admitted-runs (exit 0, reached barrier)"
else
    fail "case3 harnessed-admitted-runs: expected exit 0 at barrier, got rc=$rc; out=$OUT"
fi

# --- Case 4: admitted but stale pin -> REFUSE exit 4, names remedy doc -------
rc=0
run_validate "$REPO_ROOT" \
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD='echo HELD:agent=ci pid=1' \
    HERMIT_VALIDATE_REVERIE_PIN_CMD="$PIN_STUB" \
    HERMIT_VALIDATE_ADMISSION_SELFTEST=1 \
    || rc=$?
if ((rc == EXIT_STALE_PIN)) \
    && grep -q "REVERIE PIN IS OUT OF DATE" <<<"$OUT" \
    && grep -q "docs/updating-reverie.md" <<<"$OUT"; then
    pass "case4 stale-pin-refused (exit 4, names docs/updating-reverie.md)"
else
    fail "case4 stale-pin-refused: expected exit 4 naming docs/updating-reverie.md, got rc=$rc; out=$OUT"
fi

# --- Case 5: admitted AND fresh pin -> RUN ----------------------------------
rc=0
run_validate "$REPO_ROOT" \
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD='echo HELD:agent=ci pid=1' \
    HERMIT_VALIDATE_REVERIE_PIN_CMD=true \
    HERMIT_VALIDATE_ADMISSION_SELFTEST=1 \
    || rc=$?
if ((rc == 0)) && grep -q "admission guards PASSED" <<<"$OUT"; then
    pass "case5 fresh-pin-runs (exit 0, reached barrier)"
else
    fail "case5 fresh-pin-runs: expected exit 0 at barrier, got rc=$rc; out=$OUT"
fi

# --- Case 6: admitted, stale pin, but --allow-stale-reverie-pin -> RUN -------
rc=0
OUT="$(cd "$REPO_ROOT" && env \
    -u CI_HUB_VALIDATE_LOCK_OWNER_PID \
    -u CI_HUB_VALIDATE_LOCK_OWNER_FILE \
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD='echo HELD:agent=ci pid=1' \
    HERMIT_VALIDATE_REVERIE_PIN_CMD="$PIN_STUB" \
    HERMIT_VALIDATE_ADMISSION_SELFTEST=1 \
    ./validate.sh --allow-stale-reverie-pin 2>&1)" || rc=$?
override_ran=0
((rc == 0)) && grep -q "admission guards PASSED" <<<"$OUT" && override_ran=1

# The override must be documented as EXTREMELY DISCOURAGED in --help.
help_out="$(cd "$REPO_ROOT" && ./validate.sh --help 2>&1)"
help_documents=0
grep -q -- "--allow-stale-reverie-pin" <<<"$help_out" \
    && grep -q "EXTREMELY DISCOURAGED" <<<"$help_out" \
    && help_documents=1

if ((override_ran == 1)) && ((help_documents == 1)); then
    pass "case6 stale-pin-override-runs (exit 0) + --help documents EXTREMELY DISCOURAGED"
else
    fail "case6 stale-pin-override-runs: override_ran=$override_ran help_documents=$help_documents rc=$rc; out=$OUT"
fi

echo
if ((failures == 0)); then
    echo "validate-admission-guards-test: all 6 cases PASSED"
    exit 0
fi
echo "validate-admission-guards-test: $failures case(s) FAILED" >&2
exit 1

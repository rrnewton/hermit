#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Mutation test for validate.sh's admission guards. It brackets BOTH sides of
# BOTH refusals, the bare-clone escape, the discouraged pin override, AND the
# hand-forge attacks, by driving the real validate.sh with its documented test
# hooks and asserting the guard verdict (exit code + refusal text) without
# launching the box-consuming validation suite.
#
# The admission proof is NON-FORGEABLE by construction: validate.sh reads the
# lease owner identity (owner_pid/owner_boot_id/owner_start_ticks) ONLY from
# `ci-hub validate-lock status`, requires that owner_pid be a live ancestor of
# the validate process, and requires the owner's live /proc identity to match
# the lease record. No env var and no agent-writable file participates. The
# HERMIT_VALIDATE_ADMISSION_STATUS_CMD stand-in is honored ONLY together with
# the SELFTEST barrier (which runs NO validation and emits NO receipt), so it
# can never admit a real run. This test therefore builds a status stand-in that
# names a REAL ancestor process (this test's own PID) with that PID's REAL
# /proc boot_id + starttime, exactly as the authoritative status would.
#
#   HERMIT_VALIDATE_ADMISSION_STATUS_CMD  stand in for `ci-hub validate-lock
#                                         status` (emit an authoritative HELD:
#                                         record; honored only under SELFTEST).
#   HERMIT_VALIDATE_REVERIE_PIN_CMD       stand in for scripts/check-reverie-pin.rs
#                                         (fresh vs stale pin, honours the flag).
#   HERMIT_VALIDATE_ADMISSION_SELFTEST=1  stop at the post-guard barrier so an
#                                         ADMITTED run terminates with exit 0 and
#                                         NO validation work / receipt.
#
# The cases:
#   1 bare-clone-runs         no dev-hermit parent               -> RUN  (exit 0)
#   2 harnessed-unadmitted    dev-hermit parent, box not HELD    -> REFUSE (exit 3, names ci-hub)
#   3 harnessed-admitted-runs dev-hermit parent, HELD-by-ancestor-> RUN  (exit 0)
#   4 stale-pin-refused       admitted, pin check fails          -> REFUSE (exit 4, names docs/updating-reverie.md)
#   5 fresh-pin-runs          admitted, pin check passes         -> RUN  (exit 0)
#   6 stale-pin-override-runs admitted, stale pin + flag         -> RUN  (exit 0); --help documents flag as EXTREMELY DISCOURAGED
#   7 forge-env-owner-pid     export CI_HUB_VALIDATE_LOCK_OWNER_PID+sidecar -> REFUSE (exit 3)
#   8 forge-env-init          same with owner_pid=1 (init)       -> REFUSE (exit 3)
#   9 forge-status-nonself    HELD names a NON-ancestor owner    -> REFUSE (exit 3)
#  10 forge-status-realrun    HELD stand-in WITHOUT SELFTEST     -> REFUSE (exit 3); real status consulted, ignored

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

# Emit an AUTHORITATIVE-shaped HELD status record for a given live PID, reading
# that PID's real boot_id and starttime from /proc exactly as ci-hub does. When
# the PID is a real ancestor of validate.sh, this makes the descendant + /proc
# identity checks pass -- the same way a genuine `validate-lock run` owner would.
held_status_for() {
    local pid=$1 boot_id stat after start_ticks
    boot_id=$(cat /proc/sys/kernel/random/boot_id)
    stat=$(cat "/proc/$pid/stat")
    after=${stat##*') '}
    start_ticks=$(awk '{print $20}' <<<"$after")
    printf 'HELD:\n  owner_pid=%s\n  owner_boot_id=%s\n  owner_start_ticks=%s\n  owner_process=alive\n' \
        "$pid" "$boot_id" "$start_ticks"
}

# This test process ($$) is a genuine ancestor of every validate.sh it launches,
# so an authoritative HELD record naming $$ models a legitimate admission.
HELD_SELF="$WORK_DIR/held-self.txt"
held_status_for "$$" >"$HELD_SELF"

failures=0
pass() { printf '  ok  %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1" >&2; failures=$((failures + 1)); }

# Run validate.sh with a clean, guard-relevant environment. Lock-owner env is
# always cleared so admission is decided ONLY by the authoritative/stand-in
# status; each case adds the hooks it needs. Returns exit code; stdout+stderr
# captured in $OUT.
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
    HERMIT_VALIDATE_ADMISSION_SELFTEST=1 \
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD='echo FREE' \
    || rc=$?
if ((rc == EXIT_UNHARNESSED)) \
    && grep -q "REFUSING TO RUN UNHARNESSED" <<<"$OUT" \
    && grep -q "ci-hub" <<<"$OUT"; then
    pass "case2 harnessed-unadmitted-refused (exit 3, names ci-hub)"
else
    fail "case2 harnessed-unadmitted-refused: expected exit 3 naming ci-hub, got rc=$rc; out=$OUT"
fi

# --- Case 3: harnessed AND HELD by a live ancestor -> RUN --------------------
rc=0
run_validate "$REPO_ROOT" \
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD="cat $HELD_SELF" \
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
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD="cat $HELD_SELF" \
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
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD="cat $HELD_SELF" \
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
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD="cat $HELD_SELF" \
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

# --- Case 7: FORGE via CI_HUB_VALIDATE_LOCK_OWNER_PID + fake sidecar ---------
# The previous design trusted these agent-controllable inputs. They must now be
# completely ignored: with the real box FREE (status stand-in emits FREE), the
# forge must be REFUSED. This is the adversarial attack the owner named.
FORGE_SIDECAR="$WORK_DIR/forge-owner.txt"
printf 'host=%s\nboot_id=%s\npid=%s\nstart_ticks=%s\n' \
    "$(hostname -s 2>/dev/null || echo unknown)" \
    "$(cat /proc/sys/kernel/random/boot_id)" "$$" \
    "$(after=$(cat /proc/$$/stat); after=${after##*') '}; awk '{print $20}' <<<"$after")" \
    >"$FORGE_SIDECAR"
rc=0
OUT="$(cd "$REPO_ROOT" && env \
    CI_HUB_VALIDATE_LOCK_OWNER_PID="$$" \
    CI_HUB_VALIDATE_LOCK_OWNER_FILE="$FORGE_SIDECAR" \
    HERMIT_VALIDATE_ADMISSION_SELFTEST=1 \
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD='echo FREE' \
    HERMIT_VALIDATE_REVERIE_PIN_CMD=true \
    ./validate.sh 2>&1)" || rc=$?
if ((rc == EXIT_UNHARNESSED)) && grep -q "REFUSING TO RUN UNHARNESSED" <<<"$OUT"; then
    pass "case7 forge-env-owner-pid-refused (exit 3; owner env ignored)"
else
    fail "case7 forge-env-owner-pid-refused: expected exit 3, got rc=$rc; out=$OUT"
fi

# --- Case 8: FORGE with owner_pid=1 (init is every process's ancestor) -------
FORGE_INIT="$WORK_DIR/forge-init.txt"
printf 'host=%s\nboot_id=%s\npid=1\nstart_ticks=1\n' \
    "$(hostname -s 2>/dev/null || echo unknown)" \
    "$(cat /proc/sys/kernel/random/boot_id)" >"$FORGE_INIT"
rc=0
OUT="$(cd "$REPO_ROOT" && env \
    CI_HUB_VALIDATE_LOCK_OWNER_PID=1 \
    CI_HUB_VALIDATE_LOCK_OWNER_FILE="$FORGE_INIT" \
    HERMIT_VALIDATE_ADMISSION_SELFTEST=1 \
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD='echo FREE' \
    HERMIT_VALIDATE_REVERIE_PIN_CMD=true \
    ./validate.sh 2>&1)" || rc=$?
if ((rc == EXIT_UNHARNESSED)) && grep -q "REFUSING TO RUN UNHARNESSED" <<<"$OUT"; then
    pass "case8 forge-env-init-refused (exit 3; init never an owner)"
else
    fail "case8 forge-env-init-refused: expected exit 3, got rc=$rc; out=$OUT"
fi

# --- Case 9: HELD status naming a NON-ancestor owner -> REFUSE ---------------
# Even a genuine HELD record admits ONLY when the owner is an ancestor of this
# run (closes the HELD-by-anyone / concurrent-holder gap). PID 1 is live but is
# not reached by the strict-ancestor walk, so it is refused. (owner_pid=1 is
# rejected outright; use a live non-ancestor: pick init's identity but assert
# the ancestor walk, not the >1 guard, does the refusing by using PID 2 which is
# a kernel thread present on Linux and never an ancestor of a userspace shell.)
NONANCESTOR_HELD="$WORK_DIR/held-nonancestor.txt"
if [[ -r /proc/2/stat ]]; then
    held_status_for 2 >"$NONANCESTOR_HELD" 2>/dev/null || true
fi
if [[ -s $NONANCESTOR_HELD ]]; then
    rc=0
    run_validate "$REPO_ROOT" \
        HERMIT_VALIDATE_ADMISSION_SELFTEST=1 \
        HERMIT_VALIDATE_ADMISSION_STATUS_CMD="cat $NONANCESTOR_HELD" \
        HERMIT_VALIDATE_REVERIE_PIN_CMD=true \
        || rc=$?
    if ((rc == EXIT_UNHARNESSED)) && grep -q "REFUSING TO RUN UNHARNESSED" <<<"$OUT"; then
        pass "case9 held-by-nonancestor-refused (exit 3; not HELD-by-anyone)"
    else
        fail "case9 held-by-nonancestor-refused: expected exit 3, got rc=$rc; out=$OUT"
    fi
else
    pass "case9 held-by-nonancestor-refused (SKIPPED: no /proc/2 kernel thread on this host)"
fi

# --- Case 10: HELD stand-in WITHOUT SELFTEST -> stand-in ignored, REFUSE -----
# On a REAL run (no SELFTEST barrier), the status stand-in must be ignored and
# the authoritative `validate-lock status` consulted instead. There is no live
# lease naming an ancestor of this test, so admission must be refused even though
# the stand-in claims HELD. Because we are not admitted, the guard refuses at
# exit 3 BEFORE any box-consuming validation -- so this stays cheap.
rc=0
OUT="$(cd "$REPO_ROOT" && env \
    -u CI_HUB_VALIDATE_LOCK_OWNER_PID \
    -u CI_HUB_VALIDATE_LOCK_OWNER_FILE \
    HERMIT_VALIDATE_ADMISSION_STATUS_CMD="cat $HELD_SELF" \
    HERMIT_VALIDATE_REVERIE_PIN_CMD=true \
    timeout 300 ./validate.sh 2>&1)" || rc=$?
if grep -q "REFUSING TO RUN UNHARNESSED" <<<"$OUT"; then
    pass "case10 status-standin-ignored-on-real-run (refused; stand-in not honored without SELFTEST)"
else
    fail "case10 status-standin-ignored-on-real-run: expected refusal naming ci-hub, got rc=$rc; out=$OUT"
fi

echo
if ((failures == 0)); then
    echo "validate-admission-guards-test: all cases PASSED"
    exit 0
fi
echo "validate-admission-guards-test: $failures case(s) FAILED" >&2
exit 1

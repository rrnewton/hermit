#!/usr/bin/env bash
# Tests for bounded-run-space, covering BOTH paths that matter:
#
#   * the BOUNDED path, on a throwaway loopback btrfs with quotas enabled, and
#   * the HONEST-DEGRADATION path, which is what this host actually does today,
#     because quotas are NOT enabled on /data.
#
# WHY THE DEGRADATION CASES CARRY MORE WEIGHT HERE THAN THE HAPPY PATH. Until
# quotas are enabled, EVERY real call takes the unbounded path. A silent fallback
# would mean callers believe their runs are capped when nothing caps them, which is
# worse than not having the feature: it converts a known gap into a false guarantee.
# So cases 5-9 assert that the fallback is loud, exits 10, and leaves a marker.
#
# WHY A READBACK CASE EXISTS (case 3). `btrfs qgroup limit` reports failure by exit
# code, but the property we need is not "the command succeeded", it is "the limit is
# live on the qgroup". Those are different claims, and only the second one bounds
# anything. Case 3 asserts we report BOUNDED only when the readback agrees.
#
# The loopback is 256 MiB and is torn down on exit; /data unallocated is at amber.
#
# Run: scripts/bounded-run-space-test.sh
set -uo pipefail

BRS="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/bounded-run-space"
PASS=0
declare -a FAILURES=()
fail() { FAILURES+=("$1${2:+  -- $2}"); }
ok()   { PASS=$(( PASS + 1 )); }
check() { if [ "$1" = "yes" ]; then ok; else fail "$2" "${3:-}"; fi; }
yn()  { if eval "$1"; then echo yes; else echo no; fi; }

[ -x "$BRS" ] || { echo "FAIL  bounded-run-space not executable at $BRS"; exit 1; }

# Refuse rather than report a vacuous pass: without sudo+btrfs the bounded half of
# this suite cannot run at all, and a suite that silently tests only the easy half
# is exactly the false assurance this tool exists to avoid.
sudo -n true >/dev/null 2>&1 || { echo "FAIL  passwordless sudo unavailable; refusing to test only the degraded half"; exit 1; }
command -v mkfs.btrfs >/dev/null 2>&1 || { echo "FAIL  mkfs.btrfs unavailable"; exit 1; }

# Guard this hard. An empty WORK sends every `sudo` line below to the HOST ROOT --
# `truncate /img`, `mount /mnt`. That is not hypothetical: it happened while writing
# these tests, creating a 256 MB /img and mounting it over /mnt.
WORK=$(mktemp -d /var/tmp/brs-test.XXXXXX) || { echo "FAIL  could not create a work directory"; exit 1; }
case "$WORK" in /var/tmp/brs-test.*) ;; *) echo "FAIL  refusing to run with WORK='$WORK'"; exit 1 ;; esac
cleanup() {
    sudo -n umount "$WORK/mnt" 2>/dev/null || true
    sudo -n rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT

sudo -n truncate -s 256M "$WORK/img"
sudo -n mkfs.btrfs -q -f "$WORK/img" >/dev/null 2>&1
sudo -n mkdir -p "$WORK/mnt"
sudo -n mount -o loop,compress=zstd:3 "$WORK/img" "$WORK/mnt" || { echo "FAIL  could not mount loopback"; exit 1; }
sudo -n btrfs quota enable "$WORK/mnt" || { echo "FAIL  could not enable quotas on the loopback"; exit 1; }
sudo -n chmod 0777 "$WORK/mnt"

# --- 1-3. BOUNDED path on a quota-enabled filesystem ----------------------------
out="$("$BRS" create --limit 16M --tag t1 --base "$WORK/mnt" 2>/tmp/brs_t1.err)"; rc=$?
check "$(yn '[ "$rc" -eq 0 ]')" "bounded create exits 0 when quotas are on" "exit=$rc $(cat /tmp/brs_t1.err)"
check "$(yn '[ -d "$out" ]')" "bounded create returns a usable directory" "got: $out"
check "$(yn '[ ! -f "$out/.UNBOUNDED" ]')" "bounded create leaves NO unbounded marker" ""
# The readback claim: the live qgroup must actually carry the limit we asked for.
id="$(sudo -n btrfs subvolume show "$out" 2>/dev/null | awk '/Subvolume ID/{print $3; exit}')"
live="$(sudo -n btrfs qgroup show -re --raw "$WORK/mnt" 2>/dev/null | awk -v q="0/$id" '$1==q{print $4}')"
check "$(yn '[ "$live" = "16000000" ]')" "the limit is LIVE on the qgroup, not merely requested" "readback=$live"

# --- 4. the bound actually stops a writer, and report says so -------------------
python3 - "$out/flood.log" <<'PY' >/dev/null 2>&1
import sys
chunk = (b"INFO detcore::scheduler: [scheduler] >>>>>>>\n\n COMMIT turn 1, dettid 3\n" * 200)
w = 0
try:
    with open(sys.argv[1], "wb") as f:
        while w < 2_000_000_000:
            f.write(chunk); f.flush(); w += len(chunk)
except OSError:
    pass
PY
logical=$(stat -c %s "$out/flood.log" 2>/dev/null || echo 0)
check "$(yn '[ "${logical:-0}" -gt 0 ]')" "the flood wrote something before being stopped" "logical=$logical"
rep="$("$BRS" report "$out" 2>/tmp/brs_t4.err)"; rrc=$?
check "$(yn '[ "$rrc" -eq 11 ]')" "report exits 11 once the allowance is reached" "exit=$rrc out=$rep"
check "$(yn 'grep -q "ALLOWANCE REACHED" /tmp/brs_t4.err')" "report SAYS the contents are incomplete" "$(cat /tmp/brs_t4.err)"
# Compression is why the allowance is a disk number: far more logical bytes get through.
check "$(yn '[ "${logical:-0}" -gt 16000000 ]')" \
      "compressed accounting let MORE than the raw limit through (disk-bytes semantics)" "logical=$logical vs limit=16000000"

# --- 5. destroy reports reclaim and removes the subvolume ------------------------
"$BRS" destroy "$out" 2>/tmp/brs_t5.err; drc=$?
check "$(yn '[ "$drc" -eq 0 ]')" "destroy exits 0" "exit=$drc"
check "$(yn '[ ! -d "$out" ]')" "destroy actually removes the subvolume" ""
check "$(yn 'grep -q "reclaimed about" /tmp/brs_t5.err')" "destroy reports what came back" "$(cat /tmp/brs_t5.err)"

# --- 6-8. HONEST DEGRADATION: quotas OFF (the real host today) -------------------
# /tmp is btrfs and sudo works, but quotas are not enabled, so the limit cannot bind.
out2="$("$BRS" create --limit 16M --tag t6 --base /tmp 2>/tmp/brs_t6.err)"; rc2=$?
check "$(yn '[ "$rc2" -eq 10 ]')" "quotas-off create exits 10, NOT 0" "exit=$rc2"
check "$(yn 'grep -q "UNBOUNDED" /tmp/brs_t6.err')" "quotas-off create says UNBOUNDED on stderr" "$(cat /tmp/brs_t6.err)"
check "$(yn '[ -f "$out2/.UNBOUNDED" ]')" "quotas-off create leaves a .UNBOUNDED marker in the directory" ""
check "$(yn 'grep -q "quota not enabled" "$out2/.UNBOUNDED" 2>/dev/null')" \
      "the marker names the actual reason" "$(cat "$out2/.UNBOUNDED" 2>/dev/null)"
"$BRS" report "$out2" >/dev/null 2>/tmp/brs_t7.err; r2=$?
check "$(yn '[ "$r2" -eq 10 ]')" "report relays UNBOUNDED rather than printing reassuring totals" "exit=$r2"
"$BRS" destroy "$out2" >/dev/null 2>&1
check "$(yn '[ ! -d "$out2" ]')" "destroy cleans up an unbounded space too" ""

# --- 9. non-btrfs base degrades loudly, does not crash ---------------------------
nb=$(mktemp -d /dev/shm/brs-nonbtrfs.XXXXXX 2>/dev/null)
if [ -n "$nb" ] && [ "$(stat -fc %T "$nb")" != "btrfs" ]; then
    out3="$("$BRS" create --limit 16M --tag t9 --base "$nb" 2>/tmp/brs_t9.err)"; rc3=$?
    check "$(yn '[ "$rc3" -eq 10 ]')" "non-btrfs base exits 10" "exit=$rc3"
    check "$(yn 'grep -q "is not btrfs" /tmp/brs_t9.err')" "non-btrfs base names that as the reason" "$(cat /tmp/brs_t9.err)"
    check "$(yn '[ -d "$out3" ]')" "non-btrfs base still yields a usable directory" ""
    rm -rf "$nb"
else
    fail "non-btrfs base case could not run" "no non-btrfs tmpfs available at /dev/shm"
fi

# --- 10. usage errors ------------------------------------------------------------
"$BRS" create --limit banana --base /tmp >/dev/null 2>&1
check "$(yn '[ "$?" -eq 2 ]')" "a malformed --limit is a usage error (exit 2), never a silent default" ""
"$BRS" create --base /tmp >/dev/null 2>&1
check "$(yn '[ "$?" -eq 2 ]')" "a missing --limit is a usage error, never an unbounded default" ""

rm -f /tmp/brs_t1.err /tmp/brs_t4.err /tmp/brs_t5.err /tmp/brs_t6.err /tmp/brs_t7.err /tmp/brs_t9.err

# --- 11. END TO END through hermit-box-run: the whole chain on a bounded space ----
# This is the case the incident would have taken: an ad-hoc hermit-style command whose
# log target fills. Asserts the bound binds, the wrapper RELAYS that the allowance was
# reached (the bounded process will not say so itself), and the space is reclaimed.
HBR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/hermit-box-run"
if [ -x "$HBR" ]; then
    e2e_out="$(SPACE_BASE="$WORK/mnt" "$HBR" --passthrough --cpu-budget 60 --space 8M -- \
        sh -c 'python3 -c "
import sys, os
p = os.environ[\"HERMIT_LOG_FILE\"]
chunk = (b\"INFO detcore::scheduler: [scheduler] >>>>>>>\\n\\n COMMIT turn 1, dettid 3\\n\" * 200)
w = 0
try:
    f = open(p, \"wb\")
    while w < 900_000_000:
        f.write(chunk); f.flush(); w += len(chunk)
except OSError as e:
    print(f\"guest-stopped errno={e.errno} logical={w}\")
"' 2>/tmp/brs_e2e.err)"; e2erc=$?
    check "$(yn '[ "$e2erc" -eq 0 ]')" "e2e: the wrapper propagates the guest exit status" "exit=$e2erc"
    check "$(yn 'grep -q "bounded:" /tmp/brs_e2e.err')" "e2e: the space was reported BOUNDED" "$(head -3 /tmp/brs_e2e.err)"
    check "$(yn '[[ $e2e_out == *"errno=122"* ]]')" \
          "e2e: the guest was stopped by EDQUOT (122), not ENOSPC" "got: $e2e_out"
    check "$(yn 'grep -q "ALLOWANCE REACHED" /tmp/brs_e2e.err')" \
          "e2e: the WRAPPER relays that the evidence is incomplete" "$(cat /tmp/brs_e2e.err)"
    check "$(yn 'grep -q "destroyed:" /tmp/brs_e2e.err')" "e2e: the space is reclaimed afterwards" ""
    check "$(yn '[ -z "$(ls -d "$WORK"/mnt/brs-* 2>/dev/null)" ]')" "e2e: nothing left behind" "$(ls -d "$WORK"/mnt/brs-* 2>/dev/null)"
    rm -f /tmp/brs_e2e.err
else
    fail "e2e case could not run" "hermit-box-run not executable at $HBR"
fi

total=$(( PASS + ${#FAILURES[@]} ))
if [ ${#FAILURES[@]} -gt 0 ]; then
    echo "FAIL  ${#FAILURES[@]} of $total checks failed:"
    for f in "${FAILURES[@]}"; do echo "  - $f"; done
    exit 1
fi
echo "ok  $PASS/$total checks passed  (bounded path on a quota-enabled loopback; honest degradation on this host)"

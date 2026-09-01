#!/usr/bin/env bash
# Demo 08: schedule-dependent btrfs-convert progress-thread use-after-free.
#
# btrfs-progs' btrfs-convert runs a background "progress" subthread that
# dereferences a shared `struct task_info *info` while the main thread copies
# inodes. Before upstream commit 73e211a7, task_start() pthread_detach()ed that
# subthread and task_stop() never pthread_join()ed it, so task_deinit() could
# free(info) while the subthread was still reading it -- a use-after-free whose
# occurrence depends entirely on the teardown interleaving. It is therefore
# essentially invisible to blind/native execution, but hermit's chaos scheduler
# lands the racing schedule deterministically on specific seeds and replays it
# bit-for-bit.
#
# This demo runs prebuilt AddressSanitizer binaries (so the latent UAF becomes
# an observable abort) of two btrfs-convert variants -- `buggy` (pre-73e211a7)
# and `fixed` (73e211a7). It shows: native buggy is clean, chaos buggy crashes
# on a known seed, chaos fixed on the same seed is clean (the differential), and
# the chaos crash reproduces byte-for-byte on replay. See the companion
# 08-btrfs-convert-uaf.md for the bug, the build recipe, and the observability
# adaptation the ASAN binaries carry.

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
ASSETS="${DEMO08_DIR:-$ROOT/ignored/demo08-btrfs}"
ARTIFACTS="${DEMO08_ARTIFACTS:-$ROOT/ignored/demo08-run}"

usage() {
  cat <<'EOF'
Usage: demos/08-btrfs-convert-uaf.sh

Demonstrate a schedule-dependent btrfs-convert progress-thread use-after-free
that native execution misses and hermit --chaos finds and replays.

Requires prebuilt ASAN btrfs-convert binaries and a populated ext4 image under
the ignored asset directory (see 08-btrfs-convert-uaf.md for the build recipe):
  ignored/demo08-btrfs/buggy/btrfs-convert
  ignored/demo08-btrfs/fixed/btrfs-convert
  ignored/demo08-btrfs/pop-tiny.img
When those assets are absent the demo prints SKIPPED and exits 0.

The crashing seed comes from <asset-dir>/.crash-seed, written by
scripts/prepare-demo08-assets.sh. There is no built-in default: which schedules
reproduce this race depends on more than the fixture, so a hardcoded seed would
be wrong on most hosts. When the recorded seed no longer reproduces, this demo
says so, re-derives one, and retries -- it does not report a stale record as a
regression.

Useful overrides:
  DEMO08_DIR=/path        asset directory (buggy/, fixed/, pop-tiny.img)
  DEMO08_ARTIFACTS=/path  per-run scratch + saved ASAN report
  DEMO08_CRASH_SEED=N     drive one seed and skip re-derivation on a miss
  DEMO08_TIMEOUT=150      per-run wall-clock timeout in seconds; defaults to the
                          calibrator's per-seed budget so a calibrated seed
                          cannot be truncated by a tighter clock here
  HERMIT_RELEASE=/path    release Hermit binary
  DEMO08_REQUIRE_ASSETS=1 fail instead of skipping when assets are absent
EOF
}

case "${1:-}" in
  "") ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac

BUGGY="$ASSETS/buggy/btrfs-convert"
FIXED="$ASSETS/fixed/btrfs-convert"
IMAGE="$ASSETS/pop-tiny.img"
REQUIRE_ASSETS="${DEMO08_REQUIRE_ASSETS:-0}"
if [ "$REQUIRE_ASSETS" != 0 ] && [ "$REQUIRE_ASSETS" != 1 ]; then
  echo "error: DEMO08_REQUIRE_ASSETS must be 0 or 1" >&2
  exit 2
fi

# Gate: the ASAN binaries and populated image are large and host-specific, so
# they live in the ignored asset directory rather than the repository. Skip
# cleanly (exit 0) when they are absent so run-all.sh stays green on hosts that
# have not built them.
for f in "$BUGGY" "$FIXED" "$IMAGE"; do
  if [ ! -r "$f" ]; then
    if [ "$REQUIRE_ASSETS" = 1 ]; then
      echo "=== Demo 08: FAILURE — required asset is missing: $f ===" >&2
    else
      echo "=== Demo 08: SKIPPED — missing asset: $f ==="
    fi
    echo "Build the ASAN btrfs-convert variants and image first; see"
    echo "  demos/08-btrfs-convert-uaf.md (Build recipe)."
    [ "$REQUIRE_ASSETS" = 0 ] && exit 0
    exit 1
  fi
done

HERMIT_RELEASE="${HERMIT_RELEASE:-$ROOT/target/release/hermit}"
SAFEHERMIT="${SAFEHERMIT:-$ROOT/bin/safehermit}"
if [ ! -x "$HERMIT_RELEASE" ]; then
  make -C "$ROOT" --no-print-directory -s release-core
fi
if [ ! -x "$HERMIT_RELEASE" ]; then
  echo "error: missing Hermit release binary: $HERMIT_RELEASE" >&2
  exit 1
fi
if [ ! -x "$SAFEHERMIT" ]; then
  echo "error: missing safehermit wrapper: $SAFEHERMIT" >&2
  exit 1
fi

# The recorded seed. Format is `<seed> <fixture-sha256> <hermit-sha256>`; a crashing schedule
# is a property of the exact inputs it was derived from, so the seed travels with their
# identities (see scripts/prepare-demo08-assets.sh and issue #1877).
#
# A mismatch on either hash is a REASON TO RE-DERIVE, not an error, and never a regression
# report. Measured 2026-08-31 at Hermit head 00ed139b: seeds 3, 6, 10 and 13 reproduce the UAF
# while 15, 17 and 19 do not, against a fixture-identical binary -- so the fixture alone does
# not decide which seeds crash, and this demo used to answer that by exiting 1 with "did not
# crash", which reads as "the bug is gone" when the truth is "this seed record is out of date".
SEED_SOURCE="DEMO08_CRASH_SEED"
SEED_FIXTURE=""
SEED_HERMIT=""
if [ -n "${DEMO08_CRASH_SEED:-}" ]; then
  CRASH_SEED="$DEMO08_CRASH_SEED"
elif [ -r "$ASSETS/.crash-seed" ]; then
  CRASH_SEED="$(cut -d' ' -f1 <"$ASSETS/.crash-seed")"
  SEED_FIXTURE="$(cut -s -d' ' -f2 <"$ASSETS/.crash-seed")"
  SEED_HERMIT="$(cut -s -d' ' -f3 <"$ASSETS/.crash-seed")"
  SEED_SOURCE=recorded
elif [ -r "$ASSETS/.nightly-prep-version" ]; then
  # No recorded seed, but the fixtures carry the prep stamp, so the calibrator's cached path
  # applies and will only run seeds -- it will not clone or compile anything. Derive one now
  # so a first run from a clean tree is a single command.
  echo "=== Demo 08: no crash seed recorded yet; deriving one for these inputs ==="
  echo "Which schedules reproduce this race depends on more than the fixture, so there is no"
  echo "safe built-in default. Running scripts/prepare-demo08-assets.sh..."
  echo
  DEMO08_DIR="$ASSETS" "$ROOT/scripts/prepare-demo08-assets.sh" || exit 1
  CRASH_SEED="$(cut -d' ' -f1 <"$ASSETS/.crash-seed")"
  SEED_FIXTURE="$(cut -s -d' ' -f2 <"$ASSETS/.crash-seed")"
  SEED_HERMIT="$(cut -s -d' ' -f3 <"$ASSETS/.crash-seed")"
  SEED_SOURCE=calibrated
  echo
else
  echo "=== Demo 08: no calibrated crash seed, and the fixtures carry no prep stamp ==="
  echo "Which schedules reproduce this race depends on more than the fixture, so there is no"
  echo "safe built-in default. Build the fixtures and derive a seed:"
  echo
  echo "  scripts/prepare-demo08-assets.sh"
  echo
  echo "That records the seed in $ASSETS/.crash-seed and this demo will use it."
  echo "To drive a seed you already know, set DEMO08_CRASH_SEED=N."
  exit 2
fi
[[ $CRASH_SEED =~ ^[0-9]+$ ]] || {
  echo "error: Demo 8 crash seed must be a non-negative integer" >&2
  exit 2
}
# Must not be tighter than the calibrator's per-seed budget
# (DEMO08_CALIBRATION_TIMEOUT, default 150). A seed accepted under a 150s budget and then
# run here under a 90s one can be truncated by THIS script's clock and reported as a seed
# that does not reproduce -- the two budgets disagreeing is a defect in the harness, not a
# fact about the seed.
TIMEOUT="${DEMO08_TIMEOUT:-${DEMO08_CALIBRATION_TIMEOUT:-150}}"
mkdir -p "$ARTIFACTS"

# Fresh reflink copy of the populated image for one conversion. btrfs-convert
# rewrites the image in place, so every run needs its own copy.
fresh_image() {
  local dst="$1"
  cp --reflink=auto "$IMAGE" "$dst"
}

# The deterministic core of an ASAN heap-use-after-free report: the error line
# (faulting heap address + PC), the two guest frames, and the SUMMARY. Under
# hermit these are byte-identical across replays of the same seed; only hermit's
# own host-side log lines vary, and this filter drops them.
asan_core() {
  grep -aE 'AddressSanitizer: heap-use-after-free|task_period_wait|print_copied_inodes|SUMMARY: AddressSanitizer' "$1" || true
}

# The ASAN report TEXT is the detector, exactly as in scripts/prepare-demo08-assets.sh.
# ASAN can report the UAF on a thread whose process still exits 0 (seeds 3 and 13 at Hermit
# head 00ed139b do), so an exit-status test answers a different question. When these two
# scripts disagreed, calibration selected seeds this demo then rejected as "did not crash".
uaf_reported() {
  grep -qa 'AddressSanitizer: heap-use-after-free' "$1"
}

# Did the guest reach btrfs-convert's progress-thread path at all? A run that never got there
# observed nothing about this race, which is not the same fact as "no UAF here".
path_engaged() {
  grep -qa 'Copy inodes' "$1"
}

# hermit chaos invocation validated to reproduce this UAF. --sched-seed selects the
# interleaving. --no-virtualize-cpuid because CPUID faulting is unavailable on the demo
# hosts; that is the one relaxation here and it is stated rather than hidden.
#
# --strict IS REQUIRED, and its absence was a real defect. btrfs-convert generates a random
# UUID for the target filesystem. Without --strict hermit does not virtualize that, so every
# run produced a DIFFERENT filesystem UUID and, occasionally, a different race outcome:
# measured 2026-08-31, seed 6 hit at Step 2 and missed at Step 4 of the same demo run, with
# the two transcripts differing at the UUID line. Four --strict runs produced target UUID
# 10708a9d-7517-44b2-8a5b-dc05ab4ae2fd every time and reproduced the UAF 4/4, at ~9s each
# against ~7s without; three non-strict runs produced two different UUIDs. A demo whose
# headline claim is bit-for-bit replay must not run in the mode that does not guarantee it.
#
# scripts/prepare-demo08-assets.sh uses this identical flag set. A seed selected under one
# flag set is not evidence about another, so the two must not drift apart.
chaos_convert() {
  local conv="$1" seed="$2" img="$3" out="$4"
  timeout "$TIMEOUT" "$SAFEHERMIT" "$HERMIT_RELEASE" --log=error run \
    --chaos --sched-seed "$seed" --no-virtualize-cpuid --strict \
    -- "$conv" "$img" >"$out" 2>&1
}

# The guest's own output, with the two host-side sources removed:
#
#   * safehermit's accounting lines, which carry a wall-clock elapsed time, a per-run
#     identifier and absolute host paths; and
#   * hermit's own log lines, which begin with a real UTC wall-clock timestamp
#     (`2026-08-31T23:01:42.881384Z ERROR reverie_ptrace::lifecycle: ...`).
#
# Nothing the guest itself writes begins with an ISO-8601 UTC timestamp, so this cannot
# swallow guest output. Measured 2026-08-31: with these two sources removed, two --strict
# runs of seed 6 are byte-identical across the entire transcript, UUID and full ASAN report
# included.
#
# This drops hermit's log lines WHOLE, so it is not on its own the rule the repository's
# comparison policy applies to retained logs. hermit_log_records() below compares what it
# drops; the two together are that rule.
guest_transcript() {
  grep -av -e '^safehermit: ' \
    -e '^[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}T[0-9:.]*Z ' "$1" || true
}

# Hermit's own log records with only the real wall-clock prefix removed, which is what the
# repository's comparison policy does to a retained log: the timestamp is the part that
# cannot be compared, so everything after it is kept and compared exactly.
#
# THIS EXISTS BECAUSE DROPPING THE WHOLE LINE HID A REAL FACT. The remainder carries the
# terminating signal, the thread and process it reached, and whether a core was dumped
# (`guest terminated by signal tid=5 pid=3 signal=SIGABRT core_dumped=true`) -- none of
# which the guest's own output states. With the line dropped entirely, two runs differing
# in the signal, the thread, or the core-dump flag compared as byte-identical.
#
# The records are compared UNORDERED, and only their order is treated that way. Measured
# 2026-08-31 over 12 --strict runs of seed 6 with byte-identical inputs: all 12 emitted
# exactly the same two records, and 3 of the 12 emitted them in the opposite order while
# their guest output stayed byte-identical. So the emission order of these host-side
# records is not a fact about the guest, and comparing them in order would report a
# difference that says nothing.
#
# LC_ALL=C on both stages for the same reason the greps above pass -a: the capture is guest
# output and must be treated as bytes, not as text in the ambient locale.
hermit_log_records() {
  LC_ALL=C sed -n 's/^[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}T[0-9:.]*Z //p' "$1" | LC_ALL=C sort
}

echo "=== Demo 08: schedule-dependent btrfs-convert progress-thread UAF ==="
echo "seed=$CRASH_SEED (from ${SEED_SOURCE}) timeout=${TIMEOUT}s"
echo "buggy=$BUGGY"
echo "fixed=$FIXED"
echo

# --- Step 1: native buggy is clean (bug dormant under blind execution) --------
echo "--- Step 1: native buggy btrfs-convert (blind execution) ---"
NATIVE_IMG="$ARTIFACTS/native-buggy.img"
fresh_image "$NATIVE_IMG"
if "$BUGGY" "$NATIVE_IMG" >"$ARTIFACTS/native-buggy.out" 2>&1; then
  echo "native buggy: clean exit (UAF dormant, as expected)"
else
  rc=$?
  # A native crash is possible but rare; it does not invalidate the demo's point
  # (chaos makes it reproducible), so report it rather than failing hard.
  echo "native buggy: exited rc=$rc (rare native manifestation; continuing)"
fi
echo

# --- Step 2: chaos buggy reproduces the UAF on a known seed -------------------
# On a miss this must distinguish "this seed record is out of date for the inputs present"
# from "the bug is gone". Only the second is a regression. The recovery re-derives the seed
# through scripts/prepare-demo08-assets.sh rather than sweeping here, so seed selection keeps
# exactly one implementation -- two of them drifting apart is what produced the defect above.
CHAOS_IMG="$ARTIFACTS/chaos-buggy.img"
BUGGY_RC=0
run_buggy_seed() {
  local seed="$1" out="$2"
  fresh_image "$CHAOS_IMG"
  BUGGY_RC=0
  chaos_convert "$BUGGY" "$seed" "$CHAOS_IMG" "$out" || BUGGY_RC=$?
}

echo "--- Step 2: chaos buggy, --sched-seed $CRASH_SEED (expect ASAN UAF) ---"
run_buggy_seed "$CRASH_SEED" "$ARTIFACTS/chaos-buggy.out"

# One retry at the same seed before concluding anything. A single miss was observed once
# during development, at a seed that then reproduced 18 times out of 18 across repetition,
# concurrent host load, environment padding and cgroup boxing; it was never reproduced and
# is not explained. Recalibrating a whole sweep off one miss is expensive and, if the miss
# is transient, wrong -- so confirm it first, and keep the first output either way.
if ! uaf_reported "$ARTIFACTS/chaos-buggy.out"; then
  cp -f "$ARTIFACTS/chaos-buggy.out" "$ARTIFACTS/chaos-buggy-miss-seed-$CRASH_SEED.out"
  echo
  echo "seed $CRASH_SEED did not reproduce the UAF (guest exit $BUGGY_RC); retrying it once" >&2
  echo "before treating the record as stale. First output kept at" >&2
  echo "  $ARTIFACTS/chaos-buggy-miss-seed-$CRASH_SEED.out" >&2
  run_buggy_seed "$CRASH_SEED" "$ARTIFACTS/chaos-buggy.out"
fi

if ! uaf_reported "$ARTIFACTS/chaos-buggy.out"; then
  echo
  echo "STALE SEED RECORD: seed $CRASH_SEED did not reproduce the UAF here, twice." >&2
  echo "Second attempt: guest exit $BUGGY_RC." >&2
  if [ "$BUGGY_RC" -eq 124 ]; then
    echo "That status is this script's ${TIMEOUT}s wall-clock timeout, so the run was cut" >&2
    echo "short rather than completing without the UAF. Raise DEMO08_TIMEOUT before" >&2
    echo "concluding anything about the seed." >&2
  fi
  if path_engaged "$ARTIFACTS/chaos-buggy.out"; then
    echo "The guest ran and reached the progress-thread path, so this is a statement about" >&2
    echo "the SEED, not about the fixture or the fix." >&2
  else
    echo "The guest did not reach the progress-thread path at all (no 'Copy inodes' line)," >&2
    echo "so this run observed nothing about the race." >&2
  fi
  if [ "$SEED_SOURCE" = "DEMO08_CRASH_SEED" ]; then
    echo "The seed came from DEMO08_CRASH_SEED, so it is not re-derived. Unset it to use the" >&2
    echo "calibrated seed, or run scripts/prepare-demo08-assets.sh to derive one." >&2
    exit 1
  fi
  [ -z "$SEED_FIXTURE" ] || echo "  recorded fixture ${SEED_FIXTURE:0:12}" >&2
  [ -z "$SEED_HERMIT" ] || echo "  recorded hermit  ${SEED_HERMIT:0:12}" >&2
  echo "  present  hermit  $(sha256sum "$HERMIT_RELEASE" | cut -c1-12)" >&2
  echo >&2

  # Only auto-recalibrate when the fixtures carry the prep stamp. Without it the calibrator's
  # cached path does not apply and it would clone and compile btrfs-progs, which is not
  # something a demo run should start on its own.
  if [ ! -r "$ASSETS/.nightly-prep-version" ]; then
    echo "Re-derive the seed for the inputs you have, then re-run this demo:" >&2
    echo "  scripts/prepare-demo08-assets.sh" >&2
    exit 1
  fi

  echo "Re-deriving the crash seed for these inputs (scripts/prepare-demo08-assets.sh)..." >&2
  if ! DEMO08_DIR="$ASSETS" "$ROOT/scripts/prepare-demo08-assets.sh"; then
    echo "Recalibration failed; see its output above. Not reporting this as a UAF" >&2
    echo "regression, because no seed here has been shown to reach the race." >&2
    exit 1
  fi
  CRASH_SEED="$(cut -d' ' -f1 <"$ASSETS/.crash-seed")"
  SEED_SOURCE=recalibrated
  echo
  echo "--- Step 2 (retry): chaos buggy, --sched-seed $CRASH_SEED ---"
  run_buggy_seed "$CRASH_SEED" "$ARTIFACTS/chaos-buggy.out"
  if ! uaf_reported "$ARTIFACTS/chaos-buggy.out"; then
    echo "REGRESSION: the freshly calibrated seed $CRASH_SEED did not reproduce the UAF" >&2
    echo "either (guest exit $BUGGY_RC). A seed derived from these exact inputs failing to" >&2
    echo "reproduce is a real finding, not a stale record. The calibrator's own run of this" >&2
    echo "seed DID reproduce it, so compare the two outputs:" >&2
    echo "  $ARTIFACTS/chaos-buggy.out" >&2
    echo "  $ARTIFACTS/calibration-cold-seed-$CRASH_SEED.out" >&2
    exit 1
  fi
fi

asan_core "$ARTIFACTS/chaos-buggy.out" | tee "$ARTIFACTS/asan-report.txt"
# The guest exit status of the run Step 4 replays. safehermit reports it on a host-side
# accounting line, so it is not in the transcript compared below and has to be carried here.
BUGGY_RC_STEP2="$BUGGY_RC"
echo "chaos buggy: reproduced the use-after-free on seed $CRASH_SEED (from $SEED_SOURCE)"
echo

# --- Step 3: chaos fixed on the same seed is clean (the differential) ---------
echo "--- Step 3: chaos fixed, --sched-seed $CRASH_SEED (expect clean) ---"
FIXED_IMG="$ARTIFACTS/chaos-fixed.img"
fresh_image "$FIXED_IMG"
rc=0
chaos_convert "$FIXED" "$CRASH_SEED" "$FIXED_IMG" "$ARTIFACTS/chaos-fixed.out" || rc=$?
# Read the output whatever the exit status was. This branch used to declare the fix good on
# exit 0 without looking, while the buggy variant is known to report the UAF and still exit 0
# (seeds 3 and 13) -- so the demo's whole differential could have passed with an ASAN report
# sitting unread in this file.
if uaf_reported "$ARTIFACTS/chaos-fixed.out"; then
  echo "regression: fixed variant reproduced the UAF (guest exit $rc)" >&2
  asan_core "$ARTIFACTS/chaos-fixed.out" >&2
  exit 1
fi
if ! path_engaged "$ARTIFACTS/chaos-fixed.out"; then
  echo "inconclusive: the fixed variant never reached the progress-thread path (guest exit" >&2
  echo "$rc), so this run is no evidence that the fix closes the window. Raise DEMO08_TIMEOUT" >&2
  echo "(currently ${TIMEOUT}s) and re-run." >&2
  exit 1
fi
if [ "$rc" -eq 0 ]; then
  echo "chaos fixed: clean exit on the crashing seed, progress-thread path engaged"
  echo "            (73e211a7 closes the window)"
else
  echo "chaos fixed: no UAF, progress-thread path engaged, guest exit $rc"
fi
echo

# --- Step 4: the chaos crash replays byte-for-byte ----------------------------
# Reuse the exact same image path (hence byte-identical argv) as Step 2: hermit
# determinism is per-input, and the faulting heap address depends on argv (the
# image path length shifts the initial heap layout). A different path would give
# a legitimately different-but-still-deterministic address, which would not
# demonstrate replay. Re-copy a fresh ext4 image over the same path.
echo "--- Step 4: replay --sched-seed $CRASH_SEED, confirm identical crash ---"
run_buggy_seed "$CRASH_SEED" "$ARTIFACTS/chaos-buggy-replay.out"
if ! uaf_reported "$ARTIFACTS/chaos-buggy-replay.out"; then
  echo "replay of seed $CRASH_SEED did not reproduce the UAF that Step 2 just produced" >&2
  echo "from byte-identical inputs. That is a determinism finding, not a stale seed." >&2
  exit 1
fi
asan_core "$ARTIFACTS/chaos-buggy-replay.out" >"$ARTIFACTS/asan-report-replay.txt"

# Compare the WHOLE guest transcript, not just the four filtered ASAN lines. The filtered
# extract drops the target filesystem UUID, which is exactly where two runs were observed to
# diverge before --strict was added -- so the narrow comparison would have reported
# "byte-identical" over two genuinely different runs. Keep the ASAN extract for display and
# gate on the full text.
guest_transcript "$ARTIFACTS/chaos-buggy.out" >"$ARTIFACTS/guest-transcript.txt"
guest_transcript "$ARTIFACTS/chaos-buggy-replay.out" >"$ARTIFACTS/guest-transcript-replay.txt"
if ! cmp -s "$ARTIFACTS/guest-transcript.txt" "$ARTIFACTS/guest-transcript-replay.txt"; then
  echo "replay: the guest transcripts differ between two runs of seed $CRASH_SEED over" >&2
  echo "byte-identical inputs. Under --strict this is a determinism finding." >&2
  diff "$ARTIFACTS/guest-transcript.txt" "$ARTIFACTS/guest-transcript-replay.txt" >&2 || true
  exit 1
fi

# What the transcript above cannot carry: hermit's own log records, and the guest exit
# status. Both were dropped before, and a run differing only in one of them compared as
# byte-identical. Widening the transcript to the UUID fixed the case that was measured;
# these two are the same class of gap left inside that fix.
hermit_log_records "$ARTIFACTS/chaos-buggy.out" >"$ARTIFACTS/hermit-log-records.txt"
hermit_log_records "$ARTIFACTS/chaos-buggy-replay.out" >"$ARTIFACTS/hermit-log-records-replay.txt"
if ! cmp -s "$ARTIFACTS/hermit-log-records.txt" "$ARTIFACTS/hermit-log-records-replay.txt"; then
  echo "replay: hermit's own log records differ between two runs of seed $CRASH_SEED over" >&2
  echo "byte-identical inputs. These carry the terminating signal, the thread and process it" >&2
  echo "reached, and whether a core was dumped. The wall-clock prefix is removed and the" >&2
  echo "records are compared unordered, so this is a difference in WHAT happened, not in when" >&2
  echo "hermit logged it or in what order." >&2
  diff "$ARTIFACTS/hermit-log-records.txt" "$ARTIFACTS/hermit-log-records-replay.txt" >&2 || true
  exit 1
fi

if [ "$BUGGY_RC" -ne "$BUGGY_RC_STEP2" ]; then
  echo "replay: the guest exited $BUGGY_RC on the replay and $BUGGY_RC_STEP2 at Step 2, over" >&2
  echo "byte-identical inputs. safehermit reports the exit status on a host-side accounting" >&2
  echo "line, which is not part of the transcript compared above, so it is compared here." >&2
  exit 1
fi

if cmp -s "$ARTIFACTS/asan-report.txt" "$ARTIFACTS/asan-report-replay.txt"; then
  # Say what was actually compared. A report that ASAN did not finish writing carries the
  # faulting address and PC but no frames, and claiming frames were compared would overstate
  # what this file holds.
  if grep -qa 'SUMMARY: AddressSanitizer' "$ARTIFACTS/asan-report.txt"; then
    echo "replay: entire guest transcript byte-identical, ASAN report included"
    echo "        (same heap address, PC, frames, and filesystem UUID); hermit's own log"
    echo "        records and the guest exit status ($BUGGY_RC) match too"
  else
    echo "replay: entire guest transcript byte-identical, and hermit's own log records and"
    echo "        the guest exit status ($BUGGY_RC) match; the ASAN report is truncated, so"
    echo "        it carries the heap address and PC but no frames to compare"
  fi
else
  echo "replay: ASAN reports differ between runs" >&2
  diff "$ARTIFACTS/asan-report.txt" "$ARTIFACTS/asan-report-replay.txt" >&2 || true
  exit 1
fi

echo
echo "=== Demo 08: SUCCESS ==="
echo "native missed the UAF; chaos found it on seed $CRASH_SEED, the fix closed"
echo "it, and the crash replayed deterministically."

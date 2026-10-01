#!/usr/bin/env python3
"""Tests for demo 8's result checks and for its seed calibration.

Both scripts decide PASS or FAIL from what `hermit run --chaos` returns: the
exit status (134 is the ASAN abort, 124 the timeout, 125 a wrapper failure,
122 Hermit's refusal after a late performance-counter interrupt) and
whether the output holds a complete AddressSanitizer report. Each test puts a
stub `hermit` first on PATH that returns one scripted outcome, runs the real
script, and checks that the script accepts or refuses it for the stated reason.
No real Hermit and no btrfs-convert build are involved.

The calibration tests also build a tiny C program with a real heap
use-after-free under AddressSanitizer, so the "a crash was found" path is fed
a genuine ASAN abort rather than hand-written text. They fail, rather than
skip, when the host cannot build it: without that program the tests could not
show that calibration ever accepts a seed.
"""

import hashlib
import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

DEMO_DIR = Path(__file__).resolve().parent.parent
DEMO8 = DEMO_DIR / "08-btrfs-convert-uaf"
RUN = DEMO8 / "run.sh"
PREPARE = DEMO8 / "prepare-assets.sh"
FIXTURES = DEMO8 / "fixtures"

# Must equal PREP_VERSION and BTRFS_COMMIT in prepare-assets.sh. If they drift,
# the stamp written below no longer matches, prepare-assets.sh takes its build
# path instead of the calibration path, and every calibration test fails on
# the "Searching for a crashing seed" check.
PREP_VERSION = 2
BTRFS_COMMIT = "4ab0e80be9e3bb1db2e6038e6d4316d35fb7ba8b"

UAF_REPORT = """\
==1234==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210
    #0 0x4e69f6 in task_period_wait common/task-utils.c:154
    #1 0x4e7100 in print_copied_inodes convert/main.c:169
SUMMARY: AddressSanitizer: heap-use-after-free common/task-utils.c:154 in task_period_wait
"""

# The report cut off before ASAN's closing SUMMARY line.
PARTIAL_REPORT = """\
==1234==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210
    #0 0x4e69f6 in task_period_wait common/task-utils.c:154
"""

# A complete report at a different heap address, for the replay comparison.
OTHER_REPORT = UAF_REPORT.replace("0x606000000210", "0x606000000330")

# Stands in for `hermit ... run --chaos ... -- <btrfs-convert> <image>` in
# run.sh. The buggy binary's outcome comes from DEMO08_TEST_BUGGY_MODE and can
# differ between Step 2 (first buggy run) and Step 4 (second buggy run); the
# fixed binary's outcome comes from DEMO08_TEST_FIXED_MODE.
RUN_STUB = r"""#!/usr/bin/env bash
if [ "${1:-}" = --version ]; then
  echo 'hermit 0.2.0 (2026-09-30, g0123456789ab)'
  exit 0
fi
printf '%s\n' "$*" >>"$DEMO08_TEST_ARGS_FILE"
# What Reverie and Hermit print when a precise-timer interrupt arrives past its
# target: Hermit refuses the run with HERMIT_POLICY_REFUSAL_EXIT (122).
skid_refusal() {
  echo 'HERMIT_SKID_OVERSHOOT rcb_actual=1008 rcb_target=1000 skid_margin=32 overshoot=8' >&2
  echo 'HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=1' >&2
  exit 122
}
conv=""
seen=0
for a in "$@"; do
  if [ "$seen" = 1 ]; then conv="$a"; break; fi
  [ "$a" = "--" ] && seen=1
done
case "$conv" in
  */buggy/btrfs-convert)
    count=0
    [ ! -r "$DEMO08_TEST_COUNT_FILE" ] || count=$(cat "$DEMO08_TEST_COUNT_FILE")
    count=$((count + 1))
    printf '%s\n' "$count" >"$DEMO08_TEST_COUNT_FILE"
    case "$DEMO08_TEST_BUGGY_MODE" in
      complete-abort) cat "$DEMO08_TEST_UAF_FILE"; exit 134 ;;
      partial-rc0) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 0 ;;
      partial-rc124) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 124 ;;
      truncated-abort) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 134 ;;
      skid-refusal) skid_refusal ;;
      replay-partial-rc0|replay-partial-rc124|replay-truncated-abort|replay-different|replay-skid-refusal)
        if [ "$count" -eq 1 ]; then cat "$DEMO08_TEST_UAF_FILE"; exit 134; fi
        case "$DEMO08_TEST_BUGGY_MODE" in
          replay-partial-rc0) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 0 ;;
          replay-partial-rc124) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 124 ;;
          replay-truncated-abort) cat "$DEMO08_TEST_PARTIAL_FILE"; exit 134 ;;
          replay-different) cat "$DEMO08_TEST_OTHER_FILE"; exit 134 ;;
          replay-skid-refusal) skid_refusal ;;
        esac ;;
      *) echo "stub: unknown DEMO08_TEST_BUGGY_MODE" >&2; exit 9 ;;
    esac ;;
  */fixed/btrfs-convert)
    case "$DEMO08_TEST_FIXED_MODE" in
      clean) exit 0 ;;
      exit-zero-uaf) cat "$DEMO08_TEST_UAF_FILE"; exit 0 ;;
      timeout) echo "conversion still running"; exit 124 ;;
      wrapper-failure) echo "wrapper: limit exceeded" >&2; exit 125 ;;
      regression-uaf) cat "$DEMO08_TEST_UAF_FILE"; exit 134 ;;
      skid-refusal) skid_refusal ;;
      *) echo "stub: unknown DEMO08_TEST_FIXED_MODE" >&2; exit 9 ;;
    esac ;;
  *) echo "stub: unexpected btrfs-convert path: $conv" >&2; exit 9 ;;
esac
"""

# Stands in for the calibration's `hermit ... --sched-seed N ... -- <conv> <img>`.
# The outcome depends on DEMO08_TEST_MODE, the seed, the variant, and how many
# times this (variant, seed) pair has run, so a mode can answer differently on
# a seed's first run and on its confirmation replay.
CALIBRATION_STUB = r"""#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = --version ]; then
  echo 'hermit 0.2.0 (2026-09-30, g0123456789ab)'
  exit 0
fi
seed=""
conv=""
previous=""
seen=0
for a in "$@"; do
  if [ "$seen" = 1 ]; then conv="$a"; break; fi
  [ "$previous" != --sched-seed ] || seed="$a"
  [ "$a" != "--" ] || seen=1
  previous="$a"
done
case "$conv" in
  */buggy/btrfs-convert) variant=buggy ;;
  */fixed/btrfs-convert) variant=fixed ;;
  *) echo "stub: unexpected btrfs-convert path: $conv" >&2; exit 9 ;;
esac
[ -n "$seed" ] || { echo "stub: no --sched-seed" >&2; exit 9; }

mkdir -p "$DEMO08_TEST_COUNT_DIR"
counter="$DEMO08_TEST_COUNT_DIR/$variant-$seed"
count=1
[ ! -r "$counter" ] || count=$(($(cat "$counter") + 1))
printf '%s\n' "$count" >"$counter"

engage() { printf 'Copy inodes [o] [         0/         1]\r\n'; }
abort_with_uaf() {
  ASAN_OPTIONS=detect_leaks=0:abort_on_error=1 "${DEMO08_TEST_UAF_BIN:?}"
}
partial_report() {
  echo '==123==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000210'
  echo 'READ of size 8 at 0x606000000210 thread T1'
}
# Hermit's refusal after a late performance-counter interrupt (exit 122).
refusal() {
  echo 'HERMIT_SKID_OVERSHOOT rcb_actual=1008 rcb_target=1000 skid_margin=32 overshoot=8' >&2
  echo 'HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=1' >&2
  exit 122
}

case "${DEMO08_TEST_MODE:?}" in
  refused-once)
    # Each (variant, seed) pair is refused on its first run only; after that
    # the buggy variant crashes on DEMO08_TEST_UAF_SEED and the fixed one is
    # clean.
    if [ "$count" -eq 1 ]; then refusal; fi
    engage
    if [ "$variant" = buggy ] && [ "$seed" = "${DEMO08_TEST_UAF_SEED:?}" ]; then
      abort_with_uaf
    fi
    echo 'Conversion complete' ;;
  always-refused)
    refusal ;;
  replay-refused)
    engage
    if [ "$variant" = fixed ]; then echo 'Conversion complete'; exit 0; fi
    if [ "$count" -eq 1 ]; then abort_with_uaf; fi
    refusal ;;
  fixed-refused-uaf)
    engage
    if [ "$variant" = buggy ]; then abort_with_uaf; fi
    abort_with_uaf || true
    refusal ;;
  fixed-refused)
    engage
    if [ "$variant" = buggy ]; then abort_with_uaf; fi
    refusal ;;
  no-engagement)
    echo 'exited before the progress thread started' ;;
  engaged-no-hit)
    engage; echo 'Conversion complete' ;;
  planted-uaf)
    engage
    if [ "$variant" = buggy ] && [ "$seed" = "${DEMO08_TEST_UAF_SEED:?}" ]; then
      abort_with_uaf
    fi
    echo 'Conversion complete' ;;
  replay-timeout)
    engage
    if [ "$variant" = fixed ]; then echo 'Conversion complete'; exit 0; fi
    if [ "$count" -eq 1 ]; then abort_with_uaf; fi
    partial_report; exit 124 ;;
  replay-clean)
    engage
    if [ "$variant" = buggy ] && [ "$count" -eq 1 ]; then abort_with_uaf; fi
    echo 'Conversion complete' ;;
  fixed-timeout)
    engage
    if [ "$variant" = buggy ]; then abort_with_uaf; fi
    echo 'Conversion incomplete'; exit 124 ;;
  fixed-uaf)
    engage; abort_with_uaf ;;
  uaf-no-engagement)
    abort_with_uaf ;;
  partial-uaf-rc0)
    engage; partial_report; exit 0 ;;
  partial-uaf-rc124)
    engage; partial_report; exit 124 ;;
  complete-uaf-rc0)
    # A complete report from a guest that did not abort, which is what a
    # binary built without abort_on_error prints.
    engage
    if [ "$variant" = fixed ]; then echo 'Conversion complete'; exit 0; fi
    partial_report
    echo 'SUMMARY: AddressSanitizer: heap-use-after-free common/task-utils.c:154 in task_period_wait'
    exit 0 ;;
  runner-failure)
    echo 'could not start the guest' >&2; exit 127 ;;
  runner-failure-with-signatures)
    engage; abort_with_uaf || true; exit 127 ;;
  *)
    echo "stub: unknown DEMO08_TEST_MODE" >&2; exit 9 ;;
esac
"""

PLANTED_UAF_C = r"""#include <stdint.h>
#include <stdlib.h>

int main(void) {
  volatile uint8_t *value = malloc(1);
  if (value == NULL)
    return 2;
  *value = 7;
  free((void *)value);
  return *value;
}
"""


def _write_executable(path, text):
    path.write_text(text)
    path.chmod(0o755)


def _base_environment(stub_dir):
    """The caller's environment without DEMO08_* settings, stub first on PATH."""
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("DEMO08_")
    }
    environment["PATH"] = "{}:{}".format(stub_dir, environment.get("PATH", ""))
    environment["LC_ALL"] = "C"
    return environment


def _make_assets(assets):
    """Placeholder assets: both binaries exit 0 when run natively."""
    for variant in ("buggy", "fixed"):
        (assets / variant).mkdir(parents=True)
        _write_executable(
            assets / variant / "btrfs-convert", "#!/usr/bin/env bash\nexit 0\n"
        )
    (assets / "pop-tiny.img").write_bytes(b"")


def _sha256_hex(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def fixture_source_digest():
    """The fixture-src digest that prepare-assets.sh puts in its cache stamp.

    It mirrors fixture_source_identity: sha256sum of the patch, then one
    "<relative name> <sha256sum>" line per fixture file in byte order, hashed
    together.
    """
    lines = ["{}  -\n".format(_sha256_hex(FIXTURES / "convert-main-v7.1.patch"))]
    relatives = []
    for directory, _subdirs, files in os.walk(FIXTURES):
        for name in files:
            path = Path(directory) / name
            if path.is_file() and not path.is_symlink():
                relatives.append(str(path.relative_to(FIXTURES)))
    for relative in sorted(relatives, key=lambda name: name.encode()):
        lines.append("{} {}  -\n".format(relative, _sha256_hex(FIXTURES / relative)))
    return hashlib.sha256("".join(lines).encode()).hexdigest()


class DemoRunControlsTest(unittest.TestCase):
    """run.sh must pass only on the full result: crash, clean fix, same crash."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.stub_dir = self.tmp / "bin"
        self.stub_dir.mkdir()
        _write_executable(self.stub_dir / "hermit", RUN_STUB)
        self.assets = self.tmp / "assets"
        _make_assets(self.assets)
        self.artifacts = self.tmp / "artifacts"
        for name, text in (
            ("uaf.txt", UAF_REPORT),
            ("partial.txt", PARTIAL_REPORT),
            ("other.txt", OTHER_REPORT),
        ):
            (self.tmp / name).write_text(text)

    def _run(self, fixed_mode="clean", buggy_mode="complete-abort", **overrides):
        environment = _base_environment(self.stub_dir)
        environment.update(
            {
                "DEMO08_DIR": str(self.assets),
                "DEMO08_ARTIFACTS": str(self.artifacts),
                "DEMO08_CRASH_SEED": "7",
                "DEMO08_TIMEOUT": "30",
                "DEMO08_REQUIRE_ASSETS": "1",
                "DEMO08_TEST_FIXED_MODE": fixed_mode,
                "DEMO08_TEST_BUGGY_MODE": buggy_mode,
                "DEMO08_TEST_COUNT_FILE": str(self.tmp / "buggy-count"),
                "DEMO08_TEST_ARGS_FILE": str(self.tmp / "hermit-args"),
                "DEMO08_TEST_UAF_FILE": str(self.tmp / "uaf.txt"),
                "DEMO08_TEST_PARTIAL_FILE": str(self.tmp / "partial.txt"),
                "DEMO08_TEST_OTHER_FILE": str(self.tmp / "other.txt"),
            }
        )
        for key, value in overrides.items():
            if value is None:
                environment.pop(key, None)
            else:
                environment[key] = value
        return subprocess.run(
            [str(RUN)],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=120,
        )

    def _assert_refused(self, result, reason):
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(reason, result.stdout)
        self.assertNotIn("SUCCESS ===", result.stdout)

    def test_the_full_result_passes(self):
        """Positive control: without it, a script that refuses everything passes."""
        result = self._run()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "=== Demo 8: btrfs-convert Use-After-Free: SUCCESS ===", result.stdout
        )
        self.assertIn("replay: ASAN report byte-identical", result.stdout)
        # Step 2 saves the guest-produced core of the report, nothing else.
        self.assertEqual(
            (self.artifacts / "asan-report.txt").read_text(), UAF_REPORT
        )
        # Every chaos run uses the documented command line and the same seed.
        invocations = (self.tmp / "hermit-args").read_text().splitlines()
        self.assertEqual(len(invocations), 3)
        for line in invocations:
            self.assertRegex(
                line,
                r"^--log=error run --chaos --sched-seed 7 --no-virtualize-cpuid "
                r"--base-env=minimal --epoch=2026-01-01T00:00:00Z -- "
                r"\S+/(buggy|fixed)/btrfs-convert \S+\.img$",
            )

    def _native_buggy_prints(self, report_name, status):
        """Make the native buggy binary print one of the reports and exit."""
        _write_executable(
            self.assets / "buggy" / "btrfs-convert",
            "#!/usr/bin/env bash\ncat {}\nexit {}\n".format(
                self.tmp / report_name, status
            ),
        )

    def test_a_native_run_with_no_report_says_so(self):
        result = self._run()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "native buggy: rc=0 with no use-after-free report", result.stdout
        )
        self.assertIn("the native run showed no use-after-free;", result.stdout)

    def test_a_native_partial_report_is_reported_not_called_clean(self):
        """A native run can print half a report and still exit 0."""
        self._native_buggy_prints("partial.txt", 0)
        result = self._run()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "native buggy: rc=0; ASAN started a use-after-free report, but the "
            "process exited before the report's SUMMARY:",
            result.stdout,
        )
        self.assertIn(
            "the native run started an ASAN report but exited rc=0 before it "
            "completed;",
            result.stdout,
        )
        self.assertNotIn("no use-after-free report", result.stdout)
        self.assertNotIn("showed no use-after-free", result.stdout)

    def test_a_native_complete_report_is_reported_as_a_crash(self):
        self._native_buggy_prints("uaf.txt", 134)
        result = self._run()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "native buggy: rc=134 with a complete ASAN use-after-free report",
            result.stdout,
        )
        self.assertIn("the native run also crashed this time (rc=134);", result.stdout)
        self.assertNotIn("showed no use-after-free", result.stdout)

    def test_a_fixed_run_that_exits_0_with_a_uaf_report_is_refused(self):
        self._assert_refused(
            self._run(fixed_mode="exit-zero-uaf"),
            "regression: the fixed variant reported a use-after-free",
        )

    def test_a_fixed_run_that_crashes_with_a_uaf_is_refused(self):
        self._assert_refused(
            self._run(fixed_mode="regression-uaf"),
            "regression: the fixed variant reported a use-after-free",
        )

    def test_a_fixed_run_cut_off_by_the_timeout_is_refused(self):
        self._assert_refused(
            self._run(fixed_mode="timeout"),
            "rc=124 is the 30s timeout: the control was cut off",
        )

    def test_a_fixed_run_stopped_by_a_wrapper_is_refused(self):
        self._assert_refused(
            self._run(fixed_mode="wrapper-failure"),
            "rc=125 is a wrapper failure, not a guest result",
        )

    def test_a_fixed_run_refused_after_a_skid_overshoot_is_refused(self):
        self._assert_refused(
            self._run(fixed_mode="skid-refusal"),
            "rc=122 means Hermit refused the run",
        )

    def test_a_buggy_run_refused_after_a_skid_overshoot_is_explained(self):
        result = self._run(buggy_mode="skid-refusal")
        self._assert_refused(result, "rc=122 means Hermit refused the run")
        self.assertIn("chaos-buggy.out", result.stdout)

    def test_a_partial_report_with_exit_0_is_not_a_crash(self):
        self._assert_refused(self._run(buggy_mode="partial-rc0"), "expected 134")

    def test_a_partial_report_cut_off_by_the_timeout_is_not_a_crash(self):
        self._assert_refused(
            self._run(buggy_mode="partial-rc124"),
            "rc=124 means the 30s timeout cut the run off",
        )

    def test_an_abort_with_a_truncated_report_is_not_a_complete_crash(self):
        """Exit 134 alone is not enough: the report must reach its SUMMARY."""
        self._assert_refused(
            self._run(buggy_mode="truncated-abort"),
            "chaos buggy seed 7 exited 134 without a complete ASAN use-after-free report",
        )

    def test_a_second_run_with_exit_0_is_refused(self):
        self._assert_refused(
            self._run(buggy_mode="replay-partial-rc0"),
            "replay did not complete with the ASAN abort",
        )

    def test_a_second_run_cut_off_by_the_timeout_is_refused(self):
        self._assert_refused(
            self._run(buggy_mode="replay-partial-rc124"),
            "replay did not complete with the ASAN abort",
        )

    def test_a_second_run_refused_after_a_skid_overshoot_is_explained(self):
        """Step 4 explains a refusal as steps 2 and 3 do, and still exits 1."""
        result = self._run(buggy_mode="replay-skid-refusal")
        self._assert_refused(result, "replay did not complete with the ASAN abort: rc=122")
        self.assertIn("rc=122 means Hermit refused the run", result.stdout)
        self.assertIn("chaos-buggy-replay.out", result.stdout)

    def test_a_second_run_with_a_truncated_report_is_refused(self):
        self._assert_refused(
            self._run(buggy_mode="replay-truncated-abort"),
            "replay exited 134 without a complete",
        )

    def test_a_second_run_with_a_different_report_is_refused(self):
        self._assert_refused(
            self._run(buggy_mode="replay-different"),
            "replay: ASAN reports differ between runs",
        )

    def test_missing_assets_skip_by_default(self):
        shutil.rmtree(self.assets / "buggy")
        result = self._run(DEMO08_REQUIRE_ASSETS=None)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("=== Demo 8: SKIPPED -- missing asset:", result.stdout)
        self.assertFalse((self.tmp / "hermit-args").exists())

    def test_missing_assets_fail_when_required(self):
        shutil.rmtree(self.assets / "buggy")
        result = self._run()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("=== Demo 8: FAILURE -- required asset is missing:", result.stdout)

    def test_a_seed_recorded_for_another_binary_is_refused(self):
        (self.assets / ".crash-seed").write_text("15 {}\n".format("0" * 64))
        result = self._run(DEMO08_CRASH_SEED=None)
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("was recorded for buggy binary 000000000000", result.stdout)
        self.assertFalse((self.tmp / "hermit-args").exists())

    def test_a_missing_hermit_is_named(self):
        without_hermit = [
            entry
            for entry in os.environ.get("PATH", "").split(":")
            if entry and not os.access(os.path.join(entry, "hermit"), os.X_OK)
        ]
        result = self._run(PATH=":".join(without_hermit))
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("error: hermit is not on PATH", result.stdout)


class CalibrationControlsTest(unittest.TestCase):
    """prepare-assets.sh must record a seed only when run.sh would pass on it."""

    CALIBRATION_TIMEOUT = 5

    @classmethod
    def setUpClass(cls):
        cls._class_tmp = tempfile.TemporaryDirectory()
        cls.class_tmp = Path(cls._class_tmp.name)
        cls.planted_uaf = cls._build_planted_uaf(cls.class_tmp)
        cls.digest = fixture_source_digest()

    @classmethod
    def tearDownClass(cls):
        cls._class_tmp.cleanup()

    @staticmethod
    def _asan_link_flags(scratch):
        """Extra linker flags for AddressSanitizer, or [] when none are needed.

        On some hosts `-fsanitize=address` names a libasan that is not on the
        default library path although the runtime is installed elsewhere. Find
        it and add both -L and an rpath, so the program links and also runs.
        """
        probe = scratch / "asan-probe.c"
        probe.write_text("int main(void) { return 0; }\n")
        linked = subprocess.run(
            ["gcc", "-fsanitize=address", str(probe), "-o", str(scratch / "asan-probe")],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=120,
        )
        if linked.returncode == 0:
            return []
        script = subprocess.run(
            ["gcc", "-print-file-name=libasan.so"],
            stdout=subprocess.PIPE,
            text=True,
            timeout=60,
        ).stdout.strip()
        wanted = "libasan.so"
        try:
            match = re.search(r"libasan\.so\.[0-9.]+", Path(script).read_text(errors="replace"))
            if match:
                wanted = match.group(0)
        except OSError:
            pass
        found = subprocess.run(
            ["find", "/opt", "/usr/local", "-name", wanted, "-print", "-quit"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=300,
        ).stdout.strip()
        if not found:
            raise AssertionError(
                "no AddressSanitizer runtime ({}) found; the calibration tests "
                "need a C compiler with AddressSanitizer".format(wanted)
            )
        directory = str(Path(found).parent)
        return ["-L" + directory, "-Wl,-rpath," + directory]

    @classmethod
    def _build_planted_uaf(cls, scratch):
        if shutil.which("gcc") is None:
            raise AssertionError(
                "gcc is required: the calibration tests build a real "
                "AddressSanitizer use-after-free"
            )
        source = scratch / "planted-uaf.c"
        source.write_text(PLANTED_UAF_C)
        binary = scratch / "planted-uaf"
        subprocess.run(
            ["gcc", "-O0", "-g", "-fsanitize=address", "-fno-omit-frame-pointer",
             str(source), "-o", str(binary)] + cls._asan_link_flags(scratch),
            check=True,
            timeout=120,
        )
        # Check the program itself before relying on it: a program that stopped
        # reporting the use-after-free would make every "seed found" test vacuous.
        environment = dict(os.environ, ASAN_OPTIONS="detect_leaks=0:abort_on_error=1")
        ran = subprocess.run(
            [str(binary)],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=60,
        )
        if "heap-use-after-free" not in ran.stdout or ran.returncode == 0:
            raise AssertionError(
                "the planted use-after-free did not produce an ASAN abort "
                "(exit {}); first lines:\n{}".format(
                    ran.returncode, "\n".join(ran.stdout.splitlines()[:5])
                )
            )
        return binary

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.stub_dir = self.tmp / "bin"
        self.stub_dir.mkdir()
        _write_executable(self.stub_dir / "hermit", CALIBRATION_STUB)
        self.assets = self.tmp / "assets"
        _make_assets(self.assets)
        # The cache stamp prepare-assets.sh expects, so it skips the build and
        # goes straight to calibration.
        (self.assets / ".nightly-prep-version").write_text(
            "prep={} btrfs={} fixture-src={}\n".format(
                PREP_VERSION, BTRFS_COMMIT, self.digest
            )
        )
        self.artifacts = self.tmp / "artifacts"
        self.fixture = _sha256_hex(self.assets / "buggy" / "btrfs-convert")

    def _prepare(self, mode, seeds, **overrides):
        environment = _base_environment(self.stub_dir)
        environment.update(
            {
                "DEMO08_DIR": str(self.assets),
                "DEMO08_BUILD_ROOT": str(self.tmp / "build-unused"),
                # A local path that does not exist, so a stamp mismatch can
                # never fetch anything.
                "DEMO08_BTRFS_REPO": str(self.tmp / "no-such-repository"),
                "DEMO08_ARTIFACTS": str(self.artifacts),
                "DEMO08_CALIBRATION_SEEDS": str(seeds),
                "DEMO08_CALIBRATION_TIMEOUT": str(self.CALIBRATION_TIMEOUT),
                "DEMO08_TEST_COUNT_DIR": str(self.tmp / "counts"),
                "DEMO08_TEST_MODE": mode,
                "DEMO08_TEST_UAF_BIN": str(self.planted_uaf),
            }
        )
        environment.update(overrides)
        result = subprocess.run(
            [str(PREPARE)],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=300,
        )
        return result

    def _calibrated(self, result):
        """Assert the cached-asset calibration path ran (not the build path)."""
        self.assertIn("Searching for a crashing seed", result.stdout)
        return result

    def _rows(self):
        return (self.artifacts / "calibration.tsv").read_text().splitlines()

    def _count_rows(self, pattern):
        return sum(1 for row in self._rows() if re.search(pattern, row))

    def _assert_refused(self, result, *reasons):
        self._calibrated(result)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        for reason in reasons:
            self.assertIn(reason, result.stdout)
        self.assertFalse((self.assets / ".crash-seed").exists())

    def test_a_real_asan_crash_is_found_confirmed_and_recorded(self):
        """Positive control: calibration accepts a seed that run.sh would pass."""
        result = self._calibrated(self._prepare("planted-uaf", 3, DEMO08_TEST_UAF_SEED="1"))
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("engagement=2/2 uaf_hits=1/2", result.stdout)
        self.assertIn("Demo 8 crash seed calibrated: 1", result.stdout)
        self.assertNotIn("Demo 8 crash seed replayed:", result.stdout)
        self.assertEqual(
            (self.assets / ".crash-seed").read_text(), "1 {}\n".format(self.fixture)
        )
        # Seed 0 ran clean; seed 1 crashed, crashed again, and the fixed variant
        # was clean on it. Seed 2 was never needed.
        self.assertEqual(len(self._rows()), 5)
        self.assertEqual(self._count_rows(r"^0\tcold\tbuggy\treached\tnone\t0\t"), 1)
        self.assertEqual(
            self._count_rows(r"^1\tcold\tbuggy\treached\tcomplete\t134\t\d+\tyes\t"), 1
        )
        self.assertEqual(
            self._count_rows(r"^1\tcold\tbuggy-replay\treached\tcomplete\t134\t\d+\tyes\t"), 1
        )
        self.assertEqual(
            self._count_rows(r"^1\tcold\tfixed\treached\tnone\t0\t\d+\tn/a\t"), 1
        )
        for name in ("calibration-cold-seed-1.out", "calibration-confirm-replay-seed-1.out"):
            self.assertIn(
                "AddressSanitizer: heap-use-after-free",
                (self.artifacts / name).read_text(errors="replace"),
            )

    def test_a_recorded_seed_is_replayed_not_trusted(self):
        (self.assets / ".crash-seed").write_text("1 {}\n".format(self.fixture))
        result = self._calibrated(self._prepare("planted-uaf", 3, DEMO08_TEST_UAF_SEED="1"))
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "engagement=1/1 uaf_hits=1/1 qualified=1/1 executed=1/1", result.stdout
        )
        self.assertIn("Demo 8 crash seed replayed: cached seed 1", result.stdout)
        self.assertNotIn("Demo 8 crash seed calibrated:", result.stdout)
        self.assertEqual(len(self._rows()), 4)
        self.assertEqual(
            self._count_rows(r"^1\tcached\tbuggy\treached\tcomplete\t134\t\d+\tyes\t"), 1
        )
        self.assertEqual(
            self._count_rows(r"^1\tcached\tbuggy-replay\treached\tcomplete\t134\t\d+\tyes\t"),
            1,
        )
        self.assertEqual(
            self._count_rows(r"^1\tcached\tfixed\treached\tnone\t0\t\d+\tn/a\t"), 1
        )

    def test_a_recorded_seed_that_no_longer_runs_is_refused(self):
        (self.assets / ".crash-seed").write_text("1 {}\n".format(self.fixture))
        result = self._prepare("runner-failure", 1)
        self._calibrated(result)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("never executed the guest: 0 of 2 seeds", result.stdout)
        self.assertEqual(len(self._rows()), 3)
        self.assertEqual(self._count_rows(r"^1\tcached\tbuggy\tdid-not-reach\tnone\t127\t"), 1)

    def test_runs_that_never_reach_the_progress_thread_are_no_result(self):
        result = self._prepare("no-engagement", 2)
        self._assert_refused(result, "NO-RESULT: path engagement 0/2")
        self.assertEqual(len(self._rows()), 3)
        self.assertEqual(self._count_rows(r"\tdid-not-reach\t"), 2)
        self.assertEqual(len(list(self.artifacts.glob("calibration-cold-seed-*.out"))), 2)

    def test_engaged_runs_without_a_crash_are_not_called_no_result(self):
        result = self._prepare("engaged-no-hit", 2)
        self._assert_refused(result, "no ASAN UAF found", "path engagement 2/2")
        self.assertNotIn("NO-RESULT", result.stdout)
        self.assertEqual(self._count_rows(r"\treached\tnone\t"), 2)

    def test_a_partial_report_with_exit_0_does_not_qualify(self):
        result = self._prepare("partial-uaf-rc0", 2)
        self._assert_refused(result, "uaf_hits=2/2 qualified=0/2")
        self.assertEqual(self._count_rows(r"\treached\thit\t0\t\d+\tno\t"), 2)
        self.assertEqual(self._count_rows(r"\tyes\t"), 0)

    def test_a_partial_report_cut_off_by_the_timeout_does_not_qualify(self):
        result = self._prepare("partial-uaf-rc124", 2)
        self._assert_refused(result, "uaf_hits=2/2 qualified=0/2")
        self.assertEqual(self._count_rows(r"\treached\thit\t124\t\d+\tno\t"), 2)

    def test_a_complete_report_without_the_abort_does_not_qualify(self):
        """Only the exit status refuses this one: the report text is complete."""
        result = self._prepare("complete-uaf-rc0", 2)
        self._assert_refused(result, "uaf_hits=2/2 qualified=0/2")
        self.assertEqual(self._count_rows(r"\tbuggy\treached\tcomplete\t0\t\d+\tno\t"), 2)
        self.assertEqual(self._count_rows(r"\tyes\t"), 0)
        self.assertEqual(self._count_rows(r"\tbuggy-replay\t"), 0)
        self.assertEqual(self._count_rows(r"\tfixed\t"), 0)

    def test_a_calibration_budget_above_the_demo_timeout_is_refused(self):
        result = self._prepare(
            "planted-uaf",
            2,
            DEMO08_TEST_UAF_SEED="1",
            DEMO08_TIMEOUT="30",
            DEMO08_CALIBRATION_TIMEOUT="60",
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("exceeds the demo's per-run budget", result.stdout)
        self.assertFalse((self.assets / ".crash-seed").exists())

    def test_a_seed_whose_replay_times_out_is_not_recorded(self):
        result = self._prepare("replay-timeout", 2)
        self._assert_refused(
            result,
            "replayed past the {}s budget".format(self.CALIBRATION_TIMEOUT),
            "qualifying seed(s) but confirmed none",
            "unconfirmed=2",
        )
        self.assertNotIn("no ASAN UAF found", result.stdout)
        self.assertEqual(
            self._count_rows(r"\tbuggy-replay\treached\thit\t124\t\d+\tno\t"), 2
        )

    def test_a_seed_that_crashes_once_stops_the_calibration(self):
        """Two runs of one seed disagreeing is a failure, not a reason to move on."""
        result = self._prepare("replay-clean", 2)
        self._assert_refused(result, "did not on its replay")
        # Stopped at seed 0: its first run and its replay only.
        self.assertEqual(len(self._rows()), 3)

    def test_a_seed_whose_fixed_run_times_out_is_not_recorded(self):
        result = self._prepare("fixed-timeout", 1)
        self._assert_refused(
            result,
            "fixed control on seed 0 ran past the {}s budget".format(
                self.CALIBRATION_TIMEOUT
            ),
            "unconfirmed=1",
        )

    def test_a_fixed_variant_uaf_stops_the_calibration(self):
        result = self._prepare("fixed-uaf", 1)
        self._assert_refused(result, "fixed variant reported a use-after-free on seed 0")

    def test_a_uaf_without_the_progress_thread_does_not_qualify(self):
        result = self._prepare("uaf-no-engagement", 1)
        self._assert_refused(
            result, "NO-RESULT: path engagement 0/1", "engagement=0/1 uaf_hits=1/1"
        )

    def test_a_failure_to_start_the_guest_is_not_a_missing_crash(self):
        result = self._prepare("runner-failure", 1)
        self._assert_refused(result, "never executed the guest: 0 of 1 seeds")
        self.assertNotIn("no ASAN UAF found", result.stdout)

    def test_crash_text_from_a_failed_run_does_not_qualify(self):
        result = self._prepare("runner-failure-with-signatures", 1)
        self._assert_refused(result, "never executed the guest: 0 of 1 seeds")

    def test_a_refused_run_is_retried_and_the_seed_still_found(self):
        """A refusal (exit 122) is not "this seed did not crash": run it again."""
        result = self._calibrated(
            self._prepare("refused-once", 1, DEMO08_TEST_UAF_SEED="0")
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("Demo 8 crash seed calibrated: 0", result.stdout)
        self.assertIn(
            "Hermit refused the buggy run on seed 0 (rc=122, attempt 1 of 3)",
            result.stdout,
        )
        self.assertIn(
            "Hermit refused the fixed run on seed 0 (rc=122, attempt 1 of 3)",
            result.stdout,
        )
        self.assertIn("executed=1/1 unconfirmed=0 refused=0/1 refused_runs=2", result.stdout)
        self.assertEqual(
            (self.assets / ".crash-seed").read_text(), "0 {}\n".format(self.fixture)
        )
        # The two refused attempts are rows of their own, with their output kept.
        self.assertEqual(len(self._rows()), 6)
        self.assertEqual(self._count_rows(r"\t122\t\d+\trefused\t"), 2)
        self.assertEqual(
            self._count_rows(r"^0\tcold\tbuggy\treached\tcomplete\t134\t\d+\tyes\t"), 1
        )
        for name in (
            "calibration-cold-seed-0-refused-1.out",
            "calibration-confirm-fixed-seed-0-refused-1.out",
        ):
            self.assertIn(
                "HERMIT_POLICY_REFUSAL", (self.artifacts / name).read_text()
            )

    def test_seeds_refused_on_every_attempt_are_reported_as_refused(self):
        result = self._prepare("always-refused", 2)
        self._assert_refused(
            result,
            "refused by Hermit on every seed: each of the 2 attempted seeds "
            "exited 122 on all 3 attempt(s)",
            "refused=2/2 refused_runs=6",
            "seed 0 was not tested: Hermit refused every attempt",
        )
        self.assertNotIn("never executed the guest", result.stdout)
        self.assertEqual(len(self._rows()), 7)
        self.assertEqual(self._count_rows(r"\t122\t\d+\trefused\t"), 6)

    def test_refusal_retries_can_be_turned_off(self):
        result = self._prepare("always-refused", 1, DEMO08_REFUSAL_RETRIES="0")
        self._assert_refused(result, "exited 122 on all 1 attempt(s)", "refused_runs=1")
        self.assertEqual(len(self._rows()), 2)

    def test_a_replay_refused_on_every_attempt_is_not_a_disagreement(self):
        result = self._prepare("replay-refused", 1)
        self._assert_refused(
            result,
            "Hermit refused every replay attempt (rc=122), so the replay was not tested",
            "qualifying seed(s) but confirmed none",
            "1 had a replay or fixed control that Hermit refused on every attempt",
        )
        # refused= counts only seeds whose first run was refused; a refused
        # replay is counted under unconfirmed=, and refused_runs= counts every
        # refused attempt.
        self.assertIn("unconfirmed=1 refused=0/1 refused_runs=3", result.stdout)
        self.assertNotIn("did not on its replay", result.stdout)
        self.assertEqual(
            self._count_rows(r"\tbuggy-replay\treached\tnone\t122\t\d+\trefused\t"), 3
        )

    def test_a_fixed_run_refused_on_every_attempt_is_unconfirmed_not_refused(self):
        result = self._prepare("fixed-refused", 1)
        self._assert_refused(
            result,
            "fixed control on seed 0 was refused by Hermit on every attempt",
            "qualifying seed(s) but confirmed none",
            "unconfirmed=1 refused=0/1 refused_runs=3",
        )
        self.assertEqual(
            self._count_rows(r"\tfixed\treached\tnone\t122\t\d+\trefused\t"), 3
        )

    def test_a_refused_fixed_run_with_a_uaf_is_not_retried_away(self):
        """The fixed variant's use-after-free stops calibration even when refused."""
        result = self._prepare("fixed-refused-uaf", 1)
        self._assert_refused(result, "fixed variant reported a use-after-free on seed 0")
        self.assertEqual(self._count_rows(r"\tfixed\t"), 1)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Tests for how run-all.sh classifies a demo that declined to run.

A demo exits 0 when it cannot run -- demo 8 does this when its ASAN assets are
absent -- so the exit code alone cannot separate a pass from a skip. run-all.sh
reads the demo's SKIPPED line instead, records the demo as SKIP, and never
reports a skipped demo as passed. A sweep with a skip and no failure exits 3, so
a caller that reads only the exit status does not take it for a success;
--allow-skips accepts the skip (exit 0) but still reports the sweep as
INCOMPLETE.
"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RUN_ALL = ROOT / "demos/run-all.sh"

# run-all.sh invokes "$MAKE -C <demo dir> --no-print-directory <target>". These
# stubs stand in for that call, so the tests exercise the classification rather
# than a real demo.
SKIPPING_DEMO = """#!/usr/bin/env bash
echo "=== Demo 8: SKIPPED -- missing asset: /nonexistent/btrfs-convert ==="
exit 0
"""

PASSING_DEMO = """#!/usr/bin/env bash
echo "=== Demo 3: Chaos Concurrency Testing: SUCCESS ==="
exit 0
"""

FAILING_DEMO = """#!/usr/bin/env bash
echo "=== Demo 3: Chaos Concurrency Testing: FAILURE (exit 1) -- see errors above ==="
exit 1
"""

# Chooses by the make target, the last argument: demo 8 skips, demo 3 fails,
# and any other demo passes.
MIXED_DEMOS = """#!/usr/bin/env bash
case "${@: -1}" in
  demo8)
    echo "=== Demo 8: SKIPPED -- missing asset: /nonexistent/btrfs-convert ==="
    exit 0
    ;;
  demo3)
    echo "=== Demo 3: Chaos Concurrency Testing: FAILURE (exit 1) -- see errors above ==="
    exit 1
    ;;
esac
echo "=== Demo 1: Deterministic Execution: SUCCESS ==="
exit 0
"""


class RunAllSkipClassificationTest(unittest.TestCase):
    def _sweep(self, stub_body, target="demo8", args=()):
        with tempfile.TemporaryDirectory() as td:
            scratch = Path(td)
            stub = scratch / "fake-make"
            stub.write_text(stub_body)
            stub.chmod(0o755)
            log_dir = scratch / "logs"
            environment = os.environ.copy()
            environment.update(
                {
                    "MAKE": str(stub),
                    "DEMO_SWEEP_TARGETS": target,
                    "DEMO_SWEEP_LOG_DIR": str(log_dir),
                    "DEMO_TMP": str(scratch / "demo-tmp"),
                }
            )
            environment.pop("GITHUB_STEP_SUMMARY", None)
            result = subprocess.run(
                [str(RUN_ALL), *args],
                cwd=str(ROOT),
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=120,
            )
            summary = (log_dir / "summary.tsv").read_text()
            return result, summary

    @staticmethod
    def _statuses(summary):
        rows = [line.split("\t") for line in summary.strip().splitlines()[1:]]
        return [row[1] for row in rows]

    def test_a_skipped_demo_is_not_recorded_as_a_pass(self):
        result, summary = self._sweep(SKIPPING_DEMO)
        self.assertEqual(self._statuses(summary), ["SKIP"])
        # A sweep with a skipped demo is incomplete. It exits 3, which is
        # neither a pass (0) nor a failure (1), so a caller that reads only the
        # exit status does not take it for a success.
        self.assertEqual(result.returncode, 3, result.stdout)
        self.assertNotIn("PASS", summary)
        self.assertIn("SKIPPED", result.stdout)

    def test_the_headline_never_counts_a_skip_as_passed(self):
        result, _ = self._sweep(SKIPPING_DEMO)
        self.assertIn(
            "Demo suite: INCOMPLETE — 0 of 1 requested demos passed, "
            "1 skipped and unmeasured",
            result.stdout,
        )
        self.assertNotIn("all 1 requested demos passed", result.stdout)

    def test_the_skipped_demos_are_named(self):
        result, summary = self._sweep(MIXED_DEMOS, target="demo1 demo8")
        self.assertEqual(self._statuses(summary), ["PASS", "SKIP"])
        self.assertEqual(result.returncode, 3, result.stdout)
        self.assertIn(
            "Demo suite: INCOMPLETE — 1 of 2 requested demos passed, "
            "1 skipped and unmeasured",
            result.stdout,
        )
        self.assertIn("Skipped, with no result: demo8\n", result.stdout)
        self.assertNotIn("all 2 requested demos passed", result.stdout)

    def test_allow_skips_accepts_a_skip_but_still_reports_incomplete(self):
        result, summary = self._sweep(SKIPPING_DEMO, args=("--allow-skips",))
        self.assertEqual(self._statuses(summary), ["SKIP"])
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "Demo suite: INCOMPLETE — 0 of 1 requested demos passed, "
            "1 skipped and unmeasured",
            result.stdout,
        )
        self.assertIn("Skipped, with no result: demo8\n", result.stdout)
        self.assertNotIn("all 1 requested demos passed", result.stdout)

    def test_allow_skips_does_not_hide_a_failure(self):
        result, summary = self._sweep(
            MIXED_DEMOS, target="demo3 demo8", args=("--allow-skips",)
        )
        self.assertEqual(self._statuses(summary), ["FAIL", "SKIP"])
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("1 demo(s) failed, 0 passed, 1 skipped", result.stdout)
        self.assertIn("Skipped, with no result: demo8\n", result.stdout)
        self.assertNotIn("INCOMPLETE", result.stdout)

    def test_a_real_pass_is_still_a_pass(self):
        """Positive control: the classifier must not label every demo SKIP."""
        result, summary = self._sweep(PASSING_DEMO, target="demo3")
        self.assertEqual(self._statuses(summary), ["PASS"])
        self.assertEqual(result.returncode, 0)
        self.assertIn("all 1 requested demos passed", result.stdout)
        self.assertNotIn("INCOMPLETE", result.stdout)

    def test_a_genuine_failure_is_still_a_failure(self):
        """A skip classification must not swallow a real red."""
        result, summary = self._sweep(FAILING_DEMO, target="demo3")
        self.assertEqual(self._statuses(summary), ["FAIL"])
        self.assertEqual(result.returncode, 1)
        self.assertIn("1 demo(s) failed", result.stdout)

    def test_the_usage_documents_the_exit_statuses(self):
        result = subprocess.run(
            [str(RUN_ALL), "--help"],
            cwd=str(ROOT),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("--allow-skips", result.stdout)
        for line in (
            "  0  every selected demo passed",
            "  1  at least one selected demo failed",
            "  2  usage error",
            "  3  no selected demo failed, but at least one was skipped",
        ):
            with self.subTest(line=line):
                self.assertIn("\n" + line, result.stdout)


if __name__ == "__main__":
    unittest.main()

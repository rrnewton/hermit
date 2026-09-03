#!/usr/bin/env python3
"""Contract tests for how the demo sweep classifies a demo that declined to run.

A demo exits 0 when it cannot run -- demo8 does this when its ASAN assets are
absent -- so the exit code alone cannot separate a pass from a non-run. Before
this bracket, run-all.sh recorded that as PASS and printed "all N requested
demos passed", reporting evidence the sweep never produced.
"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RUN_ALL = ROOT / "demos/run-all.sh"

# run-all.sh invokes "$MAKE -C <demo dir> --no-print-directory <target>". These
# stubs stand in for that call so the bracket exercises the classifier itself
# rather than a real demo's runtime.
SKIPPING_DEMO = """#!/usr/bin/env bash
echo "=== Demo 08: SKIPPED — missing asset: /nonexistent/btrfs-convert ==="
exit 0
"""

PASSING_DEMO = """#!/usr/bin/env bash
echo "=== Demo 03: Chaos Concurrency: SUCCESS ==="
exit 0
"""

FAILING_DEMO = """#!/usr/bin/env bash
echo "=== Demo 03: Chaos Concurrency: FAILURE ==="
exit 1
"""


class RunAllSkipClassificationTest(unittest.TestCase):
    def _sweep(self, stub_body, target="demo8"):
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
                [str(RUN_ALL)],
                cwd=str(ROOT),
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=120,
            )
            summary = (log_dir / "summary.tsv").read_text()
            return result, summary

    def test_a_skipped_demo_is_not_recorded_as_a_pass(self):
        result, summary = self._sweep(SKIPPING_DEMO)
        rows = [line.split("\t") for line in summary.strip().splitlines()[1:]]
        self.assertEqual([row[1] for row in rows], ["SKIP"])
        # The exit code stays 0: declining to run is not a failure. What must
        # not survive is the claim that the demo passed.
        self.assertEqual(result.returncode, 0)
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

    def test_a_real_pass_is_still_a_pass(self):
        """Positive control: the classifier must not label every demo SKIP."""
        result, summary = self._sweep(PASSING_DEMO, target="demo3")
        rows = [line.split("\t") for line in summary.strip().splitlines()[1:]]
        self.assertEqual([row[1] for row in rows], ["PASS"])
        self.assertEqual(result.returncode, 0)
        self.assertIn("all 1 requested demos passed", result.stdout)
        self.assertNotIn("INCOMPLETE", result.stdout)

    def test_a_genuine_failure_is_still_a_failure(self):
        """A skip classification must not swallow a real red."""
        result, summary = self._sweep(FAILING_DEMO, target="demo3")
        rows = [line.split("\t") for line in summary.strip().splitlines()[1:]]
        self.assertEqual([row[1] for row in rows], ["FAIL"])
        self.assertEqual(result.returncode, 1)
        self.assertIn("1 demo(s) failed", result.stdout)


if __name__ == "__main__":
    unittest.main()

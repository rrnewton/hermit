#!/usr/bin/env python3
"""Tests for how run-all.sh classifies a demo that declined to run.

A demo exits 0 when it cannot run -- demo 8 does this when its ASAN assets are
absent -- so the exit code alone cannot separate a pass from a skip. run-all.sh
reads the demo's SKIPPED line instead, records the demo as SKIP, and never
reports a skipped demo as passed. A sweep with a skip and no failure exits 3, so
a caller that reads only the exit status does not take it for a success. There
is no option that accepts a skip: a caller that accepts one checks for exit
status 3 itself.

Because the SKIPPED line is read from the demo's log, a log directory, log, or
summary that cannot be created, written, or read makes the sweep exit 4. An
unread log must never let a skip count as a pass.
"""

import collections
import os
import shlex
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

# Skips, and removes its own log while tee still has it open, so the log
# cannot be read once the demo has finished. tee creates the log when it
# starts, which can be a moment after this stub starts, so wait for it. If the
# log never appears, or survives the removal, exit 99: the sweep then records a
# failure, and a test that expects exit 4 fails rather than passing by accident.
SKIPPING_DEMO_WITH_AN_UNREADABLE_LOG = """#!/usr/bin/env bash
log="$DEMO_SWEEP_LOG_DIR/${@: -1}.log"
echo "=== Demo 8: SKIPPED -- missing asset: /nonexistent/btrfs-convert ==="
for _ in $(seq 600); do
  [ -e "$log" ] && break
  sleep 0.1
done
[ -e "$log" ] || exit 99
rm -f -- "$log"
[ ! -e "$log" ] || exit 99
exit 0
"""

# Passes, and replaces summary.tsv with a directory, so that the sweep cannot
# append this demo's row.
PASSING_DEMO_THAT_BLOCKS_THE_SUMMARY = """#!/usr/bin/env bash
summary="$DEMO_SWEEP_LOG_DIR/summary.tsv"
rm -f -- "$summary"
mkdir -- "$summary" || exit 99
echo "=== Demo 3: Chaos Concurrency Testing: SUCCESS ==="
exit 0
"""

Sweep = collections.namedtuple("Sweep", "result summary calls")


class RunAllSkipClassificationTest(unittest.TestCase):
    def _run(self, stub_body, target="demo8", args=(), prepare=None):
        """Run the sweep against a stub make.

        prepare(scratch, environment), if given, runs before the sweep and may
        change the scratch directory or the environment. Returns the completed
        process, the text of summary.tsv (None if it is not a readable file),
        and the make targets that the stub was called with, in order.
        """
        with tempfile.TemporaryDirectory() as td:
            scratch = Path(td)
            body = scratch / "demo-stub"
            body.write_text(stub_body)
            body.chmod(0o755)
            calls = scratch / "make-calls"
            stub = scratch / "fake-make"
            stub.write_text(
                "#!/usr/bin/env bash\n"
                f"printf '%s\\n' \"${{@: -1}}\" >>{shlex.quote(str(calls))}\n"
                f'exec {shlex.quote(str(body))} "$@"\n'
            )
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
            environment.pop("GITHUB_ACTIONS", None)
            if prepare is not None:
                prepare(scratch, environment)
            result = subprocess.run(
                [str(RUN_ALL), *args],
                cwd=str(ROOT),
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=120,
            )
            summary_path = Path(environment["DEMO_SWEEP_LOG_DIR"]) / "summary.tsv"
            summary = summary_path.read_text() if summary_path.is_file() else None
            made = calls.read_text().split() if calls.exists() else []
            return Sweep(result, summary, made)

    def _sweep(self, stub_body, target="demo8", args=()):
        sweep = self._run(stub_body, target=target, args=args)
        self.assertIsNotNone(sweep.summary, sweep.result.stdout)
        return sweep.result, sweep.summary

    @staticmethod
    def _statuses(summary):
        rows = [line.split("\t") for line in summary.strip().splitlines()[1:]]
        return [row[1] for row in rows]

    def assertNeverAPassOrSuccess(self, sweep):
        output = sweep.result.stdout
        self.assertNotIn(": PASS", output)
        self.assertNotIn("Demo suite: SUCCESS", output)
        if sweep.summary is not None:
            self.assertNotIn("PASS", self._statuses(sweep.summary))

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

    def test_there_is_no_option_that_accepts_a_skip(self):
        """--allow-skips was removed: it is refused as a usage error."""
        sweep = self._run(SKIPPING_DEMO, args=("--allow-skips",))
        self.assertEqual(sweep.result.returncode, 2, sweep.result.stdout)
        self.assertIn("Usage: demos/run-all.sh", sweep.result.stdout)
        self.assertEqual(sweep.calls, [], "no demo may run after a usage error")
        self.assertNotIn("INCOMPLETE", sweep.result.stdout)

    def test_the_removed_option_is_refused_even_with_a_failure(self):
        sweep = self._run(
            MIXED_DEMOS, target="demo3 demo8", args=("--allow-skips",)
        )
        self.assertEqual(sweep.result.returncode, 2, sweep.result.stdout)
        self.assertIn("Usage: demos/run-all.sh", sweep.result.stdout)
        self.assertEqual(sweep.calls, [], "no demo may run after a usage error")

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

    def test_a_log_directory_that_cannot_be_created_stops_the_sweep(self):
        def log_dir_below_a_file(scratch, environment):
            blocker = scratch / "a-regular-file"
            blocker.write_text("")
            environment["DEMO_SWEEP_LOG_DIR"] = str(blocker / "logs")

        sweep = self._run(SKIPPING_DEMO, prepare=log_dir_below_a_file)
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertEqual(sweep.calls, [], "no demo may run without its log")
        self.assertIn("cannot create the log directory", sweep.result.stdout)
        self.assertIn("Set DEMO_SWEEP_LOG_DIR", sweep.result.stdout)
        self.assertIn("Demo suite: ERROR — no demo was run", sweep.result.stdout)
        self.assertNeverAPassOrSuccess(sweep)

    def test_a_summary_that_cannot_be_written_stops_the_sweep(self):
        def summary_is_a_directory(scratch, environment):
            (Path(environment["DEMO_SWEEP_LOG_DIR"]) / "summary.tsv").mkdir(
                parents=True
            )

        sweep = self._run(SKIPPING_DEMO, prepare=summary_is_a_directory)
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertEqual(sweep.calls, [], "no demo may run without a summary")
        self.assertIn("cannot write the summary", sweep.result.stdout)
        self.assertIn("Demo suite: ERROR — no demo was run", sweep.result.stdout)
        self.assertNeverAPassOrSuccess(sweep)

    def test_a_log_that_cannot_be_written_is_an_error_not_a_pass(self):
        def log_is_a_directory(scratch, environment):
            (Path(environment["DEMO_SWEEP_LOG_DIR"]) / "demo8.log").mkdir(
                parents=True
            )

        sweep = self._run(SKIPPING_DEMO, prepare=log_is_a_directory)
        self.assertEqual(sweep.calls, ["demo8"])
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["ERROR"])
        self.assertIn("tee could not write the log", sweep.result.stdout)
        self.assertIn(
            "Demo suite: ERROR — 0 of 1 requested demos passed, 0 skipped, "
            "1 unknown",
            sweep.result.stdout,
        )
        self.assertIn(
            "Unknown, because the log could not be read: demo8\n",
            sweep.result.stdout,
        )
        self.assertNeverAPassOrSuccess(sweep)
        self.assertNotIn("INCOMPLETE", sweep.result.stdout)

    def test_a_skip_whose_log_cannot_be_read_is_never_a_pass(self):
        sweep = self._run(SKIPPING_DEMO_WITH_AN_UNREADABLE_LOG)
        self.assertEqual(sweep.calls, ["demo8"])
        # The demo really did print its SKIPPED line (tee copied it here), so
        # a PASS would hide the skip.
        self.assertIn("=== Demo 8: SKIPPED", sweep.result.stdout)
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["ERROR"])
        self.assertIn("grep could not read the log", sweep.result.stdout)
        self.assertIn(
            "Unknown, because the log could not be read: demo8\n",
            sweep.result.stdout,
        )
        self.assertNeverAPassOrSuccess(sweep)

    def test_an_unreadable_log_does_not_hide_a_failure(self):
        def log_is_a_directory(scratch, environment):
            (Path(environment["DEMO_SWEEP_LOG_DIR"]) / "demo3.log").mkdir(
                parents=True
            )

        sweep = self._run(
            MIXED_DEMOS, target="demo3 demo8", prepare=log_is_a_directory
        )
        self.assertEqual(sweep.result.returncode, 1, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["FAIL", "SKIP"])
        self.assertIn("1 demo(s) failed, 0 passed, 1 skipped", sweep.result.stdout)
        self.assertIn("tee could not write the log", sweep.result.stdout)

    def test_a_summary_row_that_cannot_be_appended_is_an_error(self):
        sweep = self._run(PASSING_DEMO_THAT_BLOCKS_THE_SUMMARY, target="demo3")
        self.assertEqual(sweep.calls, ["demo3"])
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertIn("could not append its row to the summary", sweep.result.stdout)
        self.assertIn(
            "Demo suite: ERROR — 1 of 1 requested demos passed, 0 skipped, "
            "0 unknown",
            sweep.result.stdout,
        )
        self.assertNotIn("Demo suite: SUCCESS", sweep.result.stdout)

    def test_a_github_step_summary_that_cannot_be_written_is_an_error(self):
        def step_summary_is_a_directory(scratch, environment):
            step_summary = scratch / "step-summary"
            step_summary.mkdir()
            environment["GITHUB_STEP_SUMMARY"] = str(step_summary)

        sweep = self._run(
            PASSING_DEMO, target="demo3", prepare=step_summary_is_a_directory
        )
        self.assertEqual(sweep.result.returncode, 4, sweep.result.stdout)
        self.assertEqual(self._statuses(sweep.summary), ["PASS"])
        self.assertIn("could not write the GitHub step summary", sweep.result.stdout)
        self.assertNotIn("Demo suite: SUCCESS", sweep.result.stdout)

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
        self.assertNotIn("--allow-skips", result.stdout)
        for line in (
            "  0  every selected demo passed\n",
            "  1  at least one selected demo failed\n",
            "  2  usage error\n",
            "  3  no selected demo failed, but at least one was skipped\n",
            "  4  no selected demo failed, but the sweep could not create, write, or read\n",
            "When more than one applies, 1 comes before 4, and 4 before 3.",
        ):
            with self.subTest(line=line):
                self.assertIn("\n" + line, result.stdout)


if __name__ == "__main__":
    unittest.main()

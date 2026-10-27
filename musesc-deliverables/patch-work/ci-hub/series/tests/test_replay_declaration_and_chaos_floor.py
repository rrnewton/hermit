"""The series projection mirrors two Hermit reader rules (hermit b94f9215, 85d9441a).

* A replay cell inherits its verify recipe's ``expected_guest_exit``, so a
  result row may carry it in verify or replay mode
  (``runner::validate_result_expected_guest_exit``), and a declared exit is
  credited only from the invocation the runner writes for it
  (``runner::invocation_admits_declared_guest_exit``).
* A chaos cell whose population floor failed over attempts that all matched
  is ``chaos-floor-failure`` (errored, product_failure), never crash-error.
"""

import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import series  # noqa: E402


def _declared_row(mode):
    return {
        "mode": mode,
        "expected_guest_exit": {"code": 7, "signal": None, "reason": "the fixture exits 7"},
    }


class ReplayRowCarriesInheritedDeclaration(unittest.TestCase):
    def test_verify_and_replay_rows_may_carry_a_declaration(self):
        for mode in ("verify", "replay"):
            self.assertEqual(series._expected_guest_exit(_declared_row(mode)), (7, None), mode)

    def test_other_modes_refuse_a_declaration(self):
        for mode in ("chaos", "naked"):
            with self.assertRaisesRegex(series.MalformedRow, "verify or replay"):
                series._expected_guest_exit(_declared_row(mode))

    def test_a_zero_code_is_still_refused_on_a_replay_row(self):
        row = _declared_row("replay")
        row["expected_guest_exit"]["code"] = 0
        with self.assertRaises(series.MalformedRow):
            series._expected_guest_exit(row)

    def test_invocation_rule_matches_hermits(self):
        verify = ["hermit", "run", "--verify-allow=failure"]
        replay = [
            "hermit-verify-pair", "hermit", "log-diff", "--verify-allow=both",
            "hermit", "record", "start",
        ]
        admits = series._invocation_admits_declared_guest_exit
        self.assertTrue(admits("verify", verify))
        self.assertTrue(admits("replay", replay))
        self.assertFalse(admits("replay", verify))
        self.assertFalse(admits("verify", replay))
        self.assertFalse(
            admits("replay", ["hermit", "log-diff", "--verify-allow=both", "hermit", "run"])
        )
        self.assertFalse(admits("chaos", replay))


class ReplayDeclaredStdout(unittest.TestCase):
    """A replay attempt's failed declared stdout is judged, not refused for its mode.

    Mirrors the refused pressure-survey inputs of 2026-10-26/27
    (``c-programs/syscall-file-metadata/replay/ptrace`` and siblings): the
    runner inherits the verify recipe's ``expected_stdout`` into replay
    (``runner::expectation_recipe``) and writes this exact reason, which the
    projection used to refuse with "only a verify cell declares".
    """

    EMPTY_SHA256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"

    def _attempt(self):
        return {
            "reason": (
                f"verify first run stdout (0 bytes, sha256 {self.EMPTY_SHA256}) differs "
                f"from the declared expected_stdout (3 bytes, sha256 {'a' * 64})"
            ),
            "status": 0,
            "signal": None,
            "timed_out": False,
            "error_kind": None,
        }

    def _report(self):
        side = {
            "exit_code": 0,
            "signal": None,
            "stdout_bytes": 0,
            "stdout_sha256": self.EMPTY_SHA256,
            "stderr_bytes": 0,
            "stderr_sha256": self.EMPTY_SHA256,
        }
        return {"compared_outputs": {"left": dict(side), "right": dict(side)}}

    def test_replay_and_verify_reasons_are_recognised(self):
        for mode in ("verify", "replay"):
            attempt = self._attempt()
            self.assertEqual(
                series._expected_output_failure_reason(attempt, self._report(), mode=mode, index=1),
                attempt["reason"],
                mode,
            )

    def test_other_modes_still_refuse_the_reason(self):
        with self.assertRaisesRegex(
            series.MalformedRow, "only a verify or replay cell declares expected stdout"
        ):
            series._expected_output_failure_reason(
                self._attempt(), self._report(), mode="chaos", index=1
            )


class ReplayDeclaredGuestExitDisposition(unittest.TestCase):
    """A replay PASS that ends as its inherited declaration allows is ``matched``.

    Mirrors the refused 2026-10-25 resurvey inputs (``qemu-hello`` code 7,
    ``sh-exit-status`` code 23, ``pmu-skid`` code 3, ``nanosleep-threads-simple``
    signal 11, all ``replay/ptrace``): the projection credited the declaration
    only in verify mode, so every such replay pass was refused as an
    internally inconsistent matched report.
    """

    def _attempt(self):
        import hashlib
        import json

        side = {
            "exit_code": 7,
            "signal": None,
            "stdout_bytes": 0,
            "stdout_sha256": ReplayDeclaredStdout.EMPTY_SHA256,
            "stderr_bytes": 0,
            "stderr_sha256": ReplayDeclaredStdout.EMPTY_SHA256,
        }
        report = {
            "verified": True,
            "bitwise_parity": True,
            "verdict": "matched",
            "no_result_reason": None,
            "infrastructure_error": None,
            "comparison": {
                "strictness": "canonical",
                "display_name": "BitwiseInfoV1",
                "compare_logs": True,
                "compare_io_buffers": True,
                "log_scope": "info",
                "record_envelope": "all_records_v1",
                "virtualize_time": False,
                "strip_lines": False,
                "canonicalize_addresses": True,
                "full_trace": True,
                "exact_remainder": True,
                "stripped_prefixes": ["real-wall-clock-prefix/v1"],
                "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
                "ignore_lines": False,
                "skip_commit": False,
                "skip_detlog": False,
            },
            "compared_log_messages": {"left": 17, "right": 17},
            "compared_outputs": {"left": dict(side), "right": dict(side)},
            "runtime": None,
            "guest_exit_code": 7,
            "guest_signal": None,
            "first_divergent_scheduler_turn": None,
            "first_divergent_virtual_nanoseconds": None,
            "first_divergent_record": None,
            "first_divergent_syscall": None,
            "first_divergent_left_message": None,
            "first_divergent_right_message": None,
        }
        raw = json.dumps(report, sort_keys=True, separators=(",", ":"))
        return {
            "outcome": "PASS",
            "error_kind": None,
            "status": 7,
            "signal": None,
            "timed_out": False,
            "argv": [
                "hermit-verify-pair", "hermit", "log-diff", "--verify-allow=both",
                "hermit", "record", "start", "--", "/bin/true",
            ],
            "verification_report": raw,
            "verification_report_sha256": hashlib.sha256(raw.encode()).hexdigest(),
        }

    def test_replay_declared_exit_pass_is_matched(self):
        self.assertEqual(
            series._typed_comparison_attempt(
                self._attempt(), mode="replay", index=1, expected_guest_exit=(7, None)
            ),
            "matched",
        )


class ChaosFloorFailure(unittest.TestCase):
    """``_cell_outcome`` projects and polices the named chaos floor failure."""

    def _row(self, *, mode="chaos", result="chaos-floor-failure"):
        return {
            "mode": mode,
            "result": result,
            "failure_class": "product_failure",
            "outcome": "FAIL",
            "attempts": [{"index": 1}, {"index": 2}],
        }

    def _project(self, row, dispositions):
        with mock.patch.object(series, "_typed_comparison_attempt", side_effect=dispositions), \
                mock.patch.object(series, "_declares_stripped_comparator", return_value=False), \
                mock.patch.object(series, "_ineligible_execution_path", return_value=False):
            return series._cell_outcome(row)

    def test_all_matched_attempts_project_errored(self):
        self.assertEqual(self._project(self._row(), ["matched", "matched"]), "errored")

    def test_a_non_matched_attempt_is_refused(self):
        with self.assertRaisesRegex(series.MalformedRow, "every attempt to be a match"):
            self._project(self._row(), ["matched", "diverged"])

    def test_it_is_refused_outside_chaos_mode(self):
        with self.assertRaisesRegex(series.MalformedRow, "only on a chaos cell"):
            self._project(self._row(mode="verify"), ["matched", "matched"])


if __name__ == "__main__":
    unittest.main()

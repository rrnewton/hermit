#!/usr/bin/env python3
"""Both directions of the transport-versus-verdict distinction in the adapter.

The defect this covers, measured 2026-09-04 on devbig014: a GitHub ``HTTP 504``
while fetching the pinned check-status authority made ``make lint-checks`` exit
2, which the validation DAG recorded as ``check.lint_checks`` FAILED. An outage
was reported as a code defect on every lane consulting that authority.

⚠️ BOTH DIRECTIONS ARE REQUIRED AND THE SECOND IS THE ONE THAT GETS SKIPPED. A
checker that stops saying no is worse than one that says no wrongly, because the
first failure is silent.
"""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys
import unittest


SCRIPTS = Path(__file__).resolve().parent
ADAPTER = SCRIPTS / "check_outcome_adapter.py"
sys.path.insert(0, str(SCRIPTS))

import check_outcome_adapter as adapter  # noqa: E402


def _run(args: list[str], *, env: dict[str, str] | None = None):
    merged = dict(os.environ)
    if env:
        merged.update(env)
    return subprocess.run(
        [sys.executable, str(ADAPTER), *args],
        capture_output=True,
        text=True,
        check=False,
        env=merged,
    )


def _unreachable_env(tmp: Path) -> dict[str, str]:
    """Make every authority source unavailable without breaking anything else.

    ``DEV_HERMIT_PARENT`` pointing at an empty directory removes the local
    candidate (``_candidate_authorities`` returns exactly that one path when the
    variable is set), and an empty ``PATH`` removes ``gh`` and ``with-proxy`` so
    the fetch cannot start. That is a simulated unreachable authority, not a
    simulated wrong answer.
    """
    return {"DEV_HERMIT_PARENT": str(tmp), "PATH": ""}


class AuthorityUnavailableIsNotAVerdict(unittest.TestCase):
    def test_unreachable_authority_yields_could_not_determine(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            result = _run(
                ["--status", "completed", "--conclusion", "failure"],
                env=_unreachable_env(Path(tmp)),
            )

        self.assertEqual(
            result.returncode,
            adapter.EXIT_AUTHORITY_UNAVAILABLE,
            f"stdout={result.stdout!r} stderr={result.stderr!r}",
        )
        self.assertIn("COULD-NOT-DETERMINE", result.stderr)
        self.assertIn("NO SIGNAL", result.stderr)

    def test_unreachable_authority_writes_no_verdict_to_stdout(self) -> None:
        """The distinction dies if a caller can read a token off stdout."""
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            result = _run(
                ["--status", "completed", "--conclusion", "failure"],
                env=_unreachable_env(Path(tmp)),
            )

        self.assertEqual(result.stdout, "")
        for verdict in ("PASSED", "FAILED", "NO_RESULT"):
            self.assertNotIn(verdict, result.stdout)

    def test_the_two_failure_kinds_are_different_types(self) -> None:
        """A digest mismatch is a refusal and must not be caught as an outage."""
        self.assertTrue(issubclass(adapter.AuthorityUnavailable, RuntimeError))
        self.assertTrue(issubclass(adapter.AuthorityIntegrityError, RuntimeError))
        self.assertFalse(
            issubclass(adapter.AuthorityIntegrityError, adapter.AuthorityUnavailable)
        )
        self.assertFalse(
            issubclass(adapter.AuthorityUnavailable, adapter.AuthorityIntegrityError)
        )

    def test_exit_status_does_not_collide_with_usage_or_ordinary_error(self) -> None:
        self.assertNotIn(adapter.EXIT_AUTHORITY_UNAVAILABLE, (0, 1, 2))


class AReachableAuthorityStillSaysNo(unittest.TestCase):
    """⚠️ THE DIRECTION THAT GETS SKIPPED. Without this the fix could be a
    checker that never says no, which is strictly worse than the bug."""

    def test_reachable_authority_still_returns_failed(self) -> None:
        result = _run(["--status", "completed", "--conclusion", "failure"])
        if result.returncode == adapter.EXIT_AUTHORITY_UNAVAILABLE:
            self.skipTest(
                "authority genuinely unreachable here; this direction needs a "
                "reachable authority and must not be asserted without one"
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "FAILED")

    def test_reachable_authority_still_returns_passed_and_no_result(self) -> None:
        passed = _run(["--status", "completed", "--conclusion", "success"])
        if passed.returncode == adapter.EXIT_AUTHORITY_UNAVAILABLE:
            self.skipTest("authority genuinely unreachable here")
        self.assertEqual(passed.stdout.strip(), "PASSED")

        cancelled = _run(["--status", "completed", "--conclusion", "cancelled"])
        self.assertEqual(cancelled.returncode, 0, cancelled.stderr)
        self.assertEqual(cancelled.stdout.strip(), "NO_RESULT")


if __name__ == "__main__":
    unittest.main()

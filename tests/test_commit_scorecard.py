#!/usr/bin/env python3
"""Focused brackets for the receipt-sourced commit scorecard."""

from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/commit-scorecard.py"
BACKENDS = ["ptrace", "kvm", "liteinst", "e9patch", "sabre", "dbt"]


def run(
    cwd: Path, *args: str, input_text: str | None = None, check: bool = True
) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(
        args, cwd=cwd, input=input_text, text=True, capture_output=True, check=False
    )
    if check and proc.returncode:
        raise AssertionError(f"{' '.join(args)} failed\nstdout={proc.stdout}\nstderr={proc.stderr}")
    return proc


def receipt(commit: str, executed: int = 10, filtered: int = 3, failures: int = 0) -> dict:
    return {
        "repo": "hermit",
        "record_id": f"record-{commit[:8]}",
        "commit": commit,
        "producer": "hermit-validate-rs",
        "profile": "full",
        "selection_mode": "full",
        "result": "pass",
        "raw_result": "pass",
        "tree_dirty": False,
        "commit_anchored": True,
        "executed_tests": executed,
        "filtered_tests": filtered,
        "failures": failures,
        "checks": 7,
        "coverage": {
            "planned_test_nodes": 2,
            "executed_test_nodes": 2,
            "absent_nodes": [],
            "zero_executed_nodes": [],
        },
    }


class Repository:
    def __init__(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        run(self.root, "git", "init", "-q")
        run(self.root, "git", "config", "user.email", "scorecard@example.invalid")
        run(self.root, "git", "config", "user.name", "Scorecard Test")
        (self.root / "scripts").mkdir()
        (self.root / "ci/compat").mkdir(parents=True)
        shutil.copy2(SCRIPT, self.root / "scripts/commit-scorecard.py")
        (self.root / "ci/compat/scorecard-backends.json").write_text(
            json.dumps({"backends": BACKENDS}) + "\n"
        )

    def close(self) -> None:
        self.temp.cleanup()

    def import_row(self, row: dict, drop_reason: str = "") -> subprocess.CompletedProcess[str]:
        args = [
            "python3",
            "scripts/commit-scorecard.py",
            "import-receipt",
            "--output",
            "ci/compat/commit-scorecard-receipt.json",
        ]
        if drop_reason:
            args += ["--drop-reason", drop_reason]
        return run(self.root, *args, input_text=json.dumps(row, separators=(",", ":")), check=False)

    def stage(self) -> None:
        run(self.root, "git", "add", "scripts", "ci")

    def render(self, check: bool = True) -> subprocess.CompletedProcess[str]:
        self.stage()
        return run(self.root, "python3", "scripts/commit-scorecard.py", "render", check=check)

    def commit(self, subject: str) -> str:
        self.stage()
        message = self.root / "message"
        message.write_text(subject + "\n")
        run(self.root, "python3", "scripts/commit-scorecard.py", "insert", str(message))
        run(self.root, "git", "commit", "-q", "--no-verify", "-F", str(message))
        return run(self.root, "git", "rev-parse", "HEAD").stdout.strip()


class ScorecardTests(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = Repository()
        self.first = "1" * 40
        accepted = self.repo.import_row(receipt(self.first))
        self.assertEqual(0, accepted.returncode, accepted.stderr)

    def tearDown(self) -> None:
        self.repo.close()

    def test_total_comes_only_from_receipt(self) -> None:
        rendered = self.repo.render().stdout
        self.assertIn("| TOTAL |", rendered)
        self.assertIn("| 10 | 0 | 0 | 3 | 13 |", rendered)
        self.assertIn("Backend split: unavailable in this receipt", rendered)
        for backend in BACKENDS:
            row = next(line for line in rendered.splitlines() if line.startswith(f"| {backend} |"))
            self.assertIn("| — | — | — | — | — |", row)

    def test_growth_does_not_read_as_green_drop(self) -> None:
        self.repo.commit("baseline")
        second = "2" * 40
        self.assertEqual(0, self.repo.import_row(receipt(second, executed=10, filtered=5)).returncode)
        rendered = self.repo.render().stdout
        self.assertIn("Matrix change: +2; GREEN change: +0", rendered)
        self.assertIn("GREEN DROP: none", rendered)

    def test_nonzero_failures_refuse_uninvented_classification(self) -> None:
        refused = self.repo.import_row(receipt(self.first, failures=1))
        self.assertEqual(1, refused.returncode)
        self.assertIn("does not split them into STABLE FAIL and UNSTABLE", refused.stderr)

    def test_digest_tamper_refuses(self) -> None:
        self.repo.stage()
        path = self.repo.root / "ci/compat/commit-scorecard-receipt.json"
        wrapper = json.loads(path.read_text())
        wrapper["canonical_receipt"] += " "
        path.write_text(json.dumps(wrapper) + "\n")
        refused = self.repo.render(check=False)
        self.assertEqual(1, refused.returncode)
        self.assertIn("digest mismatch", refused.stderr)

    def test_range_catches_hook_bypass(self) -> None:
        baseline = self.repo.commit("baseline")
        (self.repo.root / "unrelated").write_text("x\n")
        run(self.repo.root, "git", "add", "unrelated")
        run(self.repo.root, "git", "commit", "-q", "--no-verify", "-m", "missing")
        refused = run(
            self.repo.root,
            "python3",
            "scripts/commit-scorecard.py",
            "check-range",
            "--base",
            baseline,
            check=False,
        )
        self.assertEqual(1, refused.returncode)
        self.assertIn("exactly one compatibility scorecard", refused.stderr)

    def test_scorecard_only_child_is_exact(self) -> None:
        parent = self.repo.commit("validated parent")
        self.assertEqual(0, self.repo.import_row(receipt(parent, executed=11, filtered=2)).returncode)
        child = self.repo.commit("receipt child")
        accepted = run(
            self.repo.root,
            "python3",
            "scripts/commit-scorecard.py",
            "check-scorecard-only-child",
            "--validated-parent",
            parent,
            "--candidate",
            child,
        )
        self.assertIn("inherits exact-parent green", accepted.stdout)

    def test_backend_order_is_fail_closed(self) -> None:
        self.repo.stage()
        path = self.repo.root / "ci/compat/scorecard-backends.json"
        path.write_text(json.dumps({"backends": list(reversed(BACKENDS))}) + "\n")
        refused = self.repo.render(check=False)
        self.assertEqual(1, refused.returncode)
        self.assertIn("owner-specified backend order", refused.stderr)


if __name__ == "__main__":
    unittest.main()

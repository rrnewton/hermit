#!/usr/bin/env python3
"""Focused brackets for scripts/commit-scorecard.py."""

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


def run(cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(args, cwd=cwd, text=True, capture_output=True, check=False)
    if check and proc.returncode:
        raise AssertionError(f"{' '.join(args)} failed\nstdout={proc.stdout}\nstderr={proc.stderr}")
    return proc


class Repository:
    def __init__(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        run(self.root, "git", "init", "-q")
        run(self.root, "git", "config", "user.email", "scorecard@example.invalid")
        run(self.root, "git", "config", "user.name", "Scorecard Test")
        (self.root / "scripts").mkdir()
        (self.root / "ci/compat").mkdir(parents=True)
        (self.root / "tests/e2e/manifests").mkdir(parents=True)
        shutil.copy2(SCRIPT, self.root / "scripts/commit-scorecard.py")
        self.write_json(
            "ci/compat/scorecard-backends.json",
            {
                "population_id": "hermit-e2e-manifests-v2",
                "standard_id": "strict-canonical-bitwise",
                "backends": BACKENDS,
            },
        )
        self.write_manifest(["program/a", "program/b"])
        self.write_results()

    def close(self) -> None:
        self.temp.cleanup()

    def write_json(self, path: str, value: dict) -> None:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(value, indent=2) + "\n")

    def write_manifest(self, ids: list[str]) -> None:
        lines = ["schema = 2", 'bucket = "sample"', ""]
        for test_id in ids:
            lines.extend(["[[test]]", f'id = "{test_id}"', ""])
        (self.root / "tests/e2e/manifests/sample.toml").write_text("\n".join(lines))

    def write_results(
        self,
        *,
        population_id: str = "hermit-e2e-manifests-v2",
        green: list[str] | None = None,
        drop_reason: str = "",
        measured_sha: str = "historical",
    ) -> None:
        green = green or []
        states = {
            backend: {"green": green, "stable_fail": [], "unstable": []}
            for backend in BACKENDS
        }
        self.write_json(
            "ci/compat/commit-scorecard-results.json",
            {
                "schema": 1,
                "population_id": population_id,
                "standard_id": "strict-canonical-bitwise",
                "measured_hermit_sha": measured_sha,
                "evidence": {},
                "states": states,
                "drop_reason": drop_reason,
            },
        )

    def stage(self) -> None:
        run(self.root, "git", "add", "scripts", "ci", "tests")

    def render(self, check: bool = True) -> subprocess.CompletedProcess[str]:
        self.stage()
        return run(self.root, "python3", "scripts/commit-scorecard.py", "render", check=check)

    def commit(self, subject: str = "test") -> str:
        self.stage()
        message = self.root / "message"
        message.write_text(subject + "\n")
        run(self.root, "python3", "scripts/commit-scorecard.py", "insert", str(message))
        run(self.root, "git", "commit", "-q", "--no-verify", "-F", str(message))
        return run(self.root, "git", "rev-parse", "HEAD").stdout.strip()


class CommitScorecardTests(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = Repository()

    def tearDown(self) -> None:
        self.repo.close()

    def test_denominator_and_no_verdict_are_derived(self) -> None:
        rendered = self.repo.render().stdout
        self.assertIn("2 programs x 6 backends = 12 cells", rendered)
        self.assertIn("| ptrace |", rendered)
        self.assertIn("| 0 | 0 | 0 | 2 | 2 |", rendered)
        ptrace_row = next(line for line in rendered.splitlines() if line.startswith("| ptrace |"))
        source_hash = ptrace_row.split("|")[4].strip()
        self.assertEqual(64, len(source_hash))
        for backend in BACKENDS:
            row = next(line for line in rendered.splitlines() if line.startswith(f"| {backend} |"))
            self.assertIn(source_hash, row)
            self.assertIn("hermit-e2e-manifests-v2", row)

    def test_corpus_growth_is_separate_from_green_change(self) -> None:
        self.repo.commit("baseline")
        self.repo.write_manifest(["program/a", "program/b", "program/c"])
        rendered = self.repo.render().stdout
        self.assertIn("3 programs x 6 backends = 18 cells", rendered)
        self.assertIn("program change +1, cell change +6", rendered)
        self.assertIn("GREEN change: +0; no drop.", rendered)
        self.assertIn("| 0 | 0 | 0 | 3 | 3 | +0 | +1 |", rendered)

    def test_unknown_population_cannot_replace_canonical_population(self) -> None:
        self.repo.write_results(population_id="external-program-corpus")
        refused = self.repo.render(check=False)
        self.assertEqual(1, refused.returncode)
        self.assertIn("does not name the tracked corpus definition", refused.stderr)

    def test_missing_and_altered_tables_refuse(self) -> None:
        commit = self.repo.commit("baseline")
        run(
            self.repo.root,
            "python3",
            "scripts/commit-scorecard.py",
            "check-commit",
            commit,
        )
        message = run(self.repo.root, "git", "show", "-s", "--format=%B", commit).stdout
        altered = self.repo.root / "altered"
        altered.write_text(message.replace("NO VERDICT", "STABLE FAIL", 1))
        refused = run(
            self.repo.root,
            "python3",
            "scripts/commit-scorecard.py",
            "check-message",
            str(altered),
            check=False,
        )
        self.assertEqual(1, refused.returncode)
        self.assertIn("missing, stale, or altered", refused.stderr)

    def test_insert_keeps_task_and_attribution_at_end(self) -> None:
        self.repo.stage()
        message = self.repo.root / "message"
        attribution = "[impl agent, gpt-5.6-sol] [hermit2, devbig014]"
        message.write_text("subject\n\nbody\n\nTask: example\n\n" + attribution + "\n")
        run(self.repo.root, "python3", "scripts/commit-scorecard.py", "insert", str(message))
        result = message.read_text()
        self.assertTrue(result.endswith("Task: example\n\n" + attribution + "\n"))
        self.assertLess(result.index("Compatibility scorecard"), result.index("Task: example"))

    def test_green_drop_requires_reason_and_is_explicit(self) -> None:
        self.repo.write_results(green=["program/a"])
        self.repo.commit("one green")
        self.repo.write_results(green=[])
        refused = self.repo.render(check=False)
        self.assertEqual(1, refused.returncode)
        self.assertIn("GREEN decreased but drop_reason is empty", refused.stderr)
        self.repo.write_results(green=[], drop_reason="strict evidence was withdrawn")
        rendered = self.repo.render().stdout
        self.assertIn("GREEN DROP: -6; reason: strict evidence was withdrawn", rendered)

    def test_range_check_catches_hook_bypass(self) -> None:
        baseline = self.repo.commit("baseline")
        (self.repo.root / "unrelated.txt").write_text("change\n")
        run(self.repo.root, "git", "add", "unrelated.txt")
        run(self.repo.root, "git", "commit", "-q", "--no-verify", "-m", "missing table")
        refused = run(
            self.repo.root,
            "python3",
            "scripts/commit-scorecard.py",
            "check-range",
            "--base",
            baseline,
            "--head",
            "HEAD",
            check=False,
        )
        self.assertEqual(1, refused.returncode)
        self.assertIn("must contain exactly one", refused.stderr)

    def test_scorecard_only_inheritance_is_exact_and_narrow(self) -> None:
        parent = self.repo.commit("validated parent")
        self.repo.write_results(green=["program/a"], measured_sha=parent)
        child = self.repo.commit("scorecard-only child")
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


if __name__ == "__main__":
    unittest.main()

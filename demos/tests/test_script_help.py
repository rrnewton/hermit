#!/usr/bin/env python3
"""Contract tests for the moved demo tooling's safe help probes."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class ScriptHelpTest(unittest.TestCase):
    def test_lint_checks_keeps_its_tail_checks(self):
        makefile = (ROOT / "Makefile").read_text()
        lint_recipe = makefile.split("\nlint-checks:", 1)[1].split("\ndemo-review:", 1)[0]
        for command in (
            "check-validate-refusal-predicate.py --self-test",
            "check-validate-refusal-predicate.py",
            "audit-test-binary-registration.py",
            "run-with-reverie-dbt-budget-test.sh",
        ):
            with self.subTest(command=command):
                self.assertIn(command, lint_recipe)

    def test_help_is_successful_informative_and_side_effect_free(self):
        for relative in (
            "scripts/check-demo-review.sh",
            "scripts/prepare-demo08-assets.sh",
        ):
            for flag in ("-h", "--help"):
                with self.subTest(script=relative, flag=flag), tempfile.TemporaryDirectory() as td:
                    scratch = Path(td)
                    environment = os.environ.copy()
                    environment.update(
                        {
                            "HOME": str(scratch / "home"),
                            "TMPDIR": str(scratch / "tmp"),
                            "DEMO08_DIR": str(scratch / "assets"),
                            "DEMO08_BUILD_ROOT": str(scratch / "build"),
                        }
                    )
                    before = list(scratch.iterdir())
                    result = subprocess.run(
                        [str(ROOT / relative), flag],
                        cwd=ROOT,
                        env=environment,
                        text=True,
                        capture_output=True,
                        check=False,
                    )

                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("USAGE:", result.stdout)
                    self.assertEqual(result.stderr, "")
                    self.assertEqual(list(scratch.iterdir()), before)

    def test_make_demo_review_checks_branch_changes(self):
        with tempfile.TemporaryDirectory() as td:
            checkout = Path(td)
            (checkout / "scripts").mkdir()
            (checkout / "demos").mkdir()
            shutil.copy2(ROOT / "Makefile", checkout / "Makefile")
            shutil.copy2(
                ROOT / "scripts/check-demo-review.sh",
                checkout / "scripts/check-demo-review.sh",
            )
            (checkout / "demos/01-a.sh").write_text("base\n")

            def git(*args):
                return subprocess.run(
                    ["git", *args],
                    cwd=checkout,
                    text=True,
                    capture_output=True,
                    check=True,
                )

            git("init", "-q")
            git("config", "user.name", "demo review test")
            git("config", "user.email", "demo-review-test@example.invalid")
            git("add", "Makefile", "scripts/check-demo-review.sh", "demos/01-a.sh")
            git("commit", "-q", "-m", "base")
            git("update-ref", "refs/remotes/origin/main", "HEAD")
            (checkout / "demos/01-a.sh").write_text("changed\n")
            git("add", "demos/01-a.sh")
            git("commit", "-q", "-m", "change demo")

            refused = subprocess.run(
                ["make", "--no-print-directory", "demo-review"],
                cwd=checkout,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(refused.returncode, 2)
            self.assertIn("demos/01-a.sh", refused.stdout + refused.stderr)

            git(
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "review demo",
                "-m",
                "Demo-Green-Review: reviewer=other demo=demos/01-a.sh result=GREEN evidence=log.txt",
            )
            accepted = subprocess.run(
                ["make", "--no-print-directory", "demo-review"],
                cwd=checkout,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(accepted.returncode, 0, accepted.stdout + accepted.stderr)


if __name__ == "__main__":
    unittest.main()

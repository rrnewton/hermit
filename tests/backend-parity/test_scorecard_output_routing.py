#!/usr/bin/env python3
"""Causal tests for scorecard witness classification and output routing."""

from __future__ import annotations

import contextlib
import csv
import importlib.util
import io
import os
import shlex
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("run_matrix", HERE / "run_matrix.py")
assert SPEC and SPEC.loader
run_matrix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(run_matrix)


def completed(stdout: bytes, stderr: bytes = b"", returncode: int = 0):
    return subprocess.CompletedProcess(["hermit"], returncode, stdout, stderr)


class WitnessTierTests(unittest.TestCase):
    def run_case(self, expected_stdout, runs, *, verify=False):
        evidence: dict[str, str] = {}
        case_name = "virtual_pid" if expected_stdout is None else "hello_stdout"
        with (
            mock.patch.object(
                run_matrix,
                "case_command",
                return_value=(["guest"], 0, expected_stdout),
            ),
            mock.patch.object(run_matrix, "hermit_command", return_value=["hermit"]),
            mock.patch.object(run_matrix, "run_with_timeout", side_effect=runs),
        ):
            status, detail, _ = run_matrix.run_case(
                Path("/unused/hermit"),
                "ptrace",
                case_name,
                object(),
                strict=True,
                verify=verify,
                expected_l2="stripped",
                evidence=evidence,
            )
        return status, detail, evidence

    def test_fixed_oracle_repeat_is_stdout_only(self):
        status, _, evidence = self.run_case(
            b"golden\n", [completed(b"golden\n") for _ in range(run_matrix.RUNS)]
        )
        self.assertEqual("PASS", status)
        self.assertEqual(
            run_matrix.COMPARISON_TIER_STDOUT_ONLY,
            evidence[run_matrix.COMPARISON_TIER_COLUMN],
        )
        self.assertEqual("1", evidence[run_matrix.STDOUT_PARITY_EVIDENCE])

    def test_fixed_oracle_mismatch_carries_negative_stdout_witness(self):
        status, _, evidence = self.run_case(b"golden\n", [completed(b"different\n")])
        self.assertEqual("FAIL", status)
        self.assertEqual(
            run_matrix.COMPARISON_TIER_STDOUT_ONLY,
            evidence[run_matrix.COMPARISON_TIER_COLUMN],
        )
        self.assertEqual("0", evidence[run_matrix.STDOUT_PARITY_EVIDENCE])

    def test_incomplete_fixed_oracle_batch_does_not_claim_positive_parity(self):
        status, _, evidence = self.run_case(
            b"golden\n", [completed(b"golden\n"), None]
        )
        self.assertEqual("FAIL", status)
        self.assertEqual(
            run_matrix.COMPARISON_TIER_STDOUT_ONLY,
            evidence[run_matrix.COMPARISON_TIER_COLUMN],
        )
        self.assertNotIn(run_matrix.STDOUT_PARITY_EVIDENCE, evidence)

    def test_dynamic_repeat_is_self_verify_not_stdout_parity(self):
        status, _, evidence = self.run_case(
            None, [completed(b"pid=7\n") for _ in range(run_matrix.RUNS)]
        )
        self.assertEqual("PASS", status)
        self.assertEqual(
            run_matrix.COMPARISON_TIER_SELF_VERIFY_ONLY,
            evidence[run_matrix.COMPARISON_TIER_COLUMN],
        )
        self.assertNotIn(run_matrix.STDOUT_PARITY_EVIDENCE, evidence)

    def test_verify_witness_is_self_verify_not_cross_backend_parity(self):
        status, _, evidence = self.run_case(
            b"golden\n",
            [completed(b"golden\n", run_matrix.VERIFY_WITNESS_DETLOG)],
            verify=True,
        )
        self.assertEqual("PASS", status)
        self.assertEqual(
            run_matrix.COMPARISON_TIER_SELF_VERIFY_ONLY,
            evidence[run_matrix.COMPARISON_TIER_COLUMN],
        )
        self.assertNotIn(run_matrix.STDOUT_PARITY_EVIDENCE, evidence)

    def test_verify_without_a_comparison_witness_stays_no_comparison(self):
        status, _, evidence = self.run_case(
            b"golden\n", [completed(b"golden\n")], verify=True
        )
        self.assertEqual("FAIL", status)
        self.assertEqual(
            run_matrix.COMPARISON_TIER_NO_COMPARISON,
            evidence[run_matrix.COMPARISON_TIER_COLUMN],
        )


class DocumentationConsistencyTests(unittest.TestCase):
    def test_readme_ratchets_and_case_cells_match_the_catalog(self):
        readme = (HERE / "README.md").read_text(encoding="utf-8")
        names = run_matrix.validate_catalog()
        total = len(names)
        l2_kinds = {
            "ptrace": "stripped DETLOG",
            "dbi": "native self-verify",
            "kvm": "guest-visible only",
        }
        display_names = {"ptrace": "ptrace", "dbi": "DBI", "kvm": "KVM"}
        case_tiers = {"ptrace": "stripped", "dbi": "self", "kvm": "guest"}
        for backend in run_matrix.BACKENDS:
            l1 = total - sum(
                item_backend == backend for item_backend, _ in run_matrix.L1_GAPS
            )
            l2 = total - sum(
                item_backend == backend for item_backend, _ in run_matrix.L2_GAPS
            )
            self.assertIn(
                f"| {display_names[backend]} | {l1}/{total} | {l1 / total:.0%} |",
                readme,
            )
            self.assertIn(
                f"| {display_names[backend]} | {l2}/{total} | {l2_kinds[backend]} | "
                f"{l2 / total:.0%} |",
                readme,
            )

        documented = {}
        for line in readme.splitlines():
            if line.startswith("| `"):
                cells = [cell.strip().replace("**", "") for cell in line.split("|")]
                documented[cells[1].strip("`")] = cells[2:5]
        self.assertEqual(set(names), set(documented))
        for name in names:
            for index, backend in enumerate(run_matrix.BACKENDS):
                if (backend, name) in run_matrix.L1_GAPS:
                    expected = "gap / gap"
                elif (backend, name) in run_matrix.L2_GAPS:
                    expected = "pass / gap"
                else:
                    expected = f"pass / {case_tiers[backend]}"
                self.assertEqual(expected, documented[name][index])


class OutputRoutingTests(unittest.TestCase):
    def git(self, root: Path, *args: str, check: bool = True):
        result = subprocess.run(
            ["git", "-C", str(root), *args],
            text=True,
            capture_output=True,
            check=False,
        )
        if check and result.returncode:
            self.fail(result.stdout + result.stderr)
        return result

    def planted_results(self):
        return [
            {
                "test_name": "fixed",
                "backend": "ptrace",
                "expectation": "pass",
                "result": "PASS",
                "seconds": "0.1",
                "detail": "fixed oracle matched",
                "evidence": {
                    run_matrix.COMPARISON_TIER_COLUMN:
                        run_matrix.COMPARISON_TIER_STDOUT_ONLY,
                    run_matrix.STDOUT_PARITY_EVIDENCE: "1",
                },
            },
            {
                "test_name": "dynamic",
                "backend": "dbi",
                "expectation": "pass",
                "result": "PASS",
                "seconds": "0.2",
                "detail": "within-backend repeats matched",
                "evidence": {
                    run_matrix.COMPARISON_TIER_COLUMN:
                        run_matrix.COMPARISON_TIER_SELF_VERIFY_ONLY,
                },
            },
            {
                "test_name": "gap",
                "backend": "kvm",
                "expectation": "gap",
                "result": "GAP",
                "seconds": "0.0",
                "detail": "not executed",
            },
        ]

    def test_default_is_ignored_and_explicit_path_is_the_only_tracked_write(self):
        with tempfile.TemporaryDirectory(prefix="scorecard-routing-") as temp:
            root = Path(temp) / "parent with spaces"
            compat = root / "compat-envelope"
            compat.mkdir(parents=True)
            scorecard = compat / "scorecard.csv"
            parent_header = [
                "stdout_parity" if column == "parity" else column
                for column in run_matrix.SCORECARD_HEADER
            ]
            parent_header.append("parent_only")
            scorecard.write_text(
                ",".join(parent_header) + "\n", encoding="utf-8"
            )
            (root / ".gitignore").write_text("ignored/\n", encoding="utf-8")
            self.git(root, "init", "-q")
            self.git(root, "add", ".gitignore", "compat-envelope/scorecard.csv")
            self.git(
                root,
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "user.name=Scorecard Test",
                "-c",
                "user.email=scorecard@example.invalid",
                "commit",
                "-qm",
                "baseline",
            )
            before_bytes = scorecard.read_bytes()
            before_status = self.git(root, "status", "--short").stdout

            output = io.StringIO()
            with (
                mock.patch.dict(os.environ, {"DEV_HERMIT_ROOT": str(root)}),
                contextlib.redirect_stdout(output),
            ):
                destination = run_matrix.record_parent_observations(
                    self.planted_results(),
                    requested_path=None,
                    disabled=False,
                    strict=True,
                    verify=False,
                    probe_gaps=False,
                )

            self.assertIsNotNone(destination)
            assert destination is not None
            self.assertEqual(
                compat / "ignored" / "backend-parity", destination.parent
            )
            self.assertEqual([destination], list(destination.parent.glob("*.csv")))
            self.assertEqual(before_bytes, scorecard.read_bytes())
            self.assertEqual(before_status, self.git(root, "status", "--short").stdout)
            self.assertEqual(
                0,
                self.git(root, "check-ignore", "--quiet", str(destination), check=False).returncode,
            )

            with destination.open(newline="", encoding="utf-8") as source:
                rows = list(csv.DictReader(source))
            self.assertEqual(
                [
                    run_matrix.COMPARISON_TIER_STDOUT_ONLY,
                    run_matrix.COMPARISON_TIER_SELF_VERIFY_ONLY,
                    run_matrix.COMPARISON_TIER_NO_COMPARISON,
                ],
                [row[run_matrix.COMPARISON_TIER_COLUMN] for row in rows],
            )
            self.assertEqual(["1", "", ""], [row["parity"] for row in rows])

            printed = [line.strip() for line in output.getvalue().splitlines()]
            fold = next(line for line in printed if line.startswith("python3 "))
            fold_argv = shlex.split(fold)
            self.assertEqual(
                [
                    "python3",
                    str((HERE / "run_matrix.py").resolve()),
                    "--fold-observation",
                    str(destination),
                    "--parent-scorecard",
                    str(scorecard),
                ],
                fold_argv,
            )
            folded = subprocess.run(
                fold_argv, cwd="/", text=True, capture_output=True, check=False
            )
            self.assertEqual(0, folded.returncode, folded.stderr)
            with scorecard.open(newline="", encoding="utf-8") as published:
                published_rows = list(csv.DictReader(published))
            self.assertEqual(3, len(published_rows))
            self.assertEqual(
                ["1", "", ""],
                [row["stdout_parity"] for row in published_rows],
            )
            self.assertEqual(
                [
                    run_matrix.COMPARISON_TIER_STDOUT_ONLY,
                    run_matrix.COMPARISON_TIER_SELF_VERIFY_ONLY,
                    run_matrix.COMPARISON_TIER_NO_COMPARISON,
                ],
                [row[run_matrix.COMPARISON_TIER_COLUMN] for row in published_rows],
            )
            self.assertEqual(["", "", ""], [row["parent_only"] for row in published_rows])
            self.assertIn(
                "compat-envelope/scorecard.csv",
                self.git(root, "status", "--short").stdout,
            )

            duplicate = subprocess.run(
                fold_argv, cwd="/", text=True, capture_output=True, check=False
            )
            self.assertEqual(2, duplicate.returncode)
            self.assertIn("already contains run_id", duplicate.stderr)
            with scorecard.open(newline="", encoding="utf-8") as published:
                self.assertEqual(3, len(list(csv.DictReader(published))))

            scorecard.write_bytes(before_bytes)
            self.assertEqual(before_status, self.git(root, "status", "--short").stdout)

            with mock.patch.dict(os.environ, {"DEV_HERMIT_ROOT": str(root)}):
                explicit = run_matrix.record_parent_observations(
                    [self.planted_results()[0]],
                    requested_path=scorecard,
                    disabled=False,
                    strict=True,
                    verify=False,
                    probe_gaps=False,
                )
            self.assertEqual(scorecard, explicit)
            self.assertNotEqual(before_bytes, scorecard.read_bytes())
            self.assertIn(
                "compat-envelope/scorecard.csv",
                self.git(root, "status", "--short").stdout,
            )


if __name__ == "__main__":
    unittest.main()

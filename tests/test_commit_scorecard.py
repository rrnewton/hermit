#!/usr/bin/env python3
"""Focused brackets for the receipt-sourced commit scorecard."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/commit-scorecard.py"
BACKENDS = ["ptrace", "kvm", "liteinst", "e9patch", "sabre", "dbt"]


def run(
    cwd: Path,
    *args: str,
    input_text: str | None = None,
    check: bool = True,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(
        args,
        cwd=cwd,
        input=input_text,
        text=True,
        capture_output=True,
        check=False,
        env={**os.environ, **(env or {})},
    )
    if check and proc.returncode:
        raise AssertionError(f"{' '.join(args)} failed\nstdout={proc.stdout}\nstderr={proc.stderr}")
    return proc


def plan(counts: dict[str, int]) -> dict:
    cells = []
    for backend, count in counts.items():
        for index in range(count):
            cells.append(
                {
                    "backend": backend,
                    "category": "applications",
                    "lane": "portable",
                    "mode": "verify",
                    "test": f"applications/{backend}-{index}",
                }
            )
    return {"schema": 1, "cells": cells}


def inventory(counts: dict[str, tuple[int, int]]) -> list[dict]:
    cells = []
    for backend, (declared, ci) in counts.items():
        for index in range(declared):
            cells.append(
                {
                    "backend": backend,
                    "bucket": "applications",
                    "ci": index < ci,
                    "lane": "portable",
                    "mode": "verify" if backend != "native" else "naked",
                    "test": f"applications/{backend}-{index}",
                }
            )
    return cells


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
        "gates": [
            {"name": "gate.manifest", "result": "pass"},
            {"name": "e2e.manifest_applications", "result": "pass"},
        ],
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
        (self.root / "bin").mkdir()
        (self.root / "ci/compat").mkdir(parents=True)
        shutil.copy2(SCRIPT, self.root / "scripts/commit-scorecard.py")
        (self.root / "ci/compat/scorecard-backends.json").write_text(
            json.dumps({"backends": BACKENDS}) + "\n"
        )
        self.write_plan({"ptrace": 2, "dbt": 1})
        self.write_inventory(
            {
                "ptrace": (4, 2),
                "kvm": (2, 2),
                "liteinst": (1, 0),
                "sabre": (1, 0),
                "dbt": (2, 1),
                "native": (3, 0),
            }
        )
        cargo = self.root / "bin/cargo"
        cargo.write_text("#!/bin/sh\ncat manifest-inventory.json\n")
        cargo.chmod(0o755)
        self.stage()
        run(self.root, "git", "commit", "-q", "--no-verify", "-m", "source")

    def write_plan(self, counts: dict[str, int]) -> None:
        (self.root / "ci/expected-e2e-plan.json").write_text(
            json.dumps(plan(counts), sort_keys=True, indent=2) + "\n"
        )

    def write_inventory(self, counts: dict[str, tuple[int, int]]) -> None:
        (self.root / "manifest-inventory.json").write_text(
            json.dumps(inventory(counts), sort_keys=True, indent=2) + "\n"
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
        return run(
            self.root,
            *args,
            input_text=json.dumps(row, separators=(",", ":")),
            check=False,
            env={"PATH": f"{self.root / 'bin'}:{os.environ['PATH']}"},
        )

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
        self.first = run(self.repo.root, "git", "rev-parse", "HEAD").stdout.strip()
        accepted = self.repo.import_row(receipt(self.first))
        self.assertEqual(0, accepted.returncode, accepted.stderr)

    def tearDown(self) -> None:
        self.repo.close()

    def test_total_and_backend_rows_come_from_receipt_bound_plan(self) -> None:
        rendered = self.repo.render().stdout
        self.assertIn("| TOTAL |", rendered)
        self.assertIn("| 3 | 0 | 0 | 0 | 3 |", rendered)
        ptrace = next(line for line in rendered.splitlines() if line.startswith("| ptrace |"))
        dbt = next(line for line in rendered.splitlines() if line.startswith("| dbt |"))
        kvm = next(line for line in rendered.splitlines() if line.startswith("| kvm |"))
        e9patch = next(
            line for line in rendered.splitlines() if line.startswith("| e9patch |")
        )
        native = next(line for line in rendered.splitlines() if line.startswith("| native |"))
        self.assertIn("| SELECTED | 4 | 2 | 2 | 0 | 0 | 0 | 2 |", ptrace)
        self.assertIn("| SELECTED | 2 | 1 | 1 | 0 | 0 | 0 | 1 |", dbt)
        self.assertIn("| DECLARED BUT NOT SELECTED | 2 | 2 | — | — | — | — | 0 |", kvm)
        self.assertIn("| DECLARED BUT NOT SELECTED | 3 | 0 | — | — | — | — | 0 |", native)
        self.assertIn("| NOT A BACKEND | — | — | — | — | — | — | — |", e9patch)
        self.assertNotIn("| 0 | 0 | 0 | 0 | 0 |", kvm)
        self.assertIn("Declared but not selected: kvm, liteinst, sabre, native", rendered)
        self.assertIn("not a backend: e9patch", rendered)

    def test_growth_does_not_read_as_green_drop(self) -> None:
        self.repo.write_plan({"ptrace": 79, "liteinst": 3, "sabre": 9, "dbt": 9})
        self.repo.write_inventory(
            {
                "ptrace": (336, 79),
                "kvm": (2, 2),
                "liteinst": (28, 3),
                "sabre": (141, 9),
                "dbt": (62, 9),
                "native": (33, 0),
            }
        )
        first_source = self.repo.commit("100-cell source")
        self.assertEqual(0, self.repo.import_row(receipt(first_source)).returncode)
        self.repo.commit("100-cell scorecard")

        self.repo.write_plan({"ptrace": 151, "liteinst": 3, "sabre": 9, "dbt": 9})
        self.repo.write_inventory(
            {
                "ptrace": (408, 151),
                "kvm": (2, 2),
                "liteinst": (28, 3),
                "sabre": (141, 9),
                "dbt": (62, 9),
                "native": (33, 0),
            }
        )
        second = self.repo.commit("172-cell source")
        self.assertEqual(0, self.repo.import_row(receipt(second)).returncode)
        rendered = self.repo.render().stdout
        self.assertIn("| ptrace |", rendered)
        self.assertIn("| SELECTED | 408 | 151 | 151 | 0 | 0 | 0 | 151 |", rendered)
        self.assertIn("| TOTAL |", rendered)
        self.assertIn("| 172 | 0 | 0 | 0 | 172 |", rendered)
        self.assertIn("Matrix change: +72; GREEN change: +72", rendered)
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

    def test_plan_digest_tamper_refuses(self) -> None:
        self.repo.stage()
        path = self.repo.root / "ci/compat/commit-scorecard-receipt.json"
        wrapper = json.loads(path.read_text())
        wrapper["canonical_e2e_plan"] += " "
        path.write_text(json.dumps(wrapper) + "\n")
        refused = self.repo.render(check=False)
        self.assertEqual(1, refused.returncode)
        self.assertIn("E2E plan digest mismatch", refused.stderr)

    def test_schema2_history_keeps_pre_measurement_table(self) -> None:
        path = self.repo.root / "ci/compat/commit-scorecard-receipt.json"
        wrapper = json.loads(path.read_text())
        wrapper["schema"] = 2
        path.write_text(json.dumps(wrapper) + "\n")
        rendered = self.repo.render().stdout
        self.assertNotIn("| measurement |", rendered)
        kvm = next(line for line in rendered.splitlines() if line.startswith("| kvm |"))
        self.assertIn("| 0 | 0 | 0 | 0 | 0 |", kvm)

    def test_schema3_history_keeps_unmeasured_table(self) -> None:
        path = self.repo.root / "ci/compat/commit-scorecard-receipt.json"
        wrapper = json.loads(path.read_text())
        wrapper["schema"] = 3
        wrapper.pop("canonical_manifest_inventory")
        wrapper.pop("manifest_inventory_sha256")
        path.write_text(json.dumps(wrapper) + "\n")
        rendered = self.repo.render().stdout
        self.assertNotIn("| selection |", rendered)
        kvm = next(line for line in rendered.splitlines() if line.startswith("| kvm |"))
        self.assertIn("| UNMEASURED | — | — | — | — | 0 |", kvm)

    def test_manifest_inventory_digest_tamper_refuses(self) -> None:
        self.repo.stage()
        path = self.repo.root / "ci/compat/commit-scorecard-receipt.json"
        wrapper = json.loads(path.read_text())
        wrapper["canonical_manifest_inventory"] += " "
        path.write_text(json.dumps(wrapper) + "\n")
        refused = self.repo.render(check=False)
        self.assertEqual(1, refused.returncode)
        self.assertIn("manifest inventory digest mismatch", refused.stderr)

    def test_selected_cells_cannot_exceed_declared_ci_cells(self) -> None:
        self.repo.write_inventory(
            {
                "ptrace": (2, 1),
                "kvm": (2, 2),
                "dbt": (1, 1),
                "native": (3, 0),
            }
        )
        refused = self.repo.import_row(receipt(self.first))
        self.assertEqual(1, refused.returncode)
        self.assertIn("selected ptrace cells exceed", refused.stderr)

    def test_missing_plan_gate_refuses_before_import(self) -> None:
        row = receipt(self.first)
        row["gates"] = [gate for gate in row["gates"] if gate["name"] != "e2e.manifest_applications"]
        refused = self.repo.import_row(row)
        self.assertEqual(1, refused.returncode)
        self.assertIn("passing e2e.manifest_applications", refused.stderr)

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
        self.repo.commit("baseline snapshot")
        (self.repo.root / "code-change").write_text("validated content\n")
        run(self.repo.root, "git", "add", "code-change")
        parent = self.repo.commit("validated parent")
        self.assertEqual(0, self.repo.import_row(receipt(parent)).returncode)
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

#!/usr/bin/env python3
"""Collision controls for backend-parity host temporary directories."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import threading
import unittest
from importlib.machinery import SourceFileLoader
from pathlib import Path


HERE = Path(__file__).resolve().parent


def load(name: str, filename: str):
    loader = SourceFileLoader(name, str(HERE / filename))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    loader.exec_module(module)
    return module


class BackendParityTemporaryPathTest(unittest.TestCase):
    def test_old_host_tmp_overwrites_and_all_commands_accept_private_roots(self) -> None:
        run_matrix = load("parallel_run_matrix", "run_matrix.py")
        e9patch = load("parallel_e9patch_corpus", "e9patch_corpus.py")
        mutation = load("parallel_parity_mutation", "parity_mutation.py")

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            old_tmp = root / "old-host-tmp"
            old_tmp.mkdir()
            old_child = old_tmp / "hermit-file-io-fixed"

            # Removed behavior: both top-level validates bound host /tmp.  The
            # same guest-relative name therefore denotes one host file, and the
            # second run replaces the first result.
            first_written = threading.Event()
            second_written = threading.Event()

            def first_run() -> None:
                old_child.write_text("run-a")
                first_written.set()
                second_written.wait(timeout=2)

            def second_run() -> None:
                if not first_written.wait(timeout=2):
                    return
                old_child.write_text("run-b")
                second_written.set()

            first = threading.Thread(target=first_run)
            second = threading.Thread(target=second_run)
            first.start()
            second.start()
            first.join(timeout=2)
            second.join(timeout=2)
            self.assertFalse(first.is_alive())
            self.assertFalse(second.is_alive())
            self.assertTrue(second_written.is_set())
            self.assertEqual(old_child.read_text(), "run-b")

            run_a = root / "run-a"
            run_b = root / "run-b"
            fixtures_a = run_matrix.Fixtures(run_a)
            fixtures_b = run_matrix.Fixtures(run_b)
            tmp_a = fixtures_a.host_tmp("ptrace", "file-io")
            tmp_b = fixtures_b.host_tmp("ptrace", "file-io")
            self.assertNotEqual(tmp_a, tmp_b)
            child_a = tmp_a / old_child.name
            child_b = tmp_b / old_child.name
            child_a.write_text("run-a")
            child_b.write_text("run-b")
            self.assertEqual(child_a.read_text(), "run-a")
            self.assertEqual(child_b.read_text(), "run-b")

            hermit = Path("/hermit")
            guest = Path("/guest")
            matrix_a = run_matrix.hermit_command(
                hermit, "ptrace", [str(guest)], "file-io", True, tmp_a
            )
            matrix_b = run_matrix.hermit_command(
                hermit, "ptrace", [str(guest)], "file-io", True, tmp_b
            )
            e9_a = e9patch.hermit_command(hermit, False, False, guest, tmp_a)
            e9_b = e9patch.hermit_command(hermit, False, False, guest, tmp_b)
            mutation_a = mutation._hermit_command(hermit, "ptrace", guest, None, tmp_a)
            mutation_b = mutation._hermit_command(hermit, "ptrace", guest, None, tmp_b)
            for command_a, command_b in (
                (matrix_a, matrix_b),
                (e9_a, e9_b),
                (mutation_a, mutation_b),
            ):
                self.assertIn(f"--tmp={tmp_a}", command_a)
                self.assertIn(f"--tmp={tmp_b}", command_b)
                self.assertNotIn("--tmp=/tmp", command_a)
                self.assertNotIn("--tmp=/tmp", command_b)


if __name__ == "__main__":
    unittest.main()

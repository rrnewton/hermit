#!/usr/bin/env python3
"""Scenario tests for the host-prerequisite check in ci/buck-e2e/stage --from-cargo.

Each case runs a copy of stage in a scratch tree, with a PATH holding only the commands
it needs and a scratch library directory (HERMIT_STAGE_HOST_LIBDIR) in place of
/usr/lib64. Refuse: a missing patchelf, readelf, strip or cmake, or a missing libunwind
library, stops stage with the dnf package that provides it and the documentation
section, and the previous staging is left in place. Accept: with everything present,
stage gets past the check and removes the previous staging (it then fails on the
scratch tree, which is not a git checkout). --bundle needs none of them.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


STAGE = Path(__file__).resolve().parent / "buck-e2e" / "stage"
TOOLS = ("patchelf", "readelf", "strip", "cmake")
LIBS = ("libunwind.so.8", "libunwind-x86_64.so.8")
# Commands stage runs on its way to the check and just after it.
HOST_COMMANDS = ("dirname", "realpath", "rm", "mkdir", "git", "cp", "python3", "du", "cut", "cat")
DOC = '(docs/BUCK2_OSS.md, "Host prerequisites for Buck validation")'


class StagePrerequisiteTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="buck-e2e-stage-test."))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.stage = self.tmp / "tree" / "ci" / "buck-e2e" / "stage"
        self.stage.parent.mkdir(parents=True)
        shutil.copy2(STAGE, self.stage)
        self.sentinel = self.stage.parent / "staged" / "SENTINEL"
        self.sentinel.parent.mkdir()
        self.sentinel.write_text("previous staging\n")
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        for name in HOST_COMMANDS:
            real = shutil.which(name)
            self.assertIsNotNone(real, name)
            (self.bin / name).symlink_to(real)
        for name in TOOLS:
            self.tool(name).write_text("#!/bin/sh\nexit 0\n")
            self.tool(name).chmod(0o755)
        self.libdir = self.tmp / "lib64"
        self.libdir.mkdir()
        for name in LIBS:
            (self.libdir / name).write_text(name + "\n")

    def tool(self, name: str) -> Path:
        return self.bin / name

    def run_stage(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        base = {"PATH": str(self.bin), "HOME": str(self.tmp), "HERMIT_STAGE_HOST_LIBDIR": str(self.libdir)}
        base.update(env)
        return subprocess.run(["/bin/bash", str(self.stage), *args], env=base, capture_output=True,
                              text=True, timeout=60)

    def assert_refused(self, proc: subprocess.CompletedProcess, what: str, package: str) -> None:
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn(f"ci/buck-e2e/stage: {what} is missing; install it with  sudo dnf install -y {package}",
                      proc.stderr)
        self.assertIn(DOC, proc.stderr)
        self.assertTrue(self.sentinel.exists(), "a refusal removed the previous staging")

    def test_each_missing_tool_names_its_package(self) -> None:
        for name, package in (("patchelf", "patchelf"), ("readelf", "binutils"), ("strip", "binutils"),
                              ("cmake", "cmake")):
            with self.subTest(name):
                saved = self.tool(name).read_text()
                self.tool(name).unlink()
                try:
                    self.assert_refused(self.run_stage("--from-cargo"), name, package)
                finally:
                    self.tool(name).write_text(saved)
                    self.tool(name).chmod(0o755)

    def test_cmake_named_by_the_cmake_variable(self) -> None:
        self.tool("cmake").rename(self.tool("cmake-pinned"))
        self.assert_refused(self.run_stage("--from-cargo"), "cmake", "cmake")
        proc = self.run_stage("--from-cargo", CMAKE="cmake-pinned")
        self.assertNotIn("is missing", proc.stderr)
        self.assertFalse(self.sentinel.exists(), proc.stderr)

    def test_each_missing_libunwind_library_names_libunwind_devel(self) -> None:
        for name in LIBS:
            with self.subTest(name):
                (self.libdir / name).unlink()
                try:
                    self.assert_refused(self.run_stage("--from-cargo"), f"{self.libdir}/{name}", "libunwind-devel")
                finally:
                    (self.libdir / name).write_text(name + "\n")

    def test_all_present_passes_the_check(self) -> None:
        proc = self.run_stage("--from-cargo")
        self.assertNotIn("is missing", proc.stderr)
        self.assertFalse(self.sentinel.exists(), f"stage stopped before replacing the staging: {proc.stderr}")
        self.assertNotEqual(proc.returncode, 0, "the scratch tree is not a checkout, so stage cannot finish")

    def test_bundle_mode_needs_no_host_packages(self) -> None:
        for name in TOOLS:
            self.tool(name).unlink()
        shutil.rmtree(self.libdir)
        proc = self.run_stage("--bundle", str(self.tmp / "no-bundle"))
        self.assertNotIn("is missing", proc.stderr)
        self.assertFalse(self.sentinel.exists(), proc.stderr)


if __name__ == "__main__":
    unittest.main()

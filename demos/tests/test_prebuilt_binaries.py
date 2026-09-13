#!/usr/bin/env python3
"""Exercise Demo 2/4 setup without building or launching a Hermit workload."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class PrebuiltBinariesTest(unittest.TestCase):
    def _run(self, demo, *, skip_build, override, default_exists, override_exists):
        with tempfile.TemporaryDirectory(prefix="demo paths ") as directory:
            root = Path(directory)
            demos = root / "demos"
            (demos / "lib").mkdir(parents=True)
            (root / "tools").mkdir()
            binary_name = "hermit" if demo == 2 else "hello_race"
            default = root / "target/debug" / binary_name
            alternate = root / "explicit binary"
            for path, exists in ((default, default_exists), (alternate, override_exists)):
                if exists:
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text("inert executable-check fixture; never launched\n")
                    path.chmod(0o755)
            script = "02-record-replay.sh" if demo == 2 else "04-schedule-bisection.sh"
            shutil.copy2(ROOT / "demos" / script, demos / script)
            (demos / "lib/display.sh").write_text("demo_header() { :; }\n")
            # Stop at the first use after binary selection. The real source
            # setup and executable checks above it still run. These functions
            # never invoke a Hermit binary or stand in for a passing demo.
            (demos / "common.sh").write_text(
                'demo_banner() { :; }\n'
                'safehermit() { printf "%s\\n" "$HERMIT" >"$CONTROL_SELECTED"; exit 77; }\n'
                'hermit_supports_cpuid_faulting() {\n'
                '  printf "%s\\n" "$HELLO_RACE_DEBUG" >"$CONTROL_SELECTED"; exit 77;\n'
                '}\n'
            )
            cargo = root / "tools/cargo"
            cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >>"$CONTROL_BUILDS"\n')
            cargo.chmod(0o755)
            env = os.environ.copy()
            for name in ("HERMIT_DEBUG", "HELLO_RACE_DEBUG", "DEMO_SKIP_BUILD"):
                env.pop(name, None)
            env.update(
                PATH=str(root / "tools") + os.pathsep + env["PATH"],
                HERMIT_REPO=str(root),
                DEMO_TMP=str(root / "scratch"),
                DEMO_ARTIFACTS=str(root / "artifacts"),
                CONTROL_SELECTED=str(root / "selected"),
                CONTROL_BUILDS=str(root / "builds"),
            )
            if skip_build:
                env["DEMO_SKIP_BUILD"] = "1"
            if override:
                env["HERMIT_DEBUG" if demo == 2 else "HELLO_RACE_DEBUG"] = str(alternate)
            result = subprocess.run(
                ["bash", str(demos / script)], env=env, capture_output=True, text=True, timeout=10
            )
            selected = (root / "selected").read_text().strip() if (root / "selected").exists() else None
            builds = (root / "builds").read_text().splitlines() if (root / "builds").exists() else []
            return result, selected, builds, str(default), str(alternate)

    def test_explicit_prebuilt_binary_and_no_rebuild(self):
        for demo in (2, 4):
            with self.subTest(demo=demo):
                result, selected, builds, _, alternate = self._run(
                    demo, skip_build=True, override=True, default_exists=False, override_exists=True
                )
                self.assertEqual(result.returncode, 77, result.stderr)
                self.assertEqual(selected, alternate)
                self.assertEqual(builds, [])

    def test_default_prebuilt_binary_and_no_rebuild(self):
        for demo in (2, 4):
            with self.subTest(demo=demo):
                result, selected, builds, default, _ = self._run(
                    demo, skip_build=True, override=False, default_exists=True, override_exists=False
                )
                self.assertEqual(result.returncode, 77, result.stderr)
                self.assertEqual(selected, default)
                self.assertEqual(builds, [])

    def test_default_build_command_is_preserved(self):
        expected = {2: "build --locked -p hermit --bin hermit",
                    4: "build -p hermetic_infra_hermit_flaky-tests --bin hello_race"}
        for demo in (2, 4):
            with self.subTest(demo=demo):
                result, selected, builds, default, _ = self._run(
                    demo, skip_build=False, override=False, default_exists=True, override_exists=False
                )
                self.assertEqual(result.returncode, 77, result.stderr)
                self.assertEqual(selected, default)
                self.assertEqual(builds, [expected[demo]])

    def test_missing_explicit_binary_refuses_instead_of_using_default(self):
        for demo in (2, 4):
            with self.subTest(demo=demo):
                result, selected, builds, _, _ = self._run(
                    demo, skip_build=True, override=True, default_exists=True, override_exists=False
                )
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("missing debug", result.stderr)
                self.assertIsNone(selected)
                self.assertEqual(builds, [])

    def test_missing_default_binary_refuses(self):
        for demo in (2, 4):
            with self.subTest(demo=demo):
                result, selected, builds, _, _ = self._run(
                    demo, skip_build=True, override=False, default_exists=False, override_exists=False
                )
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("missing debug", result.stderr)
                self.assertIsNone(selected)
                self.assertEqual(builds, [])


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Contract tests for host-visible, checkout-scoped QEMU demo paths."""

import os
import runpy
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
QEMU_PATHS = LIB_DIR / "qemu-paths.sh"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402


def _shell_default(root: Path) -> Path:
    return Path(
        subprocess.check_output(
            [str(QEMU_PATHS), str(root)],
            text=True,
        ).strip()
    )


def _make_default(extra_env=None) -> Path:
    env = os.environ.copy()
    if extra_env:
        env.update(extra_env)
    output = subprocess.check_output(
        [
            "make",
            "-C",
            str(DEMO_DIR),
            "--no-print-directory",
            "-s",
            "--eval=print-assets: ; @echo $(QEMU_ASSETS)",
            "print-assets",
        ],
        text=True,
        env=env,
    )
    return Path(output.strip())


class DefaultQemuAssetsTest(unittest.TestCase):
    def test_dependency_check_accepts_make_prerequisite_output(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout=(
                "make: build tools OK\n"
                "submodules OK -- 2 configured paths\n"
                "Dependency check passed: libunwind-ptrace liblzma\n"
            ),
            stderr="",
        )
        with mock.patch.object(dc.subprocess, "run", return_value=completed):
            self.assertEqual(
                dc.check_dependencies(DEMO_DIR.parent),
                "Dependency check passed: libunwind-ptrace liblzma",
            )

    def test_dependency_check_refuses_success_without_its_result_line(self):
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="submodules OK\n", stderr=""
        )
        with mock.patch.object(dc.subprocess, "run", return_value=completed):
            with self.assertRaisesRegex(RuntimeError, "unexpected dependency-check"):
                dc.check_dependencies(DEMO_DIR.parent)

    def test_tmp_checkouts_are_host_visible_and_checkout_scoped(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as first, tempfile.TemporaryDirectory(
            dir="/tmp"
        ) as second:
            first_root = Path(first)
            second_root = Path(second)
            first_assets = dc.default_qemu_assets(first_root)
            second_assets = dc.default_qemu_assets(second_root)

            self.assertEqual(first_assets, _shell_default(first_root))
            self.assertEqual(second_assets, _shell_default(second_root))
            self.assertNotEqual(first_assets, second_assets)
            self.assertEqual(first_assets.parent, Path("/var/tmp"))
            self.assertTrue(
                first_assets.name.startswith(
                    "hermit-qemu-strict-l2-{}-".format(os.getuid())
                )
            )

    def test_non_tmp_checkout_keeps_repo_local_ignored_directory(self):
        root_path = Path.home().resolve()
        expected = root_path / "ignored/qemu-linux"
        self.assertEqual(dc.default_qemu_assets(root_path), expected)
        self.assertEqual(_shell_default(root_path), expected)

    def test_symlinked_tmp_checkout_matches_python_canonicalization(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as physical, tempfile.TemporaryDirectory(
            dir="/tmp"
        ) as links:
            linked_root = Path(links) / "checkout"
            linked_root.symlink_to(physical, target_is_directory=True)
            self.assertEqual(
                dc.default_qemu_assets(linked_root), _shell_default(linked_root)
            )

    def test_tmp_checkout_adds_the_identity_tmp_mount(self):
        self.assertEqual(dc.hermit_tmp_args(Path("/tmp/work/hermit")), ["--tmp=/tmp"])
        self.assertEqual(dc.hermit_tmp_args(Path("/srv/hermit")), [])

    def test_external_paths_render_without_relative_to_failure(self):
        external = Path("/var/tmp/assets/run-metadata.json")
        self.assertEqual(dc.display_path(external, Path("/tmp/hermit")), str(external))
        self.assertEqual(
            dc.display_path(Path("/tmp/hermit/demos/out"), Path("/tmp/hermit")),
            "demos/out",
        )

    def test_python_entrypoints_preserve_explicit_override(self):
        chosen = "/var/tmp/operator-selected-qemu-assets"
        with mock.patch.dict(os.environ, {"QEMU_ASSETS": chosen}):
            for script in ("05-qemu-boot.py", "06-qemu-resume.py"):
                namespace = runpy.run_path(str(DEMO_DIR / script))
                self.assertEqual(namespace["ASSETS"], Path(chosen))

    def test_make_uses_shared_default_and_preserves_override(self):
        self.assertEqual(_make_default(), dc.default_qemu_assets(DEMO_DIR.parent))
        chosen = Path("/var/tmp/operator-selected-qemu-assets")
        self.assertEqual(_make_default({"QEMU_ASSETS": str(chosen)}), chosen)

    def test_every_shell_entrypoint_uses_the_shared_resolver(self):
        for relative in ("clean.sh", "07-drgn-kernel.sh", "lib/qemu-assets.sh"):
            text = (DEMO_DIR / relative).read_text()
            self.assertIn("qemu-paths.sh", text)
            self.assertIn("qemu_default_assets", text)

    def test_every_hermit_qemu_launch_uses_the_tmp_mount_helper(self):
        for relative in ("05-qemu-boot.py", "06-qemu-resume.py", "lib/drgn_hermit.py"):
            text = (DEMO_DIR / relative).read_text()
            self.assertIn("hermit_tmp_args(", text)

    def test_qemu_controllers_disable_bytecode_writes_before_launch(self):
        setting = 'environment["PYTHONDONTWRITEBYTECODE"] = "1"'
        for relative in ("05-qemu-boot.py", "06-qemu-resume.py"):
            with self.subTest(relative=relative):
                text = (DEMO_DIR / relative).read_text()
                environment_start = text.index("environment = os.environ.copy()")
                launch = text.index("process = subprocess.Popen(", environment_start)
                self.assertIn(setting, text[environment_start:launch])
                self.assertIn("env=environment", text[launch:])


if __name__ == "__main__":
    unittest.main()


class SocketPathBoundTest(unittest.TestCase):
    """A Unix-domain socket path must fit AF_UNIX, and must move only if it must.

    Relocating unconditionally passed locally and broke the hosted runner, where
    XDG_RUNTIME_DIR is unusable and the in-checkout path had always fit, so the
    ordering here is the fix and not an optimisation.
    """

    def test_a_path_that_fits_is_returned_unchanged(self):
        fits = Path("/runner-state/_work/hermit/hermit/ignored/q/.work/boot-ab/qmp.sock")
        self.assertLessEqual(len(str(fits).encode()), dc.AF_UNIX_PATH_MAX)
        self.assertEqual(dc.make_socket_path(fits, "boot"), fits)

    def test_a_path_over_the_bound_is_relocated_within_it(self):
        too_long = Path(
            "/home/newton/work/dev-hermit/worktrees/validate/"
            "validate-fresh-27be9dc9b2a6-3837008-c9ce93ba/hermit/ignored/"
            "qemu-linux/.work/boot-ab12cd34/qmp.sock"
        )
        self.assertGreater(len(str(too_long).encode()), dc.AF_UNIX_PATH_MAX)
        got = dc.make_socket_path(too_long, "boot")
        self.assertNotEqual(got, too_long)
        self.assertLessEqual(len(str(got).encode()), dc.AF_UNIX_PATH_MAX)
        self.assertEqual(got.name, "qmp.sock")

    def test_the_bound_is_the_kernel_constant(self):
        # sockaddr_un.sun_path is 108 bytes including the NUL.
        self.assertEqual(dc.AF_UNIX_PATH_MAX, 107)

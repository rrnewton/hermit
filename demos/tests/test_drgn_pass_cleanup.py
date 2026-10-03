#!/usr/bin/env python3
"""Tests for how demo 7 starts a pass under Hermit and what a pass leaves behind.

Each demo 7 pass copies demo 5's snapshot into a fresh run directory under
DEMO07_ARTIFACTS and restores it in QEMU under Hermit. QEMU opens the run
directory's QMP socket, serial FIFOs and disks from inside Hermit, which gives
it a private /tmp unless it is passed --tmp=/tmp. Only the checkout's location
used to decide that, so a run directory under host /tmp, from a checkout
elsewhere, was invisible to QEMU: it could not create its QMP socket, and the
demo printed only "Hermit exited before QMP connected (status 1)" and kept the
failed pass's 94 MB snapshot copy.

start() is run here with Hermit's launch and the QMP connection replaced, which
is enough to see the command line Hermit would get and what a failed pass
leaves.
"""

import contextlib
import dataclasses
import io
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

LIB_DIR = Path(__file__).resolve().parent.parent / "lib"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402
import drgn_hermit as dh  # noqa: E402


class _StopAtQmpConnect(Exception):
    """Raised in place of the QMP connection, after Hermit's command line is built."""


def _directory(test: unittest.TestCase, parent: str, prefix: str) -> Path:
    directory = tempfile.TemporaryDirectory(dir=parent, prefix=prefix)
    test.addCleanup(directory.cleanup)
    return Path(directory.name)


def _hermit_options(argv):
    """Return Hermit's own options: the words between `run` and `--`."""
    return argv[argv.index("run") + 1 : argv.index("--")]


class _PassHarness(unittest.TestCase):
    def setUp(self):
        # Neither the checkout nor the inputs are under host /tmp unless a test
        # puts them there.
        self.outside_tmp = _directory(self, "/var/tmp", "drgn-pass-test-")
        self.root = self.outside_tmp / "checkout"
        self.root.mkdir()
        self.inputs = self.outside_tmp / "inputs"
        self.inputs.mkdir()
        environment = mock.patch.dict(
            os.environ, {"QEMU_SOCKET_DIR": str(self.outside_tmp / "sockets")}
        )
        environment.start()
        self.addCleanup(environment.stop)

    def _config(self, artifact_dir: Path, inputs: Path = None) -> dh.GuestConfig:
        inputs = inputs or self.inputs
        for name in ("hermit", "qemu", "bzImage", "initramfs.cpio.gz"):
            (inputs / name).write_bytes(b"")
        (inputs / "hermit-boot.qcow2").write_bytes(b"q" * 4096)
        return dh.GuestConfig(
            root=self.root,
            hermit=inputs / "hermit",
            qemu=inputs / "qemu",
            kernel=inputs / "bzImage",
            initrd=inputs / "initramfs.cpio.gz",
            vmlinux=inputs / "vmlinux",
            snapshot_disk=inputs / "hermit-boot.qcow2",
            snapshot_name="hermit-boot",
            advance_command="echo deterministic",
            artifact_dir=artifact_dir,
        )

    def _launch_patches(self, config, popen, connect):
        stack = contextlib.ExitStack()
        stack.enter_context(mock.patch.object(dh, "ensure_vmlinux", return_value=config.vmlinux))
        stack.enter_context(mock.patch.object(dh.subprocess, "Popen", popen))
        stack.enter_context(mock.patch.object(dh.os, "getpgid", return_value=4242))
        stack.enter_context(mock.patch.object(dh.QmpClient, "connect", connect))
        return stack

    def _hermit_argv(self, config):
        """Run start() until it connects to QMP; return the command Hermit got."""
        popen = mock.Mock(return_value=mock.Mock(pid=4242))
        connect = mock.Mock(side_effect=_StopAtQmpConnect)
        program = dh.HermitGuestProgram(config)
        with self._launch_patches(config, popen, connect):
            with self.assertRaises(_StopAtQmpConnect):
                program.start()
        return program, popen.call_args.args[0]


class HostTmpBindingTest(_PassHarness):
    def test_a_run_directory_under_host_tmp_gets_the_host_tmp(self):
        # The configuration that failed: a checkout outside /tmp and
        # DEMO07_ARTIFACTS a short directory under it.
        artifact_dir = _directory(self, "/tmp", "d7-") / "artifacts"
        program, argv = self._hermit_argv(self._config(artifact_dir))
        self.assertTrue(dc._under_host_tmp(program.run_dir), program.run_dir)
        self.assertEqual(_hermit_options(argv).count("--tmp=/tmp"), 1, argv)
        # The socket stays in the run directory, where QEMU can now create it.
        self.assertEqual(program.qmp_socket, program.run_dir / "qmp.sock")

    def test_a_kernel_under_host_tmp_gets_the_host_tmp(self):
        inputs = _directory(self, "/tmp", "d7-inputs-")
        artifact_dir = self.outside_tmp / "artifacts"
        _, argv = self._hermit_argv(self._config(artifact_dir, inputs))
        self.assertEqual(_hermit_options(argv).count("--tmp=/tmp"), 1, argv)

    def test_a_library_directory_under_host_tmp_gets_the_host_tmp(self):
        # DEMO07_QEMU_LIBRARY_PATH is prepended to LD_LIBRARY_PATH, so any
        # entry of a list counts, not only the first.
        libraries = _directory(self, "/tmp", "d7-lib-")
        config = dataclasses.replace(
            self._config(self.outside_tmp / "artifacts"),
            qemu_library_path=Path("{}:{}".format(self.outside_tmp / "lib", libraries)),
        )
        _, argv = self._hermit_argv(config)
        self.assertEqual(_hermit_options(argv).count("--tmp=/tmp"), 1, argv)

    def test_nothing_under_host_tmp_keeps_the_private_tmp(self):
        # Positive control: the usual layout is unchanged.
        artifact_dir = self.outside_tmp / "artifacts"
        program, argv = self._hermit_argv(self._config(artifact_dir))
        self.assertFalse(dc._under_host_tmp(program.run_dir), program.run_dir)
        self.assertNotIn("--tmp=/tmp", argv)

    def test_the_helper_considers_every_path_it_is_given(self):
        self.assertEqual(
            dc.hermit_tmp_args(Path("/srv/hermit"), Path("/tmp/d7/run.x")), ["--tmp=/tmp"]
        )
        self.assertEqual(
            dc.hermit_tmp_args(Path("/srv/hermit"), None, Path("/var/tmp/run.x")), []
        )
        # The checkout alone still decides as before.
        self.assertEqual(dc.hermit_tmp_args(Path("/tmp/work/hermit")), ["--tmp=/tmp"])
        self.assertEqual(dc.hermit_tmp_args(Path("/srv/hermit")), [])


class FailedPassTest(_PassHarness):
    LOG_LINES = ["hermit line {:02d}".format(number) for number in range(1, 61)] + [
        "qemu-system-x86_64: -qmp unix:/tmp/d7/run.x/qmp.sock,server=on,wait=off: "
        "Failed to bind socket to /tmp/d7/run.x/qmp.sock: No such file or directory"
    ]

    def _failed_start(self, artifact_dir: Path):
        """Start a pass whose Hermit writes LOG_LINES and exits before QMP connects."""
        config = self._config(artifact_dir)

        def popen(command, **options):
            options["stdout"].write(("\n".join(self.LOG_LINES) + "\n").encode())
            process = mock.Mock(pid=4242, returncode=1)
            process.poll.return_value = 1
            return process

        connect = mock.Mock(
            side_effect=RuntimeError("Hermit exited before QMP connected (status 1)")
        )
        stderr = io.StringIO()
        with self._launch_patches(config, mock.Mock(side_effect=popen), connect):
            with contextlib.redirect_stderr(stderr):
                with self.assertRaises(RuntimeError) as raised:
                    with dh.program_from_hermit(config):
                        self.fail("the pass must not start")
        (run_dir,) = artifact_dir.iterdir()
        return run_dir, stderr.getvalue(), raised.exception

    def test_a_failed_start_shows_the_end_of_the_hermit_log(self):
        artifact_dir = self.outside_tmp / "artifacts"
        run_dir, stderr, error = self._failed_start(artifact_dir)
        # The error itself is unchanged; the log's end is printed with it.
        self.assertEqual(str(error), "Hermit exited before QMP connected (status 1)")
        lines = stderr.splitlines()
        header = "End of the failed pass's Hermit log, {}:".format(run_dir / "hermit.log")
        self.assertIn(header, lines, stderr)
        shown = dh.FAILED_LOG_TAIL_LINES
        self.assertLess(shown, len(self.LOG_LINES))
        first = lines.index(header) + 1
        self.assertEqual(
            lines[first : first + shown],
            ["  " + line for line in self.LOG_LINES[-shown:]],
            stderr,
        )
        # QEMU's reason, the log's last line, is among them; older lines are not.
        self.assertIn("  " + self.LOG_LINES[-1], lines)
        self.assertNotIn("  " + self.LOG_LINES[-shown - 1], lines)

    def test_a_failed_start_removes_the_snapshot_copy(self):
        artifact_dir = self.outside_tmp / "artifacts"
        run_dir, stderr, _ = self._failed_start(artifact_dir)
        self.assertFalse((run_dir / "snapshot.qcow2").exists(), sorted(os.listdir(run_dir)))
        self.assertIn(
            "Removed the failed pass's 4096-byte snapshot copy {}".format(
                run_dir / "snapshot.qcow2"
            ),
            stderr,
        )
        # The log and the rest of the run directory stay for inspection.
        self.assertTrue((run_dir / "hermit.log").exists())

    def test_a_failure_after_the_start_also_removes_the_snapshot_copy(self):
        run_dir = self.outside_tmp / "run.after"
        run_dir.mkdir()
        (run_dir / "snapshot.qcow2").write_bytes(b"q" * 100)
        (run_dir / "hermit.log").write_text("advance timed out\n")

        def start(program):
            program.run_dir = run_dir
            return program

        stderr = io.StringIO()
        with mock.patch.object(dh.HermitGuestProgram, "start", start):
            with contextlib.redirect_stderr(stderr):
                with self.assertRaisesRegex(TimeoutError, "^marker$"):
                    with dh.program_from_hermit(mock.Mock()):
                        raise TimeoutError("marker")
        self.assertFalse((run_dir / "snapshot.qcow2").exists())
        self.assertIn("  advance timed out\n", stderr.getvalue())

    def test_a_pass_that_succeeds_keeps_its_files_and_prints_nothing(self):
        # Positive control: the README promises a passing run's working copy.
        run_dir = self.outside_tmp / "run.ok"
        run_dir.mkdir()
        (run_dir / "snapshot.qcow2").write_bytes(b"q" * 100)
        (run_dir / "hermit.log").write_text("fine\n")

        def start(program):
            program.run_dir = run_dir
            return program

        stderr = io.StringIO()
        with mock.patch.object(dh.HermitGuestProgram, "start", start):
            with contextlib.redirect_stderr(stderr):
                with dh.program_from_hermit(mock.Mock()):
                    pass
        self.assertTrue((run_dir / "snapshot.qcow2").exists())
        self.assertEqual(stderr.getvalue(), "")

    def test_a_pass_that_fails_before_hermit_starts_prints_no_log(self):
        # No run directory yet: nothing to show and nothing to remove.
        config = self._config(self.outside_tmp / "artifacts")
        config.kernel.unlink()
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            with self.assertRaises(FileNotFoundError):
                with dh.program_from_hermit(config):
                    self.fail("the pass must not start")
        self.assertEqual(stderr.getvalue(), "")


class LogTailTest(unittest.TestCase):
    def setUp(self):
        self.directory = _directory(self, "/var/tmp", "drgn-log-tail-")

    def test_only_the_last_lines_are_returned(self):
        log = self.directory / "hermit.log"
        log.write_text("".join("line {}\n".format(number) for number in range(50)))
        self.assertEqual(
            dh._log_tail(log, 3, 1 << 20), ["line 47", "line 48", "line 49"]
        )

    def test_a_line_cut_by_the_byte_limit_is_left_out(self):
        log = self.directory / "hermit.log"
        log.write_text("a" * 100 + "\n" + "b" * 10 + "\n" + "c" * 10 + "\n")
        # The last 30 bytes start inside the run of a's.
        self.assertEqual(dh._log_tail(log, 40, 30), ["b" * 10, "c" * 10])

    def test_a_missing_log_is_none_and_an_empty_log_has_no_lines(self):
        self.assertIsNone(dh._log_tail(self.directory / "absent.log", 40, 1024))
        empty = self.directory / "empty.log"
        empty.write_bytes(b"")
        self.assertEqual(dh._log_tail(empty, 40, 1024), [])


class ArtifactDirectoryDocumentationTest(unittest.TestCase):
    DEMO07_DIR = Path(__file__).resolve().parent.parent / "07-drgn-kernel"

    def test_the_readme_controls_table_documents_the_artifact_directory(self):
        readme = (self.DEMO07_DIR / "README.md").read_text()
        rows = [line for line in readme.splitlines() if line.startswith("| `DEMO07_ARTIFACTS` |")]
        self.assertEqual(len(rows), 1, "no controls-table row for DEMO07_ARTIFACTS")
        self.assertIn("`target/demos/07-drgn-kernel`", rows[0])
        self.assertIn("--tmp=/tmp", rows[0])

    def test_the_usage_lists_the_artifact_directory(self):
        usage = subprocess.run(
            ["bash", str(self.DEMO07_DIR / "run.sh"), "--help"],
            capture_output=True,
            text=True,
            check=True,
            timeout=30,
        ).stdout
        self.assertRegex(usage, r"(?m)^  DEMO07_ARTIFACTS=/path +\S")


if __name__ == "__main__":
    unittest.main()

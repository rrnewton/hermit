#!/usr/bin/env python3
"""Protocol tests for Demo 7 command-disk resume."""

import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

LIB_DIR = Path(__file__).resolve().parent.parent / "lib"
sys.path.insert(0, str(LIB_DIR))

import drgn_hermit as dh  # noqa: E402


class FakeQmp:
    def __init__(self):
        self.commands = []

    def execute(self, command):
        self.commands.append(command)

    def status(self):
        return "paused"


class CommandDiskProtocolTest(unittest.TestCase):
    def test_command_image_is_fixed_size_and_contains_the_advance(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            image = Path(directory) / "guest-command.img"
            dh._write_command_image(image, "echo deterministic")
            payload = image.read_bytes()

        prefix = b"echo deterministic\n"
        self.assertEqual(len(payload), dh.COMMAND_IMAGE_BYTES)
        self.assertTrue(payload.startswith(prefix))
        self.assertEqual(
            payload[len(prefix) :], b"\0" * (dh.COMMAND_IMAGE_BYTES - len(prefix))
        )

    def test_start_uses_the_bounded_qmp_path_before_launch(self):
        class StopBeforeLaunch(Exception):
            pass

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "bin").mkdir()
            for name in ("bin/safehermit", "selected-safehermit", "hermit", "qemu", "kernel", "initrd", "snapshot", "vmlinux"):
                (root / name).touch()
            config = dh.GuestConfig(
                root=root, hermit=root / "hermit", qemu=root / "qemu",
                kernel=root / "kernel", initrd=root / "initrd", vmlinux=root / "vmlinux",
                snapshot_disk=root / "snapshot", snapshot_name="hermit-boot",
                advance_command="echo deterministic", artifact_dir=root / ("long" * 30),
            )
            program = dh.HermitGuestProgram(config)
            relocated = root / "short.sock"
            with mock.patch.dict(dh.os.environ, {"SAFEHERMIT": str(root / "selected-safehermit")}), mock.patch.object(
                dh, "ensure_vmlinux", return_value=config.vmlinux
            ), mock.patch.object(
                dh, "make_socket_path", return_value=relocated
            ) as socket_path, mock.patch.object(
                dh.subprocess, "Popen", side_effect=StopBeforeLaunch
            ) as launch:
                with self.assertRaises(StopBeforeLaunch):
                    program.start()
            preferred = program.run_dir / "qmp.sock"
            self.assertGreater(len(str(preferred).encode()), 107)
            socket_path.assert_called_once_with(preferred, "drgn")
            self.assertIn("unix:{},server=on,wait=off".format(relocated), launch.call_args.args[0])
            self.assertEqual(program._qmp_socket, relocated)
            self.assertEqual(launch.call_args.args[0][0], str(root / "selected-safehermit"))

    def test_close_removes_the_recorded_qmp_socket(self):
        with tempfile.TemporaryDirectory() as directory:
            recorded = Path(directory) / "recorded.sock"
            other = Path(directory) / "other.sock"
            recorded.touch()
            other.touch()
            program = dh.HermitGuestProgram(SimpleNamespace())
            program._qmp_socket = recorded
            program.close()
            self.assertFalse(recorded.exists())
            self.assertTrue(other.exists())
            self.assertIsNone(program._qmp_socket)

    def test_advance_resumes_preloaded_disk_without_serial_injection(self):
        qmp = FakeQmp()
        program = dh.HermitGuestProgram(
            SimpleNamespace(advance_command="echo deterministic")
        )
        program._frozen = True
        program._qmp = qmp
        program._qemu_pid = 456
        program._tracer_tgid = 123
        program._serial_write_fd = 99
        program._wait_for_serial = mock.Mock()

        with mock.patch.object(dh.os, "write") as serial_write, mock.patch.object(
            dh.os, "kill"
        ), mock.patch.object(dh, "_freeze_exact_tracer", return_value=(456, 789)):
            program.advance("echo deterministic", b"done")

        serial_write.assert_not_called()
        program._wait_for_serial.assert_called_once_with(b"done")
        self.assertEqual(qmp.commands, ["cont", "stop"])
        self.assertTrue(program._frozen)
        self.assertEqual(program._tracer_tgid, 789)

    def test_advance_rejects_command_other_than_preloaded_disk(self):
        program = dh.HermitGuestProgram(SimpleNamespace(advance_command="expected"))
        program._frozen = True
        program._qmp = FakeQmp()
        program._qemu_pid = 456
        program._tracer_tgid = 123

        with self.assertRaisesRegex(ValueError, "preloaded command disk"):
            program.advance("different", b"done")

    def test_safehermit_cleanup_targets_the_unit_recorded_for_the_run(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            report = Path(directory) / "safehermit-report.txt"
            report.write_text("safehermit: unit=safehermit-test-123\n")
            with mock.patch.object(
                dh.subprocess, "run", return_value=SimpleNamespace(returncode=0)
            ) as run:
                stopped = dh._stop_safehermit_unit(report)

        self.assertTrue(stopped)
        run.assert_called_once_with(
            [
                "systemctl",
                "--user",
                "kill",
                "--signal=SIGKILL",
                "safehermit-test-123.service",
            ],
            check=False,
            stdout=dh.subprocess.DEVNULL,
            stderr=dh.subprocess.DEVNULL,
        )

    def test_safehermit_cleanup_refuses_an_untrusted_unit_name(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            report = Path(directory) / "safehermit-report.txt"
            report.write_text("safehermit: unit=bad;unit\n")
            with mock.patch.object(dh.subprocess, "run") as run:
                stopped = dh._stop_safehermit_unit(report)

        self.assertFalse(stopped)
        run.assert_not_called()

    def test_close_lets_safehermit_finalize_before_process_group_fallback(self):
        program = dh.HermitGuestProgram(SimpleNamespace())
        program._safehermit_report = Path("report")
        program._process_group = 456
        program._process = mock.Mock(returncode=0)
        program._process.poll.return_value = 0

        with mock.patch.object(dh, "_stop_safehermit_unit", return_value=True), mock.patch.object(
            dh.os, "killpg"
        ) as killpg, mock.patch.object(dh, "report_safehermit"):
            program.close()

        program._process.wait.assert_called_once_with(timeout=10)
        killpg.assert_not_called()


if __name__ == "__main__":
    unittest.main()

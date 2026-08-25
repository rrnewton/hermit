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


if __name__ == "__main__":
    unittest.main()

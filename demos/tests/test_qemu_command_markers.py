#!/usr/bin/env python3
"""Only the guest's own command frame may end a QEMU resume.

Demo 6 resumes the demo 5 boot snapshot. The guest's /init (built by
demos/lib/qemu-assets.sh) reads a command from a small disk and runs it, and
the QEMU controller (demos/lib/qemu_controller.py) watches the serial
transcript for the command to finish before it saves a snapshot and stops
QEMU; demo 6 then takes the command's output from the same transcript. /init
frames the output: a BEGIN line naming the frame format, every line of the
command's stdout and stderr with "| " in front of it, and an END line carrying
the command's exit status.

Before the frame existed, /init printed a bare END marker after the command and
the controller waited for that marker anywhere in the transcript. A command
that printed the marker itself, for example `echo __HERMIT_COMMAND_END__;
sleep 1000000`, was then taken as finished: the controller saved the snapshot
and quit while the command was still running, and demo 6 cut the command's
output at the marker.

The guest-side tests run /init's frame lines, taken from qemu-assets.sh, under
the guest's BusyBox shell when it is installed (qemu-assets.sh copies the
host's BusyBox into the initramfs) and under the host's sh. The controller
tests run run_controller on a thread with QEMU and its control socket replaced
and feed it a transcript. The demo 6 tests run resume_once with Hermit
replaced by a stand-in that leaves a transcript behind.
"""

import argparse
import contextlib
import hashlib
import io
import os
import runpy
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import types
import unittest
from pathlib import Path
from typing import List, Optional
from unittest import mock

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
sys.path.insert(0, str(LIB_DIR))

import qemu_controller as qc  # noqa: E402

# The frame lines, written out here so that a test states the format rather
# than reading it back from the code under test.
FRAME_BEGIN = b"__HERMIT_COMMAND_BEGIN__ format=2"
# The file /init sends the command's output to; the tests use a scratch path.
GUEST_OUTPUT_FILE = "/tmp/.hermit-command-output"
# Seconds a shell running /init's frame lines may take; every test command
# finishes far sooner.
SHELL_TIMEOUT = 30
# Seconds the controller may take to react to a transcript it can read.
CONTROLLER_TIMEOUT = 10.0
POST_SNAPSHOT_NAME = "command-0123456789abcdef"
SAVE_AND_QUIT = [
    ("human-monitor-command", "command-line", "savevm " + POST_SNAPSHOT_NAME),
    ("quit",),
]
# A command that prints the bare END marker before it finishes.
IMPERSONATION_COMMAND = "echo __HERMIT_COMMAND_END__; echo FINISHED; exit 3"
IMPERSONATION_OUTPUT = b"__HERMIT_COMMAND_END__\nFINISHED\n"


def _init_frame_lines() -> List[str]:
    """/init's lines from the BEGIN echo through the END echo."""
    lines = (LIB_DIR / "qemu-assets.sh").read_text().splitlines()
    starts = [
        index
        for index, line in enumerate(lines)
        if line.startswith('echo "__HERMIT_COMMAND_BEGIN__')
    ]
    ends = [
        index
        for index, line in enumerate(lines)
        if line.startswith('echo "__HERMIT_COMMAND_END__')
    ]
    if len(starts) != 1 or len(ends) != 1 or ends[0] < starts[0]:
        raise AssertionError(
            "expected one BEGIN echo and, after it, one END echo in "
            "qemu-assets.sh; found BEGIN at {} and END at {}".format(starts, ends)
        )
    return lines[starts[0] : ends[0] + 1]


def _guest_busybox() -> Optional[str]:
    """The BusyBox qemu-assets.sh copies into the initramfs, if installed."""
    candidate = os.environ.get("BUSYBOX") or shutil.which("busybox")
    if candidate and os.access(candidate, os.X_OK):
        return candidate
    return None


def _preferred_shell() -> str:
    return "busybox" if _guest_busybox() else "sh"


def _run_frame(directory: Path, command: str, shell: str) -> bytes:
    """Run /init's frame lines with CMD set to ``command``; return the transcript.

    ``shell`` is "busybox" (BusyBox ash, with `sh` on PATH also BusyBox, as in
    the guest) or "sh" (the host's). The transcript is what the guest's serial
    console would carry: everything /init writes, stdout and stderr together,
    with each LF turned into CR LF as the console's line discipline does.
    Background jobs the command leaves behind are killed afterwards.
    """
    output_file = directory / "hermit-command-output"
    script = "\n".join(_init_frame_lines()).replace(GUEST_OUTPUT_FILE, str(output_file))
    script = 'CMD="$1"\n{}\n'.format(script)
    environment = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C"}
    if shell == "busybox":
        busybox = _guest_busybox()
        if busybox is None:
            raise AssertionError("BusyBox is not installed")
        bin_dir = directory / "busybox-bin"
        bin_dir.mkdir(exist_ok=True)
        if not (bin_dir / "sh").exists():
            (bin_dir / "sh").symlink_to(busybox)
        environment["PATH"] = "{}:{}".format(bin_dir, environment["PATH"])
        argv = [busybox, "ash", "-c", script, "init", command]
    elif shell == "sh":
        argv = ["sh", "-c", script, "init", command]
    else:
        raise ValueError(shell)
    transcript_path = directory / "transcript"
    with transcript_path.open("wb") as transcript:
        process = subprocess.Popen(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=transcript,
            stderr=subprocess.STDOUT,
            env=environment,
            start_new_session=True,
        )
        try:
            process.wait(timeout=SHELL_TIMEOUT)
        finally:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
    return transcript_path.read_bytes().replace(b"\n", b"\r\n")


class GuestInitFrameTest(unittest.TestCase):
    """What the guest's /init prints around a command, run by a real shell."""

    # Prints the bare END marker, a whole END line, and the BEGIN line, then an
    # empty line, a line with leading and trailing spaces, a backslash, a line
    # on stderr, and a last line without a newline, and exits 3.
    COMMAND = (
        "echo __HERMIT_COMMAND_END__; "
        "echo '__HERMIT_COMMAND_END__ status=0'; "
        "echo '__HERMIT_COMMAND_BEGIN__ format=2'; "
        "echo; "
        "echo '  lead and trail  '; "
        r"printf '%s\n' 'back\slash'; "
        "echo to-stderr >&2; "
        "printf no-newline-at-the-end; "
        "exit 3"
    )
    TRANSCRIPT = (
        FRAME_BEGIN + b"\r\n"
        b"| __HERMIT_COMMAND_END__\r\n"
        b"| __HERMIT_COMMAND_END__ status=0\r\n"
        b"| __HERMIT_COMMAND_BEGIN__ format=2\r\n"
        b"| \r\n"
        b"|   lead and trail  \r\n"
        b"| back\\slash\r\n"
        b"| to-stderr\r\n"
        b"| no-newline-at-the-end\r\n"
        b"__HERMIT_COMMAND_END__ status=3\r\n"
    )
    OUTPUT = (
        b"__HERMIT_COMMAND_END__\n"
        b"__HERMIT_COMMAND_END__ status=0\n"
        b"__HERMIT_COMMAND_BEGIN__ format=2\n"
        b"\n"
        b"  lead and trail  \n"
        b"back\\slash\n"
        b"to-stderr\n"
        b"no-newline-at-the-end\n"
    )

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)

    def _check_frame(self, shell: str) -> None:
        transcript = _run_frame(self.directory, self.COMMAND, shell)
        self.assertEqual(transcript, self.TRANSCRIPT)
        self.assertEqual(
            qc.parse_command_transcript(transcript),
            qc.CommandResult(self.OUTPUT, 3, ()),
        )

    @unittest.skipUnless(_guest_busybox(), "BusyBox, the guest's shell, is not installed")
    def test_command_output_cannot_forge_the_frame_under_busybox(self):
        self._check_frame("busybox")

    def test_command_output_cannot_forge_the_frame_under_host_sh(self):
        self._check_frame("sh")

    def test_a_background_job_does_not_delay_the_end_line(self):
        # The command's shell exits at once and leaves `sleep 60` running with
        # the output file open. /init reads a file, not a pipe, so it does not
        # wait for that job: the frame ends long before SHELL_TIMEOUT.
        started = time.monotonic()
        transcript = _run_frame(
            self.directory, "sleep 60 & echo started", _preferred_shell()
        )
        self.assertLess(time.monotonic() - started, SHELL_TIMEOUT)
        self.assertEqual(
            transcript,
            FRAME_BEGIN + b"\r\n| started\r\n__HERMIT_COMMAND_END__ status=0\r\n",
        )


class _FakeQemu:
    """Stands in for the QEMU process the controller starts."""

    def wait(self, timeout=None):
        return 0

    def poll(self):
        return 0


class _ControllerRun:
    """run_controller in resume mode on a thread, without QEMU.

    QEMU, its control socket, and process cleanup are replaced; the controller
    tails ``serial_log`` as it would QEMU's console file. ``calls`` lists the
    control-socket commands it sends, ``idle`` is set whenever it has read the
    whole transcript and sleeps waiting for more, and ``outcome`` holds its
    return value or exception once it ends.
    """

    def __init__(
        self, test: unittest.TestCase, serial_log: Path, no_save_snapshot: bool = False
    ) -> None:
        self.calls: list = []
        self.outcome: dict = {}
        self.idle = threading.Event()
        directory = serial_log.parent
        arguments = argparse.Namespace(
            mode="resume",
            qemu="qemu-system-x86_64",
            qmp_socket=directory / "qmp.sock",
            command_image=directory / "guest-command.img",
            serial_log=serial_log,
            disk=directory / "hermit-snapshot.qcow2",
            kernel=directory / "bzImage",
            initrd=directory / "initramfs.cpio.gz",
            snapshot_name="hermit-boot",
            timeout=CONTROLLER_TIMEOUT,
            post_snapshot_name=POST_SNAPSHOT_NAME,
            no_save_snapshot=no_save_snapshot,
        )

        def record_qmp(socket, command, *arguments, **keywords):
            self.calls.append((command,) + arguments)

        def sleep(seconds):
            self.idle.set()
            time.sleep(0.01)

        replacements = (
            ("subprocess", types.SimpleNamespace(Popen=lambda command, env: _FakeQemu())),
            ("wait_for_socket", lambda path, process, timeout: None),
            ("qmp_command", record_qmp),
            ("stop_process", lambda process: None),
            ("time", types.SimpleNamespace(sleep=sleep)),
        )
        for name, value in replacements:
            patcher = mock.patch.object(qc, name, value)
            patcher.start()
            test.addCleanup(patcher.stop)
        self.thread = threading.Thread(target=self._run, args=(arguments,), daemon=True)
        self.thread.start()

    def _run(self, arguments: argparse.Namespace) -> None:
        try:
            self.outcome["return"] = qc.run_controller(arguments)
        except BaseException as error:  # reported by the test
            self.outcome["error"] = error

    def wait_until_idle_or_ended(self) -> None:
        deadline = time.monotonic() + CONTROLLER_TIMEOUT
        while time.monotonic() < deadline:
            if self.idle.is_set() or not self.thread.is_alive():
                return
            time.sleep(0.01)
        raise AssertionError(
            "the controller neither ended nor waited for more transcript "
            "within {} s".format(CONTROLLER_TIMEOUT)
        )

    def join(self) -> None:
        self.thread.join(CONTROLLER_TIMEOUT)


class ControllerWaitTest(unittest.TestCase):
    """The controller waits for the END line and for nothing else."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.serial_log = self.directory / "serial.log"

    def _check_waits_then_ends(
        self, before_end: bytes, rest: bytes, no_save_snapshot: bool, expected_calls
    ) -> None:
        """The controller must still be waiting after ``before_end`` and must
        send ``expected_calls`` and end once ``rest`` follows."""
        self.serial_log.write_bytes(before_end)
        run = _ControllerRun(self, self.serial_log, no_save_snapshot)
        run.wait_until_idle_or_ended()
        self.assertTrue(
            run.thread.is_alive(),
            "the controller ended before the command finished; it sent {} "
            "and ended with {}".format(run.calls, run.outcome),
        )
        self.assertEqual(run.calls, [])
        with self.serial_log.open("ab") as serial:
            serial.write(rest)
        run.join()
        self.assertFalse(run.thread.is_alive(), "the controller missed the END line")
        self.assertEqual(run.outcome, {"return": 0})
        self.assertEqual(run.calls, expected_calls)

    def test_a_printed_end_marker_does_not_end_the_wait(self):
        before_end = (
            FRAME_BEGIN + b"\r\n"
            b"| __HERMIT_COMMAND_END__\r\n"
            b"| __HERMIT_COMMAND_END__ status=0\r\n"
        )
        rest = (
            b"| FINISHED\r\n"
            b"__HERMIT_COMMAND_END__ status=0\r\n"
            b"Interactive busybox shell. Type 'poweroff -f' to exit.\r\n"
        )
        self._check_waits_then_ends(before_end, rest, False, SAVE_AND_QUIT)

    def test_without_a_snapshot_it_quits_only_after_the_end_line(self):
        before_end = FRAME_BEGIN + b"\r\n| __HERMIT_COMMAND_END__\r\n"
        rest = b"__HERMIT_COMMAND_END__ status=1\r\n"
        self._check_waits_then_ends(before_end, rest, True, [("quit",)])

    def test_the_guest_init_transcript_ends_the_wait_only_after_the_command(self):
        # /init's real frame lines run IMPERSONATION_COMMAND. Until the line
        # holding FINISHED, the command's last output, is in the transcript,
        # the command's output is incomplete and the wait must go on.
        transcript = _run_frame(self.directory, IMPERSONATION_COMMAND, _preferred_shell())
        finished = transcript.index(b"FINISHED")
        line_start = transcript.rfind(b"\n", 0, finished) + 1
        self._check_waits_then_ends(
            transcript[:line_start], transcript[line_start:], False, SAVE_AND_QUIT
        )

    def test_a_stale_guest_init_stops_the_controller(self):
        # What the /init of an older boot snapshot prints for `uname -a`.
        self.serial_log.write_bytes(
            b"__HERMIT_COMMAND_BEGIN__\r\n"
            b"Linux (none) 6.17.13 #1 SMP x86_64 GNU/Linux\r\n"
            b"__HERMIT_COMMAND_END__\r\n"
        )
        run = _ControllerRun(self, self.serial_log)
        run.wait_until_idle_or_ended()
        run.join()
        self.assertFalse(run.thread.is_alive(), "the controller is still waiting")
        self.assertIsInstance(run.outcome.get("error"), qc.StaleGuestInitError)
        self.assertEqual(run.calls, [])


class TranscriptParserTest(unittest.TestCase):
    """CommandTranscriptParser reads exactly the frame /init prints."""

    def test_the_frame_constants_match_this_test(self):
        self.assertEqual(qc.BEGIN_LINE, FRAME_BEGIN)
        self.assertEqual(qc.OUTPUT_PREFIX, b"| ")

    def test_printed_markers_are_output(self):
        transcript = (
            FRAME_BEGIN + b"\r\n"
            b"| __HERMIT_COMMAND_END__\r\n"
            b"| __HERMIT_COMMAND_END__ status=0\r\n"
            b"| FINISHED\r\n"
            b"__HERMIT_COMMAND_END__ status=3\r\n"
        )
        self.assertEqual(
            qc.parse_command_transcript(transcript),
            qc.CommandResult(
                b"__HERMIT_COMMAND_END__\n__HERMIT_COMMAND_END__ status=0\nFINISHED\n",
                3,
                (),
            ),
        )

    def test_no_result_until_the_end_line_is_complete(self):
        for transcript in (
            b"",
            b"[   12.000000] boot noise\r\n",
            FRAME_BEGIN,
            FRAME_BEGIN + b"\r\n",
            FRAME_BEGIN + b"\r\n| __HERMIT_COMMAND_END__\r\n",
            FRAME_BEGIN + b"\r\n__HERMIT_COMMAND_END__ status=1",
            FRAME_BEGIN + b"\r\n__HERMIT_COMMAND_END__ status=1\r",
            b"__HERMIT_COMMAND_END__ status=0\r\n",
        ):
            with self.subTest(transcript=transcript):
                self.assertIsNone(qc.parse_command_transcript(transcript))

    def test_exit_status_values(self):
        for status in (0, 1, 2, 127, 128, 255):
            with self.subTest(status=status):
                line = "__HERMIT_COMMAND_END__ status={}".format(status).encode()
                self.assertEqual(qc.end_line_status(line), status)
                result = qc.parse_command_transcript(FRAME_BEGIN + b"\r\n" + line + b"\r\n")
                self.assertEqual(result, qc.CommandResult(b"", status, ()))
        for line in (
            b"__HERMIT_COMMAND_END__",
            b"__HERMIT_COMMAND_END__ status=",
            b"__HERMIT_COMMAND_END__ status=256",
            b"__HERMIT_COMMAND_END__ status=1000",
            b"__HERMIT_COMMAND_END__ status=007",
            b"__HERMIT_COMMAND_END__ status=00",
            b"__HERMIT_COMMAND_END__ status=-1",
            b"__HERMIT_COMMAND_END__ status=+3",
            b"__HERMIT_COMMAND_END__ status=3 ",
            b"__HERMIT_COMMAND_END__ status=3x",
            b"__HERMIT_COMMAND_END__  status=3",
            b" __HERMIT_COMMAND_END__ status=3",
            b"| __HERMIT_COMMAND_END__ status=3",
            b"__HERMIT_COMMAND_END__ status=3\r",
        ):
            with self.subTest(line=line):
                self.assertIsNone(qc.end_line_status(line))

    def test_unprefixed_lines_in_the_frame_are_kept_and_marked(self):
        transcript = (
            FRAME_BEGIN + b"\r\n"
            b"| one\r\n"
            b"[   12.345678] random: crng init done\r\n"
            b"__HERMIT_COMMAND_END__ status=256\r\n"
            b"__HERMIT_COMMAND_END__\r\n"
            b"| two\r\n"
            b"__HERMIT_COMMAND_END__ status=0\r\n"
        )
        self.assertEqual(
            qc.parse_command_transcript(transcript),
            qc.CommandResult(
                b"one\n"
                b"[console] [   12.345678] random: crng init done\n"
                b"[console] __HERMIT_COMMAND_END__ status=256\n"
                b"[console] __HERMIT_COMMAND_END__\n"
                b"two\n",
                0,
                (
                    b"[   12.345678] random: crng init done",
                    b"__HERMIT_COMMAND_END__ status=256",
                    b"__HERMIT_COMMAND_END__",
                ),
            ),
        )

    def test_lines_before_the_frame_are_ignored(self):
        transcript = (
            b"__HERMIT_COMMAND_END__ status=0\r\n"
            b"| not the command's\r\n"
            + FRAME_BEGIN
            + b" \r\n"
            + FRAME_BEGIN
            + b"\r\n| inside\r\n__HERMIT_COMMAND_END__ status=5\r\n"
        )
        self.assertEqual(
            qc.parse_command_transcript(transcript),
            qc.CommandResult(b"inside\n", 5, ()),
        )

    def test_a_stale_guest_init_is_named(self):
        with self.assertRaises(qc.StaleGuestInitError) as caught:
            qc.parse_command_transcript(
                b"[   12.000000] boot noise\r\n"
                b"__HERMIT_COMMAND_BEGIN__\r\n"
                b"Linux (none) 6.17.13 #1 SMP x86_64 GNU/Linux\r\n"
                b"__HERMIT_COMMAND_END__\r\n"
            )
        self.assertEqual(str(caught.exception), qc.STALE_GUEST_INIT_MESSAGE)
        self.assertIn("demos/clean.sh", qc.STALE_GUEST_INIT_MESSAGE)
        self.assertIn("demo 5", qc.STALE_GUEST_INIT_MESSAGE)
        # Inside a current frame the bare BEGIN line is output, not a stale init.
        self.assertEqual(
            qc.parse_command_transcript(
                FRAME_BEGIN + b"\r\n"
                b"| __HERMIT_COMMAND_BEGIN__\r\n"
                b"__HERMIT_COMMAND_BEGIN__\r\n"
                b"__HERMIT_COMMAND_END__ status=0\r\n"
            ),
            qc.CommandResult(
                b"__HERMIT_COMMAND_BEGIN__\n[console] __HERMIT_COMMAND_BEGIN__\n",
                0,
                (b"__HERMIT_COMMAND_BEGIN__",),
            ),
        )

    def test_feeding_in_pieces_matches_the_whole(self):
        transcript = (
            FRAME_BEGIN + b"\r\n"
            b"| a\r\n"
            b"| b\r\n"
            b"__HERMIT_COMMAND_END__ status=7\r\n"
            b"Interactive busybox shell. Type 'poweroff -f' to exit.\r\n"
        )
        whole = qc.parse_command_transcript(transcript)
        self.assertEqual(whole, qc.CommandResult(b"a\nb\n", 7, ()))
        end = transcript.index(b"status=7\r\n") + len(b"status=7\r\n")
        parser = qc.CommandTranscriptParser()
        for index in range(len(transcript)):
            result = parser.feed(transcript[index : index + 1])
            if index + 1 < end:
                self.assertIsNone(result, "result after byte {}".format(index))
            else:
                self.assertEqual(result, whole, "result after byte {}".format(index))
        for split in range(len(transcript) + 1):
            with self.subTest(split=split):
                parser = qc.CommandTranscriptParser()
                parser.feed(transcript[:split])
                self.assertEqual(parser.feed(transcript[split:]), whole)

    def test_only_one_carriage_return_before_the_newline_is_removed(self):
        transcript = (
            FRAME_BEGIN + b"\r\n"
            b"| a\rb\r\r\n"
            b"| plain\n"
            b"__HERMIT_COMMAND_END__ status=0\r\n"
        )
        self.assertEqual(
            qc.parse_command_transcript(transcript),
            qc.CommandResult(b"a\rb\r\nplain\n", 0, ()),
        )


class Demo6ResumeTest(unittest.TestCase):
    """Demo 6 reports the command's whole output and its exit status."""

    @classmethod
    def setUpClass(cls):
        cls.demo6 = runpy.run_path(str(DEMO_DIR / "06-qemu-resume" / "run.py"))

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)

    def _resume(self, transcript: bytes, command: str):
        """Run resume_once (without saving a snapshot) with Hermit replaced.

        The stand-in for Hermit leaves ``transcript`` as the serial log and
        exits 0; demo 5's snapshot, the demo lock, and the reference run are
        replaced too. Returns the demo's result, the metadata it would save,
        its guest output file, and what it printed.
        """
        resume_once = self.demo6["resume_once"]
        assets = self.directory / "assets"
        assets.mkdir()
        boot_disk = assets / "hermit-boot.qcow2"
        boot_disk.write_bytes(b"stand-in for the demo 5 boot snapshot")
        saved = {}

        def finish_hermit(process, timeout, **keywords):
            (assets / "serial.log").write_bytes(transcript)
            return 0

        def record_metadata(run_dir, disk, info_log, extra):
            saved["run_dir"] = Path(run_dir)
            saved["extra"] = dict(extra)
            return "metadata of this run"

        copier = mock.Mock()
        copier.is_alive.return_value = False
        replacements = {
            "ASSETS": assets,
            "QEMU": "qemu-system-x86_64",
            "SNAPSHOT_DISK": assets / "hermit-snapshot.qcow2",
            "BOOT_SNAPSHOT_DISK": boot_disk,
            "check_dependencies": lambda root: "dependency check replaced by the test",
            "hermit_binary": lambda: "hermit",
            "ensure_boot_snapshot": lambda: None,
            "acquire_demo_lock": lambda path: None,
            "release_demo_lock": lambda handle: None,
            "stage_guest_controller": lambda destination: destination,
            "subprocess": types.SimpleNamespace(
                Popen=lambda command, **keywords: mock.Mock(),
                DEVNULL=subprocess.DEVNULL,
                PIPE=subprocess.PIPE,
                STDOUT=subprocess.STDOUT,
            ),
            "start_output_copier": lambda process, log: copier,
            "wait_for_process": finish_hermit,
            "extract_info_tail": lambda path: [],
            "save_metadata": record_metadata,
            "load_anchor": lambda command_root: None,
            "save_anchor": lambda command_root, current: command_root / "anchor",
            "stop_process": lambda process: None,
        }
        printed = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", write_through=True)
        with mock.patch.dict(resume_once.__globals__, replacements), mock.patch.dict(
            os.environ
        ), contextlib.redirect_stdout(printed):
            result = resume_once(command, False)
        output = (saved["run_dir"] / "guest-output.txt").read_bytes()
        return result, saved["extra"], output, printed.buffer.getvalue()

    def test_the_whole_output_and_the_exit_status_are_reported(self):
        transcript = _run_frame(self.directory, IMPERSONATION_COMMAND, _preferred_shell())
        result, extra, output, printed = self._resume(transcript, IMPERSONATION_COMMAND)
        self.assertEqual(output, IMPERSONATION_OUTPUT)
        self.assertEqual(
            extra["guest_output_sha256"], hashlib.sha256(IMPERSONATION_OUTPUT).hexdigest()
        )
        self.assertEqual(extra.get("guest_exit_status"), 3)
        self.assertIn(b"Guest command exit status: 3\n", printed)
        self.assertEqual(result, "FIRST RUN SAVED")

    def test_a_transcript_without_its_end_line_is_refused(self):
        transcript = FRAME_BEGIN + b"\r\n| partial output\r\n"
        with self.assertRaises(RuntimeError) as caught:
            self._resume(transcript, "echo partial output; sleep 1000000")
        self.assertIn("holds no complete command frame", str(caught.exception))

    def test_a_failed_run_names_a_stale_guest_init(self):
        failed_run_message = self.demo6["failed_run_message"]
        serial_log = self.directory / "serial.log"
        # No serial log: a run stopped before QEMU wrote one.
        self.assertEqual(
            failed_run_message(124, serial_log), "Hermit/QEMU exited with status 124"
        )
        serial_log.write_bytes(FRAME_BEGIN + b"\r\n| still running\r\n")
        self.assertEqual(
            failed_run_message(124, serial_log), "Hermit/QEMU exited with status 124"
        )
        serial_log.write_bytes(b"__HERMIT_COMMAND_BEGIN__\r\nLinux (none)\r\n")
        self.assertEqual(
            failed_run_message(1, serial_log),
            "Hermit/QEMU exited with status 1: " + qc.STALE_GUEST_INIT_MESSAGE,
        )


if __name__ == "__main__":
    unittest.main()

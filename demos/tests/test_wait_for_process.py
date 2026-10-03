#!/usr/bin/env python3
"""Tests for the bounds ``demo_common.wait_for_process`` puts on a child.

The QEMU demos launch Hermit with ``start_new_session=True`` and wait for it
with ``wait_for_process``. Two bounds can stop a run: the log cap (the INFO log
grew past ``max_log_bytes``) and the wall-clock timeout. These tests launch a
real child that leads its own process group and has a descendant, so they
check what a demo depends on: the error names the bound that fired, and the
whole group, descendant included, is gone. They need no Hermit, QEMU or
kernel. Run directly (``python3 demos/tests/test_wait_for_process.py``) or via
``make -C demos test``.
"""

import contextlib
import io
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Tuple

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402

# A child that starts a descendant in its own process group, records the
# descendant's PID, and then either appends to a log forever or just waits.
CHILD = r"""
import subprocess, sys, time
from pathlib import Path
descendant = subprocess.Popen(["sleep", "1000"])
Path(sys.argv[1]).write_text(str(descendant.pid))
log = Path(sys.argv[2]) if len(sys.argv) > 2 else None
while True:
    if log is not None:
        with log.open("ab") as handle:
            handle.write(b"x" * 1024)
    time.sleep(0.01)
"""


def _gone(pid: int) -> bool:
    """Whether ``pid`` has exited (a zombie awaiting its reaper counts)."""
    try:
        state = Path("/proc/{}/stat".format(pid)).read_text().rsplit(")", 1)[1].split()[0]
    except (FileNotFoundError, ProcessLookupError, IndexError):
        return True
    return state in ("Z", "X")


class WaitForProcessBoundsTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.pid_file = self.directory / "descendant.pid"

    def _launch(self, *arguments: str) -> Tuple[subprocess.Popen, int]:
        process = subprocess.Popen(
            [sys.executable, "-c", CHILD, str(self.pid_file), *arguments],
            start_new_session=True,
        )
        # Whatever the test does, leave nothing running.
        self.addCleanup(dc.stop_process, process)
        deadline = time.monotonic() + 30
        while not self.pid_file.is_file() or not self.pid_file.read_text():
            self.assertLess(time.monotonic(), deadline, "the child never started its descendant")
            time.sleep(0.01)
        descendant = int(self.pid_file.read_text())
        self.addCleanup(self._kill_if_running, descendant)
        return process, descendant

    @staticmethod
    def _kill_if_running(pid: int) -> None:
        if not _gone(pid):
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, 9)

    def _assert_descendant_stops(self, descendant: int) -> None:
        # The descendant was signalled with its group; allow its reaper a moment.
        deadline = time.monotonic() + 5
        while not _gone(descendant) and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(_gone(descendant), "the child's descendant {} kept running".format(descendant))

    def test_a_log_past_its_cap_stops_the_group_and_names_the_cap(self):
        log = self.directory / "hermit-info.log"
        process, descendant = self._launch(str(log))
        with contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(dc.LogCapExceeded) as caught:
                dc.wait_for_process(process, timeout=60, log_path=log, max_log_bytes=4096)
        error = caught.exception
        self.assertIsInstance(error, RuntimeError)
        self.assertEqual(error.log_path, log)
        self.assertEqual(error.max_log_bytes, 4096)
        self.assertGreater(error.log_size, 4096)
        self.assertLess(error.elapsed, 60)
        self.assertEqual(
            str(error),
            "{} grew to {} bytes, past the 4096-byte log cap; the run was stopped".format(
                log, error.log_size
            ),
        )
        self.assertIsNotNone(process.poll(), "the child kept running past the log cap")
        self._assert_descendant_stops(descendant)


if __name__ == "__main__":
    unittest.main()

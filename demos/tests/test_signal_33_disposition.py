#!/usr/bin/env python3
"""Every `hermit` a QEMU demo starts must inherit the same signal 33 disposition.

Hermit passes the guest's `SigIgn` mask through from the process that started
`hermit` (https://github.com/rrnewton/hermit/issues/3441), and QEMU reads it,
so a difference between two launches shows up in the Hermit logs that demos 5
and 6 compare. See signal_33.settle_signal_33_disposition for the mechanism.

The child script below starts two children the way demos 5 and 6 start
`hermit` (subprocess.Popen, then an output-copier thread), and reports whether
each child inherited signal 33 as ignored. It is started with os.posix_spawn,
which in glibc leaves signal 33 ignored in the new process, the same starting
state that GNU make gives a recipe.
"""

import ast
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

LIB_DIR = Path(__file__).resolve().parent.parent / "lib"

# Bit n of SigIgn stands for signal n + 1, so signal 33 is bit 32.
CHILD_SCRIPT = r"""
import json
import subprocess
import sys
import threading

sys.path.insert(0, sys.argv[1])
import signal_33


def signal_33_ignored(status_text):
    for line in status_text.splitlines():
        if line.startswith("SigIgn:"):
            return bool(int(line.split()[1], 16) & (1 << 32))
    raise RuntimeError("no SigIgn line")


def launch():
    # The same Popen options that demos 5 and 6 use for `hermit`.
    process = subprocess.Popen(
        ["cat", "/proc/self/status"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    output = []
    copier = threading.Thread(target=lambda: output.append(process.stdout.read()))
    copier.start()
    process.wait()
    copier.join()
    return signal_33_ignored(output[0].decode())


with open("/proc/self/status") as status:
    started_ignored = signal_33_ignored(status.read())
if sys.argv[2] == "settle":
    signal_33.settle_signal_33_disposition()
first = launch()
second = launch()
print(json.dumps({"started_ignored": started_ignored, "first": first, "second": second}))
"""


def _spawn_child(mode):
    """Run CHILD_SCRIPT through os.posix_spawn and return its JSON report."""
    with tempfile.TemporaryDirectory() as scratch:
        script = Path(scratch) / "child.py"
        script.write_text(CHILD_SCRIPT)
        read_end, write_end = os.pipe()
        try:
            pid = os.posix_spawn(
                sys.executable,
                [sys.executable, str(script), str(LIB_DIR), mode],
                dict(os.environ, PYTHONDONTWRITEBYTECODE="1"),
                file_actions=[(os.POSIX_SPAWN_DUP2, write_end, 1)],
            )
        finally:
            os.close(write_end)
        with os.fdopen(read_end, "rb") as stream:
            output = stream.read()
        _, status = os.waitpid(pid, 0)
    if os.waitstatus_to_exitcode(status) != 0:
        raise AssertionError("child script failed: {!r}".format(output))
    return json.loads(output)


class Signal33DispositionTest(unittest.TestCase):
    def test_without_settling_the_second_launch_differs(self):
        """Control: the flip that settle_signal_33_disposition removes exists here."""
        report = _spawn_child("control")
        if not report["started_ignored"]:
            self.skipTest(
                "this C library does not leave signal 33 ignored in a "
                "posix_spawn child, so the starting state cannot be set up"
            )
        self.assertEqual(
            (report["first"], report["second"]),
            (True, False),
            "expected the launch before the first thread to inherit signal 33 "
            "ignored and the launch after it not to: {}".format(report),
        )

    def test_settling_gives_every_launch_the_same_signal_33_disposition(self):
        """Only signal 33 is checked: glibc never resets signal 32 for us.

        GNU make also leaves signal 32 ignored, and nothing the demo does
        changes that, so a launch through make and a launch from a shell can
        still differ in signal 32 (see the demo 5 and 6 READMEs).
        """
        report = _spawn_child("settle")
        if not report["started_ignored"]:
            self.skipTest(
                "this C library does not leave signal 33 ignored in a "
                "posix_spawn child, so the starting state cannot be set up"
            )
        self.assertEqual(
            (report["first"], report["second"]),
            (False, False),
            "every launch after settle_signal_33_disposition should run with "
            "signal 33 at its default disposition: {}".format(report),
        )

    def test_qemu_demos_settle_before_anything_else(self):
        """Demos 5 and 6 must settle before main() can start any `hermit`."""
        demos_dir = LIB_DIR.parent
        for script in ("05-qemu-boot/run.py", "06-qemu-resume/run.py"):
            with self.subTest(script=script):
                tree = ast.parse((demos_dir / script).read_text())
                mains = [
                    node
                    for node in tree.body
                    if isinstance(node, ast.FunctionDef) and node.name == "main"
                ]
                self.assertEqual(len(mains), 1, "expected one main() in " + script)
                first = mains[0].body[0]
                self.assertTrue(
                    isinstance(first, ast.Expr)
                    and isinstance(first.value, ast.Call)
                    and isinstance(first.value.func, ast.Name)
                    and first.value.func.id == "settle_signal_33_disposition",
                    "the first statement of main() in {} must be "
                    "settle_signal_33_disposition()".format(script),
                )

    def test_the_helper_stays_out_of_the_guest(self):
        """The guest runs copies of these sources; editing them changes the snapshot."""
        sys.path.insert(0, str(LIB_DIR))
        try:
            import demo_common
        finally:
            sys.path.remove(str(LIB_DIR))
        self.assertNotIn("signal_33.py", demo_common.GUEST_CONTROLLER_SOURCES)
        for name in demo_common.GUEST_CONTROLLER_SOURCES:
            with self.subTest(source=name):
                self.assertNotIn(
                    "settle_signal_33_disposition", (LIB_DIR / name).read_text()
                )


if __name__ == "__main__":
    unittest.main()

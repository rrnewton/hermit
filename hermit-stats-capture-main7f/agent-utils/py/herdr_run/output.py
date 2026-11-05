"""Writing to stdout and stderr with every failure accounted for.

``print`` raises a traceback when stdout cannot be written, and ``sys.stdout`` is ``None`` when the
descriptor was closed before the interpreter started. herdr-run promises one behaviour in both
editions instead: a stdout write that fails is reported as ``herdr-run: cannot write stdout:
<reason>`` with exit status 1, whether the reason is a closed descriptor, a full disk, or a reader
that went away. Diagnostics on stderr are best effort, because there is nowhere left to report a
failure to write them.

Every write is encoded as UTF-8 and goes straight to the descriptor with ``os.write``, so output
order matches the native edition's and nothing is ever left in a buffer. That last part is what
makes a failed write end the same way in both editions: a buffered stream keeps the bytes it could
not write, interpreter shutdown tries to flush them again, fails again, prints ``Exception ignored``
and exits 120 -- unless ``PYTHONUNBUFFERED`` happened to be set. Writing directly leaves shutdown
nothing to retry, whatever the environment says.
"""

from __future__ import annotations

import os
import sys
from typing import TextIO

from herdr_run.errors import HerdrRunError
from herdr_run.textfmt import os_error_text

__all__ = ["OutputError", "diagnostic", "stderr_bytes", "stdout", "stdout_bytes", "write_labelled"]


class OutputError(HerdrRunError):
    """Writing the command's own output failed."""

    exit_code = 1


def _encode(text: str) -> bytes:
    return text.encode("utf-8", errors="surrogateescape")


def _original_stream(descriptor: int) -> TextIO | None:
    """The stream the interpreter opened on ``descriptor`` (1 or 2) at start-up, if any."""
    return sys.__stdout__ if descriptor == 1 else sys.__stderr__


def _write_descriptor(descriptor: int, payload: bytes) -> None:
    """Write all of ``payload`` to ``descriptor`` with ``os.write``, through no buffer at all.

    A write can be SHORT without failing: past ``RLIMIT_FSIZE`` (SIGXFSZ is ignored, so the write
    fails with EFBIG instead of killing the process), or when a disk fills mid-write. So the
    remainder is written again until it is all written or the retry raises the error, as the native
    edition's ``write_all`` does; a write that makes no progress at all fails as ``write_all``
    reports it.
    """
    view = memoryview(payload)
    while view:
        try:
            written = os.write(descriptor, view)
        except InterruptedError:
            continue
        if not written:
            raise OSError("failed to write whole buffer")
        view = view[written:]


def _write(stream: TextIO | None, descriptor: int, payload: bytes) -> None:
    """Write all of ``payload`` to ``descriptor``, or through ``stream`` when it was substituted.

    The process's own stream (or none at all, when the descriptor was closed before the
    interpreter started) means the real descriptor: the bytes go to it directly, so a failed write
    leaves nothing buffered for shutdown to retry. A stream somebody put in its place -- a test
    capturing output -- is written through, with the same retry of a short write.
    """
    if stream is None or stream is _original_stream(descriptor):
        if stream is not None:
            stream.flush()
        _write_descriptor(descriptor, payload)
        return
    buffer = getattr(stream, "buffer", None)
    if buffer is None:
        stream.write(payload.decode("utf-8", errors="surrogateescape"))
        stream.flush()
        return
    stream.flush()
    view = memoryview(payload)
    while view:
        written = buffer.write(view)
        if not written:
            raise OSError("failed to write whole buffer")
        view = view[written:]
    buffer.flush()


def write_labelled(descriptor: int, payload: bytes, label: str) -> None:
    """Write bytes to stdout (1) or stderr (2), failing with ``cannot write <label>: <reason>``."""
    stream = sys.stdout if descriptor == 1 else sys.stderr
    try:
        _write(stream, descriptor, payload)
    except (OSError, ValueError) as exc:
        raise OutputError(f"cannot write {label}: {os_error_text(exc)}") from exc


def stdout_bytes(payload: bytes) -> None:
    """Write bytes to stdout, failing with ``cannot write stdout: <reason>``."""
    write_labelled(1, payload, "stdout")


def stdout(text: str) -> None:
    """Write text to stdout as UTF-8, failing with ``cannot write stdout: <reason>``."""
    stdout_bytes(_encode(text))


def stderr_bytes(payload: bytes) -> None:
    """Write a command's captured stderr, failing with ``cannot write stderr: <reason>``."""
    write_labelled(2, payload, "stderr")


def diagnostic(text: str) -> None:
    """Write one diagnostic line to stderr, ignoring failure."""
    try:
        _write(sys.stderr, 2, _encode(text + "\n"))
    except (OSError, ValueError):
        pass

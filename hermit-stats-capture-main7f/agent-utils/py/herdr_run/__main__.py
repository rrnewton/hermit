#!/usr/bin/env python3
"""Executable entry point.

Runnable three ways, all equivalent:
  * `python -m herdr_run`
  * the installed console script `herdr-run`
  * directly, as a script, via a symlink to this file (no install needed)

The last case is why we fix up sys.path here: when this file is executed directly, the directory
holding the package may not be importable yet.

SIGINT is given its default action as the very first statement, before anything else is imported,
exactly as importing the package does (see ``herdr_run/__init__.py``, which also describes the
part of the interpreter's own start-up nothing here can cover). Run as a script, this file is the
first code that runs, and the package cannot be imported until the path is fixed up, so it cannot
rely on the package's own reset. The interpreter's handler turns an interrupt into
``KeyboardInterrupt``; one that arrived while the package was still importing printed a traceback
where the native edition simply dies by the signal. An interrupt that slips in before the reset is
caught and re-delivered with the default action, so the process ends by SIGINT either way. A SIGINT
the caller ignored stays ignored.
"""

from __future__ import annotations

try:
    # Built in and already loaded: ``signal`` (and ``typing``) would import modules first.
    import _signal  # type: ignore[import-not-found,unused-ignore]

    if _signal.getsignal(_signal.SIGINT) is _signal.default_int_handler:
        _signal.signal(_signal.SIGINT, _signal.SIG_DFL)

    import os
    import sys
    from typing import NoReturn

    _PKG_PARENT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    if _PKG_PARENT not in sys.path:
        sys.path.insert(0, _PKG_PARENT)

    from herdr_run.cli import main  # noqa: E402  (import after the sys.path fixup, by design)
except KeyboardInterrupt:
    import os
    import _signal  # type: ignore[import-not-found,unused-ignore,no-redef]

    _signal.signal(_signal.SIGINT, _signal.SIG_DFL)
    _signal.raise_signal(_signal.SIGINT)
    # Not reached unless SIGINT is blocked; 130 is what a shell reports for death by SIGINT.
    os._exit(128 + _signal.SIGINT)


def _die_by_sigint() -> NoReturn:
    """End this process by SIGINT itself, the way an uncaught interrupt ends the native edition."""
    _signal.signal(_signal.SIGINT, _signal.SIG_DFL)
    _signal.raise_signal(_signal.SIGINT)
    os._exit(128 + _signal.SIGINT)


if __name__ == "__main__":
    # When the process must restart in UTF-8 mode it restarts from /proc/self/cmdline. Without
    # /proc it rebuilds the command instead, the same way it was started: as the module, or as
    # this script (through whatever symlink named it).
    _REEXEC = ["-m", "herdr_run"] if __spec__ is not None else [os.path.abspath(sys.argv[0])]
    try:
        _STATUS = main(reexec=_REEXEC)
    except KeyboardInterrupt:
        _die_by_sigint()
    raise SystemExit(_STATUS)

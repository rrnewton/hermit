"""herdr-run — run an allowlisted command in a terminal pane and get its real result back.

The pane belongs to a separate terminal server and is not a child of the calling process, so
whatever constrains that process does not constrain the command: not its network policy, not its
environment, not its lifetime. This package turns that into one narrow, allowlisted, cooperatively
audited interface rather than an ad-hoc pile of send-keys. It is not a same-user containment
boundary; the full trust model is included in the installed guide.

An agent whose sandbox blocks a destination it legitimately needs is ONE use of that, and the one
``herdr-run net-doctor`` checks. It is an example, not the definition.

Two design points are load-bearing and easy to undo by accident: the result is read back from FILES
rather than scraped off the terminal (the terminal has no exit code on it), and readiness keys on
the pane's foreground process group rather than on a prompt regex.

Importing this package gives SIGINT its default action, as the package's very first statement,
when the interpreter's own handler (the one that turns an interrupt into ``KeyboardInterrupt``) is
installed; a SIGINT the caller ignored stays ignored. Every entry point -- ``python -m herdr_run``,
the installed ``herdr-run`` console script (``herdr_run.cli:main``), and ``__main__.py`` run as a
script through a link to it, which does the same itself before it can import anything -- runs
this before any other package code, so an interrupt while herdr-run is starting ends the process
by the signal, as it ends the native edition, instead of printing a traceback. An interrupt that
arrives inside the reset is caught and delivered again with the default action.

What no package code can cover is the interpreter's own start-up before the first statement of
the entry point runs: its ``site`` and encoding set-up, and in the console script the wrapper's own
``import re`` and ``import sys``, which come before it imports the package. An interrupt there is
the interpreter's own, and it reports it its own way: a ``KeyboardInterrupt`` traceback and then
death by SIGINT, or, while ``site`` is still being imported, a fatal start-up error and exit
status 1. A program that imports this package as a library and wants ``KeyboardInterrupt`` back
reinstalls ``signal.default_int_handler`` itself.
"""

from __future__ import annotations

try:
    # ``_signal`` is built in and already loaded at start-up; ``signal`` would import ``enum``
    # first, which is the very window this closes.
    import _signal  # type: ignore[import-not-found,unused-ignore]

    if _signal.getsignal(_signal.SIGINT) is _signal.default_int_handler:
        _signal.signal(_signal.SIGINT, _signal.SIG_DFL)
except KeyboardInterrupt:  # pragma: no cover - exercised in a subprocess by the CLI tests
    import os
    import _signal  # type: ignore[import-not-found,unused-ignore,no-redef]

    _signal.signal(_signal.SIGINT, _signal.SIG_DFL)
    _signal.raise_signal(_signal.SIGINT)
    # Not reached unless SIGINT is blocked; 130 is what a shell reports for death by SIGINT.
    os._exit(128 + _signal.SIGINT)
del _signal

__all__ = ["__version__"]

__version__ = "0.4.0"

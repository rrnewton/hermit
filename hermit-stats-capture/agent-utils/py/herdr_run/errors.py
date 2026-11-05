"""Typed failure modes, one per distinguishable outcome.

Each maps to a stable process exit code so a caller can branch on WHY the wrapper failed without
parsing prose. In particular ``Refused`` (policy said no) must never be confused with ``PaneBusy``
(policy said yes, the terminal was not in a state where we could safely type) or with a non-zero
exit code from the command itself, which is not a herdr-run failure at all.
"""

from __future__ import annotations

__all__ = [
    "HerdrRunError",
    "ConfigError",
    "Refused",
    "HerdrUnavailable",
    "PaneBusy",
    "RunTimeout",
    "EXIT_CONFIG",
    "EXIT_REFUSED",
    "EXIT_UNAVAILABLE",
    "EXIT_BUSY",
    "EXIT_TIMEOUT",
]

#: Exit codes for wrapper failures. A wrapped command's status is passed through untouched, so a
#: child that itself returns one of these numbers is numerically ambiguous in raw-output mode; the
#: installed guide documents how JSON mode distinguishes that case.
EXIT_CONFIG = 78  # EX_CONFIG: the project config is malformed or unreadable.
EXIT_REFUSED = 77  # EX_NOPERM: the allowlist rejected the command. Nothing was executed.
# EX_UNAVAILABLE: Herdr server / workspace / pane could not be brought up. NOT a retry signal: it
# also carries the max_panes refusal, which only clears when somebody closes tabs. EXIT_BUSY is the
# only code that promises retrying is meaningful.
EXIT_UNAVAILABLE = 69
EXIT_BUSY = 75  # EX_TEMPFAIL: the pane was not idle. Nothing was executed; retry is meaningful.
EXIT_TIMEOUT = 76  # EX_PROTOCOL: the command was launched but did not finish in time.


class HerdrRunError(Exception):
    """Base class so a caller can catch every herdr-run failure with one except clause."""

    exit_code = 1


class ConfigError(HerdrRunError):
    """The per-project YAML config is malformed or unreadable."""

    exit_code = EXIT_CONFIG


class Refused(HerdrRunError):
    """The allowlist rejected the command. NOTHING was sent to any pane.

    This is the security-relevant outcome: it must be raised before any pane interaction, so a
    refusal can never be confused with a command that ran and failed.
    """

    exit_code = EXIT_REFUSED


class HerdrUnavailable(HerdrRunError):
    """The Herdr server, workspace, tab, or pane could not be established."""

    exit_code = EXIT_UNAVAILABLE


class PaneBusy(HerdrRunError):
    """The target pane was not observably idle, so typing into it was unsafe.

    Conservative by construction: we refuse rather than risk interleaving with whatever the pane is
    already doing (or appending to a half-typed command line a human left there).
    """

    exit_code = EXIT_BUSY


class RunTimeout(HerdrRunError):
    """The command was launched in the pane but produced no exit-code file in time.

    The command is NOT killed by default: it is running in a terminal this process does not own.

    Carries whatever the command printed before the deadline. A timeout that reported only a
    message would make a partially-successful run look like a run that produced nothing, and a
    caller cannot tell a false empty result from a real one.
    """

    exit_code = EXIT_TIMEOUT

    #: Output captured before the deadline. Empty string, never None, so callers need no guard.
    partial_stdout: str = ""
    partial_stderr: str = ""
    spool_directory: str = ""

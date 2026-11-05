"""``herdr-run`` command line.

The surface is a plain multiplexed CLI::

    herdr-run [GLOBAL OPTIONS] <subcommand> [OPTIONS]

Options that identify the invocation and its configuration are global and go before the
subcommand; options that only mean something to one subcommand go after it. Neither level
documents nor accepts the other's options, so ``--help`` at each level describes exactly the
options that level takes.
"""

from __future__ import annotations

import base64
import math
import os
import re
import signal
import subprocess
import sys
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace

from herdr_run import __version__
from herdr_run.allowlist import Admission, admit, quote_word
from herdr_run.audit import audit_path, record, warn_if_spool_is_tracked
from herdr_run.client import (
    HerdrClient,
    bounded_control_command,
    child_environment,
    environment_entries,
)
from herdr_run.config import (
    MAX_TIMEOUT_SECONDS,
    Config,
    load_config,
    missing_defaults_warning,
    whole_milliseconds,
)
from herdr_run.errors import (
    EXIT_BUSY,
    ConfigError,
    HerdrRunError,
    HerdrUnavailable,
    Refused,
    RunTimeout,
)
from herdr_run.evict import describe_skipped, survey_workspace
from herdr_run import output
from herdr_run.readiness import assess, infer_prompt_tail
from herdr_run.runner import RunResult, execute, read_output_bytes, write_meta
from herdr_run.session import resolve_target, tab_label_for
from herdr_run.sweep import load_run_records, measure_occupancy, sweep
from herdr_run.textfmt import (
    display,
    format_round3,
    json_text,
    name_violation,
    os_error_text,
    quote,
    strip_space,
)

__all__ = [
    "check_text",
    "help_text",
    "main",
    "subcommand_help_text",
    "surface_document",
    "usage_line",
]


#: Every subcommand, in the order ``herdr-run --help`` lists them, with its one-line summary.
#:
#: Listed by what a reader is most likely to want first rather than alphabetically: the action,
#: then the two questions asked about it, then setup, then the reports.
SUBCOMMANDS: tuple[tuple[str, str], ...] = (
    ("run", "run one allowlisted command in a pane and return its result"),
    ("check", "say whether a command would be admitted; touch no pane"),
    (
        "status",
        "report the configuration, policy, and session in effect; change nothing",
    ),
    ("init", "write an annotated .herdr-run.yaml in this directory"),
    ("config", "print the fully resolved configuration as JSON"),
    ("target", "resolve this agent's pane and print its ids and readiness"),
    ("reap", "report which command tabs are provably finished with"),
    ("net-doctor", "smoke-test one scenario: a caller whose own network is blocked"),
    ("quickstart", "print the one-screen introduction"),
    ("userguide", "print the complete reference"),
)

#: Packaged document each documentation subcommand prints.
_DOCUMENTS: dict[str, str] = {
    "quickstart": "QUICKSTART.md",
    "userguide": "USER_GUIDE.md",
}

#: Every option accepted before the subcommand, with the value it takes (``None`` for a flag).
#:
#: The parser, the refusal messages, and ``--dump-surface`` all read this one table, so the surface
#: the tool reports is the surface it accepts.
GLOBAL_OPTIONS: tuple[tuple[str, str | None], ...] = (
    ("--agent", "NAME"),
    ("--config", "PATH"),
    ("--dump-surface", None),
    ("--help", None),
    ("--json", None),
    ("--version", None),
    ("-h", None),
)

_HELP_OPTIONS: tuple[tuple[str, str | None], ...] = (("--help", None), ("-h", None))

#: Every option each subcommand accepts after its name, with the value it takes.
SUBCOMMAND_OPTIONS: tuple[tuple[str, tuple[tuple[str, str | None], ...]], ...] = (
    ("check", _HELP_OPTIONS),
    ("config", _HELP_OPTIONS),
    ("init", (("--force", None), ("--help", None), ("--upgrade", None), ("-h", None))),
    ("net-doctor", _HELP_OPTIONS),
    ("quickstart", _HELP_OPTIONS),
    ("reap", _HELP_OPTIONS),
    (
        "run",
        (
            ("--cwd", "PATH"),
            ("--dry-run", None),
            ("--help", None),
            ("--no-cache", None),
            ("--timeout", "SECONDS"),
            ("--wait-ready", "SECONDS"),
            ("-h", None),
        ),
    ),
    ("status", _HELP_OPTIONS),
    ("target", (("--help", None), ("--no-cache", None), ("-h", None))),
    ("userguide", _HELP_OPTIONS),
)

#: Subcommands that take no configuration file, and the reason each one does not.
#:
#: ``--config`` is refused for these rather than accepted and ignored. It is not an inert flag: it
#: reads as an instruction about which configuration file to use, and ``herdr-run --config P init``
#: reads as an instruction to write ``P``. Accepting an instruction and then disregarding it is the
#: defect this surface exists to remove, so the answer is a refusal that says why.
_CONFIG_BLIND: dict[str, str] = {
    "init": "it WRITES one into the current directory, or with --upgrade edits the one there",
    "quickstart": "it prints a packaged document",
    "userguide": "it prints a packaged document",
}

#: Subcommands that name no tab, and the reason each one does not.
#:
#: ``--agent NAME`` says which agent this invocation speaks for, and the one thing that does is name
#: the tab the work happens in. A subcommand that resolves no tab therefore cannot observe it, and a
#: caller who wrote it was saying "do this as NAME" -- an instruction, not a decoration. It is
#: refused for the same reason ``--config`` is.
_AGENT_BLIND: dict[str, str] = {
    "check": "it answers a policy question about a command and touches no pane",
    "init": "it writes a configuration file and touches no pane",
    "reap": "it reports on the whole workspace, not on one agent's tab",
    "quickstart": "it prints a packaged document",
    "userguide": "it prints a packaged document",
}

#: Subcommands with no machine-readable output, and the reason each one has none.
#:
#: ``--json`` promises machine-readable output *where a subcommand has it*. Where one does not, the
#: promise cannot be kept: the flag is accepted, prose comes out, and the caller finds out at the
#: parse of the output rather than at the flag -- which is the worst place to find out, because the
#: failure looks like corrupt data rather than a rejected request.
#:
#: ``config``, ``target``, and ``reap`` are deliberately absent. They print JSON with or without the
#: flag, so a caller who asks them for machine-readable output gets exactly that; the flag is
#: redundant there, not disregarded.
_JSON_BLIND: dict[str, str] = {
    "net-doctor": "it prints a human diagnosis and a verdict, in prose",
    "quickstart": "it prints a packaged document",
    "userguide": "it prints a packaged document",
}

#: Subcommands that name a tab, and so resolve which agent this invocation speaks for.
_AGENT_SIGHTED = frozenset({"run", "status", "config", "target", "net-doctor"})

#: The seconds grammar ``--timeout`` and ``--wait-ready`` accept: ASCII digits only, an optional
#: sign, an optional fraction and exponent, or a spelling of infinity or NaN (refused afterwards as
#: not finite). No whitespace, no underscores, no other script's digits -- ``float`` alone would
#: accept all three, and which digits count would depend on the interpreter's Unicode tables.
_SECONDS_GRAMMAR = re.compile(
    r"[+-]?(?:(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?"
    r"|[iI][nN][fF](?:[iI][nN][iI][tT][yY])?|[nN][aA][nN])"
)


class _UsageError(Exception):
    """A command line this surface cannot accept, carrying the message to print."""


class _EarlyExit(Exception):
    """A help or version request that has already printed everything it owes the caller."""


@dataclass(frozen=True)
class _Globals:
    """Options accepted only before the subcommand."""

    config: str | None = None
    agent: str | None = None
    json: bool = False


@dataclass(frozen=True)
class _RunOptions:
    """Options accepted only after ``run``."""

    command: str = ""
    cwd: str | None = None
    timeout: float | None = None
    wait_ready: float | None = None
    no_cache: bool = False
    dry_run: bool = False


@dataclass(frozen=True)
class _Invocation:
    """One fully parsed invocation."""

    globals: _Globals
    subcommand: str
    run: _RunOptions | None = None
    command: str | None = None
    force: bool = False
    upgrade: bool = False
    no_cache: bool = False


# --- the option tables the two levels are built from --------------------------------------------


def _local_option_owners(name: str) -> tuple[str, ...]:
    """Return the subcommands that own ``name``, or an empty tuple if no subcommand does.

    Knowing the owner is what turns "unrecognized argument" into a usable message. A caller who
    writes ``herdr-run --cwd /tmp run ...`` has the right option and the wrong level, and being
    told which level it belongs to is the whole difference between a typo and a dead end.
    """
    if name in ("--help", "-h"):
        return ()
    return tuple(
        subcommand
        for subcommand, table in SUBCOMMAND_OPTIONS
        if any(option == name for option, _value in table)
    )


def _is_global_option(name: str) -> bool:
    return any(option == name for option, _value in GLOBAL_OPTIONS)


def _positional_for(subcommand: str) -> list[str]:
    return ["<command>"] if subcommand in ("run", "check") else []


def surface_document() -> dict[str, object]:
    """The JSON document ``--dump-surface`` prints: every subcommand and option, from the tables."""
    return {
        "global_options": {option: value for option, value in GLOBAL_OPTIONS},
        "subcommands": {
            subcommand: {
                "options": {option: value for option, value in table},
                "positional": _positional_for(subcommand),
            }
            for subcommand, table in SUBCOMMAND_OPTIONS
        },
        "version": __version__,
    }


def _find_subcommand(name: str) -> bool:
    return any(entry[0] == name for entry in SUBCOMMANDS)


def _subcommand_names() -> str:
    return ", ".join(sorted(name for name, _summary in SUBCOMMANDS))


def _join_quoted(values: Sequence[str]) -> str:
    return " or ".join(f"'{value}'" for value in values)


def _split_option(token: str) -> tuple[str, str | None]:
    """Split ``--name=value`` into its parts; a bare ``--name`` yields no inline value."""
    name, separator, value = token.partition("=")
    return (name, value if separator else None)


# --- parsing ------------------------------------------------------------------------------------


def usage_line() -> str:
    """The one-line synopsis printed above every argument error."""
    return (
        "herdr-run [-h] [--version] [--config PATH] [--agent NAME] [--json] "
        "<subcommand> [OPTIONS]"
    )


def _option_value(
    raw: Sequence[str], index: int, name: str, inline: str | None
) -> tuple[str, int]:
    if inline is not None:
        return inline, index
    if index + 1 >= len(raw):
        raise _UsageError(f"argument {name}: expected one value")
    value = raw[index + 1]
    if value.startswith("-") and value != "-":
        raise _UsageError(f"argument {name}: expected one value")
    return value, index + 1


def _parse_seconds(option: str, value: str) -> float:
    if not _SECONDS_GRAMMAR.fullmatch(value):
        raise _UsageError(f"argument {option}: invalid numeric value: {quote(value)}")
    try:
        parsed = float(value)
    except (OverflowError, ValueError):
        raise _UsageError(
            f"argument {option}: invalid numeric value: {quote(value)}"
        ) from None
    if not math.isfinite(parsed) or parsed < 0:
        raise _UsageError(f"argument {option}: must be a finite non-negative number")
    if parsed > MAX_TIMEOUT_SECONDS:
        raise _UsageError(
            f"argument {option}: must not exceed {MAX_TIMEOUT_SECONDS:.0f} seconds"
        )
    if not whole_milliseconds(parsed):
        raise _UsageError(f"argument {option}: must be a whole number of milliseconds")
    return parsed


def _local_argument_error(subcommand: str, token: str) -> str:
    """Describe an option that appeared after the subcommand but does not belong to it."""
    name, _inline = _split_option(token)
    if _is_global_option(name) and name not in ("--help", "-h"):
        return (
            f"argument {name}: this is a GLOBAL option; put it before the subcommand:\n"
            f"  herdr-run {name} ... {subcommand} ..."
        )
    owners = _local_option_owners(name)
    if owners and subcommand not in owners:
        return (
            f"argument {name}: this is a {_join_quoted(owners)} option, "
            f"not a '{subcommand}' option"
        )
    return f"{subcommand}: unrecognized arguments: {token}"


def _config_blind_error(subcommand: str) -> str:
    """Describe ``--config`` offered to a subcommand that takes no configuration file."""
    message = (
        f"argument --config: '{subcommand}' takes no configuration file; "
        f"{_CONFIG_BLIND[subcommand]}."
    )
    if subcommand == "init":
        return (
            message + "\nRun it from the directory you want the file in:"
            f"\n  cd DIRECTORY && herdr-run {subcommand}"
        )
    return message + "\nDrop --config."


def _agent_blind_error(subcommand: str) -> str:
    """Describe ``--agent`` offered to a subcommand that names no tab."""
    return (
        f"argument --agent: '{subcommand}' names no tab; {_AGENT_BLIND[subcommand]}."
        "\nDrop --agent."
    )


def _json_blind_error(subcommand: str) -> str:
    """Describe ``--json`` offered to a subcommand with no machine-readable output."""
    return (
        f"argument --json: '{subcommand}' has no machine-readable output; "
        f"{_JSON_BLIND[subcommand]}."
        "\nDrop --json."
    )


def _misplaced_global(name: str) -> str:
    """Describe an option that appeared before the subcommand but belongs to one of them."""
    owners = _local_option_owners(name)
    if not owners:
        return f"unrecognized arguments: {name}"
    return (
        f"argument {name}: this is a {_join_quoted(owners)} option; "
        f"put it AFTER the subcommand:\n  herdr-run {owners[0]} {name} ..."
    )


def _unknown_subcommand(token: str, following: Sequence[str]) -> str:
    """Describe a first positional that is not a subcommand, naming the removed form's replacement.

    ``herdr-run <agent> '<command>'`` and ``herdr-run '<command>'`` used to run a command with no
    subcommand at all. Removing that is the point of this surface, so the removal has to be said
    out loud, with the exact line to type instead -- an "unknown subcommand" alone would leave a
    caller guessing that the tool had been withdrawn.
    """
    message = f"unknown subcommand '{token}'"
    replacement: str | None = None
    if len(following) == 1 and not following[0].startswith("-"):
        replacement = f"herdr-run --agent {token} run '{following[0]}'"
    elif not following and " " in token:
        replacement = f"herdr-run run '{token}'"
    if replacement is not None:
        message += (
            "\nRunning a command without a subcommand is no longer accepted. "
            f"Use 'run':\n    {replacement}"
        )
    return message + f"\nSubcommands: {_subcommand_names()}"


def _one_argument_error(subcommand: str, positional: Sequence[str]) -> str:
    joined = " ".join(positional)
    return (
        f"{subcommand}: pass the command as ONE quoted argument, not as separate words.\n"
        f"  you wrote:  herdr-run {subcommand} {joined}\n"
        f"  instead:    herdr-run {subcommand} '{joined}'\n"
        "Re-joining loose words would silently change the quoting of your arguments."
    )


def _parse_bare(name: str, rest: Sequence[str]) -> list[str]:
    """Parse a subcommand that takes no options, returning its positional arguments."""
    positional: list[str] = []
    options = True
    for token in rest:
        if options and token == "--":
            options = False
            continue
        if options and token in ("--help", "-h"):
            _print_subcommand_help(name)
            raise _EarlyExit()
        if options and token.startswith("-"):
            raise _UsageError(_local_argument_error(name, token))
        positional.append(token)
    return positional


def _parse_run(rest: Sequence[str]) -> _RunOptions:
    options = _RunOptions()
    positional: list[str] = []
    parsing = True
    index = 0
    while index < len(rest):
        token = rest[index]
        if parsing and token == "--":
            parsing = False
            index += 1
            continue
        if not parsing or not token.startswith("-"):
            positional.append(token)
            index += 1
            continue
        if token in ("--help", "-h"):
            _print_subcommand_help("run")
            raise _EarlyExit()
        if token == "--no-cache":
            options = replace(options, no_cache=True)
        elif token == "--dry-run":
            options = replace(options, dry_run=True)
        else:
            name, inline = _split_option(token)
            if name == "--cwd":
                value, index = _option_value(rest, index, name, inline)
                options = replace(options, cwd=value)
            elif name == "--timeout":
                value, index = _option_value(rest, index, name, inline)
                options = replace(options, timeout=_parse_seconds(name, value))
            elif name == "--wait-ready":
                value, index = _option_value(rest, index, name, inline)
                options = replace(options, wait_ready=_parse_seconds(name, value))
            else:
                raise _UsageError(_local_argument_error("run", name))
        index += 1
    if not positional:
        raise _UsageError("run: needs a command to run")
    if len(positional) > 1:
        raise _UsageError(_one_argument_error("run", positional))
    return replace(options, command=positional[0])


def _parse_init(rest: Sequence[str]) -> tuple[bool, bool]:
    """Parse ``init``'s own arguments into ``(force, upgrade)``."""
    force = upgrade = False
    for token in rest:
        if token in ("--help", "-h"):
            _print_subcommand_help("init")
            raise _EarlyExit()
        if token == "--force":
            force = True
        elif token == "--upgrade":
            upgrade = True
        else:
            raise _UsageError(_local_argument_error("init", token))
    if force and upgrade:
        raise _UsageError(
            "init: --force and --upgrade cannot be combined: --force writes a fresh file over "
            "the existing one, and --upgrade edits the existing one in place"
        )
    return force, upgrade


def _parse_flag_only(name: str, rest: Sequence[str], flag: str) -> bool:
    present = False
    for token in rest:
        if token in ("--help", "-h"):
            _print_subcommand_help(name)
            raise _EarlyExit()
        if token == flag:
            present = True
            continue
        raise _UsageError(_local_argument_error(name, token))
    return present


def _parse_subcommand(name: str, rest: Sequence[str], globals_: _Globals) -> _Invocation:
    """Parse one subcommand's own arguments."""
    if name == "run":
        return _Invocation(globals_, name, run=_parse_run(rest))
    if name == "check":
        positional = _parse_bare(name, rest)
        if not positional:
            raise _UsageError("check: needs a command to check")
        if len(positional) > 1:
            raise _UsageError(_one_argument_error("check", positional))
        return _Invocation(globals_, name, command=positional[0])
    if name == "init":
        force, upgrade = _parse_init(rest)
        return _Invocation(globals_, name, force=force, upgrade=upgrade)
    if name == "target":
        return _Invocation(
            globals_, name, no_cache=_parse_flag_only(name, rest, "--no-cache")
        )
    positional = _parse_bare(name, rest)
    if positional:
        raise _UsageError(
            f"{name}: takes no positional arguments; got {len(positional)}"
        )
    return _Invocation(globals_, name)


def _parse(raw: Sequence[str]) -> _Invocation:
    if any(
        any(0xD800 <= ord(character) <= 0xDFFF for character in token) for token in raw
    ):
        raise _UsageError("arguments must be valid UTF-8")

    globals_ = _Globals()
    index = 0
    chosen: str | None = None
    while index < len(raw):
        token = raw[index]
        if not token.startswith("-"):
            if not _find_subcommand(token):
                raise _UsageError(_unknown_subcommand(token, raw[index + 1 :]))
            chosen = token
            index += 1
            break
        if token == "--":
            raise _UsageError(
                f"expected a subcommand before '--'. Subcommands: {_subcommand_names()}"
            )
        if token in ("--help", "-h"):
            _print_help()
            raise _EarlyExit()
        if token == "--version":
            output.stdout(f"herdr-run {__version__}\n")
            raise _EarlyExit()
        if token == "--dump-surface":
            _emit_json(surface_document())
            raise _EarlyExit()
        if token == "--json":
            globals_ = replace(globals_, json=True)
        else:
            name, inline = _split_option(token)
            if name == "--config":
                value, index = _option_value(raw, index, name, inline)
                globals_ = replace(globals_, config=value)
            elif name == "--agent":
                value, index = _option_value(raw, index, name, inline)
                globals_ = replace(globals_, agent=value)
            elif name in ("--json", "--help", "--version", "--dump-surface"):
                raise _UsageError(f"argument {name}: takes no value")
            else:
                raise _UsageError(_misplaced_global(name))
        index += 1

    if chosen is None:
        # No subcommand at all is not an error: it is somebody asking what this command is.
        _print_help()
        raise _EarlyExit()
    invocation = _parse_subcommand(chosen, raw[index:], globals_)
    # After the subcommand's own parse, so that `--config P init --help` still prints init's help
    # and `--config P init --nonsense` still names the option it could not accept. Checked in the
    # order the global option block lists them, so a command line that offers two blind globals at
    # once reports the first of them and both editions report the same one.
    if globals_.config is not None and chosen in _CONFIG_BLIND:
        raise _UsageError(_config_blind_error(chosen))
    if globals_.agent is not None and chosen in _AGENT_BLIND:
        raise _UsageError(_agent_blind_error(chosen))
    if globals_.json and chosen in _JSON_BLIND:
        raise _UsageError(_json_blind_error(chosen))
    return invocation


# --- help ---------------------------------------------------------------------------------------


def help_text() -> str:
    """Build the top-level help so it can be asserted on as text, not only printed."""
    lines = [
        f"usage: {usage_line()}\n",
        "\nRun an allowlisted command in a terminal pane that is not subject to whatever\n"
        "constrains the caller, and get its real stdout, stderr, and exit code back.\n",
        "\nsubcommands:\n",
    ]
    for name, summary in SUBCOMMANDS:
        lines.append(f"  {name:<11} {summary}\n")
    lines.append(
        "\nglobal options (before the subcommand):\n"
        "  -h, --help            show this help message and exit\n"
        "  --version             show version and exit\n"
        "  --config PATH         explicit configuration file\n"
        "  --agent NAME          the agent this invocation speaks for; names its tab\n"
        "  --json                emit machine-readable output where a subcommand has it\n"
        "  --dump-surface        print every subcommand and option as JSON and exit\n"
    )
    lines.append(
        "\nEach subcommand has its own options: run 'herdr-run <subcommand> --help'.\n"
        "Options are not shared between the two levels — '--cwd' is a 'run' option and\n"
        "goes after 'run'; '--agent' is global and goes before it.\n"
    )
    lines.append(
        "\nTwo documentation commands, and they are for different moments:\n"
        "  quickstart  one screen. What this is for, the four commands worth trying\n"
        "              first, and the five things to know before running anything real.\n"
        "  userguide   the complete reference: configuration, exit codes, readiness,\n"
        "              retention, the pane cap, and the trust model.\n"
    )
    return "".join(lines)


#: Each subcommand's own help: usage line, what it does, and the options it alone accepts.
_SUBCOMMAND_HELP: dict[str, str] = {
    "run": (
        "usage: herdr-run [GLOBAL OPTIONS] run [OPTIONS] '<command>'\n"
        "\nRun one allowlisted command in this agent's pane and return its stdout, stderr,\n"
        "and exit code. The command is ONE argument: quote it.\n"
        "\npositional arguments:\n"
        "  <command>             the whole command line, as a single quoted argument\n"
        "\noptions:\n"
        "  -h, --help            show this help message and exit\n"
        "  --cwd PATH            working directory for the command\n"
        "  --timeout SECONDS     how long to wait for the command to finish\n"
        "  --wait-ready SECONDS  how long to wait for the pane to go idle\n"
        "  --no-cache            ignore the session cache and re-resolve from labels\n"
        "  --dry-run             admit and render the command; execute nothing\n"
        "\nExample:\n"
        "  herdr-run --agent release-agent run 'git push origin HEAD'\n"
    ),
    "check": (
        "usage: herdr-run [GLOBAL OPTIONS] check '<command>'\n"
        "\nDecide whether a command would be admitted by the policy in effect. Touches no\n"
        "pane and executes nothing. Exits 0 when allowed and 77 when refused.\n"
        "\nWhen allowed, prints 'ALLOWED: program=P subcommand=S' (S is '-' when there is\n"
        "no subcommand) and 'rendered: C', the command line as run would type it. A value\n"
        "holding an invisible or bidirectional character, or any other character a name\n"
        "may not contain, is shown quoted with that character escaped as \\u{hex}.\n"
        "\npositional arguments:\n"
        "  <command>             the whole command line, as a single quoted argument\n"
        "\noptions:\n"
        "  -h, --help            show this help message and exit\n"
        "\nExample:\n"
        "  herdr-run check 'git push origin HEAD'\n"
    ),
    "init": (
        "usage: herdr-run [GLOBAL OPTIONS] init [OPTIONS]\n"
        "\nWrite an annotated .herdr-run.yaml into the current directory, the way 'git init'\n"
        "writes into the current directory. Every knob is present and set to the value in\n"
        "force today, so adopting the file changes nothing until you edit it. Refuses to\n"
        "overwrite an existing configuration.\n"
        "\nWith --upgrade, edit the .herdr-run.yaml (or .herdr-run.yml) already in the current\n"
        "directory instead. Each setting it still holds exactly as an earlier herdr-run wrote\n"
        "it is rewritten as this one writes it, which adopts the built-in deny entries added\n"
        "since; nothing else in the file changes. Built-in deny entries it still leaves out,\n"
        "in lists somebody edited, are listed to be added by hand.\n"
        "\noptions:\n"
        "  -h, --help            show this help message and exit\n"
        "  --force               overwrite an existing configuration file\n"
        "  --upgrade             bring the settings an earlier herdr-run wrote up to date\n"
    ),
    "status": (
        "usage: herdr-run [GLOBAL OPTIONS] status\n"
        "\nReport what is in effect here: which configuration file was found and where its\n"
        "root is, what the policy admits, whether Herdr is reachable and running, and how\n"
        "many panes the workspace already holds. Strictly non-mutating — it starts no\n"
        "server and creates no workspace, tab, or pane.\n"
        "\noptions:\n"
        "  -h, --help            show this help message and exit\n"
    ),
    "config": (
        "usage: herdr-run [GLOBAL OPTIONS] config\n"
        "\nPrint the fully resolved configuration as JSON: every value in effect, the file\n"
        "it came from, and the tab label it renders for this agent.\n"
        "\noptions:\n"
        "  -h, --help            show this help message and exit\n"
    ),
    "target": (
        "usage: herdr-run [GLOBAL OPTIONS] target [OPTIONS]\n"
        "\nResolve this agent's pane, creating the workspace, tab, and pane if they are\n"
        "missing, and print their ids together with the readiness verdict.\n"
        "\noptions:\n"
        "  -h, --help            show this help message and exit\n"
        "  --no-cache            ignore the session cache and re-resolve from labels\n"
    ),
    "reap": (
        "usage: herdr-run [GLOBAL OPTIONS] reap\n"
        "\nReport which command tabs are provably finished with, and why every other one\n"
        "was declined. Closes nothing.\n"
        "\noptions:\n"
        "  -h, --help            show this help message and exit\n"
    ),
    "net-doctor": (
        "usage: herdr-run [GLOBAL OPTIONS] net-doctor\n"
        "\nSmoke-test one narrow scenario: a caller whose own network access is blocked,\n"
        "reaching the network through a pane that is not blocked. It runs one probe\n"
        "directly and again through the pane and compares the two. This is not a health\n"
        "check of the command as a whole.\n"
        "\noptions:\n"
        "  -h, --help            show this help message and exit\n"
    ),
    "quickstart": (
        "usage: herdr-run [GLOBAL OPTIONS] quickstart\n"
        "\nPrint the one-screen introduction: what this command is for, the four commands\n"
        "worth trying first, the shape of the command line, and the five things worth\n"
        "knowing before running anything real.\n"
        "\noptions:\n"
        "  -h, --help            show this help message and exit\n"
    ),
    "userguide": (
        "usage: herdr-run [GLOBAL OPTIONS] userguide\n"
        "\nPrint the complete reference: configuration, exit codes, readiness, retention,\n"
        "the pane cap, and the trust model.\n"
        "\noptions:\n"
        "  -h, --help            show this help message and exit\n"
    ),
}

#: Said by every subcommand ``_CONFIG_BLIND`` names, so the refusal is never a surprise.
_CONFIG_BLIND_NOTE = (
    "\nThis command takes no configuration file, so the global --config option is\n"
    "refused here rather than accepted and ignored.\n"
)

#: Said by every subcommand ``_AGENT_BLIND`` names.
_AGENT_BLIND_NOTE = (
    "\nThis command names no tab, so the global --agent option is refused here rather\n"
    "than accepted and ignored.\n"
)

#: Said by every subcommand ``_JSON_BLIND`` names.
_JSON_BLIND_NOTE = (
    "\nThis command has no machine-readable output, so the global --json option is\n"
    "refused here rather than accepted and ignored.\n"
)


def subcommand_help_text(name: str) -> str:
    """Build one subcommand's help so it can be asserted on as text, not only printed."""
    text = _SUBCOMMAND_HELP.get(name, f"usage: herdr-run [GLOBAL OPTIONS] {name}\n")
    # Derived from the very tables the refusals are decided by, rather than written out again per
    # subcommand, so a subcommand cannot come to refuse a global option without saying so here.
    # Listed in the order the global option block lists them.
    if name in _CONFIG_BLIND:
        text += _CONFIG_BLIND_NOTE
    if name in _AGENT_BLIND:
        text += _AGENT_BLIND_NOTE
    if name in _JSON_BLIND:
        text += _JSON_BLIND_NOTE
    return text


def _print_help() -> None:
    output.stdout(help_text())


def _print_subcommand_help(name: str) -> None:
    output.stdout(subcommand_help_text(name))



# --- shared helpers -----------------------------------------------------------------------------


def _resolve_agent(globals_: _Globals, environ: Mapping[str, str]) -> str:
    """The agent this invocation speaks for: ``--agent``, else :func:`_default_agent`.

    An agent name, from either source, may hold no character a name may not contain
    (:func:`herdr_run.textfmt.name_violation`): no control, no invisible character, no
    noncharacter, no private-use character, and no unassigned code point. It names a tab somebody
    will read, and is printed.
    """
    if globals_.agent:
        violation = name_violation(globals_.agent)
        if violation is not None:
            raise ConfigError(f"argument --agent: an agent name may not contain {violation}")
        return globals_.agent
    return _default_agent(environ)


def _default_agent(environ: Mapping[str, str]) -> str:
    """Infer the invoking agent from the environment the orchestrator sets.

    The first of the three variables that is set and non-empty once Unicode ``White_Space`` is
    trimmed from both ends (the explicit table in :mod:`herdr_run.textfmt`, so a value of only
    U+3000 or U+00A0 counts as empty and the next variable is consulted). A value that still holds
    a character a name may not contain after trimming is an error rather than a tab label. A
    variable that is not valid UTF-8 is an error too: skipping it would make this invocation speak
    for whoever the NEXT variable names.
    """
    for key in ("HERDR_RUN_AGENT", "DG_AGENT_NAME", "ORC_AGENT_NAME"):
        raw = environ.get(key)
        if raw is None:
            continue
        if any(0xD800 <= ord(character) <= 0xDFFF for character in raw):
            raise ConfigError(f"environment variable {key} is not valid UTF-8")
        value = strip_space(raw)
        if not value:
            continue
        violation = name_violation(value)
        if violation is not None:
            raise ConfigError(
                f"environment variable {key}: an agent name may not contain {violation}"
            )
        return value
    return "unknown-agent"


def _client(config: Config, environ: Mapping[str, str]) -> HerdrClient:
    return HerdrClient(broker=config.broker, environ=environ)


def _emit_json(document: object) -> None:
    output.stdout(
        json_text(document, indent=2) + "\n"
    )


def _current_directory() -> str:
    try:
        return os.getcwd()
    except OSError as exc:
        raise ConfigError(f"cannot determine current directory: {os_error_text(exc)}") from exc


def _resolved_cwd(config: Config, override: str | None) -> str:
    """Where the command runs: --cwd, else the project's configured cwd, else THE CALLER'S CWD.

    Defaulting to ``project_root`` was a silent-mistargeting bug. Config discovery walks UP to the
    nearest ``.herdr-run.yaml``, so a config file at the top of a multi-repo tree makes every nested
    worktree resolve ``project_root`` to that top directory -- and the command then ran there
    instead of in the slot the caller was standing in. It did not look like an error; it looked like
    the wrong repository answering. "Where the policy lives" and "where the command runs" are
    different questions, and only the first is anchored to the config file.

    An absolute path is passed through exactly as written and the pane's own ``cd`` resolves it:
    resolving ``..`` lexically here could pick a different directory than the shell would when a
    symbolic link precedes it. A relative path is resolved against the project root.
    """
    cwd = override if override is not None else config.cwd
    if cwd is None:
        return _current_directory()
    if not os.path.isabs(cwd):
        cwd = os.path.normpath(os.path.join(config.project_root, cwd))
    return cwd


def _print_document(subcommand: str) -> int:
    resource = _DOCUMENTS[subcommand]
    try:
        from importlib.resources import files

        text = (files("herdr_run") / resource).read_text(encoding="utf-8")
    except (OSError, ModuleNotFoundError):
        output.diagnostic(
            f"herdr-run: the {subcommand} document is not available in this installation"
        )
        return 1
    output.stdout(text)
    return 0


# --- subcommands --------------------------------------------------------------------------------


def _cmd_init(directory: str, *, force: bool, upgrade: bool, json_output: bool) -> int:
    from herdr_run.init import write_config_template

    if upgrade:
        return _cmd_init_upgrade(directory, json_output=json_output)
    path = write_config_template(directory, force=force)
    if json_output:
        _emit_json({"created": True, "path": path})
    else:
        output.stdout(
            f"wrote {display(path)}\n"
            "Every knob is in that file, set to the value in force today. The allowlist near the\n"
            "top is a HUMAN-ONLY knob: an agent that can widen its own allowlist does not have one.\n"
        )
    return 0


def _cmd_init_upgrade(directory: str, *, json_output: bool) -> int:
    """Upgrade the configuration file in ``directory`` and say what changed and what did not."""
    from herdr_run.init import upgrade_config_file

    result = upgrade_config_file(directory)
    if json_output:
        _emit_json(
            {
                "missing": [
                    {"entries": list(item.entries), "setting": item.setting}
                    for item in result.missing
                ],
                "path": result.path,
                "upgraded": [
                    {"added": list(upgrade.added), "setting": upgrade.setting}
                    for upgrade in result.upgraded
                ],
            }
        )
        return 0
    if result.upgraded:
        lines = [f"upgraded {display(result.path)}"]
        lines.extend(
            f"  {upgrade.setting}: added {', '.join(quote(entry) for entry in upgrade.added)}"
            for upgrade in result.upgraded
        )
    else:
        lines = [f"nothing to rewrite in {display(result.path)}"]
    if result.missing:
        lines.append(
            "Still left out, from lists that are not as an earlier herdr-run wrote them; add "
            "them by hand to adopt them:"
        )
        lines.extend(
            f"  {item.setting}: {', '.join(quote(entry) for entry in item.entries)}"
            for item in result.missing
        )
    else:
        lines.append("Every built-in deny entry is in force.")
    output.stdout("".join(f"{line}\n" for line in lines))
    return 0


def _cmd_status(
    config: Config, agent: str, environ: Mapping[str, str], *, json_output: bool
) -> int:
    """Report what is in effect here, and change nothing while doing it."""
    from herdr_run.status import (
        inspect_session,
        status_document,
        status_text,
        unreachable,
    )

    tab_label = tab_label_for(config, agent)
    # A resolution failure is a fact about the session worth REPORTING, not an error to exit on:
    # "herdr is not installed" is one of the most useful things status can tell somebody.
    client = _client(config, environ)
    try:
        client.preflight()
    except HerdrUnavailable as exc:
        session = unreachable(str(exc))
    else:
        session = inspect_session(client, config, tab_label)
    if json_output:
        _emit_json(status_document(config, agent, tab_label, session))
    else:
        output.stdout(status_text(config, agent, tab_label, session))
    return 0


def _cmd_config(config: Config, agent: str) -> int:
    document = {
        "source": config.source_path or "(built-in defaults)",
        "project_root": config.project_root,
        "workspace": config.workspace,
        "tab_name": config.tab_name,
        "tab_label": tab_label_for(config, agent),
        "cwd": config.cwd,
        "allow": list(config.allow),
        "prefixes": list(config.prefixes),
        "deny_global": {key: list(value) for key, value in config.deny_global.items()},
        "deny_subcommand": {
            key: list(value) for key, value in config.deny_subcommand.items()
        },
        "deny_anywhere": list(config.deny_anywhere),
        "deny_subcommand_option": {
            program: {subcommand: list(options) for subcommand, options in entries.items()}
            for program, entries in config.deny_subcommand_option.items()
        },
        "allow_subcommand": {
            key: list(value) for key, value in config.allow_subcommand.items()
        },
        "value_options": {
            key: list(value) for key, value in config.value_options.items()
        },
        "spool_dir": config.spool_dir,
        "timeout_seconds": config.timeout_seconds,
        "retention_days": config.retention_days,
        "max_panes": config.max_panes,
        "ready_timeout_seconds": config.ready_timeout_seconds,
        "readiness": config.readiness,
        "prompt_tail": config.prompt_tail,
        "shells": list(config.shells),
        "probe_remote": config.probe_remote,
        "broker": config.broker,
    }
    _emit_json(document)
    return 0


def _cmd_check(config: Config, command: str, *, json_output: bool) -> int:
    """Answer only the policy question, touching no pane and no Herdr server."""
    try:
        admission = admit(command, config)
    except Refused as exc:
        if json_output:
            _emit_json({"reason": str(exc), "verdict": "refused"})
        else:
            output.diagnostic(f"REFUSED: {exc}")
        return Refused.exit_code
    if json_output:
        _emit_json(
            {
                "program": admission.program,
                "rendered": admission.rendered,
                "subcommand": admission.subcommand,
                "verdict": "allowed",
            }
        )
    else:
        output.stdout(check_text(admission.program, admission.subcommand, admission.rendered))
    return 0


def check_text(program: str, subcommand: str | None, rendered: str) -> str:
    """The two lines ``check`` prints for an admitted command.

    The program, the subcommand, and the rendered command are each shown with
    :func:`herdr_run.textfmt.display`: as they are, unless one holds a character a name may not
    contain -- an invisible or bidirectional character, a control, a noncharacter, a private-use
    character, or an unassigned code point -- in which case that value is quoted with the
    character escaped, so it never reaches the terminal raw. The subcommand and every argument are
    whatever was typed, and under ``allow: ["*"]`` so is the program. ``-`` stands for an ABSENT
    subcommand only (a subcommand never starts with ``-``); an empty one is printed as empty, the
    way the JSON form distinguishes ``null`` from ``""``.
    """
    shown = "-" if subcommand is None else display(subcommand)
    return (
        f"ALLOWED: program={display(program)} subcommand={shown}\n"
        f"rendered: {display(rendered)}\n"
    )


def _cmd_target(
    config: Config, agent: str, environ: Mapping[str, str], *, use_cache: bool
) -> int:
    client = _client(config, environ)
    target = resolve_target(client, config, agent, use_cache=use_cache)
    prompt_tail = infer_prompt_tail(config)
    readiness = assess(client, target.pane_id, config, prompt_tail=prompt_tail)
    document = {
        "workspace": {"label": target.workspace_label, "id": target.workspace_id},
        "tab": {"label": target.tab_label, "id": target.tab_id},
        "pane_id": target.pane_id,
        "created": list(target.created),
        "from_cache": target.from_cache,
        "ready": readiness.ready,
        "readiness": readiness.describe(),
        "prompt_tail": prompt_tail,
    }
    _emit_json(document)
    return 0 if readiness.ready else EXIT_BUSY


#: How to read ``reap``'s ``occupancy`` block. Compared verbatim by the differential harness.
OCCUPANCY_NOTE = (
    "max_panes counts every pane herdr lists in the workspace (live_panes); reap can judge only "
    "the panes a surviving run record names (with_record), so without_record panes hold their "
    "share of max_panes until closed by hand or replaced as idle at the cap; replaceable counts "
    "the tabs the cap would replace if an agent arrived now (unsplit, shell idle), judged without "
    "the pane lock and closing nothing, and cap_refusal is what that agent would be told when "
    "none is"
)


def _cmd_reap(config: Config, environ: Mapping[str, str]) -> int:
    """Report which command tabs are PROVABLY finished with. Closes nothing.

    Report-only is the point, not a limitation. The expensive mistake is closing a tab whose agent
    is merely thinking, and a reaper that is wrong once in that direction is switched off for good;
    so the first version of this has to be checkable against a known-good population before anyone
    lets it act. Every declined pane carries its reason, and every verdict is counted including the
    zeros, because "reaped 0 because nothing was stale" and "reaped 0 because the detector is inert"
    are otherwise the same output.

    ``candidate_source`` states the bound on what could POSSIBLY have been considered. The candidate
    set is the panes named by surviving run records, and herdr-run prunes a run record
    ``retention_days`` after the run finished -- so the oldest leaked tabs, which are exactly the
    ones the pane cap exists to bound, drop out of this report while still counting against
    ``max_panes``. Printing the window is the difference between a report an operator can reason
    about and a count that quietly means something narrower than it says.

    ``occupancy`` measures that gap instead of only naming it: herdr's live pane list, which is what
    the cap counts, split by whether a surviving run record names each pane. Measured 2026-10-02 on
    devbig030, the cap was held by live panes with no record while ``reap`` listed only records,
    so its output could not say what was holding the cap.

    ``replaceable`` says whether that matters: the cap replaces an idle unsplit tab before it
    refuses, so a full workspace with an idle tab still admits the next agent. It is judged as the
    cap judges except for the pane lock, by :func:`herdr_run.evict.survey`, which closes nothing.

    The three readings are taken in one fixed order in both editions: the evidence for every
    recorded pane first, then the occupancy listing, then the survey.
    """
    client = _client(config, environ)
    records = load_run_records(config)
    plan = sweep(client, config, records=records)
    occupancy = measure_occupancy(client, config, records)
    replaceable: int | None = None
    cap_refusal: str | None = None
    survey_error: str | None = None
    if occupancy.listing_error is None:
        try:
            survey = survey_workspace(client, config, records)
        except HerdrRunError as exc:
            survey_error = str(exc)
        else:
            replaceable = len(survey.replaceable)
            if not survey.replaceable and survey.kept:
                cap_refusal = describe_skipped(survey.kept)
    document = {
        "workspace": config.workspace,
        "occupancy": {
            "max_panes": config.max_panes,
            "live_panes": occupancy.live_panes,
            "with_record": occupancy.with_record,
            "without_record": occupancy.without_record,
            "listing_error": occupancy.listing_error,
            "replaceable": replaceable,
            "cap_refusal": cap_refusal,
            "survey_error": survey_error,
            "note": OCCUPANCY_NOTE,
        },
        "candidate_source": {
            "spool_dir": config.spool_dir,
            "retention_days": config.retention_days,
            "note": (
                "candidates are the panes named by surviving run records; run records are pruned "
                "retention_days after the run finished, so a tab whose agent last ran longer ago "
                "than that is not considered here; at the cap it is replaced first if its shell is "
                "idle, otherwise it must be closed by hand"
            ),
        },
        "counts": plan.counts(),
        "reapable": [
            {
                "pane_id": decision.pane_id,
                "tab_id": decision.tab_id,
                "tab_label": decision.tab_label,
                "reason": decision.reason,
            }
            for decision in plan.reapable
        ],
        "declined": [
            {
                "pane_id": decision.pane_id,
                "tab_id": decision.tab_id,
                "tab_label": decision.tab_label,
                "verdict": decision.verdict,
                "reason": decision.reason,
            }
            for decision in plan.declined
        ],
    }
    _emit_json(document)
    return 0


#: The scope disclaimer ``net-doctor`` prints before it does anything.
#:
#: It goes FIRST, not in the verdict, because a diagnostic that only says what it covered once it
#: has already failed reads as an excuse. ``net-doctor`` answers exactly one question -- does
#: routing a network command through a pane get past a block on the caller's own network -- and a
#: reader whose interest in this command is something else should be able to stop at line one.
NET_DOCTOR_SCOPE = (
    "This is a smoke test for ONE scenario, not a health check of this command as a whole:\n"
    "a caller whose own network access is blocked, running a network command through a pane\n"
    "that is not blocked. It runs the same probe twice — directly, then through the pane —\n"
    "and compares. If you route commands through a pane for some other reason, the verdict\n"
    "below says nothing about that reason.\n"
)


def _net_doctor_error_fields(direct_exit_code: int | None = None) -> dict[str, object]:
    fields: dict[str, object] = {"net_doctor": True}
    if direct_exit_code is not None:
        fields["direct_exit_code"] = direct_exit_code
    return fields


def _first_line(text: str) -> str:
    """The first line of ``text`` after trimming, splitting on line feeds only."""
    return strip_space(text).split("\n")[0]


def _last_line(text: str) -> str:
    """The last line of ``text`` after trimming, splitting on line feeds only."""
    return strip_space(text).split("\n")[-1]


def _cmd_net_doctor(config: Config, agent: str, environ: Mapping[str, str]) -> int:
    """Bracket ONE scenario in both directions instead of asserting it.

    Positive: the probe through the pane must succeed. Negative: run directly, under whatever
    constrains this process, it must fail. If BOTH succeed the pane is not buying anything for this
    scenario; if both fail the pane path is broken. Reporting one side only would leave "the pane is
    outside the caller's confinement" as an inference rather than an observation.
    """
    argv = ["git", "ls-remote", config.probe_remote, "HEAD"]
    if "with-proxy" in config.prefixes:
        argv.insert(0, "with-proxy")
    source = display(config.source_path) if config.source_path else "defaults"
    output.stdout(
        f"herdr-run net-doctor  (agent={display(agent)}, config={source})\n"
        f"\n{NET_DOCTOR_SCOPE}\n"
    )

    log = audit_path(config.project_root, config.spool_dir)
    warn_if_spool_is_tracked(config.project_root, config.spool_dir)
    # Admitted exactly as `run` admits a command: its words quoted as written, `~` expanded by the
    # admission and judged expanded, so the probe that is printed, audited, and executed is the
    # rendered line the policy admitted.
    command = " ".join(quote_word(token) for token in argv)
    try:
        admission = admit(command, config)
    except HerdrRunError as exc:
        _record_audit(
            log, agent, command, type(exc).__name__.upper(), str(exc), _net_doctor_error_fields()
        )
        raise
    probe = admission.rendered
    _record_audit(
        log,
        agent,
        probe,
        "ADMITTED",
        "net-doctor probe accepted; target resolution and launch have not yet completed",
        {"net_doctor": True, "rendered": admission.rendered},
    )

    try:
        direct = bounded_control_command(["/bin/bash", "-lc", probe], timeout=120)
    except subprocess.TimeoutExpired as exc:
        error = HerdrUnavailable(
            "cannot run the direct probe: control command timed out after 120s"
        )
        _record_audit(
            log, agent, probe, "HERDRUNAVAILABLE", str(error), _net_doctor_error_fields()
        )
        raise error from exc
    except (OSError, ValueError) as exc:
        error = HerdrUnavailable(f"cannot run the direct probe: {os_error_text(exc)}")
        _record_audit(
            log, agent, probe, "HERDRUNAVAILABLE", str(error), _net_doctor_error_fields()
        )
        raise error from exc
    direct_code = direct.returncode
    direct_ok = direct_code == 0
    direct_text = (
        f"[direct  ] {probe}\n"
        f"           rc={direct_code} {'SUCCEEDED' if direct_ok else 'failed'}\n"
    )
    if not direct_ok:
        direct_text += f"           {_last_line(direct.stderr)}\n"
    output.stdout(direct_text)

    pane_rc = -1
    try:
        client = _client(config, environ)
        target = resolve_target(client, config, agent)
        result = execute(
            client,
            config,
            target,
            admission,
            agent=agent,
            cwd=_resolved_cwd(config, None),
            ready_timeout=max(config.ready_timeout_seconds, 30.0),
            timeout=min(config.timeout_seconds, 180.0),
        )
    except HerdrRunError as exc:
        _record_audit(
            log,
            agent,
            probe,
            type(exc).__name__.upper(),
            str(exc),
            _net_doctor_error_fields(direct_code),
        )
        output.stdout(f"[via pane] FAILED: {exc}\n")
    else:
        pane_rc = result.exit_code
        meta, meta_error = _write_meta_best_effort(result, admission, config, agent)
        fields = _net_doctor_error_fields(direct_code)
        fields.update(
            {
                "exit_code": pane_rc,
                "run_id": result.run_id,
                "pane_id": result.target.pane_id,
                "meta": meta,
            }
        )
        if meta_error is not None:
            fields["meta_error"] = meta_error
        _record_audit(
            log,
            agent,
            probe,
            "RAN",
            f"net-doctor pane probe exited {pane_rc}",
            fields,
        )
        head = _first_line(result.stdout)[:80]
        output.stdout(
            f"[via pane] {probe}   (pane {target.pane_id}, tab {target.tab_label})\n"
            f"           rc={pane_rc} {'SUCCEEDED' if pane_rc == 0 else 'failed'}  {head}\n"
        )

    if pane_rc == 0 and not direct_ok:
        output.stdout(
            "\nVERDICT: this scenario works — the probe is blocked directly and succeeds "
            "through the pane.\n"
        )
        return 0
    if pane_rc == 0:
        output.stdout(
            "\nVERDICT: the pane works, but so does running the probe directly. For THIS scenario "
            "the pane is buying you nothing; prefer running the command directly. Other reasons "
            "to route a command through a pane are untouched by that.\n"
        )
        return 0
    output.stdout(
        "\nVERDICT: the pane path is NOT working. Most likely the Herdr server was started from "
        "inside a confined process, so its panes inherit the confinement. Stop it "
        "('herdr server stop') and let herdr-run restart it via systemd-run, or start it from an "
        "unconfined shell.\n"
    )
    return 1


def _run_command(
    config: Config,
    agent: str,
    options: _RunOptions,
    environ: Mapping[str, str],
    *,
    json_output: bool,
) -> int:
    command = options.command
    log = audit_path(config.project_root, config.spool_dir)
    warn_if_spool_is_tracked(config.project_root, config.spool_dir)

    try:
        admission: Admission = admit(command, config)
    except Refused as exc:
        _record_audit(log, agent, command, "REFUSED", str(exc))
        output.diagnostic(f"herdr-run: REFUSED: {exc}")
        return Refused.exit_code

    if options.dry_run:
        _record_audit(log, agent, command, "DRY-RUN", admission.rendered)
        if json_output:
            _emit_json(
                {
                    "verdict": "allowed",
                    "program": admission.program,
                    "rendered": admission.rendered,
                }
            )
        else:
            output.stdout(admission.rendered + "\n")
        return 0

    _record_audit(
        log,
        agent,
        command,
        "ADMITTED",
        "policy accepted command; target resolution and launch have not yet completed",
        {"rendered": admission.rendered},
    )
    ready_timeout = (
        options.wait_ready
        if options.wait_ready is not None
        else config.ready_timeout_seconds
    )
    timeout = options.timeout if options.timeout is not None else config.timeout_seconds

    try:
        client = _client(config, environ)
        cwd = _resolved_cwd(config, options.cwd)
        target = resolve_target(client, config, agent, use_cache=not options.no_cache)
        result = execute(
            client,
            config,
            target,
            admission,
            agent=agent,
            cwd=cwd,
            ready_timeout=ready_timeout,
            timeout=timeout,
        )
    except RunTimeout as exc:
        # Emit whatever the command managed to print BEFORE reporting the timeout. Dropping it would
        # make a partially-successful run look like one that produced nothing, and a caller cannot
        # distinguish a false empty result from a real one.
        _record_audit(log, agent, command, "RUNTIMEOUT", str(exc))
        output.write_labelled(1, exc.partial_stdout.encode("utf-8"), "partial stdout")
        output.write_labelled(2, exc.partial_stderr.encode("utf-8"), "partial stderr")
        output.diagnostic(f"herdr-run: {exc}")
        return exc.exit_code
    except HerdrRunError as exc:
        _record_audit(log, agent, command, type(exc).__name__.upper(), str(exc))
        output.diagnostic(f"herdr-run: {exc}")
        return exc.exit_code

    meta, meta_error = _write_meta_best_effort(result, admission, config, agent)
    stdout_bytes, stderr_bytes = read_output_bytes(result)
    audit_fields: dict[str, object] = {
        "run_id": result.run_id,
        "pane_id": result.target.pane_id,
        "tab": result.target.tab_label,
        "exit_code": result.exit_code,
        "rendered": admission.rendered,
        "meta": meta,
    }
    if meta_error is not None:
        audit_fields["meta_error"] = meta_error
    _record_audit(
        log,
        agent,
        command,
        "RAN",
        f"exit {result.exit_code} in {result.duration_seconds:.1f}s",
        audit_fields,
    )

    if json_output:
        _emit_json(
            {
                "exit_code": result.exit_code,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "stdout_base64": base64.b64encode(stdout_bytes).decode("ascii"),
                "stderr_base64": base64.b64encode(stderr_bytes).decode("ascii"),
                "run_id": result.run_id,
                "pane_id": result.target.pane_id,
                "tab": result.target.tab_label,
                "created": list(result.target.created),
                "duration_seconds": format_round3(result.duration_seconds),
                "spool_dir": result.spool.directory,
                "meta": meta,
            }
        )
    else:
        output.stdout_bytes(stdout_bytes)
        output.stderr_bytes(stderr_bytes)
    return result.exit_code


def _record_audit(
    path: str,
    agent: str,
    command: str,
    verdict: str,
    detail: str,
    fields: dict[str, object] | None = None,
) -> None:
    """Attempt one audit append and make degradation visible without changing the result."""
    if not record(
        path,
        agent=agent,
        command=command,
        verdict=verdict,
        detail=detail,
        fields=fields,
    ):
        output.diagnostic(f"herdr-run: WARNING: could not append audit record to {path}")


def _write_meta_best_effort(
    result: RunResult, admission: Admission, config: Config, agent: str
) -> tuple[str | None, str | None]:
    """Write supplemental metadata without replacing an already-completed command result."""
    try:
        return write_meta(result, admission, config, agent), None
    except HerdrRunError as exc:
        output.diagnostic(f"herdr-run: WARNING: {exc}")
        return None, str(exc)


# --- dispatch -----------------------------------------------------------------------------------


def _dispatch(invocation: _Invocation, environ: Mapping[str, str]) -> int:
    start_dir = _current_directory()

    if invocation.subcommand == "init":
        # Deliberately before load_config: the reason to reach for `init` is often that discovery
        # found nothing, or found something broken, and refusing to write a fresh template because
        # the old one will not parse would be exactly the wrong moment to be strict.
        return _cmd_init(
            start_dir,
            force=invocation.force,
            upgrade=invocation.upgrade,
            json_output=invocation.globals.json,
        )

    config = load_config(explicit_path=invocation.globals.config, start_dir=start_dir)
    # Every subcommand that reads the configuration says so when a deny list in it leaves out a
    # built-in entry: the list replaces the built-in one, so the entry is not enforced, and a file
    # written by an older release would otherwise lose each new entry without a word.
    warning = missing_defaults_warning(config)
    if warning is not None:
        output.diagnostic(warning)
    json_output = invocation.globals.json
    # The agent is resolved only by the subcommands that name a tab with it, so a malformed agent
    # variable cannot fail a `check` or a `reap` that never asks who is speaking.
    agent = (
        _resolve_agent(invocation.globals, environ)
        if invocation.subcommand in _AGENT_SIGHTED
        else ""
    )

    if invocation.subcommand == "run":
        assert invocation.run is not None
        return _run_command(
            config, agent, invocation.run, environ, json_output=json_output
        )
    if invocation.subcommand == "check":
        assert invocation.command is not None
        return _cmd_check(config, invocation.command, json_output=json_output)
    if invocation.subcommand == "status":
        return _cmd_status(config, agent, environ, json_output=json_output)
    if invocation.subcommand == "config":
        return _cmd_config(config, agent)
    if invocation.subcommand == "target":
        return _cmd_target(
            config, agent, environ, use_cache=not invocation.no_cache
        )
    if invocation.subcommand == "reap":
        return _cmd_reap(config, environ)
    if invocation.subcommand == "net-doctor":
        return _cmd_net_doctor(config, agent, environ)
    raise AssertionError(f"unhandled subcommand {invocation.subcommand!r}")


def _proc_self_strings(name: str) -> list[bytes] | None:
    """The NUL-separated byte strings of ``/proc/self/<name>``, or ``None`` when it is unreadable.

    ``cmdline`` and ``environ`` hold exactly what this process was executed with, as bytes: no
    locale has decoded them, and nothing the interpreter did at start-up has changed them.
    """
    try:
        with open(f"/proc/self/{name}", "rb") as handle:
            payload = handle.read()
    except OSError:
        return None
    if not payload:
        return []
    if payload.endswith(b"\0"):
        payload = payload[:-1]
    return payload.split(b"\0")


def _original_environment() -> dict[bytes, bytes] | None:
    """The environment this process was executed with, under the rule every child's is built by.

    ``None`` when ``/proc/self/environ`` cannot be read. The entries are put through
    :func:`herdr_run.client.child_environment`: an entry without ``=`` or with an empty name is
    dropped, and the first of two entries with one name is kept where it stood.
    """
    entries = _proc_self_strings("environ")
    if entries is None:
        return None
    return child_environment(entries)


def _restore_environment(original: Mapping[bytes, bytes]) -> None:
    """Make this process's environment, and so every child's, the one it was executed with.

    The interpreter may have changed it before any of this code ran: PEP 538 locale coercion adds
    ``LC_CTYPE=C.UTF-8`` when the locale is ``C``, and children would inherit that. The native
    edition builds its children's environment from its caller's alone, so this one does too.
    """
    current = os.environb
    for key in list(current):
        if key not in original:
            try:
                del current[key]
            except (OSError, ValueError):
                pass
    for key, value in original.items():
        if current.get(key) != value:
            try:
                current[key] = value
            except (OSError, ValueError):
                pass


def _utf8_mode_disabled() -> bool:
    """True when the interpreter was started with ``-X utf8=0``, which a restart could not undo.

    Asked of the interpreter's own ``-X`` options, never of the command line: herdr-run's own
    arguments are on that line too, and an argument that happened to read ``-Xutf8=0`` (a command
    to check, say) must not keep the process in the locale's encoding.
    """
    options: dict[str, object] = getattr(sys, "_xoptions", {})
    return options.get("utf8") == "0"


def _restart_in_utf8_mode(reexec: Sequence[str]) -> None:
    """Re-execute this interpreter once with ``-X utf8``, exactly as it was started otherwise.

    The command line and the environment are taken from ``/proc/self`` as bytes. Rebuilding them
    from ``sys.argv`` would mean encoding back what the locale decoded, which a multibyte locale
    such as ``ja_JP.eucJP`` cannot always do: it raised UnicodeEncodeError for an argument holding
    UTF-8 text. When ``/proc`` is unavailable, the fallback rebuilds the command from ``sys.argv``
    and ``reexec`` and keeps running in the locale's encoding if that cannot be encoded.

    The restarted interpreter is a child like any other: its environment is built by
    :func:`herdr_run.client.child_environment` and passed explicitly, from ``/proc/self/environ``
    or, without ``/proc``, from ``os.environ``.
    """
    executable = sys.executable
    command = _proc_self_strings("cmdline")
    environment = _original_environment()
    if _utf8_mode_disabled():
        return
    if command:
        argv: list[bytes | str] = [command[0], b"-X", b"utf8", *command[1:]]
    else:
        try:
            argv = [
                os.fsencode(executable),
                b"-X",
                b"utf8",
                *(os.fsencode(argument) for argument in (*reexec, *sys.argv[1:])),
            ]
        except (UnicodeError, ValueError):
            return
    if environment is None:
        environment = child_environment(environment_entries())
    try:
        os.execve(executable, argv, environment)
    except (OSError, ValueError):
        return


def _prepare_process(reexec: Sequence[str]) -> None:
    """Make this interpreter behave as the native edition's runtime does, before anything runs.

    * A closed standard descriptor is reopened on ``/dev/null``, as the native runtime does before
      ``main``, and left inheritable as it leaves it: output to it is discarded, never an error and
      never a write into a file that happened to receive the descriptor number, and a child finds
      ``/dev/null`` there too rather than a closed descriptor.
    * When the locale selected a filesystem encoding other than UTF-8, the process re-executes
      once with ``-X utf8`` (:func:`_restart_in_utf8_mode`): arguments, environment values, paths,
      and output are then UTF-8 whatever the locale, as they are in the native edition, and bytes
      that are not UTF-8 are visible and refusable rather than silently reinterpreted.
    * The environment is put back to the one this process was executed with
      (:func:`_restore_environment`), so every child's environment is built from the caller's by
      the shared rule (:func:`herdr_run.client.child_environment`), with nothing the interpreter
      added.
    * SIGINT gets its default action when the interpreter installed its own handler, so an
      interrupt ends the process BY the signal, which is what lets a calling loop stop instead of
      carrying on. A SIGINT the caller ignored stays ignored, as it does in the native edition.
      (Importing the package already did this, as its first statement -- see :mod:`herdr_run` --
      and every entry point imports the package before anything else; it is repeated here for a
      caller that installed its own handler afterwards.)
    * SIGCHLD gets its default action. A caller can hand an ignored SIGCHLD down across ``exec``,
      and then the kernel reaps every child at once: waiting for one fails with ECHILD, which
      ``subprocess`` reads as exit status 0, so every failed child would count as a success. The
      native edition restores the default too, and the processes either edition starts inherit it.

    SIGXFSZ stays ignored, as the interpreter set it (and the native edition sets it), and the
    children ``subprocess`` starts get it, and SIGPIPE, back at their default actions and inherit
    no descriptor above 2.
    """
    for descriptor in (0, 1, 2):
        try:
            os.fstat(descriptor)
        except OSError:
            try:
                reopened = os.open(os.devnull, os.O_RDWR)
                os.set_inheritable(reopened, True)
            except OSError:
                pass
    encoding = sys.getfilesystemencoding().lower().replace("-", "").replace("_", "")
    if encoding != "utf8" and not sys.flags.utf8_mode and sys.executable:
        _restart_in_utf8_mode(reexec)
    original = _original_environment()
    if original is not None:
        _restore_environment(original)
    if signal.getsignal(signal.SIGINT) is signal.default_int_handler:
        signal.signal(signal.SIGINT, signal.SIG_DFL)
    signal.signal(signal.SIGCHLD, signal.SIG_DFL)


def main(argv: Sequence[str] | None = None, *, reexec: Sequence[str] | None = None) -> int:
    """Entry point. Return the wrapped status or the documented code for a wrapper failure.

    ``reexec`` is what follows the interpreter to run this same entry point again (a script path,
    or ``-m herdr_run``) when the process must restart in UTF-8 mode and ``/proc/self/cmdline``
    cannot be read; it defaults to the script the interpreter was started with.
    """
    if argv is None:
        try:
            _prepare_process(reexec if reexec is not None else [sys.argv[0]])
        except KeyboardInterrupt:
            # Only before the handler is reset: end by the signal, as the native edition does.
            signal.signal(signal.SIGINT, signal.SIG_DFL)
            os.kill(os.getpid(), signal.SIGINT)
            raise
    raw = list(argv) if argv is not None else sys.argv[1:]
    try:
        try:
            invocation = _parse(raw)
        except _EarlyExit:
            return 0
        except _UsageError as exc:
            output.diagnostic(f"usage: {usage_line()}")
            output.diagnostic(f"herdr-run: error: {exc}")
            return 2

        # Printed before any configuration is read: documentation is exactly what somebody with a
        # broken configuration file needs, and refusing to show it would be perverse.
        if invocation.subcommand in _DOCUMENTS:
            return _print_document(invocation.subcommand)

        return _dispatch(invocation, dict(os.environ))
    except HerdrRunError as exc:
        output.diagnostic(f"herdr-run: {exc}")
        return exc.exit_code


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())

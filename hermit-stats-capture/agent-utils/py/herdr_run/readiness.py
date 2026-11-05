"""Decide whether a pane is safe to type a command into.

This is the dangerous part of the tool. The foreground-process signal must prove idleness; an
independent prompt signal vetoes only when it has positive evidence of a half-typed command.

**Primary — the foreground process group (an observable, not a proxy).** ``herdr pane
process-info`` reports the pane's ``shell_pid`` and its ``foreground_process_group_id``. When the
shell is at its prompt, the foreground process group IS the shell: the pgid equals the shell pid and
the only foreground process is the shell itself. When anything is running, the kernel has moved the
foreground group to the job. This dereferences the actual running process rather than inferring
liveness from what the screen looks like, so it does not care about prompt themes, colour, spinners,
or how a TUI happens to redraw.

**Secondary — the prompt tail (a veto, never an authority).** The process signal cannot see a
command a human TYPED without pressing Enter: the shell is genuinely idle, yet typing into that
pane would concatenate our command onto their half-written line. So we also look at the last
rendered line. We infer the prompt's trailing literal from the shell rc file (for a ``PS1`` ending
``...\\n\\$ `` the tail is ``"$ "``), then:

* last line ends with the tail            -> ``clean``
* tail occurs earlier in the line         -> ``dirty``  (something is typed after the prompt)
* tail not found, or could not be inferred -> ``abstain``

``abstain`` is honest rather than optimistic: this signal only ever VETOES on positive evidence of
dirt, and never upgrades a pane to ready on its own. If it could not understand the prompt it says
so, and the record shows ``prompt=abstain`` instead of implying a check that did not happen.

A pane is READY only when the process signal says idle and the prompt signal does not veto — and,
because a command being launched has a brief window where the pgid has not moved yet, the process
signal must hold for two consecutive polls.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass

from herdr_run.client import HerdrClient, ProcessInfo, account_home
from herdr_run.config import Config
from herdr_run.errors import HerdrUnavailable
from herdr_run.textfmt import is_blank, quote, rstrip_space, strip_space

__all__ = ["ProcessSignal", "PromptSignal", "Readiness", "assess", "infer_prompt_tail", "parse_ps1"]

#: ``\[ ... \]`` marks zero-width (non-printing) sequences in a bash PS1 — colour codes and the
#: like. They render to nothing, so they must be removed before asking what the prompt LOOKS like.
_ZERO_WIDTH = re.compile(r"\\\[.*?\\\]")

#: A backslash escape (``\u``, ``\h``, ``\w``, ``\$``, ...) or a shell expansion (``$(...)``,
#: ``${...}``, ``$VAR``). Everything after the LAST of these is fixed literal text, which is exactly
#: the trailing run we can match against a rendered line.
_DYNAMIC = re.compile(r"\\.|\$\([^)]*\)|\$\{[^}]*\}|\$[A-Za-z_][A-Za-z0-9_]*")

#: ``PS1='...'`` / ``PS1="..."`` / ``PROMPT=...`` assignment at the start of a line (optionally
#: `export`ed). Deliberately anchored so a commented-out example (`# PS1=...`) is not picked up.
#: The unquoted form stops at whitespace, spelled out as the Unicode ``White_Space`` list rather
#: than ``\s``, whose meaning differs between regular-expression engines.
_PS1_ASSIGN = re.compile(
    r"^[ \t]*(?:export[ \t]+)?(?:PS1|PROMPT)=(\"(?:[^\"\\]|\\.)*\"|'[^']*'"
    r"|[^\t\n\x0b\x0c\r \x85\xa0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000#]+)"
    r"[ \t]*(?:#.*)?$",
    re.MULTILINE,
)


@dataclass(frozen=True)
class ProcessSignal:
    """What the pane's foreground process group says about whether the shell is at a prompt."""

    idle: bool
    reason: str
    shell_pid: int
    foreground_pgid: int
    foreground: tuple[str, ...]


@dataclass(frozen=True)
class PromptSignal:
    """What the last rendered line says. A veto only: it never declares a pane ready."""

    #: One of ``clean``, ``dirty``, ``abstain``.
    verdict: str
    reason: str
    tail: str | None
    last_line: str | None


@dataclass(frozen=True)
class Readiness:
    """The combined verdict of both signals, and the reasoning behind it."""

    ready: bool
    reason: str
    process: ProcessSignal
    prompt: PromptSignal

    def describe(self) -> str:
        """One line naming both signals and their reasons, for logs and error messages."""
        return (
            f"process={'idle' if self.process.idle else 'busy'} ({self.process.reason}); "
            f"prompt={self.prompt.verdict} ({self.prompt.reason})"
        )


def parse_ps1(rc_text: str) -> str | None:
    """Return the LAST uncommented ``PS1``/``PROMPT`` assignment in a shell rc file, unquoted.

    Last wins because that is what the shell itself ends up with when an rc file reassigns the
    prompt inside a conditional branch further down.
    """
    # `findall` is typed as returning Any; take the capture group off the match objects instead so
    # the value stays a concrete `str` under `disallow_any_explicit`.
    raw: str | None = None
    for match in _PS1_ASSIGN.finditer(rc_text):
        raw = match.group(1)
    if raw is None:
        return None
    if len(raw) >= 2 and raw[0] == raw[-1] and raw[0] in "\"'":
        return raw[1:-1]
    return raw


def prompt_tail_of(ps1: str) -> str | None:
    """Extract the fixed literal text a rendered prompt ends with, or ``None`` if there is none.

    ``"\\[\\033[0;34m\\][\\u@\\h \\w] \\n\\$ \\[\\033[0m\\]"`` -> ``"$ "``.
    """
    visible = _ZERO_WIDTH.sub("", ps1)
    last_dynamic_end = 0
    for match in _DYNAMIC.finditer(visible):
        # `\$` renders as a literal `$` (or `#` for root) and is the single most common prompt
        # terminator, so treat it as part of the tail rather than as an opaque expansion.
        if match.group(0) == r"\$":
            last_dynamic_end = match.start()
            continue
        last_dynamic_end = match.end()
    tail = visible[last_dynamic_end:]
    tail = tail.replace(r"\$", "$")
    return tail if strip_space(tail) else None


def infer_prompt_tail(config: Config, *, home: str | None = None) -> str | None:
    """Resolve the prompt tail from explicit config, else by reading the user's shell rc files."""
    if config.prompt_tail is not None:
        return config.prompt_tail
    if home is not None:
        base = home
    else:
        try:
            base = account_home()
        except HerdrUnavailable:
            return None
    for name in (".bashrc", ".zshrc", ".bash_profile", ".profile"):
        path = os.path.join(base, name)
        try:
            with open(path, "rb") as handle:
                payload = handle.read()
        except OSError:
            continue
        # CRLF and CR line ends read as LF, so a file saved by a Windows editor still names its
        # prompt and the dirty-line veto keeps working.
        text = (
            payload.decode("utf-8", errors="replace").replace("\r\n", "\n").replace("\r", "\n")
        )
        ps1 = parse_ps1(text)
        if ps1 is None:
            continue
        tail = prompt_tail_of(ps1)
        if tail is not None:
            return tail
    return None


def assess_process(info: ProcessInfo, config: Config) -> ProcessSignal:
    """Judge idleness from the foreground process group. Idle only when the shell owns it alone."""
    names = tuple(name for _pid, name, _cmdline in info.foreground)
    common = ProcessSignal(
        idle=False,
        reason="",
        shell_pid=info.shell_pid,
        foreground_pgid=info.foreground_pgid,
        foreground=names,
    )
    if info.foreground_pgid != info.shell_pid:
        running = ", ".join(names) or "unknown"
        return _with_reason(
            common,
            False,
            f"foreground pgid {info.foreground_pgid} != shell pid {info.shell_pid}; running: {running}",
        )
    if len(info.foreground) != 1:
        return _with_reason(common, False, f"{len(info.foreground)} foreground processes, expected only the shell")
    pid, name, _cmdline = info.foreground[0]
    if pid != info.shell_pid:
        return _with_reason(common, False, f"foreground process {pid} ({name}) is not the shell {info.shell_pid}")
    if name not in config.shells:
        # A pane whose shell slot is occupied by something else (an editor, an agent TUI, a REPL) is
        # not a shell prompt, whatever its process group says.
        return _with_reason(
            common, False, f"foreground process is {quote(name)}, not a known shell"
        )
    return _with_reason(common, True, f"shell {name} ({info.shell_pid}) owns the foreground group")


def _with_reason(signal: ProcessSignal, idle: bool, reason: str) -> ProcessSignal:
    return ProcessSignal(
        idle=idle,
        reason=reason,
        shell_pid=signal.shell_pid,
        foreground_pgid=signal.foreground_pgid,
        foreground=signal.foreground,
    )


def assess_prompt(text: str, tail: str | None) -> PromptSignal:
    """Judge the last rendered line: ``clean``, ``dirty``, or ``abstain`` when it is unreadable."""
    if tail is None:
        return PromptSignal("abstain", "no prompt tail configured or inferable", None, None)
    # Lines are split on line feeds only, with a trailing carriage return dropped: a Unicode line
    # separator inside a half-typed command must not hide the command from the veto.
    lines = [
        line[:-1] if line.endswith("\r") else line for line in text.split("\n")
    ]
    lines = [line for line in lines if not is_blank(line)]
    if not lines:
        return PromptSignal("abstain", "pane has no rendered output yet", tail, None)
    last = lines[-1]
    # `herdr pane read` strips trailing whitespace from each line, so compare against the stripped
    # tail; an idle prompt of "$ " renders as the line "$".
    needle = rstrip_space(tail)
    if not needle:
        return PromptSignal("abstain", "prompt tail is whitespace only", tail, last)
    stripped_last = rstrip_space(last)
    occurrences = stripped_last.count(needle)
    if occurrences == 1 and stripped_last.endswith(needle):
        return PromptSignal(
            "clean", f"last line ends with prompt tail {quote(needle)}", tail, last
        )
    if occurrences:
        excerpt = stripped_last[-80:]
        if occurrences > 1:
            reason = (
                f"prompt tail {quote(needle)} occurs {occurrences} times; possible text follows "
                f"an earlier prompt: {quote(excerpt)}"
            )
        else:
            reason = f"text typed after prompt tail {quote(needle)}: {quote(excerpt)}"
        return PromptSignal("dirty", reason, tail, last)
    return PromptSignal(
        "abstain", f"prompt tail {quote(needle)} not found in last line", tail, last
    )


def assess(
    client: HerdrClient,
    pane_id: str,
    config: Config,
    *,
    prompt_tail: str | None,
    read_lines: int = 4,
) -> Readiness:
    """Take one readiness reading of ``pane_id``. Callers poll this; it does not sleep."""
    process = assess_process(client.process_info(pane_id), config)
    if config.readiness == "process":
        prompt = PromptSignal("abstain", "prompt check disabled (readiness: process)", prompt_tail, None)
    else:
        prompt = assess_prompt(client.read(pane_id, lines=read_lines), prompt_tail)

    if not process.idle:
        return Readiness(False, "pane is busy", process, prompt)
    if prompt.verdict == "dirty":
        return Readiness(False, "pane has an unsubmitted command line", process, prompt)
    return Readiness(True, "pane is idle at a shell prompt", process, prompt)

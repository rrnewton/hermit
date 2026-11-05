#!/usr/bin/env python3
"""Pin documented shell-executor and agent-control numeric defaults to their code constants.

The guides are shipped artifacts: `py/agentctl/USER_GUIDE.md` is embedded into the agent-control
crate and included in the wheel, the chat bridge guide `rs/agentctl/src/embedded_chat_userguide.md`
is compiled into the same crate, the shell guides go inside both distributions, and
`CONFIG_TEMPLATE.yaml` is what `herdr-run init` writes into a project. Agents read those documents
as fact. Until this guard existed, "up to `--ready-timeout` (900 seconds by default)" was held true
by nothing at all: the constant lives in several places across two implementations, and changing them
left the sentence quietly asserting the old number.

**This is not a self-fulfilling test.** The repository has repeatedly written checks that read a
constant, format the very sentence they then compare against, and pass whatever the constant is.
The comparison here is between two SEPARATE artifacts: a number parsed out of prose that nothing
generated, and a number parsed out of the source that defines it. Neither side is derived from the
other, and `--self-test` proves it the only way that means anything — by mutating each code
constant in memory and requiring this check to go red and name the document.

What is pinned, and the rule for each:

* A pin's CODE sites must all agree. `ready_timeout` is written out in library defaults and both canonical and
  compatibility CLI defaults; an edition that
  changes one of them is a bug before any document is consulted.
* A pin's DOC sites must each match at least once, and EVERY occurrence must equal the code value.
  At-least-once matters as much as the equality: a sentence that is reworded until the pattern
  stops matching would otherwise make this guard silently vacuous, so zero matches is a failure
  that names the file.

Numbers are compared numerically, so `900` in prose, `900.0` in Rust and `900.0` in Python are the
same number, `31,536,000` equals `31_536_000.0`, and a spelled-out `four` equals `4` — the guides
write small counts as words, and that spelling drifts exactly as easily as a digit.

Not covered, deliberately:

* **The other tools' guides.** `dagrun` states numbers in prose too — a two-hundred step ceiling,
  an 85%-of-`MemTotal` budget with an 8 GiB margin. They are real drift candidates and each is one
  more row in `PINS`, but they belong to a different tool and a different survey; this landed with
  `#88 herdr-run-pin-documented-defaults`, which is herdr-run's.
* **Numbers the chat guide derives from the breaker constants.** Its breaker paragraph says a loop
  under the limit can post 480 times an hour, a faster loop trips on its 9th post, one posting
  every 3 seconds trips 24 seconds in, and a tripped loop sends about 96 posts an hour. A pin
  compares one stated number with one constant, so these are not checked. They sit in the same
  paragraph as the pinned numbers they come from, so changing any constant they come from makes
  this guard name lines of that paragraph.
* **Exit codes.** The guides tabulate `75`/`76`/`77`/`78`, but those are a wire contract already
  asserted by the Python and Rust suites and by the cross-language differential, so a change to one
  cannot reach main with the table still standing.
* **The template's NON-numeric defaults** — `workspace`, `spool_dir`, `readiness`, `broker`,
  `probe_remote`, `shells`. `CONFIG_TEMPLATE.yaml` promises those are the tool's values too, and
  they can drift the same way; this guard compares numbers, which is the drift `#88` names, and
  pinning a string wants a second comparison rather than a tenth row.

Usage:
    python3 scripts/check_documented_defaults.py
    python3 scripts/check_documented_defaults.py --self-test
"""

from __future__ import annotations

import argparse
import re
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

#: Reads a repository-relative path and returns its text. Injectable so `--self-test` can hand the
#: real check a MUTATED copy of a real file and watch it fail.
Reader = Callable[[str], str]

#: Small counts the guides spell out. Only as far as the guides actually go: an unknown word is a
#: parse failure, not a silent skip, so extending the prose past this needs a deliberate edit here.
NUMBER_WORDS: dict[str, float] = {
    "zero": 0,
    "one": 1,
    "two": 2,
    "three": 3,
    "four": 4,
    "five": 5,
    "six": 6,
    "seven": 7,
    "eight": 8,
    "nine": 9,
    "ten": 10,
    "eleven": 11,
    "twelve": 12,
}


class Unparseable(Exception):
    """A captured number that is neither a numeral nor a word this guard knows."""


def parse_number(raw: str) -> float:
    """`1,000,000`, `31_536_000.0` and `four` all become a float."""
    cleaned = raw.strip().replace(",", "").replace("_", "")
    word = NUMBER_WORDS.get(cleaned.lower())
    if word is not None:
        return word
    try:
        return float(cleaned)
    except ValueError as exc:
        raise Unparseable(f"{raw!r} is not a number this guard can read") from exc


def show(value: float) -> str:
    """Render a number the way a reader would say it: `900`, not `900.0`."""
    return str(int(value)) if value == int(value) else str(value)


@dataclass(frozen=True)
class Site:
    """One place a number is written down, and the pattern that lifts it back out.

    `pattern` must capture the number as the group `value`, and must be anchored on enough
    surrounding text that it cannot drift onto a different number in the same file.
    """

    path: str
    pattern: str
    says: str

    def occurrences(self, read: Reader) -> tuple[list[tuple[int, float]], list[str]]:
        """`(occurrences, problems)`, where each occurrence is `(line number, value)`."""
        try:
            text = read(self.path)
        except OSError as exc:
            return [], [f"{self.path}: cannot be read ({exc})"]
        found: list[tuple[int, float]] = []
        problems: list[str] = []
        for match in re.finditer(self.pattern, text):
            # The line of the NUMBER, not of the match: a pattern may be anchored several lines
            # above the value it captures, and the reader needs the line to go and edit.
            line = text.count("\n", 0, match.start("value")) + 1
            raw = match.group("value")
            try:
                found.append((line, parse_number(raw)))
            except Unparseable as exc:
                problems.append(f"{self.path}:{line}: {exc}")
        if not found and not problems:
            problems.append(
                f"{self.path}: nothing matches the pattern for {self.says!r} any more — the text was"
                " reworded or moved, and this guard was about to stop checking it"
            )
        return found, problems


@dataclass(frozen=True)
class Pin:
    """One default: the code that defines it, and the prose that promises it."""

    slug: str
    what: str
    code: tuple[Site, ...]
    docs: tuple[Site, ...]


PINS: tuple[Pin, ...] = (
    Pin(
        "agent-ready-timeout",
        "agent-control `--ready-timeout`, in seconds",
        code=(
            Site(
                "rs/agentctl/src/agent.rs",
                r"(?s)impl Default for DrainOptions \{.{0,200}?"
                r"ready_timeout: Duration::from_secs\((?P<value>[\d_]+)\)",
                "the Rust delivery default",
            ),
            Site(
                "rs/agentctl/src/legacy_cli.rs",
                r"(?m)^\s+ready_timeout: (?P<value>[\d_.]+),$",
                "the Rust CLI default",
            ),
            Site(
                "py/agentctl/agent.py",
                r"(?m)^\s+ready_timeout: float = (?P<value>[\d_.]+),$",
                "the Python delivery default",
            ),
            Site(
                "py/agentctl/legacy_cli.py",
                r'"--ready-timeout", type=_ascii_float, default=(?P<value>[\d_.]+)\)',
                "the Python CLI default",
            ),
            Site(
                "py/agentctl/cli.py",
                r'"--ready-timeout", type=_ascii_float, default=(?P<value>[\d_.]+),',
                "the canonical Python CLI default",
            ),
            Site(
                "rs/agentctl/src/cli.rs",
                r'(?s)#\[arg\(long, default_value = "(?P<value>[\d_]+)", value_parser = seconds\)\]'
                r'\s+ready_timeout: f64',
                "the canonical Rust CLI default",
            ),
        ),
        docs=(
            Site(
                "py/agentctl/USER_GUIDE.md",
                r"default readiness wait is (?P<value>[\d,]+) seconds",
                "the readiness wait the agent guide promises",
            ),
        ),
    ),
    Pin(
        "command-timeout-seconds",
        "`timeout_seconds`: the wait for a command's exit code",
        code=(
            Site(
                "rs/herdr-run/src/config.rs",
                r"(?m)^\s+timeout_seconds: (?P<value>[\d_.]+),$",
                "the Rust config default",
            ),
            Site(
                "py/herdr_run/config.py",
                r"(?m)^\s+timeout_seconds: float = (?P<value>[\d_.]+)$",
                "the Python config default",
            ),
        ),
        docs=(
            Site(
                "common/docs/herdr-run/CONFIG_TEMPLATE.yaml",
                r"(?m)^timeout_seconds: (?P<value>[\d_.,]+)$",
                "the value `herdr-run init` writes",
            ),
        ),
    ),
    Pin(
        "pane-ready-timeout-seconds",
        "`ready_timeout_seconds`: the wait for a busy pane",
        code=(
            Site(
                "rs/herdr-run/src/config.rs",
                r"(?m)^\s+ready_timeout_seconds: (?P<value>[\d_.]+),$",
                "the Rust config default",
            ),
            Site(
                "py/herdr_run/config.py",
                r"(?m)^\s+ready_timeout_seconds: float = (?P<value>[\d_.]+)$",
                "the Python config default",
            ),
        ),
        docs=(
            Site(
                "common/docs/herdr-run/CONFIG_TEMPLATE.yaml",
                r"(?m)^ready_timeout_seconds: (?P<value>[\d_.,]+)$",
                "the value `herdr-run init` writes",
            ),
        ),
    ),
    Pin(
        "control-timeout-seconds",
        "the bound on one Herdr/systemd control subprocess, in seconds",
        code=(
            Site(
                "rs/herdr-run/src/client.rs",
                r"(?m)^pub\(crate\) const CONTROL_TIMEOUT: Duration = Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust bound",
            ),
            Site(
                "py/herdr_run/client.py",
                r"(?m)^CONTROL_TIMEOUT_SECONDS = (?P<value>[\d_.]+)$",
                "the Python bound",
            ),
        ),
        docs=(
            Site(
                "common/docs/herdr-run/USER_GUIDE.template.md",
                r"control subprocess is bounded to (?P<value>[A-Za-z\d_,]+) seconds",
                "the control bound the user guide states",
            ),
            Site(
                "common/docs/herdr-run/rendered/python/USER_GUIDE.md",
                r"control subprocess is bounded to (?P<value>[A-Za-z\d_,]+) seconds",
                "the control bound the packaged Python guide states",
            ),
            Site(
                "common/docs/herdr-run/rendered/rust/USER_GUIDE.md",
                r"control subprocess is bounded to (?P<value>[A-Za-z\d_,]+) seconds",
                "the control bound the packaged Rust guide states",
            ),
        ),
    ),
    Pin(
        "max-panes",
        "`max_panes`: the pane ceiling before a NEW tab is refused",
        code=(
            Site(
                "rs/herdr-run/src/config.rs",
                r"(?m)^pub const DEFAULT_MAX_PANES: u64 = (?P<value>[\d_]+);$",
                "the Rust constant",
            ),
            Site(
                "py/herdr_run/config.py",
                r"(?m)^DEFAULT_MAX_PANES = (?P<value>[\d_]+)$",
                "the Python constant",
            ),
        ),
        docs=(
            Site(
                "common/docs/herdr-run/CONFIG_TEMPLATE.yaml",
                r"(?m)^max_panes: (?P<value>[\d_,]+)$",
                "the value `herdr-run init` writes",
            ),
            Site(
                "common/docs/herdr-run/USER_GUIDE.template.md",
                r"`max_panes` \((?P<value>[\d_,]+) by default\)",
                "the cap the user guide states",
            ),
            Site(
                "common/docs/herdr-run/rendered/python/USER_GUIDE.md",
                r"`max_panes` \((?P<value>[\d_,]+) by default\)",
                "the cap the packaged Python guide states",
            ),
            Site(
                "common/docs/herdr-run/rendered/rust/USER_GUIDE.md",
                r"`max_panes` \((?P<value>[\d_,]+) by default\)",
                "the cap the packaged Rust guide states",
            ),
            # The rationale beside each constant argues from the number. It ships in the crate's
            # rustdoc and the wheel's docstrings, so it is documentation like any other.
            Site(
                "rs/herdr-run/src/config.rs",
                r"(?P<value>[\d_,]+) keeps an eightfold margin",
                "the margin the Rust rationale claims",
            ),
            Site(
                "py/herdr_run/config.py",
                r"(?P<value>[\d_,]+) keeps an eightfold margin",
                "the margin the Python rationale claims",
            ),
        ),
    ),
    Pin(
        "retention-days",
        "`retention_days`: how long captured run output is kept",
        code=(
            Site(
                "rs/herdr-run/src/retention.rs",
                r"(?m)^pub const RETENTION_DAYS: u64 = (?P<value>[\d_]+);$",
                "the Rust constant",
            ),
            Site(
                "py/herdr_run/retention.py",
                r"(?m)^RETENTION_DAYS = (?P<value>[\d_]+)$",
                "the Python constant",
            ),
        ),
        docs=(
            Site(
                "common/docs/herdr-run/CONFIG_TEMPLATE.yaml",
                r"(?m)^retention_days: (?P<value>[\d_,]+)$",
                "the value `herdr-run init` writes",
            ),
            # Twice in this document, and both are checked: a guide that is right in one paragraph
            # and stale in the next is exactly the failure this guards.
            Site(
                "common/docs/herdr-run/USER_GUIDE.template.md",
                r"`retention_days` \((?P<value>[A-Za-z\d_,]+) by default\)",
                "the retention window the user guide states",
            ),
            Site(
                "common/docs/herdr-run/rendered/python/USER_GUIDE.md",
                r"`retention_days` \((?P<value>[A-Za-z\d_,]+) by default\)",
                "the retention window the packaged Python guide states",
            ),
            Site(
                "common/docs/herdr-run/rendered/rust/USER_GUIDE.md",
                r"`retention_days` \((?P<value>[A-Za-z\d_,]+) by default\)",
                "the retention window the packaged Rust guide states",
            ),
            Site(
                "py/herdr_run/retention.py",
                r"(?P<value>[A-Za-z\d_,]+) days spans a long weekend",
                "the reason the Python constant gives for itself",
            ),
        ),
    ),
    Pin(
        "max-timeout-seconds",
        "the largest accepted timeout, in seconds",
        code=(
            Site(
                "rs/herdr-run/src/config.rs",
                r"(?m)^pub const MAX_TIMEOUT_SECONDS: f64 = (?P<value>[\d_.]+);$",
                "the Rust config bound",
            ),
            Site(
                "py/herdr_run/config.py",
                r"(?m)^MAX_TIMEOUT_SECONDS = (?P<value>[\d_.]+)$",
                "the Python config bound",
            ),
            Site(
                "rs/agentctl/src/legacy_cli.rs",
                r"(?m)^const MAX_WAIT_SECONDS: f64 = (?P<value>[\d_.]+);$",
                "the Rust herdr-agent bound",
            ),
            Site(
                "py/agentctl/legacy_cli.py",
                r"(?m)^_MAX_WAIT_SECONDS = (?P<value>[\d_.]+)$",
                "the Python herdr-agent bound",
            ),
            Site(
                "py/agentctl/cli.py",
                r"value < 0 or value > (?P<value>[\d_]+)",
                "the canonical Python CLI timeout bound",
            ),
            Site(
                "rs/agentctl/src/cli.rs",
                r"!\(0\.0\.\.=(?P<value>[\d_.]+)\)\.contains\(&value\)",
                "the canonical Rust CLI timeout bound",
            ),
        ),
        docs=(
            Site(
                "py/agentctl/USER_GUIDE.md",
                r"finite seconds no\s+greater than (?P<value>[\d_,]+)",
                "the wait ceiling the agent guide states",
            ),
            Site(
                "common/docs/herdr-run/USER_GUIDE.template.md",
                r"no greater than (?P<value>[\d_,]+) seconds \(one year\)",
                "the timeout ceiling the user guide states",
            ),
            Site(
                "common/docs/herdr-run/rendered/python/USER_GUIDE.md",
                r"no greater than (?P<value>[\d_,]+) seconds \(one year\)",
                "the timeout ceiling the packaged Python guide states",
            ),
            Site(
                "common/docs/herdr-run/rendered/rust/USER_GUIDE.md",
                r"no greater than (?P<value>[\d_,]+) seconds \(one year\)",
                "the timeout ceiling the packaged Rust guide states",
            ),
        ),
    ),
    Pin(
        "max-count",
        "the largest accepted `--max-attempts` / `--lines`",
        code=(
            Site(
                "rs/agentctl/src/legacy_cli.rs",
                r"(?m)^const MAX_COUNT: u64 = (?P<value>[\d_]+);$",
                "the Rust bound",
            ),
            Site(
                "py/agentctl/legacy_cli.py",
                r"(?m)^_MAX_COUNT = (?P<value>[\d_]+)$",
                "the Python bound",
            ),
            Site(
                "rs/agentctl/src/cli.rs",
                r"value_parser = clap::value_parser!\((?:u64|u32)\)\.range\(1\.\.=(?P<value>[\d_]+)\)",
                "the canonical Rust CLI count bounds",
            ),
        ),
        docs=(
            Site(
                "py/agentctl/USER_GUIDE.md",
                r"must be between 1 and (?P<value>[\d_,]+)",
                "the count ceiling the agent guide states",
            ),
        ),
    ),
    Pin(
        "max-retention-days",
        "the largest accepted `retention_days`",
        code=(
            Site(
                "rs/herdr-run/src/retention.rs",
                r"(?m)^pub const MAX_RETENTION_DAYS: u64 = (?P<value>[\d_]+);$",
                "the Rust bound",
            ),
            Site(
                "py/herdr_run/retention.py",
                r"(?m)^MAX_RETENTION_DAYS = (?P<value>[\d_]+)$",
                "the Python bound",
            ),
        ),
        docs=(
            Site(
                "common/docs/herdr-run/USER_GUIDE.template.md",
                r"retention beyond (?P<value>[\d_,]+) days",
                "the retention ceiling the user guide states",
            ),
            Site(
                "common/docs/herdr-run/rendered/python/USER_GUIDE.md",
                r"retention beyond (?P<value>[\d_,]+) days",
                "the retention ceiling the packaged Python guide states",
            ),
            Site(
                "common/docs/herdr-run/rendered/rust/USER_GUIDE.md",
                r"retention beyond (?P<value>[\d_,]+) days",
                "the retention ceiling the packaged Rust guide states",
            ),
        ),
    ),
    # The chat bridge's post-rate breaker. Its guide argues from these numbers: what a normal
    # thread may send, how long a loop is held, and how fast a loop still gets through. The
    # patterns allow any whitespace between words, so a reflow of the guide does not break them.
    Pin(
        "chat-thread-reply-limit",
        "the reply operations one chat thread may reserve within the breaker window",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^const MAX_THREAD_REPLIES_PER_WINDOW: usize = (?P<value>[\d_]+);$",
                "the Rust breaker limit",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"may\s+reserve\s+(?P<value>[A-Za-z\d,]+)\s+distinct\s+reply\s+operations",
                "the per-thread budget the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"goes\s+out\s+(?P<value>[A-Za-z\d,]+)\s+at\s+a\s+time",
                "the backlog batch the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"after\s+each\s+group\s+of\s+(?P<value>[A-Za-z\d,]+)\.",
                "the backlog batch the chat guide repeats",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"receipt\s+of\s+the\s+post\s+(?P<value>[A-Za-z\d,]+)\s+before\s+it",
                "the fastest loop the chat guide says never trips",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"sends\s+(?P<value>[A-Za-z\d,]+)\s+more\s+posts\s+after\s+each",
                "the per-hold post count the chat guide gives for a tripped loop",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"even\s+if\s+(?P<value>[A-Za-z\d,]+)\s+of\s+its\s+reservations",
                "the reservation count the chat guide says trips an unlisted thread again",
            ),
        ),
    ),
    Pin(
        "chat-thread-reply-window-seconds",
        "the chat breaker's reply window, in seconds",
        code=(
            # The constant is in milliseconds. The pattern stops before its final `_000`, so the
            # value it lifts is the seconds the guide states.
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^const THREAD_REPLY_WINDOW_MILLIS: u64 = (?P<value>[\d_]+)_000;$",
                "the Rust breaker window",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"distinct\s+reply\s+operations\s+within\s+(?P<value>[A-Za-z\d,]+)\s+seconds",
                "the window the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"starts\s+at\s+least\s+(?P<value>[A-Za-z\d,]+)\s+seconds\s+after",
                "the window of the loop the chat guide says never trips",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"its\s+\w+\s+post\s+within\s+(?P<value>[A-Za-z\d,]+)\s+seconds",
                "the window in which the chat guide says a faster loop trips",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"(?P<value>[A-Za-z\d,]+)-second\s+retention\s+window",
                "the receipt retention the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-thread-breaker-cooldown-seconds",
        "how long a tripped chat thread holds its replies, in seconds",
        code=(
            # In milliseconds, lifted as seconds, as for the window.
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^const THREAD_BREAKER_COOLDOWN_MILLIS: u64 = (?P<value>[\d_]+)_000;$",
                "the Rust breaker cooldown",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"captured\s+but\s+unsent\s+for\s+(?P<value>[A-Za-z\d,]+)\s+seconds",
                "the hold the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"after\s+each\s+(?P<value>[A-Za-z\d,]+)-second\s+hold",
                "the hold the chat guide's loop bound assumes",
            ),
        ),
    ),
    Pin(
        "chat-already-reported-log-ids",
        "the already reported reply IDs one chat recovery-scan log line names",
        code=(
            Site(
                "rs/agentctl/src/chat_service.rs",
                r"(?m)^const ALREADY_REPORTED_LOG_IDS: usize = (?P<value>[\d_]+);$",
                "the Rust log bound",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"names\s+up\s+to\s+(?P<value>[A-Za-z\d,]+)\s+of\s+them",
                "the log line the chat guide describes",
            ),
        ),
    ),
    Pin(
        "chat-feedback-ids-per-scan",
        "the unavailable reply IDs one chat capture keeps in each of its lists",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^const MAX_FEEDBACK_UNAVAILABLE_IDS: usize = (?P<value>[\d_]+);$",
                "the Rust list bound",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"up\s+to\s+(?P<value>[A-Za-z\d,]+)\s+per\s+scan",
                "the per-scan bound the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-feedback-available-ids",
        "the open requests with no reply yet that one chat routing-error prompt names",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^const MAX_FEEDBACK_AVAILABLE_IDS: usize = (?P<value>[\d_]+);$",
                "the Rust display bound",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"The\s+first\s+list\s+names\s+up\s+to\s+(?P<value>[A-Za-z\d,]+)\s+requests",
                "the prompt the chat guide describes",
            ),
        ),
    ),
    Pin(
        "chat-feedback-replied-ids",
        "the open requests with a reply that one chat routing-error prompt names",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^const MAX_FEEDBACK_REPLIED_IDS: usize = (?P<value>[\d_]+);$",
                "the Rust display bound",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"requests\s+and\s+the\s+second\s+up\s+to\s+(?P<value>[A-Za-z\d,]+),\s+and\s+each",
                "the prompt the chat guide describes",
            ),
        ),
    ),
    Pin(
        "chat-held-back-digest-chars",
        "the characters of a partial chat reply block that identify it in a routing-error report",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^const HELD_BACK_DIGEST_CHARS: usize = (?P<value>[\d_]+);$",
                "the Rust digest span",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"a\s+digest\s+of\s+(?P<value>[A-Za-z\d,]+)\s+characters\s+of\s+it",
                "the digest span the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"the\s+last\s+(?P<value>[A-Za-z\d,]+)\s+of\s+an\s+unopened\s+block",
                "the end of a block the chat guide says the digest covers",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"the\s+first\s+(?P<value>[A-Za-z\d,]+)\s+of\s+a\s+block\s+with\s+no\s+closing",
                "the start of a block the chat guide says the digest covers",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"at\s+least\s+(?P<value>[A-Za-z\d,]+)\s+characters\s+of\s+it\s+are\s+in\s+view",
                "the scrolling bound the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"fewer\s+than\s+(?P<value>[A-Za-z\d,]+)\s+such\s+characters",
                "the short block the chat guide describes",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"fewer\s+than\s+(?P<value>[A-Za-z\d,]+)\s+characters\s+of\s+one\s+at\s+the\s+top",
                "the remnant the chat guide describes",
            ),
        ),
    ),
    Pin(
        "chat-skipped-reply-aliases",
        "the unassigned short reply IDs seen in chat reply markers that are kept so none is "
        "assigned later",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^const MAX_BURNED_REPLY_ALIASES: usize = (?P<value>[\d_]+);$",
                "the Rust skip-list bound",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"up\s+to\s+(?P<value>[A-Za-z\d,]+)\s+such\s+numbers\s+are\s+kept",
                "the skip-list bound the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"count\s+toward\s+the\s+(?P<value>[A-Za-z\d,]+)\s+skipped\s+numbers\s+kept",
                "the skip-list bound the chat guide states for numbers seen before a prompt",
            ),
        ),
    ),
    Pin(
        "chat-remembered-reply-blocks",
        "the chat reply blocks read under the short reply ID of a request whose prompt has not "
        "reached the agent that are remembered so their text is never sent to that request",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^const MAX_WITHHELD_REPLIES: usize = (?P<value>[\d_]+);$",
                "the Rust bound on remembered blocks",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"Up\s+to\s+(?P<value>[A-Za-z\d,]+)\s+remembered\s+blocks\s+are\s+kept",
                "the bound on remembered blocks the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"forgotten\s+once\s+(?P<value>[A-Za-z\d,]+)\s+newer\s+ones\s+are\s+remembered",
                "the bound on remembered blocks the chat guide states among the gaps",
            ),
        ),
    ),
    Pin(
        "chat-saturated-poll-seconds",
        "how often `chat run` reads the agent's pane while a closing line under an open request's "
        "short reply ID is in view, in seconds",
        code=(
            Site(
                "rs/agentctl/src/chat_service.rs",
                r"(?m)^const SATURATED_POLL_INTERVAL: Duration = "
                r"Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust poll interval",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"reads\s+the\s+pane\s+itself\s+every\s+(?P<value>[A-Za-z\d,]+)\s+seconds?\b",
                "the poll interval the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"logged\s+once\s+and\s+retried\s+every\s+(?P<value>[A-Za-z\d,]+)\s+seconds?\b",
                "the retry interval of a failed poll the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"ends\s+its\s+next\s+wait\s+within\s+(?P<value>[A-Za-z\d,]+)\s+seconds?\b",
                "the retry interval of a failed status lookup the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_quickstart.md",
                r"`run`\s+reads\s+the\s+pane\s+every\s+(?P<value>[A-Za-z\d,]+)\s+seconds?\b",
                "the poll interval the chat quickstart states",
            ),
        ),
    ),
    Pin(
        "chat-thread-history-default",
        "the messages `agentctl chat thread` prints, and a request prompt's command asks for",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^pub const DEFAULT_THREAD_HISTORY_MESSAGES: u32 = (?P<value>[\d_]+);$",
                "the Rust default",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"\(default\s+(?P<value>[A-Za-z\d,]+),\s+at\s+most\s+[A-Za-z\d,]+\)",
                "the --last default the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"--thread\s+spaces/example/threads/one\s+--last\s+(?P<value>\d+)\n```",
                "the prompt's history command the chat guide shows",
            ),
        ),
    ),
    Pin(
        "chat-thread-history-maximum",
        "the most messages `agentctl chat thread --last` accepts",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^pub const MAX_THREAD_HISTORY_MESSAGES: u32 = (?P<value>[\d_]+);$",
                "the Rust maximum",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"\(default\s+[A-Za-z\d,]+,\s+at\s+most\s+(?P<value>[A-Za-z\d,]+)\)",
                "the --last maximum the chat guide states",
            ),
            Site(
                "rs/agentctl/src/cli.rs",
                r"retained\s+messages,\s+oldest\s+first\s+\(1-(?P<value>\d+)\)",
                "the --last help",
            ),
        ),
    ),
    Pin(
        "chat-provider-retry-min-seconds",
        "the first wait before `chat run` reconnects its provider, in seconds",
        code=(
            Site(
                "rs/agentctl/src/chat_service.rs",
                r"(?m)^const PROVIDER_RETRY_MIN: Duration = Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust shortest wait",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"provider\s+reconnect\s+wait\s+starts\s+at\s+(?P<value>[A-Za-z\d,]+)\s+seconds?\b",
                "the first wait the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"the\s+wait\s+after\s+it\s+starts\s+again\s+at\s+(?P<value>[A-Za-z\d,]+)\s+seconds?\b",
                "the wait the chat guide states after a healthy attempt",
            ),
        ),
    ),
    Pin(
        "chat-provider-retry-max-seconds",
        "the longest wait before `chat run` reconnects its provider, and the attempt length that "
        "counts as healthy, in seconds",
        code=(
            Site(
                "rs/agentctl/src/chat_service.rs",
                r"(?m)^const PROVIDER_RETRY_MAX: Duration = Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust longest wait",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"attempt\s+that\s+lasted\s+less\s+than\s+(?P<value>[A-Za-z\d,]+)\s+seconds",
                "the attempt length below which the chat guide says the wait doubles",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"less\s+than\s+[A-Za-z\d,]+\s+seconds,\s+up\s+to\s+(?P<value>[A-Za-z\d,]+)\s+seconds",
                "the longest wait the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"lasted\s+at\s+least\s+(?P<value>[A-Za-z\d,]+)\s+seconds\s+counts\s+as\s+healthy",
                "the healthy attempt length the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-delivery-retry-seconds",
        "the period at which `chat run` scans its request records for prompts left waiting to be"
        " typed, in seconds",
        code=(
            Site(
                "rs/agentctl/src/chat_service.rs",
                r"(?m)^const DELIVERY_RETRY_INTERVAL: Duration = Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust scan period",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"Every\s+(?P<value>[A-Za-z\d,]+)\s+seconds\s+`run`\s+also\s+scans\s+the\s+request\s+records",
                "the scan period the chat guide's run paragraph states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"Every\s+(?P<value>[A-Za-z\d,]+)\s+seconds\s+`run`\s+scans\s+the\s+request\s+records\.",
                "the scan period the chat guide's prompt delivery section states",
            ),
        ),
    ),
    Pin(
        "chat-delivery-stall-seconds",
        "the age after admission at which a request whose prompt has not reached the agent is"
        " stalled, in seconds",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^pub\(crate\) const DELIVERY_STALL_AFTER: Duration = Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust stall age",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"stalled\s+once\s+(?P<value>[A-Za-z\d,]+)\s+seconds\s+have\s+passed\s+since\s+its\s+admission",
                "the stall age the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-delivery-stall-repeat-seconds",
        "the period at which `chat run` logs again a stalled request whose prompt it is still"
        " trying to type, in seconds",
        code=(
            Site(
                "rs/agentctl/src/chat_service.rs",
                r"(?m)^const DELIVERY_STALL_REPEAT: Duration = Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust repeat period",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"service\s+logs\s+it\s+again\s+every\s+(?P<value>[A-Za-z\d,]+)\s+seconds",
                "the repeat period the chat guide states",
            ),
        ),
    ),
    # The ✅ receipt reaction: how long the queue looks for printed evidence, how often it reads
    # the screen meanwhile, and the bounds on adding the reactions it saves.
    Pin(
        "chat-print-grace-seconds",
        "the longest extra wait for evidence that the pane printed a submitted prompt, in seconds",
        code=(
            Site(
                "rs/agentctl/src/submission.rs",
                r"(?m)^pub const PRINT_GRACE: Duration = Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust grace period",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"for\s+up\s+to\s+(?P<value>[A-Za-z\d,]+)\s+more\s+seconds,\s+pressing\s+no\s+other\s+key",
                "the grace period the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-submission-poll-millis",
        "how often the queue reads the screen while it waits on a submitted prompt, in milliseconds",
        code=(
            Site(
                "rs/agentctl/src/submission.rs",
                r"(?m)^pub const POLL: Duration = Duration::from_millis\((?P<value>[\d_]+)\);$",
                "the Rust screen polling interval",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"reads\s+the\s+screen\s+every\s+(?P<value>[A-Za-z\d,]+)\s+milliseconds\s+for\s+up\s+to",
                "the polling interval the chat guide states for printed evidence",
            ),
        ),
    ),
    Pin(
        "chat-credential-record-stale-seconds",
        "the age at which `chat status` calls the saved credential observation stale, in seconds",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^pub\(crate\) const CREDENTIAL_RECORD_STALE_AFTER: Duration = Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust stale threshold",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"once\s+the\s+observation\s+is\s+older\s+than\s+(?P<value>[A-Za-z\d,]+)\s+seconds",
                "the stale threshold the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-credential-record-refresh-seconds",
        "the longest `run` leaves `provider-credentials.json` unrewritten while the files are unchanged",
        code=(
            Site(
                "rs/agentctl/src/chat_service.rs",
                r"(?m)^const CREDENTIAL_RECORD_REFRESH: Duration = Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust refresh interval",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"at\s+least\s+every\s+(?P<value>[A-Za-z\d,]+)\s+seconds,\s+with\s+`observed_at_millis`",
                "the refresh interval the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-late-prompt-seconds",
        "how much later than its create time a prompt is typed before it says how long it waited, in seconds",
        code=(
            Site(
                "rs/agentctl/src/prompt_time.rs",
                r"(?m)^pub\(crate\) const LATE_PROMPT_AFTER: Duration = Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust lateness threshold",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"typed\s+(?P<value>[A-Za-z\d,]+)\s+seconds\s+or\s+more\s+after\s+that\s+time",
                "the lateness threshold the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-credential-expiry-warning-hours",
        "how long before a credential's notAfter `delivery-alarm.json` lists it, in hours",
        code=(
            Site(
                "rs/agentctl/src/credentials.rs",
                r"(?m)^pub\(crate\) const CREDENTIAL_EXPIRY_WARNING: Duration = Duration::from_secs\((?P<value>[\d_]+) \* 60 \* 60\);$",
                "the Rust warning lead, in hours",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"within\s+(?P<value>[A-Za-z\d,]+)\s+hours\s+of\s+its\s+notAfter\s+is\s+listed",
                "the warning lead the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-provider-down-after-failures",
        "how many failures in a row put a provider path in `delivery-alarm.json` as down",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^pub\(crate\) const PROVIDER_DOWN_AFTER_FAILURES: u64 = (?P<value>[\d_]+);$",
                "the Rust failure count",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"Once\s+a\s+path\s+has\s+failed\s+(?P<value>[A-Za-z\d,]+)\s+times\s+in\s+a\s+row",
                "the failure count the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-ack-retry-seconds",
        "the least time `chat run` waits before it tries a failed or uncertain reaction again, in"
        " seconds",
        code=(
            Site(
                "rs/agentctl/src/chat_service.rs",
                r"(?m)^const ACK_RETRY_DELAY: Duration = Duration::from_secs\((?P<value>[\d_]+)\);$",
                "the Rust retry delay",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"wait\s+at\s+least\s+(?P<value>[A-Za-z\d,]+)\s+seconds\s+before\s+an\s+in-process\s+retry",
                "the ACK retry delay the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"waits\s+at\s+least\s+(?P<value>[A-Za-z\d,]+)\s+seconds\s+before\s+the\s+worker\s+tries\s+it\s+again",
                "the receipt reaction retry delay the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-keys-per-pass",
        "the requests one chat pass handles, and the saved ✅ reactions one `chat tick` adds",
        code=(
            Site(
                "rs/agentctl/src/chat_service.rs",
                r"(?m)^const MAX_KEYS_PER_PASS: usize = (?P<value>[\d_]+);$",
                "the Rust per-pass bound",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"That\s+pass\s+handles\s+at\s+most\s+(?P<value>[A-Za-z\d,]+)\s+requests",
                "the startup recovery pass bound the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"`chat\s+tick`\s+adds\s+at\s+most\s+(?P<value>[A-Za-z\d,]+)\s+saved\s+✅\s+reactions",
                "the receipt reactions per tick the chat guide states",
            ),
        ),
    ),
    Pin(
        "chat-request-limit",
        "the active requests one chat state holds, and the ✅ reactions that may wait at once",
        code=(
            Site(
                "rs/agentctl/src/chat_runtime.rs",
                r"(?m)^const MAX_REQUESTS: u64 = (?P<value>[\d_]+);$",
                "the Rust request cap",
            ),
        ),
        docs=(
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"The\s+active\s+population\s+remains\s+capped\s+at\s+(?P<value>[A-Za-z\d,]+)\s+requests",
                "the request cap the chat guide states",
            ),
            Site(
                "rs/agentctl/src/embedded_chat_userguide.md",
                r"At\s+most\s+(?P<value>[A-Za-z\d,]+)\s+✅\s+reactions\s+wait\s+at\s+once",
                "the receipt reaction cap the chat guide states",
            ),
        ),
    ),
)


def read_repo(path: str) -> str:
    return (REPO_ROOT / path).read_text(encoding="utf-8")


def check_pin(pin: Pin, read: Reader) -> list[str]:
    """Every way this default can be wrong, reported with file, line and both numbers."""
    failures: list[str] = []

    # 1. What the code says. Every definition site must agree before any prose is consulted.
    code_values: list[tuple[Site, int, float]] = []
    for site in pin.code:
        found, problems = site.occurrences(read)
        failures.extend(f"{pin.slug}: {problem}" for problem in problems)
        code_values.extend((site, line, value) for line, value in found)
    if not code_values:
        return failures

    agreed = code_values[0][2]
    for site, line, value in code_values[1:]:
        if value != agreed:
            first = code_values[0]
            failures.append(
                f"{pin.slug}: the implementations disagree about {pin.what} — "
                f"{first[0].path}:{first[1]} says {show(agreed)}, "
                f"{site.path}:{line} says {show(value)}"
            )

    # 2. What the documents say. Compared against the code, never derived from it.
    for site in pin.docs:
        found, problems = site.occurrences(read)
        failures.extend(f"{pin.slug}: {problem}" for problem in problems)
        for line, value in found:
            if value != agreed:
                source = code_values[0]
                failures.append(
                    f"{pin.slug}: {site.path}:{line} states {show(value)} for {pin.what}, "
                    f"but the code default is {show(agreed)} "
                    f"({source[0].path}:{source[1]}) — {site.says} is stale"
                )
    return failures


def check(read: Reader, pins: tuple[Pin, ...] = PINS) -> list[str]:
    failures: list[str] = []
    for pin in pins:
        failures.extend(check_pin(pin, read))
    return failures


def occurrence_count(read: Reader, pins: tuple[Pin, ...] = PINS) -> int:
    """How many documented occurrences were actually compared, for an honest success line."""
    total = 0
    for pin in pins:
        for site in pin.docs:
            found, _ = site.occurrences(read)
            total += len(found)
    return total


def _rewrite_value(replacement: str) -> Callable[[re.Match[str]], str]:
    """Substitution that swaps only the captured `value`, leaving its surroundings intact."""

    def rewrite(match: re.Match[str]) -> str:
        whole = match.group(0)
        start, end = match.span("value")
        return whole[: start - match.start()] + replacement + whole[end - match.start() :]

    return rewrite


def mutated_reader(read: Reader, edits: tuple[tuple[Site, str], ...]) -> Reader:
    """A reader that rewrites the captured number at each given site. Used only by `--self-test`."""
    overrides: dict[str, str] = {}
    for site, replacement in edits:
        text = overrides.get(site.path, read(site.path))
        overrides[site.path] = re.sub(site.pattern, _rewrite_value(replacement), text)

    def reader(path: str) -> str:
        return overrides[path] if path in overrides else read(path)

    return reader


def self_test() -> int:
    """Prove the guard is not tautological. No git, no network, no build.

    The controls that matter are the mutation ones: for every pin, this rewrites the code constant
    in memory and requires the real check to go red and NAME every document that promises it. A
    check that formatted the sentence from the constant would stay green here.
    """
    failures: list[str] = []

    def expect(condition: bool, why: str) -> None:
        if not condition:
            failures.append(why)

    expect(parse_number("1,000,000") == 1_000_000, "a comma-grouped numeral must parse")
    expect(parse_number("31_536_000.0") == 31_536_000, "an underscore-grouped float must parse")
    expect(parse_number("Four") == 4, "a spelled-out count must parse, case-insensitively")
    try:
        parse_number("several")
        failures.append("an unknown word must be a parse failure, not a silent skip")
    except Unparseable:
        pass

    expect(not check(read_repo), "the tree as committed must pass")

    for pin in PINS:
        # (a) The whole point. Change the code default in EVERY edition -- the realistic edit --
        #     and every document that states it must be reported, by path and by both numbers.
        code_value = code_value_of(pin, read_repo)
        if code_value is None:
            failures.append(f"{pin.slug}: no code site could be read, so nothing was proven")
            continue
        bumped = show(code_value + 1)
        read = mutated_reader(read_repo, tuple((site, bumped) for site in pin.code))
        reported = check_pin(pin, read)
        for site in pin.docs:
            named = [line for line in reported if line.startswith(f"{pin.slug}: {site.path}:")]
            expect(
                bool(named),
                f"{pin.slug}: changing the code default to {bumped} left {site.path} unreported —"
                " this guard is not comparing the document against the code",
            )
            expect(
                any(
                    f"states {show(code_value)} for" in line and f"code default is {bumped}" in line
                    for line in named
                ),
                f"{pin.slug}: the failure for {site.path} must name BOTH numbers — what the"
                f" document says and what the code now says — got {named}",
            )

        # (b) One edition changed alone is a divergence, reported before any prose.
        if len(pin.code) > 1:
            drifted = mutated_reader(read_repo, ((pin.code[1], bumped),))
            expect(
                any("disagree" in line for line in check_pin(pin, drifted)),
                f"{pin.slug}: one implementation changing alone must be reported as a divergence",
            )

        # (c) The prose changing alone is caught too, from the other direction.
        for site in pin.docs:
            edited = mutated_reader(read_repo, ((site, bumped),))
            expect(
                any(line.startswith(f"{pin.slug}: {site.path}:") for line in check_pin(pin, edited)),
                f"{pin.slug}: editing {site.path} to {bumped} must be caught",
            )

        # (d) A pattern that stops matching is loud. A silently vacuous guard is the worse bug:
        #     it reads as green forever while checking nothing.
        for site in (*pin.code, *pin.docs):
            def blanked(path: str, site: Site = site) -> str:
                text = read_repo(path)
                return re.sub(site.pattern, "<reworded>", text) if path == site.path else text

            expect(
                any("nothing matches the pattern" in line for line in check_pin(pin, blanked)),
                f"{pin.slug}: rewording {site.path} past the pattern must fail, not pass vacuously",
            )

    for failure in failures:
        print(f"FAIL  {failure}", file=sys.stderr)
    if failures:
        print(f"\n{len(failures)} self-test failure(s)", file=sys.stderr)
        return 1
    controls = 5 + sum(3 * len(pin.docs) + len(pin.code) + (1 if len(pin.code) > 1 else 0) for pin in PINS)
    print(f"check-documented-defaults --self-test: PASSED ({controls} controls, {len(PINS)} pins)")
    return 0


def code_value_of(pin: Pin, read: Reader) -> float | None:
    """The value the code currently defines for `pin`, or `None` if no site could be read."""
    for site in pin.code:
        found, _ = site.occurrences(read)
        if found:
            return found[0][1]
    return None


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--self-test", action="store_true", help="check the guard offline, then exit")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    failures = check(read_repo)
    if failures:
        print("check-documented-defaults: FAIL", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        print(
            "\n  A document that states a default is a promise about the code. Change both, or\n"
            "  stop stating the number. See `#88 herdr-run-pin-documented-defaults`.",
            file=sys.stderr,
        )
        return 1

    print(
        f"check-documented-defaults: ok — {occurrence_count(read_repo)} documented occurrences of "
        f"{len(PINS)} defaults agree with the code"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

"""Verified prompt submission for terminal agent composers.

A terminal agent accepts a prompt in two separate steps: the text is staged in
its composer, and a submission key moves it into the conversation (or into the
agent's own queue while it is busy). Sending the key is not proof that the
second step happened. An agent can drop a key while it is redrawing or running
its own hooks, and the text then stays in the composer indefinitely.

This module drives both steps against the rendered screen and reports one of
three outcomes:

* nothing was typed (``PromptNotStaged``) -- the prompt is safe to retry;
* the text was staged but never left the composer
  (``PromptStagedNotSubmitted``) -- it is visible and unsubmitted, so it must
  not be typed again automatically;
* the text left the composer and the screen corroborates a submission
  (a ``SubmissionReceipt``).

Any other failure after typing began is ambiguous and is raised as
``HerdrUnavailable``, which callers treat as possibly submitted.
"""

from __future__ import annotations

import re
import time
from collections.abc import Callable
from dataclasses import dataclass
from typing import Protocol

from agentctl.errors import HerdrUnavailable

__all__ = [
    "ComposerView",
    "PromptNotStaged",
    "PromptStagedNotSubmitted",
    "SubmissionReceipt",
    "VERIFIED_HARNESSES",
    "codex_idle_composer",
    "composer_view",
    "render_screen",
    "submit_verified",
]

#: Harnesses whose composer layout this module can read.
VERIFIED_HARNESSES = frozenset({"claude", "codex"})

_BRACKETED_PASTE_START = "\x1b[200~"
_BRACKETED_PASTE_END = "\x1b[201~"
#: Screen rows requested for every composer read.
SCREEN_LINES = 200
#: Maximum wait for pasted text to appear in the composer before any key is sent.
#:
#: The wait ends at the first read that shows the paste, about 0.1 s after it in the
#: usual case. It is long because giving up leaves the paste unsubmitted in the
#: composer, where the draft check then holds every later prompt, and a paste typed
#: into a busy agent's pane has gone unseen for 3 s and then been found in its composer.
#: A paste that shows late or never costs the rest of the wait: the queue drain that
#: typed it keeps its queue and pane locks meanwhile, so other agentctl senders to the
#: pane wait too, and any new paste placeholder drawn in that time, even another
#: sender's, counts as this paste.
STAGE_TIMEOUT_SECONDS = 60.0
#: Maximum time spent retrying the submission key and waiting for corroboration.
SUBMIT_TIMEOUT_SECONDS = 60.0
#: Maximum time spent waiting, before typing anything, for a recognisable composer.
#: Herdr can report a freshly launched harness ready before it has drawn its
#: composer; nothing is typed while waiting, so the wait cannot duplicate a prompt.
COMPOSER_WAIT_SECONDS = 15.0
#: First wait before a still-staged prompt receives another submission key.
FIRST_RETRY_SECONDS = 0.5
#: Upper bound on the doubling wait between submission keys.
MAX_RETRY_SECONDS = 4.0
#: Screen polling interval while waiting.
POLL_SECONDS = 0.1

_SGR = re.compile(r"\x1b\[([0-9;:]*)m")
_ESCAPE = re.compile(
    r"\x1b(?:\[[0-9;:?<=>]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[PX^_][^\x1b]*\x1b\\|[@-Z\\-_])"
)
_RULE_CHARACTERS = frozenset("─━═")
_PASTE_PLACEHOLDER = re.compile(r"\[Pasted (?:text #\d+|Content \d+ chars)")
_QUEUE_MARKERS = (
    "Press up to edit queued messages",
    "edit last queued message",
    "Queued follow-up inputs",
)
_WORKING_MARKER = "esc to interrupt"
_CODEX_QUEUE_HINT = "tab to queue message"
#: Glyphs Codex draws in column 0 of its composer's first row. Earlier releases
#: draw ``›``; v0.159.1 draws ``»``, and keeps ``›`` for selection lists such as
#: its folder-trust prompt.
_CODEX_COMPOSER_MARKERS = ("›", "»")
_CODEX_READY_BLOCKERS = (
    _WORKING_MARKER, _CODEX_QUEUE_HINT, *_QUEUE_MARKERS,
    "paste again to expand", "trust this folder?", "trust and continue",
    "approval required", "requires approval", "approve this", "would you like to run",
    "press enter to continue", "enter to confirm", "esc to cancel",
    " to interrupt", "messages to be submitted after next tool call",
    "messages to be submitted at end of turn", "enter to submit answer",
    "enter to submit all", "allow command?", "do you trust the contents of this directory?",
)
_CODEX_LIVE_TIMER = re.compile(
    r"^(?:[•◦][ \t]+)?[^\s›•◦■✗✓─][^\r\n]* "
    r"\((?:[0-9]+[hm] )*[0-9]+s(?: • [^\r\n]+? to interrupt)?\)"
    r"(?: · [^\r\n]*)?$"
)
_CODEX_QUEUED_ROWS = (
    "queued follow-up inputs", "messages to be submitted after next tool call",
    "messages to be submitted at end of turn",
)


class PromptNotStaged(HerdrUnavailable):
    """No prompt text was typed; the durable request remains safe to retry."""


class PromptStagedNotSubmitted(HerdrUnavailable):
    """Prompt text is visible in the composer and was not submitted."""


@dataclass(frozen=True)
class SubmissionReceipt:
    """Positive evidence that a staged prompt left the composer."""

    key: str
    key_presses: int
    elapsed_seconds: float
    evidence: str


@dataclass(frozen=True)
class ComposerView:
    """One screen split around the agent's composer."""

    transcript: str
    composer: str
    composer_solid: str
    footer: str


class PromptTerminal(Protocol):
    """Terminal operations used by :func:`submit_verified`."""

    def read_screen(self, pane_id: str) -> str:
        """Return visible rows with SGR styling retained."""

    def send_text(self, pane_id: str, text: str) -> None:
        """Insert literal bytes without a submission key."""

    def send_keys(self, pane_id: str, keys: str) -> None:
        """Send one named key."""


def render_screen(screen: str) -> list[tuple[str, str]]:
    """Return ``(plain, solid)`` per row; ``solid`` omits faint and reverse-video cells.

    Agents draw composer placeholders and the cursor cell in faint or reverse
    video. Omitting those cells lets an empty composer be told apart from one
    holding a typed draft without a list of placeholder sentences.
    """
    rows: list[tuple[str, str]] = []
    faint = False
    reverse = False
    for raw in screen.replace("\r", "").split("\n"):
        plain: list[str] = []
        solid: list[str] = []
        index = 0
        while index < len(raw):
            character = raw[index]
            if character == "\x1b":
                sgr = _SGR.match(raw, index)
                if sgr is not None:
                    faint, reverse = _apply_sgr(sgr.group(1), faint, reverse)
                    index = sgr.end()
                    continue
                escape = _ESCAPE.match(raw, index)
                index = escape.end() if escape is not None else index + 1
                continue
            if character < " " and character != "\t":
                index += 1
                continue
            plain.append(character)
            solid.append(" " if faint or reverse else character)
            index += 1
        rows.append(("".join(plain), "".join(solid)))
    return rows


def _apply_sgr(parameters: str, faint: bool, reverse: bool) -> tuple[bool, bool]:
    codes = [part.split(":")[0] for part in parameters.split(";")] if parameters else ["0"]
    index = 0
    while index < len(codes):
        code = codes[index]
        if code in ("", "0"):
            faint = reverse = False
        elif code == "2":
            faint = True
        elif code == "22":
            faint = False
        elif code == "7":
            reverse = True
        elif code == "27":
            reverse = False
        elif code in ("38", "48", "58") and index + 1 < len(codes):
            # Extended colours carry operands (5;N or 2;R;G;B) that are not attributes.
            index += 2 if codes[index + 1] == "5" else 4 if codes[index + 1] == "2" else 0
        index += 1
    return faint, reverse


def _is_rule(line: str) -> bool:
    stripped = line.strip()
    return len(stripped) >= 3 and all(character in _RULE_CHARACTERS for character in stripped)


def _is_labelled_rule(line: str) -> bool:
    """Return whether ``line`` is a rule, possibly with a label drawn into it.

    Claude can draw session state into the top border of its composer, as in
    ``──────── ultracode ─``. Such a row still starts and ends with rule
    characters; whatever lies between the two runs is the label.
    """
    stripped = line.strip()
    leading = len(stripped) - len(stripped.lstrip("".join(_RULE_CHARACTERS)))
    trailing = len(stripped) - len(stripped.rstrip("".join(_RULE_CHARACTERS)))
    return len(stripped) >= 3 and leading >= 1 and trailing >= 1 and leading + trailing >= 3


def _claude_view(rows: list[tuple[str, str]]) -> ComposerView | None:
    # A labelled top border is tried only when two plain rules do not already
    # frame a composer, so a draft row that happens to look like a labelled
    # rule cannot change how an unlabelled screen is split.
    view = _claude_view_framed(rows, _is_rule)
    return view if view is not None else _claude_view_framed(rows, _is_labelled_rule)


def _claude_view_framed(
    rows: list[tuple[str, str]], is_top: Callable[[str], bool],
) -> ComposerView | None:
    """Split ``rows`` at the last plain rule and the nearest row above it accepted by ``is_top``."""
    bottom = next((index for index in range(len(rows) - 1, -1, -1) if _is_rule(rows[index][0])), None)
    if bottom is None:
        return None
    top = next((index for index in range(bottom - 1, -1, -1) if is_top(rows[index][0])), None)
    if top is None:
        return None
    body = rows[top + 1:bottom]
    if not body or not body[0][0].lstrip().startswith("❯"):
        return None
    plain_lines = [plain for plain, _ in body]
    solid_lines = [solid for _, solid in body]
    marker_offset = len(body[0][0]) - len(body[0][0].lstrip())
    plain_lines[0] = plain_lines[0][marker_offset + 1:]
    solid_lines[0] = solid_lines[0][marker_offset + 1:]
    return ComposerView(
        transcript="\n".join(plain for plain, _ in rows[:top]),
        composer="\n".join(plain_lines),
        composer_solid="\n".join(solid_lines),
        footer="\n".join(plain for plain, _ in rows[bottom + 1:]),
    )


def _codex_view(rows: list[tuple[str, str]]) -> ComposerView | None:
    populated = [index for index, (plain, _) in enumerate(rows) if plain.strip()]
    if len(populated) < 2:
        return None
    footer = populated[-1]
    if not rows[footer][0].startswith("  "):
        return None
    start = None
    for index in range(footer - 1, -1, -1):
        plain = rows[index][0]
        if plain.startswith(_CODEX_COMPOSER_MARKERS):
            start = index
            break
        if plain.strip() and not plain.startswith("  "):
            return None
    if start is None:
        return None
    body = rows[start:footer]
    plain_lines = [plain for plain, _ in body]
    solid_lines = [solid for _, solid in body]
    plain_lines[0] = plain_lines[0][1:]
    solid_lines[0] = solid_lines[0][1:]
    return ComposerView(
        transcript="\n".join(plain for plain, _ in rows[:start]),
        composer="\n".join(plain_lines),
        composer_solid="\n".join(solid_lines),
        footer="\n".join(plain for plain, _ in rows[footer:]),
    )


def composer_view(harness: str, screen: str) -> ComposerView | None:
    """Locate the composer of a supported harness, or ``None`` when it is not recognisable."""
    rows = render_screen(screen)
    if harness == "claude":
        return _claude_view(rows)
    if harness == "codex":
        return _codex_view(rows)
    return None


def codex_idle_composer(screen: str) -> bool:
    """Recognize an empty idle Codex editor; this is no proof of its recipient.

    Busy Codex screens retain their editor. A live elapsed timer without a later
    response, or an active queue or dialog in the final sixteen rows, refuses readiness.
    """
    view = composer_view("codex", screen)
    if (view is None or view.composer_solid.strip()
            or _PASTE_PLACEHOLDER.search(view.composer) is not None):
        return False
    footer = view.footer.strip()
    if not (
        ("·" in footer and re.search(r"(?:^|\s)(?:~/|/)(?:\S|$)", footer) is not None)
        or "? for shortcuts" in footer
        or re.search(r"\b\d+(?:\.\d+)?% context (?:left|remaining)\b", footer) is not None
    ):
        return False
    active_timer = False
    for row in view.transcript.splitlines():
        if _CODEX_LIVE_TIMER.fullmatch(row.rstrip()) is not None:
            active_timer = True
        elif (row.startswith(("•", "◦", "■", "✗", "✓"))
                and not any(label in row.casefold() for label in _CODEX_QUEUED_ROWS)):
            active_timer = False
    if active_timer:
        return False
    rows = [plain for plain, _solid in render_screen(screen)]
    while rows and not rows[-1].strip():
        rows.pop()
    active = "\n".join(
        row for row in rows[-16:] if _CODEX_LIVE_TIMER.fullmatch(row.rstrip()) is None
    ).casefold()
    if all(label in active for label in ("all results", "filesystem only", "plugins")):
        return False
    return not any(marker.casefold() in active for marker in _CODEX_READY_BLOCKERS)


def _compact(text: str) -> str:
    return "".join(text.split())


def _suffix(text: str, length: int) -> str:
    compact = _compact(text)
    return compact[-length:] if len(compact) > length else compact


def _staged(view: ComposerView, text: str, placeholders_before: int) -> bool:
    # Faint placeholder text is excluded so a short prompt is not "found" inside it.
    composer = _compact(view.composer_solid)
    wanted = _compact(text)
    if not wanted:
        return False
    if wanted in composer or _suffix(text, 80) in composer:
        return True
    return len(_PASTE_PLACEHOLDER.findall(view.composer)) > placeholders_before


def _transcript_count(view: ComposerView, text: str) -> int:
    return _compact(view.transcript).count(_suffix(text, 40))


def _corroborated(before: ComposerView, after: ComposerView, text: str) -> str | None:
    if _transcript_count(after, text) > _transcript_count(before, text):
        return "prompt text appeared above the composer"
    if (len(_PASTE_PLACEHOLDER.findall(after.transcript))
            > len(_PASTE_PLACEHOLDER.findall(before.transcript))):
        return "pasted prompt appeared above the composer"
    screen = f"{after.transcript}\n{after.footer}"
    for marker in _QUEUE_MARKERS:
        if marker in screen:
            return f"agent queue marker is visible ({marker!r})"
    if _WORKING_MARKER in screen:
        return "agent reports an active turn"
    return None


def _submit_key(harness: str, view: ComposerView) -> str:
    # A busy Codex steers the active turn on Enter and queues on Tab. It shows
    # the Tab hint only while it is busy and holds staged text.
    if harness == "codex" and _CODEX_QUEUE_HINT in view.footer:
        return "Tab"
    return "Enter"


def submit_verified(
    terminal: PromptTerminal,
    pane_id: str,
    harness: str,
    text: str,
    *,
    stage_timeout: float | None = None,
    submit_timeout: float | None = None,
    composer_timeout: float | None = None,
    sleep: Callable[[float], None] = time.sleep,
    monotonic: Callable[[], float] = time.monotonic,
) -> SubmissionReceipt:
    """Stage ``text`` in an empty composer, submit it, and prove that it left the composer.

    The submission key is repeated with a doubling wait while the exact text is
    still staged, so a key the agent dropped is retried without typing the
    prompt twice. A key is never repeated once the text has left the composer.
    Before typing, it waits up to ``composer_timeout`` for a recognisable composer,
    because a freshly launched harness can be reported ready before it draws one.
    Omitted timeouts use the module defaults at call time.
    """
    stage_timeout = STAGE_TIMEOUT_SECONDS if stage_timeout is None else stage_timeout
    submit_timeout = SUBMIT_TIMEOUT_SECONDS if submit_timeout is None else submit_timeout
    composer_timeout = COMPOSER_WAIT_SECONDS if composer_timeout is None else composer_timeout
    if harness not in VERIFIED_HARNESSES:
        raise PromptNotStaged(f"no composer model for harness {harness!r}; nothing was typed")
    if not text.strip():
        raise PromptNotStaged("prompt text is empty; nothing was typed")
    if "\x1b" in text or "\0" in text:
        raise PromptNotStaged(
            "prompt text contains NUL or terminal escape characters; nothing was typed"
        )
    composer_deadline = monotonic() + composer_timeout
    while True:
        try:
            screen = terminal.read_screen(pane_id)
        except HerdrUnavailable as exc:
            raise PromptNotStaged(
                f"pane {pane_id}: composer read failed before typing: {exc}"
            ) from exc
        before = composer_view(harness, screen)
        if before is not None:
            break
        if monotonic() >= composer_deadline:
            raise PromptNotStaged(
                f"pane {pane_id} does not show a recognisable {harness} composer "
                f"after {composer_timeout:g}s (a dialog or menu may be open); "
                "nothing was typed"
            )
        sleep(POLL_SECONDS)
    draft = before.composer_solid.strip()
    if draft:
        preview = " ".join(draft.split())[:120]
        raise PromptNotStaged(
            f"pane {pane_id} composer already holds unsubmitted text {preview!r}; "
            "refusing to append to it; nothing was typed"
        )
    placeholders_before = len(_PASTE_PLACEHOLDER.findall(before.composer))
    terminal.send_text(pane_id, f"{_BRACKETED_PASTE_START}{text}{_BRACKETED_PASTE_END}")

    started = monotonic()
    stage_deadline = started + stage_timeout
    while True:
        view = composer_view(harness, terminal.read_screen(pane_id))
        if view is not None and _staged(view, text, placeholders_before):
            break
        if monotonic() >= stage_deadline:
            raise HerdrUnavailable(
                f"pane {pane_id}: pasted prompt was not observed in the {harness} composer "
                f"within {stage_timeout:g}s; no submission key was sent"
            )
        sleep(POLL_SECONDS)

    key = _submit_key(harness, view)
    terminal.send_keys(pane_id, key)
    presses = 1
    wait = FIRST_RETRY_SECONDS
    next_press = monotonic() + wait
    submit_deadline = monotonic() + submit_timeout
    left_composer = False
    while True:
        sleep(POLL_SECONDS)
        now = monotonic()
        view = composer_view(harness, terminal.read_screen(pane_id))
        if view is not None and _staged(view, text, placeholders_before):
            left_composer = False
            if now >= submit_deadline:
                raise PromptStagedNotSubmitted(
                    f"pane {pane_id}: prompt is still staged in the {harness} composer after "
                    f"{presses} {key} key press(es) over {now - started:.1f}s; it was NOT "
                    "submitted and is left visible so it is never typed twice"
                )
            if now >= next_press:
                key = _submit_key(harness, view)
                terminal.send_keys(pane_id, key)
                presses += 1
                wait = min(wait * 2, MAX_RETRY_SECONDS)
                next_press = now + wait
            continue
        if view is not None:
            left_composer = True
            evidence = _corroborated(before, view, text)
            if evidence is not None:
                return SubmissionReceipt(key, presses, round(now - started, 3), evidence)
        if now >= submit_deadline:
            state = (
                "left the composer without visible submission evidence"
                if left_composer else "is no longer in a recognisable composer"
            )
            raise HerdrUnavailable(
                f"pane {pane_id}: prompt {state} after {presses} {key} key press(es); "
                "outcome is unknown"
            )

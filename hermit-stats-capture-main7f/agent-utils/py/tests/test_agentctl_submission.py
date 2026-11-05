"""Verified prompt submission against a fake terminal agent and a fake Herdr process."""

from __future__ import annotations

import json
import os
import subprocess
from collections.abc import Sequence
from pathlib import Path

import pytest

from agentctl.agent import Target, drain, enqueue
from agentctl.client import AgentPaneInfo, HerdrClient
from agentctl.errors import HerdrUnavailable
from agentctl.submission import (
    PromptNotStaged,
    PromptStagedNotSubmitted,
    _corroborated,
    _is_labelled_rule,
    _staged,
    codex_idle_composer,
    composer_view,
    submit_verified,
)

_PASTE_START = "\x1b[200~"
_PASTE_END = "\x1b[201~"
_RULE = "─" * 60


def _dim(text: str) -> str:
    return f"\x1b[2m{text}\x1b[0m"


class FakeAgent:
    """A terminal agent composer with a controllable key-drop and redraw lag."""

    def __init__(self, harness: str, *, busy: bool = False) -> None:
        self.harness = harness
        self.busy = busy
        self.composer = ""
        self.transcript: list[str] = ["• earlier output"]
        self.queued: list[str] = []
        self.submitted: list[str] = []
        self.steered: list[str] = []
        self.pastes: list[str] = []
        self.keys: list[str] = []
        self.drop_keys = 0
        self.paste_visible_after_reads = 0
        self.redraw_lag_reads = 0
        self.stale_screen: str | None = None
        self.dialog = False
        # Label Claude draws into the composer's top border, such as a mode name.
        self.top_border_label: str | None = None
        # Glyph Codex draws before its composer: › before v0.159.1, » from it.
        self.codex_marker = "›"
        self._pending_reads = 0
        self._paste_counter = 0
        self._placeholder: str | None = None

    # Terminal operations.
    def read_screen(self, pane_id: str) -> str:
        del pane_id
        if self.stale_screen is not None and self._pending_reads > 0:
            self._pending_reads -= 1
            return self.stale_screen
        self.stale_screen = None
        if self.paste_visible_after_reads > 0:
            self.paste_visible_after_reads -= 1
            return self._render(hide_composer=True)
        return self._render()

    def send_text(self, pane_id: str, text: str) -> None:
        del pane_id
        assert text.startswith(_PASTE_START) and text.endswith(_PASTE_END)
        body = text[len(_PASTE_START):-len(_PASTE_END)]
        self.pastes.append(body)
        self.composer += body
        if self.harness == "claude" and body.count("\n") >= 3:
            self._paste_counter += 1
            self._placeholder = f"[Pasted text #{self._paste_counter} +{body.count(chr(10))} lines]"

    def send_keys(self, pane_id: str, keys: str) -> None:
        del pane_id
        self.keys.append(keys)
        if self.drop_keys > 0:
            self.drop_keys -= 1
            return
        before = self._render()
        text = self.composer
        if not text:
            return
        if self.harness == "claude" and keys == "Enter":
            self.transcript.append(f"❯ {text}")
            (self.queued if self.busy else self.submitted).append(text)
            if not self.busy:
                self.transcript.append("✻ Working… (running UserPromptSubmit hooks…)")
            self.busy = True
        elif self.harness == "codex" and keys == "Enter":
            self.transcript.append(f"› {text}")
            (self.steered if self.busy else self.submitted).append(text)
            if not self.busy:
                self.transcript.append("• Working (0s • esc to interrupt)")
            self.busy = True
        elif self.harness == "codex" and keys == "Tab" and self.busy:
            self.queued.append(text)
            self.transcript += ["• Queued follow-up inputs", f"  ↳ {text}"]
        else:
            return
        self.composer = ""
        self._placeholder = None
        if self.redraw_lag_reads:
            self.stale_screen = before
            self._pending_reads = self.redraw_lag_reads

    # Rendering.
    def _render(self, *, hide_composer: bool = False) -> str:
        composer = "" if hide_composer else self.composer
        lines = list(self.transcript)
        if self.dialog:
            return "\n".join(lines + ["Replace goal?", "  › 1. Replace", "  2. Cancel"])
        if self.harness == "claude":
            if self.busy and not any("Working" in line for line in lines):
                lines.append("✻ Working… (running PreToolUse hooks…)")
            if self.top_border_label is None:
                lines.append(_RULE)
            else:
                # Coloured as Claude draws it: grey rule, violet label, grey tail.
                lines.append(
                    f"\x1b[38;5;244m{_RULE}\x1b[0m \x1b[38;5;147m{self.top_border_label} "
                    "\x1b[38;5;244m─\x1b[0m"
                )
            if composer and self._placeholder and not hide_composer:
                lines.append(f"❯ {self._placeholder}")
            elif composer:
                first, *rest = composer.split("\n")
                lines.append(f"❯ {first}")
                lines += [f"  {line}" for line in rest]
            elif self.queued:
                lines.append("❯ \x1b[7mP\x1b[0m" + _dim("ress up to edit queued messages"))
            else:
                lines.append("❯\xa0")
            lines.append(_RULE)
            footer = "  ⏵⏵ auto mode on"
            if self.busy:
                footer += " · esc to interrupt"
            lines.append(footer)
        else:
            if composer:
                first, *rest = composer.split("\n")
                lines.append(f"{self.codex_marker} {first}")
                lines += [f"  {line}" for line in rest]
            else:
                lines.append(f"{self.codex_marker} " + _dim("Ask Codex to do anything"))
            lines.append("")
            if self.busy and composer:
                lines.append("  tab to queue message                 99% context")
            else:
                lines.append("  GPT default · /tmp/project")
        return "\n".join(lines) + "\n"


class Clock:
    def __init__(self) -> None:
        self.now = 1000.0

    def sleep(self, seconds: float) -> None:
        self.now += seconds

    def monotonic(self) -> float:
        return self.now


def _submit(agent: FakeAgent, text: str, **kwargs: float) -> object:
    clock = Clock()
    return submit_verified(
        agent, "w1:p1", agent.harness, text,
        sleep=clock.sleep, monotonic=clock.monotonic, **kwargs,
    )


def test_screen_parser_separates_placeholder_from_draft() -> None:
    agent = FakeAgent("claude")
    view = composer_view("claude", agent._render())
    assert view is not None and view.composer_solid.strip() == ""
    agent.queued.append("x")
    view = composer_view("claude", agent._render())
    assert view is not None and view.composer_solid.strip() == ""
    assert "Press up" not in view.composer_solid and "ress up" in view.composer
    agent.composer = "draft"
    view = composer_view("claude", agent._render())
    assert view is not None and view.composer_solid.strip() == "draft"
    codex = FakeAgent("codex")
    view = composer_view("codex", codex._render())
    assert view is not None and view.composer_solid.strip() == ""
    assert "Ask Codex" in view.composer


@pytest.mark.parametrize("harness", ["claude", "codex"])
def test_dropped_submit_key_is_retried_without_retyping(harness: str) -> None:
    agent = FakeAgent(harness)
    agent.drop_keys = 1
    receipt = _submit(agent, "please run the report")
    assert agent.pastes == ["please run the report"]
    assert agent.keys == ["Enter", "Enter"]
    assert agent.submitted == ["please run the report"]
    assert getattr(receipt, "key_presses") == 2


@pytest.mark.parametrize("harness", ["claude", "codex"])
def test_submission_already_accepted_is_never_repeated(harness: str) -> None:
    # The agent accepts the first key but redraws late; no second key may be sent
    # merely because the next screen reads still show the staged text.
    agent = FakeAgent(harness)
    agent.redraw_lag_reads = 3
    receipt = _submit(agent, "exactly once")
    assert agent.pastes == ["exactly once"]
    assert agent.keys == ["Enter"]
    assert agent.submitted == ["exactly once"]
    assert agent.queued == [] and agent.steered == []
    assert getattr(receipt, "key_presses") == 1


@pytest.mark.parametrize("harness", ["claude", "codex"])
def test_late_redraw_retry_lands_on_an_empty_composer(harness: str) -> None:
    # A redraw slower than the first retry interval draws a second key, which
    # reaches an empty composer and therefore cannot submit anything twice.
    agent = FakeAgent(harness)
    agent.redraw_lag_reads = 8
    _submit(agent, "exactly once")
    assert agent.pastes == ["exactly once"]
    assert agent.keys == ["Enter", "Enter"]
    assert agent.submitted == ["exactly once"]
    assert agent.queued == [] and agent.steered == []


def test_busy_claude_queues_with_enter() -> None:
    agent = FakeAgent("claude", busy=True)
    receipt = _submit(agent, "follow up while busy")
    assert agent.keys == ["Enter"]
    assert agent.queued == ["follow up while busy"]
    assert "above the composer" in getattr(receipt, "evidence")


def test_busy_codex_queues_with_tab_and_never_steers() -> None:
    agent = FakeAgent("codex", busy=True)
    agent.drop_keys = 1
    receipt = _submit(agent, "queue this behind the turn")
    assert agent.keys == ["Tab", "Tab"]
    assert agent.queued == ["queue this behind the turn"]
    assert agent.steered == [] and agent.submitted == []
    assert getattr(receipt, "key") == "Tab"


def test_idle_codex_submits_with_enter() -> None:
    agent = FakeAgent("codex")
    _submit(agent, "start now")
    assert agent.keys == ["Enter"]
    assert agent.submitted == ["start now"]


def test_submit_key_that_never_lands_times_out_as_staged() -> None:
    agent = FakeAgent("claude")
    agent.drop_keys = 1_000
    with pytest.raises(PromptStagedNotSubmitted, match="NOT submitted") as caught:
        _submit(agent, "stuck prompt", submit_timeout=20.0)
    assert agent.pastes == ["stuck prompt"]
    assert agent.composer == "stuck prompt"
    assert agent.submitted == []
    # Backoff 0.5, 1, 2, 4, 4, ... bounds the key count within the deadline.
    assert 5 <= len(agent.keys) <= 8
    assert f"{len(agent.keys)} Enter key press(es)" in str(caught.value)


def test_existing_draft_is_refused_before_typing() -> None:
    agent = FakeAgent("claude")
    agent.composer = "a human's half-written note"
    with pytest.raises(PromptNotStaged, match="refusing to append"):
        _submit(agent, "new prompt")
    assert agent.pastes == [] and agent.keys == []


def test_unrecognised_screen_is_refused_before_typing() -> None:
    agent = FakeAgent("codex")
    agent.dialog = True
    with pytest.raises(PromptNotStaged, match="recognisable codex composer"):
        _submit(agent, "new prompt")
    assert agent.pastes == [] and agent.keys == []


class LateComposerAgent(FakeAgent):
    """A freshly launched harness whose composer is drawn only after a few reads."""

    def __init__(self, harness: str, *, hidden_reads: int) -> None:
        super().__init__(harness)
        self.hidden_reads = hidden_reads
        self.dialog = True

    def read_screen(self, pane_id: str) -> str:
        if self.hidden_reads > 0:
            self.hidden_reads -= 1
            if self.hidden_reads == 0:
                self.dialog = False
        return super().read_screen(pane_id)


def test_late_composer_is_awaited_before_typing() -> None:
    agent = LateComposerAgent("claude", hidden_reads=3)
    receipt = _submit(agent, "the brief")
    assert receipt is not None
    assert agent.pastes == ["the brief"] and agent.submitted == ["the brief"]


def test_composer_wait_is_bounded_and_types_nothing() -> None:
    agent = FakeAgent("claude")
    agent.dialog = True
    clock = Clock()
    with pytest.raises(PromptNotStaged, match=r"after 2s .*nothing was typed"):
        submit_verified(
            agent, "w1:p1", "claude", "the brief", composer_timeout=2.0,
            sleep=clock.sleep, monotonic=clock.monotonic,
        )
    assert agent.pastes == [] and agent.keys == []
    assert clock.now - 1000.0 >= 2.0


def test_labelled_claude_top_border_still_frames_the_composer() -> None:
    agent = FakeAgent("claude")
    agent.top_border_label = "ultracode"
    view = composer_view("claude", agent._render())
    assert view is not None, "a label in the top border must not hide the composer"
    assert view.composer_solid.strip() == ""
    assert "ultracode" not in view.transcript
    assert "auto mode on" in view.footer

    receipt = _submit(agent, "reply to the owner")
    assert getattr(receipt, "key_presses") == 1
    assert agent.submitted == ["reply to the owner"]

    busy = FakeAgent("claude", busy=True)
    busy.top_border_label = "ultracode"
    _submit(busy, "after this turn")
    assert busy.queued == ["after this turn"]

    drafted = FakeAgent("claude")
    drafted.top_border_label = "ultracode"
    drafted.composer = "someone else's words"
    with pytest.raises(PromptNotStaged, match="refusing to append"):
        _submit(drafted, "mine")
    assert drafted.pastes == []


def test_labelled_rules_only_frame_a_composer_that_plain_rules_do_not() -> None:
    rule = "─" * 40
    # Plain rules win: a draft row shaped like a labelled rule stays part of the draft.
    view = composer_view("claude", f"• earlier\n{rule}\n❯ first line\n  ── notes ──\n{rule}\n  footer\n")
    assert view is not None and "── notes ──" in view.composer_solid
    assert view.transcript == "• earlier"
    # A labelled rule with no composer marker below it frames nothing.
    assert composer_view("claude", f"{rule} label ─\n  Replace goal?\n{rule}\n") is None
    # Edge runs of rule characters are required on both sides of a label.
    for row in ("label ───", "─── label", "─ x", "──"):
        assert not _is_labelled_rule(row), row
    for row in ("─── label ─", "── label ─────", "───"):
        assert _is_labelled_rule(row), row


def test_escape_characters_are_refused_before_typing() -> None:
    agent = FakeAgent("claude")
    with pytest.raises(PromptNotStaged, match="escape"):
        _submit(agent, "bad \x1b[201~ text")
    assert agent.pastes == []


def test_paste_that_never_appears_sends_no_submit_key() -> None:
    agent = FakeAgent("claude")
    agent.paste_visible_after_reads = 1_000
    with pytest.raises(HerdrUnavailable, match="no submission key was sent") as caught:
        _submit(agent, "invisible")
    assert not isinstance(caught.value, (PromptNotStaged, PromptStagedNotSubmitted))
    assert agent.keys == []


def test_slow_paste_is_waited_for_before_the_key() -> None:
    agent = FakeAgent("claude")
    agent.paste_visible_after_reads = 5
    _submit(agent, "slow terminal")
    assert agent.keys == ["Enter"]
    assert agent.submitted == ["slow terminal"]


def test_long_paste_placeholder_counts_as_staged() -> None:
    agent = FakeAgent("claude")
    text = "\n".join(f"line {index}" for index in range(12))
    agent.drop_keys = 1
    _submit(agent, text)
    assert agent.keys == ["Enter", "Enter"]
    assert agent.submitted == [text]



# Rows Codex v0.159.1 drew at the bottom of its pane, read with
# `herdr pane read --source visible --format ansi`. Escape sequences, attributes and row
# layout are as captured; runs of padding are shortened, and the model name, working
# directory, links and prompt text were replaced with neutral words.
_V159_PROMPT = "Reply with exactly READY and nothing else."
_V159_SHADED_BLANK = "\x1b[0m\x1b[48;2;30;30;30m                    \x1b[0m"
_V159_EMPTY_COMPOSER = (
    "\x1b[0m\x1b[1m\x1b[38;2;190;142;250m\x1b[48;2;30;30;30m»\x1b[0m"
    "\x1b[48;2;30;30;30m \x1b[0m\x1b[2m\x1b[48;2;30;30;30mAsk Codex to do anything"
    "\x1b[0m\x1b[48;2;30;30;30m          \x1b[0m"
)
_V159_STAGED_COMPOSER = (
    "\x1b[0m\x1b[1m\x1b[38;2;190;142;250m\x1b[48;2;30;30;30m»\x1b[0m"
    "\x1b[48;2;30;30;30m Reply with exactly READY and nothing else.          \x1b[0m"
)
_V159_FOOTER = (
    "  \x1b[0m\x1b[38;2;246;226;183mmodel high\x1b[0m"
    "\x1b[38;2;132;132;132m · \x1b[0m\x1b[38;2;171;223;167m~/project\x1b[0m"
    "          \x1b[0m\x1b[38;2;132;132;132m⚠ \x1b[0m"
    "\x1b[38;2;196;167;103m1 warning\x1b[0m\x1b[38;2;132;132;132m · \x1b[0m"
    "\x1b[1m\x1b[38;2;220;220;220mf2\x1b[0m\x1b[38;2;132;132;132m to view\x1b[0m"
)
_V159_SESSION_FOOTER = (
    "  \x1b[0m\x1b[38;5;6msession\x1b[0m\x1b[2m · \x1b[0m"
    "\x1b[38;2;246;226;183mmodel high\x1b[0m\x1b[38;2;132;132;132m · \x1b[0m"
    "\x1b[38;2;171;223;167m~/project\x1b[0m\x1b[38;2;132;132;132m · \x1b[0m"
    "\x1b[38;2;242;205;205mReply READY\x1b[0m          \x1b[0m"
)
_V159_NOTICE = "\x1b[0m\x1b[2m• \x1b[0mTip: /status shows the session."
# The submitted prompt as the transcript shows it: Codex still draws › there.
_V159_SUBMITTED_PROMPT = (
    "\x1b[0m\x1b[1m\x1b[2m\x1b[48;2;40;40;40m› \x1b[0m"
    "\x1b[48;2;40;40;40mReply with exactly READY and nothing else.          \x1b[0m"
)
_V159_HOOK = "\x1b[0m\x1b[2m↳ Hook · \x1b[0mDiscovered 3 skills"
_V159_WORKING = (
    "\x1b[0m\x1b[1m\x1b[38;2;151;151;151m•\x1b[0m \x1b[0m"
    "\x1b[38;2;167;167;167mW\x1b[0m\x1b[38;2;118;118;118mo\x1b[0m"
    "\x1b[38;2;110;110;110mrking\x1b[0m \x1b[0m\x1b[2m(0s • \x1b[0m"
    "\x1b[1m\x1b[38;2;220;220;220mesc\x1b[0m\x1b[2m to interrupt)\x1b[0m"
)
_V159_REPLY = "\x1b[0m\x1b[2m• \x1b[0mREADY"
_V159_WORKED = "\x1b[0m\x1b[2m  Worked for 52s • 12:56 PM\x1b[0m"
_V159_COMPOSER_ROWS = [_V159_SHADED_BLANK, _V159_EMPTY_COMPOSER, _V159_SHADED_BLANK]


def _screen(*rows: str) -> str:
    return "\n".join(rows) + "\n"


# A freshly started session, before any prompt.
_V159_FRESH_IDLE = _screen(_V159_NOTICE, "", " ", *_V159_COMPOSER_ROWS, _V159_FOOTER)
# The same session with the prompt pasted and not yet submitted.
_V159_STAGED = _screen(
    _V159_NOTICE, "", " ", _V159_SHADED_BLANK, _V159_STAGED_COMPOSER, _V159_SHADED_BLANK,
    _V159_FOOTER,
)
# The first redraw after Enter: the prompt moved into the transcript.
_V159_SUBMITTED = _screen(
    _V159_NOTICE, "", _V159_SUBMITTED_PROMPT, "", " ", *_V159_COMPOSER_ROWS, _V159_FOOTER,
)
# The turn is running; the composer stays drawn below the progress row.
_V159_WORKING_SCREEN = _screen(
    _V159_SUBMITTED_PROMPT, "", "", _V159_HOOK, " ", _V159_WORKING, " ",
    *_V159_COMPOSER_ROWS, _V159_FOOTER,
)
# The working screen with its composer rows removed.
_V159_WORKING_WITHOUT_COMPOSER = _screen(
    _V159_SUBMITTED_PROMPT, "", "", _V159_HOOK, " ", _V159_WORKING, " ", _V159_FOOTER,
)
# Idle again after a finished turn, with the session footer.
_V159_SESSION_IDLE = _screen(
    _V159_SUBMITTED_PROMPT, "", _V159_REPLY, "", _V159_WORKED, "", " ",
    *_V159_COMPOSER_ROWS, _V159_SESSION_FOOTER,
)
# The folder-trust question Codex asks before its first composer in a new directory.
_V159_TRUST_PROMPT = _screen(
    _V159_SHADED_BLANK,
    "\x1b[0m\x1b[1m\x1b[48;2;30;30;30m  Folder access\x1b[0m\x1b[48;2;30;30;30m          \x1b[0m",
    "\x1b[0m\x1b[48;2;30;30;30m  \x1b[0m\x1b[2m\x1b[48;2;30;30;30m/tmp/project          "
    "\x1b[0m\x1b[48;2;30;30;30m  \x1b[0m",
    _V159_SHADED_BLANK,
    "\x1b[0m\x1b[48;2;30;30;30m  Trust this folder? Codex can read, edit, and run files here.   \x1b[0m",
    "\x1b[0m\x1b[48;2;30;30;30m  Continue only if you trust these files.          \x1b[0m",
    _V159_SHADED_BLANK,
    "\x1b[0m\x1b[1m\x1b[38;2;0;0;46m\x1b[48;2;99;168;248m› 1. Trust and continue          \x1b[0m",
    "\x1b[0m\x1b[48;2;30;30;30m  2. Quit          \x1b[0m",
    _V159_SHADED_BLANK,
    "\x1b[0m\x1b[48;2;30;30;30m  \x1b[0m\x1b[1m\x1b[38;2;220;220;220m"
    "\x1b[48;2;30;30;30menter\x1b[0m\x1b[2m\x1b[48;2;30;30;30m continue · \x1b[0m"
    "\x1b[1m\x1b[38;2;220;220;220m\x1b[48;2;30;30;30mesc\x1b[0m\x1b[2m"
    "\x1b[48;2;30;30;30m quit\x1b[0m\x1b[48;2;30;30;30m          \x1b[0m",
)


class Replay:
    """Plays captured screens back: before the paste, while staged, and after Enter.

    ``undrawn_reads`` holds the staged screen back for that many reads after the paste,
    as when a busy agent draws a paste late.
    """

    def __init__(self, before: str, staged: str, after: str) -> None:
        self.frames = (before, staged, after)
        self.phase = 0
        self.undrawn_reads = 0
        self.pastes: list[str] = []
        self.keys: list[str] = []

    def read_screen(self, pane_id: str) -> str:
        del pane_id
        if self.phase == 1 and self.undrawn_reads > 0:
            self.undrawn_reads -= 1
            return self.frames[0]
        return self.frames[self.phase]

    def send_text(self, pane_id: str, text: str) -> None:
        del pane_id
        self.pastes.append(text)
        self.phase = 1

    def send_keys(self, pane_id: str, keys: str) -> None:
        del pane_id
        self.keys.append(keys)
        if keys == "Enter":
            self.phase = 2


def _replay_submit(replay: Replay, text: str, harness: str = "codex") -> object:
    clock = Clock()
    return submit_verified(
        replay, "w1:p1", harness, text, sleep=clock.sleep, monotonic=clock.monotonic,
    )


@pytest.mark.parametrize(
    "screen", [_V159_FRESH_IDLE, _V159_SESSION_IDLE], ids=["fresh-start", "after-a-turn"],
)
def test_codex_0_159_1_idle_composer_is_recognised(screen: str) -> None:
    view = composer_view("codex", screen)
    assert view is not None, screen
    assert view.composer_solid.strip() == ""
    assert "Ask Codex to do anything" in view.composer
    assert "~/project" in view.footer
    assert "Ask Codex" not in view.transcript


@pytest.mark.parametrize("screen", [
    _V159_FRESH_IDLE, _V159_SESSION_IDLE, FakeAgent("codex")._render(),
    "›\n  ? for shortcuts\n", "»\n  99% context left\n",
])
def test_suggestion13_codex_unknown_readiness_recognizes_supported_idle_screens(screen: str) -> None:
    assert codex_idle_composer(screen)


@pytest.mark.parametrize("screen", [
    _V159_WORKING_SCREEN, _V159_WORKING_WITHOUT_COMPOSER, _V159_TRUST_PROMPT,
    _V159_STAGED,
    "› a human draft\n  GPT default · /work\n",
    "› \x1b[2m[Pasted text #1 +2 lines]\x1b[0m\n  GPT default · /work\n",
    "›\n  choose one\n",
    "• Working (0s • esc to interrupt)\n›\n  GPT default · /work\n",
    "Queued follow-up inputs\n›\n  GPT default · /work\n",
    "›\n  tab to queue message · /work\n",
    "Approval required\n›\n  GPT default · /work\n",
    "Trust this folder?\n›\n  GPT default · /work\n",
    "›\n  GPT default · /work · paste again to expand\n",
    "›\n  GPT default · /work · esc to cancel\n",
])
def test_suggestion13_codex_unknown_readiness_refuses_busy_drafts_and_dialogs(screen: str) -> None:
    assert not codex_idle_composer(screen)


def test_suggestion13_codex_unknown_readiness_ignores_old_status_words_above_active_rows() -> None:
    screen = "Approval required; esc to interrupt\n" + "old output\n" * 16 + _V159_FRESH_IDLE
    assert codex_idle_composer(screen)


@pytest.mark.parametrize("activity", [
    "Processing inputs (7s)", "• Pondering request (1m 2s • Ctrl+c to interrupt)",
    "◦ Checking context (1h 2m 3s)", "\x1b[2mCompiling reply (7s)\x1b[0m   ",
])
def test_suggestion13_codex_unknown_refuses_hidden_or_remapped_active_timers(activity: str) -> None:
    assert not codex_idle_composer(activity + "\n›\n  GPT default · /work\n")


@pytest.mark.parametrize("queue", [
    "Queued follow-up inputs", "Messages to be submitted after next tool call",
    "Messages to be submitted at end of turn",
])
def test_suggestion13_codex_unknown_refuses_active_timer_above_long_queue(queue: str) -> None:
    screen = "Processing inputs (7s)\n• " + queue + "\n" + "  queued text\n" * 20
    assert not codex_idle_composer(screen + "›\n  GPT default · /work\n")


@pytest.mark.parametrize("activity", [
    "Processing inputs (7s)", "• Pondering request (1m 2s • Ctrl+c to interrupt)",
    "• Working (2s • esc to interrupt)",
])
def test_suggestion13_codex_unknown_ignores_completed_timer_before_later_response(activity: str) -> None:
    assert codex_idle_composer(activity + "\n■ Completed response\n›\n  GPT default · /work\n")


@pytest.mark.parametrize("cue", [
    "enter to submit answer", "enter to submit all", "allow command?",
    "Messages to be submitted after next tool call", "Messages to be submitted at end of turn",
    "Ctrl+c to interrupt", "Do you trust the contents of this directory?",
])
def test_suggestion13_codex_unknown_refuses_manifest_dialog_and_queue_cues(cue: str) -> None:
    assert not codex_idle_composer(cue + "\n›\n  GPT default · /work\n")


@pytest.mark.parametrize("menu", [
    "All Results   Filesystem Only   Plugins", "all RESULTS\nfilesystem ONLY\nPLUGINS",
])
def test_suggestion13_codex_unknown_refuses_manifest_grouped_active_menu(menu: str) -> None:
    assert not codex_idle_composer(menu + "\n»\n  ? for shortcuts\n")


@pytest.mark.parametrize("labels", [
    "All Results", "Filesystem Only", "Plugins", "All Results   Filesystem Only",
    "All Results   Plugins", "Filesystem Only   Plugins",
])
def test_suggestion13_codex_unknown_does_not_treat_partial_menu_labels_as_active_menu(labels: str) -> None:
    assert codex_idle_composer(labels + "\n»\n  ? for shortcuts\n")


def test_suggestion13_codex_unknown_ignores_grouped_menu_outside_active_window() -> None:
    screen = "All Results   Filesystem Only   Plugins\n" + "historical output\n" * 16
    assert codex_idle_composer(screen + "»\n  ? for shortcuts\n")


def test_codex_0_159_1_prompt_is_staged_submitted_and_verified() -> None:
    replay = Replay(_V159_FRESH_IDLE, _V159_STAGED, _V159_SUBMITTED)
    receipt = _replay_submit(replay, _V159_PROMPT)
    assert getattr(receipt, "key") == "Enter"
    assert getattr(receipt, "key_presses") == 1
    assert getattr(receipt, "evidence") == "prompt text appeared above the composer"
    assert replay.pastes == [f"{_PASTE_START}{_V159_PROMPT}{_PASTE_END}"]
    assert replay.keys == ["Enter"]


def test_codex_0_159_1_working_screen_keeps_its_progress_row_out_of_the_draft() -> None:
    view = composer_view("codex", _V159_WORKING_SCREEN)
    assert view is not None
    assert view.composer_solid.strip() == ""
    assert "esc to interrupt" in view.transcript
    assert "tab to queue message" not in view.footer


@pytest.mark.parametrize(
    "screen", [_V159_WORKING_WITHOUT_COMPOSER, _V159_TRUST_PROMPT],
    ids=["working-without-composer", "folder-trust-prompt"],
)
def test_codex_0_159_1_screens_without_a_composer_are_refused_before_typing(screen: str) -> None:
    replay = Replay(screen, screen, screen)
    with pytest.raises(PromptNotStaged, match="nothing was typed"):
        _replay_submit(replay, "hello")
    assert replay.pastes == [] and replay.keys == []


def test_codex_0_159_1_working_screen_without_composer_has_no_view() -> None:
    assert composer_view("codex", _V159_WORKING_WITHOUT_COMPOSER) is None


@pytest.mark.parametrize("marker", ["›", "»"], ids=["before-0.159.1", "0.159.1"])
def test_codex_flows_hold_under_both_composer_markers(marker: str) -> None:
    idle = FakeAgent("codex")
    idle.codex_marker = marker
    view = composer_view("codex", idle._render())
    assert view is not None and view.composer_solid.strip() == ""
    assert getattr(_submit(idle, "start now"), "key") == "Enter"
    assert idle.submitted == ["start now"]

    busy = FakeAgent("codex", busy=True)
    busy.codex_marker = marker
    busy.drop_keys = 1
    assert getattr(_submit(busy, "follow up later"), "key") == "Tab"
    assert busy.queued == ["follow up later"] and busy.steered == []

    drafted = FakeAgent("codex")
    drafted.codex_marker = marker
    drafted.composer = "someone else's words"
    with pytest.raises(PromptNotStaged, match="refusing to append"):
        _submit(drafted, "mine")
    assert drafted.pastes == []

    dialog = FakeAgent("codex")
    dialog.codex_marker = marker
    dialog.dialog = True
    with pytest.raises(PromptNotStaged, match="recognisable codex composer"):
        _submit(dialog, "hello")
    assert dialog.pastes == []


# Rows Claude Code drew at the bottom of its pane while a turn was running, read with
# `herdr pane read --source visible --format ansi`: before a paste, with an 11-line paste
# collapsed into a placeholder, and after one Enter queued that paste. The composer, rule
# and footer rows are as captured, escape sequences included, except that runs of padding
# and rule characters are shortened and the placeholder carries the paste number of the
# session the bridge failed on. The rows above them are simplified. Among the differences:
# their words are neutral, a 10-row file preview and a hint row are left out, the later
# frames reuse the first frame's transcript rows and the staged frame its spinner row, and
# the queued prompt rows lack their trailing reset.
_CLAUDE_BUSY_PROMPT = "\n".join(
    ["Reply with one line for each item below."] + [f"item {n}" for n in range(2, 12)]
)
_CLAUDE_BUSY_TURN = (
    "\x1b[0m\x1b[38;5;114m● \x1b[0m\x1b[1mRead\x1b[0m(notes.md)",
    "\x1b[0m\x1b[38;5;246m  ⎿ \xa0\x1b[0mRead \x1b[0m\x1b[1m45\x1b[0m lines\x1b[0m",
    "",
    "\x1b[0m\x1b[38;5;246m  \x1b[0mRunning the test suite\x1b[0m\x1b[38;5;246m · 3s\x1b[0m",
    "\x1b[0m\x1b[38;5;246m  ⎿  $ cargo test\x1b[0m",
)
_CLAUDE_BUSY_SPINNER = (
    "\x1b[0m\x1b[38;5;174m·\x1b[0m \x1b[0m\x1b[38;5;180mFermenting…\x1b[0m"
    "\x1b[38;5;174m \x1b[0m\x1b[38;5;246m(16m 8s · ↓\x1b[0m \x1b[0m"
    "\x1b[38;5;246m12.3k tokens)\x1b[0m"
)
# The spinner just after Enter, while Claude Code runs its prompt hooks.
_CLAUDE_BUSY_SPINNER_HOOKS = (
    "\x1b[0m\x1b[38;5;174m✢\x1b[0m \x1b[0m\x1b[38;5;216mFermenting…\x1b[0m"
    "\x1b[38;5;174m \x1b[0m\x1b[38;5;246m(running UserPromptSubmit hooks… 8/10 · "
    "16m 9s · ↓ 12.3k tokens)\x1b[0m"
)
_CLAUDE_BUSY_PADDING = " " * 20
_CLAUDE_BUSY_RULE = "\x1b[0m\x1b[38;5;244m" + "─" * 40 + "\x1b[0m"
_CLAUDE_BUSY_WORKING_FOOTER = (
    "  \x1b[0m\x1b[38;5;220m⏵⏵ auto mode on\x1b[0m"
    "\x1b[38;5;246m (shift+tab to cycle) · esc to interrupt · ← 2 agents\x1b[0m"
)
# While a placeholder is in the composer, this replaces the running-turn footer.
_CLAUDE_BUSY_PASTE_FOOTER = "  \x1b[0m\x1b[38;5;246mpaste again to expand\x1b[0m"
# The running turn with an empty composer, before the paste.
_CLAUDE_BUSY_BEFORE = _screen(
    *_CLAUDE_BUSY_TURN, "", _CLAUDE_BUSY_SPINNER, _CLAUDE_BUSY_PADDING, _CLAUDE_BUSY_RULE,
    "\x1b[0m\x1b[38;5;246m❯\xa0\x1b[0m\x1b[7m \x1b[0m",
    _CLAUDE_BUSY_RULE, _CLAUDE_BUSY_WORKING_FOOTER,
)
# The same turn with the prompt pasted: only its placeholder is drawn.
_CLAUDE_BUSY_STAGED = _screen(
    *_CLAUDE_BUSY_TURN, "", _CLAUDE_BUSY_SPINNER, _CLAUDE_BUSY_PADDING, _CLAUDE_BUSY_RULE,
    "\x1b[0m\x1b[38;5;246m❯\xa0\x1b[0m[Pasted text #923 +10 lines]\x1b[0m\x1b[7m \x1b[0m",
    _CLAUDE_BUSY_RULE, _CLAUDE_BUSY_PASTE_FOOTER,
)
# After one Enter: the prompt is queued above the spinner, in full.
_CLAUDE_BUSY_QUEUED = _screen(
    *_CLAUDE_BUSY_TURN, "     ",
    *(
        ("\x1b[0m\x1b[38;5;239m\x1b[48;5;237m❯ " if index == 0 else "\x1b[0m\x1b[48;5;237m  ")
        + f"\x1b[0m\x1b[38;5;246m\x1b[48;5;237m{line}\x1b[0m\x1b[48;5;237m{_CLAUDE_BUSY_PADDING}"
        for index, line in enumerate(_CLAUDE_BUSY_PROMPT.splitlines())
    ),
    "", _CLAUDE_BUSY_SPINNER_HOOKS, _CLAUDE_BUSY_PADDING, _CLAUDE_BUSY_RULE,
    "\x1b[0m\x1b[38;5;246m❯\xa0\x1b[0m\x1b[7mP\x1b[0m\x1b[2mress up to edit queued messages\x1b[0m",
    _CLAUDE_BUSY_RULE, _CLAUDE_BUSY_PASTE_FOOTER,
)


def test_busy_claude_screens_show_the_paste_only_as_a_placeholder() -> None:
    before = composer_view("claude", _CLAUDE_BUSY_BEFORE)
    assert before is not None
    assert before.composer_solid.strip() == ""
    assert "[Pasted" not in before.composer
    assert "esc to interrupt" in before.footer

    pasted = composer_view("claude", _CLAUDE_BUSY_STAGED)
    assert pasted is not None
    assert pasted.composer_solid.strip() == "[Pasted text #923 +10 lines]"
    assert _staged(pasted, _CLAUDE_BUSY_PROMPT, 0)
    # The footer stops saying that a turn is running while the placeholder is drawn.
    assert "esc to interrupt" not in pasted.footer
    assert "paste again to expand" in pasted.footer

    queued = composer_view("claude", _CLAUDE_BUSY_QUEUED)
    assert queued is not None
    assert queued.composer_solid.strip() == ""
    assert not _staged(queued, _CLAUDE_BUSY_PROMPT, 0)
    assert _corroborated(before, queued, _CLAUDE_BUSY_PROMPT) == (
        "prompt text appeared above the composer"
    )


def test_placeholder_left_in_a_busy_claude_composer_holds_the_next_prompt() -> None:
    replay = Replay(_CLAUDE_BUSY_STAGED, _CLAUDE_BUSY_STAGED, _CLAUDE_BUSY_STAGED)
    with pytest.raises(PromptNotStaged, match="refusing to append"):
        _replay_submit(replay, "the next prompt", harness="claude")
    assert replay.pastes == [] and replay.keys == []


# Verified submission once saw no paste in a busy Claude Code composer within 3 s of typing
# it; the paste was there later, unsubmitted, and held later prompts:
# https://github.com/rrnewton/agent-utils/issues/215
def test_busy_claude_paste_drawn_late_is_submitted_with_one_enter() -> None:
    replay = Replay(_CLAUDE_BUSY_BEFORE, _CLAUDE_BUSY_STAGED, _CLAUDE_BUSY_QUEUED)
    replay.undrawn_reads = 45
    receipt = _replay_submit(replay, _CLAUDE_BUSY_PROMPT, harness="claude")
    assert getattr(receipt, "key") == "Enter"
    assert getattr(receipt, "key_presses") == 1
    assert getattr(receipt, "evidence") == "prompt text appeared above the composer"
    # Reads 0.1 s apart missed the placeholder for 4.5 s; the evidence came one poll after
    # the Enter.
    assert getattr(receipt, "elapsed_seconds") == 4.6
    assert replay.pastes == [f"{_PASTE_START}{_CLAUDE_BUSY_PROMPT}{_PASTE_END}"]
    assert replay.keys == ["Enter"]


def test_busy_claude_paste_never_drawn_gives_up_at_the_stage_timeout_without_a_key() -> None:
    replay = Replay(_CLAUDE_BUSY_BEFORE, _CLAUDE_BUSY_STAGED, _CLAUDE_BUSY_QUEUED)
    replay.undrawn_reads = 1_000_000
    clock = Clock()
    with pytest.raises(HerdrUnavailable, match="within 60s; no submission key was sent") as caught:
        submit_verified(
            replay, "w1:p1", "claude", _CLAUDE_BUSY_PROMPT,
            sleep=clock.sleep, monotonic=clock.monotonic,
        )
    assert not isinstance(caught.value, (PromptNotStaged, PromptStagedNotSubmitted))
    # The clock adds 0.1 s per poll in floating point, so the read that reaches the deadline
    # can land one poll after it.
    assert 60.0 <= clock.now - 1000.0 < 60.2
    assert len(replay.pastes) == 1 and replay.keys == []


def _completed(
    command: Sequence[str], returncode: int = 0, stdout: str = "", stderr: str = "",
) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(list(command), returncode, stdout, stderr)


class FakeHerdrProcess:
    """Answer the Herdr subcommands that a prompt delivery issues."""

    def __init__(self, agent: FakeAgent) -> None:
        self.agent = agent
        self.calls: list[tuple[str, ...]] = []

    def __call__(self, command: Sequence[str]) -> subprocess.CompletedProcess[str]:
        args = tuple(command[1:])
        self.calls.append(args)
        pane = args[2] if len(args) > 2 else ""
        if args[:2] == ("agent", "prompt"):
            # Native prompt: paste, then an unverified Enter a moment later.
            self.agent.send_text(pane, f"{_PASTE_START}{args[3]}{_PASTE_END}")
            self.agent.send_keys(pane, "Enter")
            return _completed(command, stdout='{"result":{"type":"agent_prompted"}}')
        if args[:2] == ("pane", "send-text"):
            self.agent.send_text(pane, args[3])
            return _completed(command)
        if args[:2] == ("pane", "send-keys"):
            self.agent.send_keys(pane, args[3])
            return _completed(command)
        if args[:2] == ("pane", "read"):
            assert "--format" in args and "ansi" in args
            return _completed(command, stdout=self.agent.read_screen(pane))
        if args[:2] == ("agent", "wait"):
            if self.agent.busy:
                event = {"result": {"agent": {"pane_id": pane, "agent_status": "working"}}}
                return _completed(command, stdout=json.dumps(event))
            return _completed(command, returncode=1, stderr="timed out waiting for working")
        raise AssertionError(f"unexpected Herdr invocation: {args!r}")


class ProcessBackedClient(HerdrClient):
    def __init__(self, agent: FakeAgent) -> None:
        self.process = FakeHerdrProcess(agent)
        super().__init__(herdr_bin="fixture-herdr", run=self.process)
        self.agent = agent

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        return AgentPaneInfo(pane_id, "w1", "/work/project", self.agent.harness,
                             "idle", None, None)


@pytest.mark.parametrize("harness", ["claude", "codex"])
def test_queue_delivers_through_a_dropped_submit_key(tmp_path: Path, harness: str) -> None:
    agent = FakeAgent(harness)
    agent.drop_keys = 1
    client = ProcessBackedClient(agent)
    root = tmp_path / "queue"
    target = Target(pane_id="w1:p1")
    enqueue(str(root), "deliver me once", message_id="m1")
    result = drain(client, target, str(root), working_timeout=1.0)
    assert result.delivered == ("m1",), result
    assert result.quarantined == ()
    assert agent.submitted == ["deliver me once"]
    assert agent.pastes == ["deliver me once"]
    assert not any(call[:2] == ("agent", "prompt") for call in client.process.calls)


def test_queue_keeps_prompt_pending_when_nothing_was_typed(tmp_path: Path) -> None:
    agent = FakeAgent("claude")
    agent.composer = "someone else's draft"
    client = ProcessBackedClient(agent)
    root = tmp_path / "queue"
    enqueue(str(root), "wait your turn", message_id="m1")
    result = drain(client, Target(pane_id="w1:p1"), str(root), working_timeout=1.0)
    assert result.outcome == "pending"
    assert result.pending == ("m1",)
    assert result.delivered == () and result.quarantined == ()
    assert "refusing to append" in (result.blocked or "")
    assert agent.pastes == [] and agent.keys == []
    document = json.loads((root / "inbox" / "m1.json").read_text())
    assert document["delivery_state"] == "pending"
    assert "possibly_submitted" not in document
    assert not os.listdir(root / "inflight")

    agent.composer = ""
    result = drain(client, Target(pane_id="w1:p1"), str(root), working_timeout=1.0)
    assert result.delivered == ("m1",)
    assert agent.submitted == ["wait your turn"]


def test_queue_quarantines_a_prompt_left_staged(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import agentctl.submission as submission

    monkeypatch.setattr(submission, "SUBMIT_TIMEOUT_SECONDS", 0.3)
    agent = FakeAgent("claude")
    agent.drop_keys = 1_000
    client = ProcessBackedClient(agent)
    root = tmp_path / "queue"
    enqueue(str(root), "never lands", message_id="m1")
    result = drain(client, Target(pane_id="w1:p1"), str(root), working_timeout=1.0)
    assert result.quarantined == ("m1",)
    assert result.outcome == "possibly_submitted"
    failed = json.loads((root / "failed" / "m1.json").read_text())
    assert "still staged" in failed["delivery_error"]
    assert "NOT submitted" in failed["delivery_error"]
    assert agent.pastes == ["never lands"]


def test_slash_commands_keep_the_native_prompt_path() -> None:
    agent = FakeAgent("codex")
    client = ProcessBackedClient(agent)
    assert client.prompt_agent("w1:p1", "/goal ship it") is None
    assert client.process.calls[0][:2] == ("agent", "prompt")

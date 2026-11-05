#!/usr/bin/env python3
"""Drive the REAL voice agent, headless, and prove it actually called this server.

WHY THIS EXISTS
===============

scripts/verify-deployment.sh checks that this server is reachable and correct. On 2026-08-19 the
owner's session failed while every one of those checks passed. The access log shows the whole
handshake succeeding --

    mcp initialize 200 . notifications/initialized 202 . tools/list 200

-- and then ZERO tool calls. The agent said "the tools are showing as out of date" and never
invoked anything. Nothing we ran asserted that a tool is actually CALLED, so nothing went red.

This script closes exactly that gap. It holds a real conversation with the deployed agent and
fails unless a tool invocation appears in THIS SERVER'S OWN access log during the run.

IT COSTS VENDOR MINUTES. It is opt-in, manual, and is not part of any suite. Run it when you want
to see what the owner sees.

HOW IT DRIVES THE CONVERSATION WITHOUT A MICROPHONE
===================================================

ElevenLabs documents a client-to-server event that makes this fully headless:

    {"type": "user_message", "text": "..."}

"triggers the same response flow as spoken user input"
-- https://elevenlabs.io/docs/eleven-agents/customization/events/client-to-server-events

So the test types instead of speaking. Everything else on the wire is byte-identical to what
web/voice.js does in the owner's browser: mint a signed URL from OUR /api/v1/signed-url, open the
WebSocket, send a bare `conversation_initiation_client_data`, answer `ping` with `pong`. The
initiation payload is deliberately BARE rather than carrying a text-only override, because an
override only takes effect if it has been enabled in the agent's security settings, and a
conversation begun differently from the owner's is not the conversation this is meant to observe.
Audio frames are simply ignored.

THE TWO ASSERTIONS
==================

1. THE MECHANISM (primary). A tool line -- recent_activity, digest_channel, read_message or
   find_message -- must appear in this server's access log during the run. This is the direct
   check for the failure above, and it does not depend on interpreting a word of the agent's
   prose. It is also
   unforgeable from our side: access::tool_call fires ONLY on the MCP path (src/mcp/protocol.rs),
   so nothing this script does over the REST API can plant one.

2. THE OUTCOME (corroboration). A fluent reply about nothing is the failure mode of a confabulating
   agent -- this one has invented a whole channel digest before. So rather than asking for a
   summary, which can only be judged by eye, this asks the agent to RELAY THE MOST RECENT MESSAGE
   AND ITS TIMESTAMP, which is comparable by string. We fetch that message from our own API first
   and assert the reply carries it back.

   THE CHEAP ROUND IS PROOF, not merely confidence, and the argument is structural rather than
   probabilistic. Nothing lets the agent produce today's latest message without invoking a tool:
   the test opens a FRESH conversation, so there is no prior history to draw on; the channel's
   recent content POSTDATES any training data; and every message in it is from the owner, from an
   agent of his, or from the bot, none of which is in the model's context at turn one. So a green
   run means something on its own -- no second conversation, no writing to the channel.

   THE TOKEN ROUND IS AN AUTOMATIC ESCALATION ON FAILURE, not a mode anyone has to choose. It
   exists to diagnose a false NEGATIVE, which is the error that IS possible here: when the cheap
   round fails, either the agent is not really reading, or it read fine and this script's
   substring matching was too strict. Those have OPPOSITE fixes. So a failing cheap round posts a
   `gtverify-<epoch>-<random>` token through OUR OWN write API -- never through the agent -- and
   asks again, requiring it back verbatim. The two outcomes are reported as different things:

     cheap fails, token PASSES  ->  THE TEST is wrong (exit 18). Fix check_relay(), not the agent.
     cheap fails, token FAILS   ->  THE AGENT is not reading (exit 14), now proven.

   A failing run therefore costs roughly twice the vendor minutes of a passing one. That is the
   price of not sending the next session after the wrong system. --nonce forces the token round
   directly, for when you want that evidence on a run that would otherwise pass cheaply.

   AND SOME RUNS ARE REFUSED RATHER THAN DECIDED. If the channel is empty, or its newest message
   has nothing distinctive to match on, the grounding assertion would pass or fail purely on how
   it happens to be written. That is a green that means nothing, or a red that means nothing. Such
   a run stops BEFORE any conversation is opened, with exit 20 -- neither a pass nor a fail --
   and nothing is billed.

   THAT ESCALATION HAS ALREADY FIRED ONCE, on 2026-08-19, and it was right: the agent was reading
   and this script was wrong. It is a VOICE agent, so it SPEAKS the timestamp -- "thirteen
   fifty-one Eastern Time on August nineteenth, two thousand twenty-six" -- and a digit-only
   matcher rejects that. The matcher now accepts spoken numerals and a time rendered in any real
   zone; what it did NOT do is stop requiring a timestamp, because that is the half that forces
   the reply to commit to WHICH message it is relaying. See RelayVerdict.grounded for the argument
   and check_spoken_timestamp_controls() for the controls that keep the leniency honest.

   Each assertion ships with its controls, run every time (--self-test runs them alone, offline):
   the log scanner must find a real captured tool line and must NOT fire on a handshake-only log,
   the relay check must reject a fluent confabulated reply AND a different time spoken in the same
   style, and a FAILED run must return the exit code documented below rather than 0.

WHAT IT WILL NOT DO
===================

It never asks the agent to post. Summarising and relaying are read-only. If the agent's tool calls
arrive with the WRITE credential -- which the access log records -- post_reply was reachable, and
that is reported as a finding.

USAGE
=====

    scripts/smoke-agent.py --url URL --channel SNOWFLAKE [--nonce] [--container NAME] ...

Tokens come from $VIBE_TALK_READ_TOKEN / $VIBE_TALK_WRITE_TOKEN. They are never accepted on the
command line, because a command line is visible to every process on the box, and never printed.

    scripts/smoke-agent.py --self-test     offline; runs every control and costs nothing.

Normally you get here through the launcher, which already knows the URL, channel and container:

    scripts/run.sh --smoke-agent

EXIT CODES -- a failure says WHICH failure
==========================================

     0  passed
     2  usage or configuration error
    10  mint_failed          could not mint a signed URL from our own /api/v1/signed-url
    11  socket_refused       the conversation WebSocket would not open
    12  no_reply             connected, but no agent reply arrived within the timeout
    13  no_tool_call         the agent replied and called NO tool  <-- 2026-08-19's failure
    14  ungrounded           a tool was called, but the reply does not carry the message back --
                             and the token round did not either, so this is proven
    15  credential_leak      a credential appeared in this script's own output
    16  baseline_failed      could not read the channel ourselves, so grounding is undecidable
    17  control_failed       an assertion's own control failed: the check cannot be trusted
    18  relay_check_too_strict  the agent IS reading -- the token came back -- but this script's
                             matching rejected the cheap round. The defect is HERE, not there.
    20  channel_unusable     REFUSED, neither pass nor fail: the channel is empty or has nothing
                             distinctive to match on, so no run could have concluded anything.
                             No conversation was opened and nothing was billed.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import io
import json
import os
import random
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Iterable, Sequence, TypeAlias, cast
from unittest.mock import patch

JsonValue: TypeAlias = (
    str | int | float | bool | None | list["JsonValue"] | dict[str, "JsonValue"]
)

# --- exit codes ---------------------------------------------------------------------------------

EXIT_OK = 0
EXIT_USAGE = 2
EXIT_MINT_FAILED = 10
EXIT_SOCKET_REFUSED = 11
EXIT_NO_REPLY = 12
EXIT_NO_TOOL_CALL = 13
EXIT_UNGROUNDED = 14
EXIT_CREDENTIAL_LEAK = 15
EXIT_BASELINE_FAILED = 16
EXIT_CONTROL_FAILED = 17
EXIT_RELAY_TOO_STRICT = 18
#: Neither a pass nor a fail. The channel cannot decide the question, so the run does not pretend
#: to have answered it.
EXIT_CHANNEL_UNUSABLE = 20
#: `--replay-check`: the vendor did not act on the replayed transcript. A DISTINCT result, not a
#: generic failure -- "the payload is not honoured" is the single most useful thing this check can
#: report, and folding it into "smoke failed" would lose it.
EXIT_REPLAY_NOT_HONOURED = 21
#: `--replay-check`: the CONTROL conversation answered too, so the check proved fluency rather than
#: memory. That invalidates the positive result rather than joining it.
EXIT_REPLAY_CONTROL_LEAKED = 22

#: The read tools whose appearance in the access log proves the agent really reached this server.
#: post_reply is deliberately NOT here: this test never asks for a post, so a post_reply line would
#: be a finding, not a pass.
READ_TOOLS = ("recent_activity", "digest_channel", "read_message", "find_message")


class SmokeFailure(Exception):
    """A failure with a specific taxonomy code, so the report can say WHICH failure."""

    def __init__(self, code: int, label: str, detail: str) -> None:
        super().__init__(detail)
        self.code = code
        self.label = label
        self.detail = detail


# --- output, and the credential scan over it ----------------------------------------------------


class Transcript:
    """Everything this script prints, kept so it can be scanned for credentials at the end.

    The scan is the same one scripts/test-run-sh.sh runs over run.sh's output, and it carries the
    same positive control: a "no credential found" result is worthless unless the scanner has been
    shown it can find one in the very text it is scanning.
    """

    def __init__(self) -> None:
        self.lines: list[str] = []

    def emit(self, text: str = "") -> None:
        self.lines.append(text)
        print(text, flush=True)

    def text(self) -> str:
        return "\n".join(self.lines)


def secrets_found(haystack: str, secrets: Iterable[tuple[str, str]]) -> list[str]:
    """Names of the secrets whose VALUE appears in `haystack`.

    `secrets` is (name, value). A blank or absurdly short value is skipped rather than searched
    for: an empty needle matches everywhere and would make every run look like a leak.
    """
    found = []
    for name, value in secrets:
        if value and len(value) >= 8 and value in haystack:
            found.append(name)
    return found


# --- the access log: did a tool actually get called? --------------------------------------------

ANSI = re.compile(r"\x1b\[[0-9;]*m")

#: One access-log tool line, after ANSI stripping. Real captured example:
#:
#:   2026-08-19T12:56:01.953439Z  INFO vibe_talk::access: tool tool="digest_channel"
#:   channel="1538907155300745327" credential="read" outcome="ok" reason="-" text_len=-1
#:
#: Anchored on the target and the "tool" message so a `request` or `mcp` line cannot match. The
#: fields after `tool=` are matched individually rather than positionally, so a re-ordering or an
#: added field degrades to "found the tool, not the channel" rather than to silence.
TOOL_LINE = re.compile(r'vibe_talk::access:\s+tool\s+tool="(?P<tool>[^"]*)"(?P<rest>.*)$')
FIELD = re.compile(r'(?P<key>[a-z_]+)="(?P<value>[^"]*)"')


def strip_ansi(text: str) -> str:
    """Remove SGR colour codes.

    Load-bearing, not cosmetic: the container logs in colour, so every field name in a raw line is
    wrapped in escape sequences and an anchored grep over it silently matches nothing. A check that
    can only report "no tool was called" is not a check.
    """
    return ANSI.sub("", text)


@dataclass(frozen=True)
class ToolLine:
    """One observed tool invocation."""

    tool: str
    channel: str
    credential: str
    outcome: str
    reason: str


def tool_lines_in(log_text: str) -> list[ToolLine]:
    """Every tool invocation recorded in a slice of access log."""
    out = []
    for raw in strip_ansi(log_text).splitlines():
        match = TOOL_LINE.search(raw)
        if not match:
            continue
        fields = {m.group("key"): m.group("value") for m in FIELD.finditer(match.group("rest"))}
        out.append(
            ToolLine(
                tool=match.group("tool"),
                channel=fields.get("channel", ""),
                credential=fields.get("credential", ""),
                outcome=fields.get("outcome", ""),
                reason=fields.get("reason", ""),
            )
        )
    return out


#: A REAL line, captured from the live container on 2026-08-19 by calling digest_channel over
#: /mcp with the read token. It is the positive control for `tool_lines_in`: if the log format
#: ever drifts -- a renamed field, a different tracing formatter, colour handling changing -- this
#: fixture stops matching and the run fails loudly, instead of the scanner going quietly vacuous
#: and reporting "no tool was called" forever after.
REAL_TOOL_LINE = (
    '2026-08-19T12:56:01.953439Z  INFO vibe_talk::access: tool tool="digest_channel" '
    'channel="1538907155300745327" credential="read" outcome="ok" reason="-" text_len=-1'
)

#: The 2026-08-19 failure itself, as the log recorded it: a complete, successful handshake with no
#: tool call anywhere in it. This is the NEGATIVE control -- the scanner must stay silent here, or
#: it is matching something incidental and would have called that morning a pass.
HANDSHAKE_ONLY_LOG = (
    '2026-08-19T12:46:03.194060Z  INFO vibe_talk::access: mcp rpc_method="initialize" '
    'credential="write" is_notification=false\n'
    '2026-08-19T12:46:03.194108Z  INFO vibe_talk::access: request method="POST" path="/mcp" '
    'credential="write" status=200 millis=0\n'
    '2026-08-19T12:46:03.284917Z  INFO vibe_talk::access: mcp '
    'rpc_method="notifications/initialized" credential="write" is_notification=true\n'
    '2026-08-19T12:46:03.284946Z  INFO vibe_talk::access: request method="POST" path="/mcp" '
    'credential="write" status=202 millis=0\n'
    '2026-08-19T12:46:03.432390Z  INFO vibe_talk::access: mcp rpc_method="tools/list" '
    'credential="write" is_notification=false\n'
    '2026-08-19T12:46:03.432511Z  INFO vibe_talk::access: request method="POST" path="/mcp" '
    'credential="write" status=200 millis=0'
)


def check_log_scanner_controls() -> list[str]:
    """Run the log scanner's own controls. Returns a list of failures, empty when healthy."""
    problems = []

    positive = tool_lines_in(REAL_TOOL_LINE)
    if len(positive) != 1 or positive[0].tool != "digest_channel":
        problems.append(
            "positive control: the scanner did not recognise a REAL captured tool line. The "
            "access-log format has drifted away from this script, so 'no tool was called' would "
            f"now be reported for every run. Line: {REAL_TOOL_LINE}"
        )
    elif positive[0].credential != "read" or positive[0].outcome != "ok":
        problems.append(
            "positive control: the scanner matched the tool but not its fields "
            f"(credential={positive[0].credential!r} outcome={positive[0].outcome!r})."
        )

    # The same scanner, over a colour-coded copy of the same line: the container logs in colour,
    # so a scanner that only works on stripped text works only in tests.
    coloured = (
        "\x1b[2m2026-08-19T12:56:01.953439Z\x1b[0m \x1b[32m INFO\x1b[0m "
        "\x1b[2mvibe_talk::access\x1b[0m\x1b[2m:\x1b[0m tool \x1b[3mtool\x1b[0m\x1b[2m=\x1b[0m"
        '"digest_channel" \x1b[3mchannel\x1b[0m\x1b[2m=\x1b[0m"1538907155300745327" '
        '\x1b[3mcredential\x1b[0m\x1b[2m=\x1b[0m"read" \x1b[3moutcome\x1b[0m\x1b[2m=\x1b[0m"ok"'
    )
    if not tool_lines_in(coloured):
        problems.append(
            "positive control: the scanner missed a COLOUR-CODED tool line. The container logs in "
            "colour; this is the form it will actually meet."
        )

    if "post_reply" in READ_TOOLS:
        problems.append(
            "READ_TOOLS contains post_reply. This test never asks the agent to post, so a post "
            "must never be what satisfies the primary assertion: that would make a run pass on the "
            "strength of the one thing it is supposed to stay away from."
        )
    if [t for t in tool_lines_in(REAL_TOOL_LINE) if t.tool in READ_TOOLS] == []:
        problems.append(
            "positive control: a real digest_channel line was parsed but does not count as one of "
            f"the read tools. READ_TOOLS is {READ_TOOLS!r} and would never fire."
        )
    if [t for t in tool_lines_in(HANDSHAKE_ONLY_LOG) if t.tool in READ_TOOLS]:
        problems.append(
            "negative control: a handshake-only log matched one of READ_TOOLS. The primary "
            "assertion would pass on a run where nothing was invoked."
        )

    if tool_lines_in(HANDSHAKE_ONLY_LOG):
        problems.append(
            "negative control: the scanner reported a tool call in a handshake-only log. That log "
            "IS the 2026-08-19 failure, so the check would have passed the very run it exists to "
            "catch."
        )

    return problems


def mcp_connection_summary(diagnostics: Sequence[str]) -> str | None:
    """One line about what the VENDOR thinks of our MCP server, from its own status events.

    This exists because of what the 2026-08-19 failure actually looked like from both sides at
    once. The agent said the tools "appear to be out of date"; ElevenLabs, in the same
    conversation, reported the server connected with all five tools visible. Those two facts
    together move the investigation off the MCP server and onto the agent's own configuration --
    and neither one alone would have.

    The event's field shape is NOT documented by the vendor, so every access is guarded and an
    unrecognised payload yields None rather than a confident wrong summary. The raw payloads are
    printed alongside this regardless, so nothing is lost if the shape drifts.
    """
    for raw in diagnostics:
        try:
            payload = json.loads(raw)
        except json.JSONDecodeError:
            continue
        status = payload.get("mcp_connection_status")
        if not isinstance(status, dict):
            continue
        integrations = status.get("integrations")
        if not isinstance(integrations, list) or not integrations:
            continue
        parts = []
        for entry in integrations:
            if not isinstance(entry, dict):
                continue
            connected = entry.get("is_connected")
            count = entry.get("tool_count")
            if connected is None and count is None:
                continue
            parts.append(
                f"{entry.get('integration_id', 'an MCP server')}: "
                f"{'CONNECTED' if connected else 'NOT connected'}"
                + (f", {count} tool(s) visible" if count is not None else "")
            )
        if parts:
            return "; ".join(parts)
    return None


# --- the relay assertion: is the reply grounded in the channel? ----------------------------------

#: A fluent, confident, entirely invented reply. The NEGATIVE control for the relay check: an agent
#: that read nothing still produces prose like this, and a grep that passes on it is matching
#: something incidental rather than the message.
CONFABULATED_REPLY = (
    "Sure! Looking at the channel now. The most recent message is from earlier today -- someone "
    "was discussing the deployment and mentioned that things are looking good on their end. There "
    "were a few follow-ups about scheduling and one about the weekend. Let me know if you would "
    "like me to go further back or read any of them out in full!"
)

WORD = re.compile(r"[0-9A-Za-z][0-9A-Za-z'_-]*")
WHITESPACE = re.compile(r"\s+")


def normalize(text: str) -> str:
    """Lowercase with runs of whitespace collapsed, for substring comparison."""
    return WHITESPACE.sub(" ", text).strip().lower()


def despace(text: str) -> str:
    """Only the alphanumerics, lowercased.

    A nonce read back by a speech-shaped model can arrive with punctuation or spacing of its own
    ("gtverify - 1755..."). Matching on this is weaker than verbatim, so which one matched is
    reported rather than flattened away.
    """
    return re.sub(r"[^0-9a-z]", "", text.lower())


def parse_hm(iso: str) -> tuple[str, str, str, int, int] | None:
    """(year, month, day, hour, minute) from an ISO-8601 timestamp, or None if it is not one."""
    match = re.match(r"(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})", iso)
    if not match:
        return None
    year, month, day, hour24, minute = match.groups()
    return year, month, day, int(hour24), int(minute)


def timestamp_forms(iso: str) -> list[str]:
    """WRITTEN renderings of an ISO-8601 timestamp, in the zone we hold it in.

    Matching is to the MINUTE. A minute is already far beyond what a model can invent -- there are
    1440 of them in a day and the reply would have to land on the right one -- while insisting on
    the second would fail honestly-grounded replies that round, which is a false alarm, not rigour.

    This is only half the matcher; a VOICE agent usually speaks the time instead of writing it, and
    spoken_timestamp_hit() below handles that. This function is also what Latest.assertable asks
    "is there a timestamp here at all?", so it stays cheap and total.
    """
    parsed = parse_hm(iso)
    if not parsed:
        return []
    year, month, day, hour, minute = parsed
    hour12 = hour % 12 or 12
    forms = [
        f"{year}-{month}-{day}t{hour:02d}:{minute:02d}",
        f"{year}-{month}-{day} {hour:02d}:{minute:02d}",
        f"{hour:02d}:{minute:02d}",
        f"{hour12}:{minute:02d}",
    ]
    # Deduplicated, longest first, so the report names the most specific form that matched.
    return sorted(dict.fromkeys(forms), key=len, reverse=True)


# --- spoken, and possibly re-zoned, timestamps ---------------------------------------------------
#
# It is a VOICE agent, so it SPEAKS the time rather than writing it. Two replies it actually gave:
#
#   "thirteen ten and fifty-two seconds UTC on August nineteenth, twenty twenty-six"
#   "thirteen fifty-one Eastern Time on August nineteenth, two thousand twenty-six"
#
# A digit-only matcher rejects both. That is a false negative about THIS SCRIPT, not a finding
# about the agent -- it is the failure the token round diagnosed on 2026-08-19 (exit 18).
#
# Two separate things have to be accepted here, and they do NOT cost the same:
#
#   * SPOKEN NUMERALS -- free. "thirteen fifty-one" and "13:51" are the same claim in different
#     notation. Accepting the words gives up no discrimination whatsoever.
#   * A CONVERTED TIME ZONE -- not free, and worth stating plainly rather than glossing. Once any
#     real UTC offset is allowed, the HOUR carries essentially no information: some offset maps our
#     instant onto almost any hour of the day. What is left discriminating is the MINUTE -- one
#     chance in sixty, loosened to three-in-sixty by the offsets that are not whole hours (:30,
#     :45). The hour is still REQUIRED to be spoken adjacent to the minute, because that is what
#     makes it a time rather than a number that happens to appear in a sentence, but it is not
#     itself evidence.
#
# The consequence is recorded in RelayVerdict.grounded: the BODY quote is the evidence and the
# timestamp is corroboration. The conjunct is kept anyway; see that docstring for why.

_UNITS = (
    "zero one two three four five six seven eight nine ten eleven twelve thirteen fourteen "
    "fifteen sixteen seventeen eighteen nineteen"
).split()
_TENS = {20: "twenty", 30: "thirty", 40: "forty", 50: "fifty"}

#: Words that can CONTINUE a number to the left or right of a match. "twenty twenty-six" (the year)
#: must not be read as 20:20, and "two thousand twenty-six" must not be read as 20:06.
_UNIT_WORDS = frozenset(_UNITS[1:10])
_TENS_WORDS = frozenset(_TENS.values())
_SCALE_WORDS = frozenset({"hundred", "thousand"})

#: Every UTC offset in real-world use, in minutes. Written out rather than derived so that the
#: breadth of the leniency is visible to whoever reads this next.
_UTC_OFFSETS: tuple[int, ...] = tuple(
    sorted(
        {h * 60 for h in range(-12, 15)}
        | {-570, -210, 210, 270, 330, 390, 570, 630}  # the :30 zones
        | {345, 525, 765}  # the :45 zones (Nepal, Chatham, Chatham DST)
    )
)


def spoken_number(value: int) -> str:
    """0-59 as it is said: 7 -> 'seven', 51 -> 'fifty one'."""
    if value < 20:
        return _UNITS[value]
    tens, unit = divmod(value, 10)
    return _TENS[tens * 10] + (f" {_UNITS[unit]}" if unit else "")


def speech_tokens(text: str) -> list[str]:
    """The reply as spoken words: lowercased, and every non-alphanumeric run a separator.

    Transcription choices are the model's, and must not be evidence about what it read. Hyphens
    ("fifty-one") and apostrophes ("o\'clock") are both just separators here, so "fifty one",
    "fifty-one", "o clock" and "o'clock" all arrive as the same tokens; "oclock" written solid
    arrives as one, which is why _minute_words offers both spellings.

    It looked as though hyphens and apostrophes each needed their own normalising pass, and both
    were written. Mutation testing removed each in turn and NO control noticed, because the
    character class below had already done the work. They are gone; the two on-the-hour controls
    are what would notice if this stopped handling "o\'clock".
    """
    return re.findall(r"[a-z0-9]+", text.lower())


def _hour_words(hour: int) -> list[str]:
    """How an hour hand might be said.

    Only the 24-hour word, which looks too thin and is not. A 12-hour rendering ("one fifty-one")
    needs no form of its own, because the offsets below already span more than a day: whatever
    hour%12 is, some real zone reads this instant as exactly that hour, so the 12-hour form is
    matched by the zone loop instead. The military "oh eight hundred" needs none either -- it
    CONTAINS "eight hundred", and the leading "oh" is not a word that can extend a number, so the
    plain form matches inside it.

    Both were written out here first. Mutation testing removed each and no control noticed, which
    is what redundant code looks like from the outside; the reasons above are why no control
    COULD notice, so they were deleted rather than pinned.
    """
    return [spoken_number(hour)]


def _minute_words(minute: int) -> list[str]:
    """How the minutes might be said, including the on-the-hour idioms."""
    if minute == 0:
        return ["oclock", "o clock", "hundred", "hundred hours", "zero zero", "oh oh", "double oh"]
    if minute < 10:
        word = _UNITS[minute]
        return [f"oh {word}", f"o {word}", f"zero {word}", word]
    return [spoken_number(minute)]


def _is_standalone(tokens: Sequence[str], start: int, end: int) -> bool:
    """Is tokens[start:end] a time, or a fragment of a longer spoken number?

    This is the guard that stops "August nineteenth, twenty twenty-six" from reading as 20:20 and
    "two thousand twenty-six" from reading as 20:06. It rejects ONLY the continuations that could
    actually extend the number -- a tens word followed by a unit, or a scale word beside it -- so a
    genuine "thirteen fifty-one, two thousand twenty-six" still matches on the time.
    """
    before = tokens[start - 1] if start > 0 else ""
    after = tokens[end] if end < len(tokens) else ""
    if tokens[end - 1] in _TENS_WORDS and after in _UNIT_WORDS:
        return False
    if after in _SCALE_WORDS:
        return False
    if before in _SCALE_WORDS:
        return False
    if before in _TENS_WORDS and tokens[start] in _UNIT_WORDS:
        return False
    return True


def _offset_label(offset: int) -> str:
    if offset == 0:
        return "UTC"
    sign = "+" if offset > 0 else "-"
    hours, minutes = divmod(abs(offset), 60)
    return f"UTC{sign}{hours:02d}:{minutes:02d}"


def spoken_timestamp_hit(reply: str, iso: str) -> str | None:
    """The same instant, SPOKEN, in any real time zone -- or None.

    Returns a description of what matched, naming the offset, so the transcript says which
    rendering was accepted rather than just "yes".
    """
    parsed = parse_hm(iso)
    if not parsed:
        return None
    _, _, _, hour, minute = parsed
    tokens = speech_tokens(reply)
    if not tokens:
        return None
    utc_minutes = hour * 60 + minute
    # UTC first, so an unconverted reply is reported as the plain reading rather than as some
    # coincidentally-equal offset.
    for offset in sorted(_UTC_OFFSETS, key=lambda o: (o != 0, abs(o))):
        local = (utc_minutes + offset) % (24 * 60)
        local_hour, local_minute = divmod(local, 60)
        for hour_form in _hour_words(local_hour):
            hour_tokens = hour_form.split()
            for minute_form in _minute_words(local_minute):
                sequence = hour_tokens + minute_form.split()
                width = len(sequence)
                for start in range(len(tokens) - width + 1):
                    if tokens[start : start + width] != sequence:
                        continue
                    if not _is_standalone(tokens, start, start + width):
                        continue
                    spoken = " ".join(sequence)
                    return f"{local_hour:02d}:{local_minute:02d} spoken as {spoken!r} ({_offset_label(offset)})"
    return None


def written_timestamp_hit(reply: str, iso: str) -> str | None:
    """A written rendering, in the zone we hold it in or converted to another one."""
    haystack = normalize(reply)
    for form in timestamp_forms(iso):
        if form in haystack:
            return form
    parsed = parse_hm(iso)
    if not parsed:
        return None
    _, _, _, hour, minute = parsed
    utc_minutes = hour * 60 + minute
    for offset in _UTC_OFFSETS:
        if offset == 0:
            continue
        local = (utc_minutes + offset) % (24 * 60)
        local_hour, local_minute = divmod(local, 60)
        pattern = rf"(?<!\d){local_hour:02d}:{local_minute:02d}(?!\d)"
        if re.search(pattern, haystack):
            return f"{local_hour:02d}:{local_minute:02d} ({_offset_label(offset)})"
    return None


def body_evidence(content: str, minimum_length: int = 6) -> list[str]:
    """Substrings of a message body distinctive enough that quoting one implies having read it.

    Longest first: a rare long word is much harder to produce by chance than a common short one.
    A phrase of consecutive words is included as well, because a relayed message is usually quoted
    in runs rather than word by word.
    """
    words = WORD.findall(content)
    candidates: list[str] = []

    phrase_words = [w for w in words if w]
    if len(phrase_words) >= 4:
        candidates.append(" ".join(phrase_words[:6]))

    long_words = sorted({w for w in words if len(w) >= minimum_length}, key=len, reverse=True)
    candidates.extend(long_words[:8])

    seen: set[str] = set()
    out = []
    for candidate in candidates:
        key = normalize(candidate)
        if key and key not in seen:
            seen.add(key)
            out.append(candidate)
    return sorted(out, key=len, reverse=True)


@dataclass
class RelayVerdict:
    """What the reply carried back, and how."""

    timestamp_hit: str | None = None
    body_hit: str | None = None
    nonce_hit: str | None = None
    nonce_hit_kind: str | None = None  # "verbatim" or "despaced"

    @property
    def grounded(self) -> bool:
        """A nonce alone settles it; without one, both halves of the relay are required.

        WHY THE CONJUNCT SURVIVED the 2026-08-19 loosening, since the obvious move was to drop it.
        The body quote is now much the stronger half: it is a distinctive run of the message's own
        words, which cannot be guessed, while the timestamp -- once a converted time zone is
        accepted -- is worth about the minute alone. The tempting conclusion is "require the body,
        treat the timestamp as corroboration".

        It was not taken, for one reason: the body-only control. A reply that repeats some of the
        message's words while placing it vaguely ("some time ago") is what a partial or
        second-hand answer looks like, and this check is supposed to reject it. Making the body
        sufficient would pass it. The timestamp's job is not to be strong evidence on its own; it
        is to force the reply to commit to WHICH message it is relaying. So the conjunct stays and
        the LOOSENING IS CONFINED TO RENDERING -- how the timestamp is said, not whether one is
        required. Every negative control that stood before still stands.
        """
        if self.nonce_hit:
            return True
        return bool(self.timestamp_hit and self.body_hit)


def check_relay(reply: str, timestamp: str, content: str, nonce: str | None) -> RelayVerdict:
    """Decide whether `reply` really carries the channel message back."""
    haystack = normalize(reply)
    verdict = RelayVerdict()

    verdict.timestamp_hit = written_timestamp_hit(reply, timestamp) or spoken_timestamp_hit(
        reply, timestamp
    )

    for evidence in body_evidence(content):
        if normalize(evidence) in haystack:
            verdict.body_hit = evidence
            break

    if nonce:
        if normalize(nonce) in haystack:
            verdict.nonce_hit, verdict.nonce_hit_kind = nonce, "verbatim"
        elif despace(nonce) in despace(reply):
            verdict.nonce_hit, verdict.nonce_hit_kind = nonce, "despaced"

    return verdict


def check_relay_controls() -> list[str]:
    """Run the relay check's own controls. Returns a list of failures, empty when healthy."""
    problems = []
    timestamp = "2026-08-19T12:45:33.412000+00:00"
    content = "Rebuilt the container after the sparkplug refactor; digest looks right now."
    nonce = "gtverify-1755607000-a3f9c1"

    truthful = (
        "The most recent message, at 12:45 on 2026-08-19, says: Rebuilt the container after the "
        "sparkplug refactor; digest looks right now."
    )
    verdict = check_relay(truthful, timestamp, content, None)
    if not verdict.grounded:
        problems.append(
            "positive control: the relay check rejected a reply that DOES carry the message back "
            f"(timestamp_hit={verdict.timestamp_hit!r} body_hit={verdict.body_hit!r}). It would "
            "now fail every honest run."
        )

    timestamp_only = "The most recent message came in at 12:45; it was about the usual things."
    verdict = check_relay(timestamp_only, timestamp, content, None)
    if verdict.grounded:
        problems.append(
            "negative control: a reply carrying the TIMESTAMP but nothing of the message body was "
            "accepted as a relay. Half the evidence is a description, not the message."
        )
    body_only = "Someone said they rebuilt the container after the sparkplug refactor, some time ago."
    verdict = check_relay(body_only, timestamp, content, None)
    if verdict.grounded:
        problems.append(
            "negative control: a reply carrying part of the BODY but no timestamp was accepted as "
            "a relay."
        )

    # Case and spacing are the model's to choose, not evidence about whether it read anything. A
    # matcher that insists on the channel's exact rendering produces false negatives, which is the
    # error the token round exists to diagnose -- better not to manufacture them here.
    ragged = (
        "THE MOST RECENT MESSAGE  (2026-08-19T12:45)  READS:   REBUILT THE CONTAINER AFTER THE\n"
        "SPARKPLUG REFACTOR; DIGEST LOOKS RIGHT NOW."
    )
    verdict = check_relay(ragged, timestamp, content, None)
    if not verdict.grounded:
        problems.append(
            "positive control: a truthful relay was rejected over CAPITALISATION and line breaks "
            f"(timestamp_hit={verdict.timestamp_hit!r} body_hit={verdict.body_hit!r}). The matcher "
            "is comparing presentation rather than content."
        )

    verdict = check_relay(CONFABULATED_REPLY, timestamp, content, None)
    if verdict.grounded:
        problems.append(
            "negative control: the relay check PASSED a fluent, entirely invented reply "
            f"(timestamp_hit={verdict.timestamp_hit!r} body_hit={verdict.body_hit!r}). It is "
            "matching something incidental, not the message."
        )

    verdict = check_relay(CONFABULATED_REPLY + f" The token was {nonce}.", timestamp, content, nonce)
    if not (verdict.grounded and verdict.nonce_hit_kind == "verbatim"):
        problems.append("positive control: a verbatim nonce in the reply was not recognised.")

    spaced = f"The token reads {nonce[:9]} - {nonce[9:]}, and that is the latest message."
    verdict = check_relay(spaced, timestamp, content, nonce)
    if not (verdict.grounded and verdict.nonce_hit_kind == "despaced"):
        problems.append(
            "positive control: a nonce broken up by punctuation was not recognised by the despaced "
            f"fallback (kind={verdict.nonce_hit_kind!r}). A model that reads a token aloud would be "
            "called ungrounded for punctuating it."
        )

    verdict = check_relay(CONFABULATED_REPLY, timestamp, content, nonce)
    if verdict.grounded:
        problems.append(
            "negative control: a reply with no nonce in it was reported as carrying the nonce."
        )

    # A body with nothing distinctive in it must yield no evidence, so the run says "ask for
    # --nonce" rather than silently asserting nothing and passing.
    if body_evidence("ok"):
        problems.append("negative control: 'ok' was treated as distinctive channel content.")

    problems += check_spoken_timestamp_controls()
    return problems


#: WHAT THE AGENT ACTUALLY SAID on 2026-08-19, verbatim, in the run that exited 18. These are the
#: ground truth for what a correct answer looks like from a VOICE agent, and they are the reason
#: the timestamp matcher was loosened at all -- so they are fixtures here rather than a note in a
#: commit message, and a future tightening has to walk past them.
SPOKEN_UTC_REPLY = "thirteen ten and fifty-two seconds UTC on August nineteenth, twenty twenty-six"
SPOKEN_UTC_INSTANT = "2026-08-19T13:10:52.000000+00:00"

#: The same shape, but with a ZONE NAME on it. Read this one carefully, because the obvious
#: interpretation is not what the live run showed.
#:
#: It looks like a conversion: 13:51 Eastern (EDT, UTC-04:00) is 17:51 UTC, and an agent that
#: converted our stored UTC into the owner's local zone would be doing the right thing. The
#: verification run of 2026-08-19 says otherwise. The channel's latest message was stamped
#: 13:51:25 UTC and the agent said "thirteen fifty-one EASTERN TIME" -- the DIGITS were the UTC
#: ones and the ZONE LABEL was simply wrong. (The other reply above labelled its digits UTC, and
#: that one was right.)
#:
#: So the spoken zone name is NOT checked, and this is the evidence for that decision rather than
#: a guess: an agent that names the zone wrongly while reading the clock correctly would be failed
#: by a matcher that trusted the label, and that is a false negative about a reply which did carry
#: the message back. Conversion is still accepted -- a future agent may genuinely convert -- so
#: both readings of this sentence are asserted below, the converted one and the literal one.
SPOKEN_EASTERN_REPLY = (
    "thirteen fifty-one Eastern Time on August nineteenth, two thousand twenty-six"
)
SPOKEN_EASTERN_INSTANT = "2026-08-19T17:51:07.000000+00:00"


def check_spoken_timestamp_controls() -> list[str]:  # noqa: PLR0912 - a flat list of controls
    """Controls for the spoken-timestamp leniency added on 2026-08-19.

    The danger with this particular loosening is specific and worth naming: "accepts a spoken
    timestamp" degrades very easily into "accepts any sentence with number words in it", and the
    degraded version passes the confabulation control too, because that reply happens to contain no
    numbers. So the negative controls below are about the leniency itself -- a DIFFERENT time in
    the SAME spoken style, and number words that are not a time at all.
    """
    problems = []

    hit = spoken_timestamp_hit(SPOKEN_UTC_REPLY, SPOKEN_UTC_INSTANT)
    if not hit:
        problems.append(
            "positive control: the agent's REAL spoken reply of 2026-08-19 "
            f"({SPOKEN_UTC_REPLY!r}) was not recognised as 13:10 UTC. This is the exact false "
            "negative that made the run escalate and exit 18."
        )
    elif "(UTC)" not in hit:
        problems.append(
            f"positive control: an UNCONVERTED spoken time matched as {hit!r} rather than as the "
            "plain UTC reading. The report would send the next reader looking at the wrong zone."
        )

    hit = spoken_timestamp_hit(SPOKEN_EASTERN_REPLY, SPOKEN_EASTERN_INSTANT)
    if not hit:
        problems.append(
            "positive control: the agent's REAL spoken reply of 2026-08-19 "
            f"({SPOKEN_EASTERN_REPLY!r}) was not recognised as 17:51 UTC rendered in Eastern time. "
            "Converting the zone is correct behaviour, not evidence of not having read."
        )
    elif "UTC-04:00" not in hit:
        problems.append(
            f"positive control: the Eastern reply matched, but as {hit!r} rather than as the "
            "UTC-04:00 rendering it is. The report would name the wrong zone."
        )

    # The same sentence, read the way the live run of 2026-08-19 actually produced it: UTC digits
    # under a wrong zone label. Both readings have to pass, because we cannot tell them apart and
    # both carry the message back.
    literal = spoken_timestamp_hit(SPOKEN_EASTERN_REPLY, "2026-08-19T13:51:25.635000+00:00")
    if not literal:
        problems.append(
            "positive control: the agent's real reply was rejected against the timestamp it was "
            "actually given (13:51:25 UTC, spoken as 'thirteen fifty-one Eastern Time'). This is "
            "the live case, not a hypothetical one."
        )
    elif "(UTC)" not in literal:
        problems.append(
            f"positive control: the live case matched as {literal!r} rather than as the plain UTC "
            "reading it is. The spoken zone label must not steer the reported zone."
        )

    # -- the leniency's OWN negative controls ----------------------------------------------------
    wrong_time = "fourteen twenty-two Eastern Time on August nineteenth, twenty twenty-six"
    if spoken_timestamp_hit(wrong_time, SPOKEN_UTC_INSTANT):
        problems.append(
            "negative control: a DIFFERENT time spoken in the same style was accepted as the "
            f"message's timestamp ({wrong_time!r} against 13:10:52 UTC). The matcher is reacting "
            "to the spoken style rather than to the time."
        )
    if spoken_timestamp_hit(wrong_time, SPOKEN_EASTERN_INSTANT):
        problems.append(
            "negative control: a DIFFERENT time spoken in the same style was accepted against "
            "17:51 UTC. The matcher is reacting to the spoken style rather than to the time."
        )

    # The hour must be spoken ADJACENT to the minute. Otherwise the check collapses into "does any
    # number word in the reply happen to equal the minute", which a long reply satisfies by chance.
    loose_number = "There were fifty-one messages in the channel, and August nineteenth was busy."
    if spoken_timestamp_hit(loose_number, SPOKEN_EASTERN_INSTANT):
        problems.append(
            "negative control: a bare quantity ('fifty-one messages') was read as the MINUTE of "
            "the timestamp. Without an hour beside it that is a number, not a time."
        )

    # A spoken YEAR is two number words in a row and looks exactly like a spoken time. 20:20 UTC
    # against "twenty twenty-six" is the collision that actually bites.
    year_only = "The message was posted in twenty twenty-six, some time back."
    if spoken_timestamp_hit(year_only, "2026-08-19T20:20:00+00:00"):
        problems.append(
            "negative control: the spoken YEAR 'twenty twenty-six' was read as the time 20:20."
        )
    if spoken_timestamp_hit("posted in two thousand twenty-six", "2026-08-19T20:06:00+00:00"):
        problems.append(
            "negative control: the spoken year 'two thousand twenty-six' was read as 20:06."
        )

    # The same leniency exists for a WRITTEN time the agent converted ("13:51 Eastern"), and it
    # needs its own pair: a hit on the converted time, a miss on a different one.
    written = written_timestamp_hit("posted at 13:51 Eastern Time", SPOKEN_EASTERN_INSTANT)
    if not written:
        problems.append(
            "positive control: a WRITTEN time converted to another zone ('13:51 Eastern' for "
            "17:51 UTC) was not recognised."
        )
    elif "UTC-04:00" not in written:
        problems.append(
            f"positive control: the converted written time matched as {written!r} rather than as "
            "the UTC-04:00 rendering it is."
        )
    if written_timestamp_hit("posted at 13:22 Eastern Time", SPOKEN_EASTERN_INSTANT):
        problems.append(
            "negative control: a DIFFERENT written time was accepted as a zone conversion of "
            "17:51 UTC. Allowing any offset must not amount to allowing any time."
        )

    # On the hour is said with an idiom rather than a minute word, and the apostrophe in
    # "o'clock" is transcribed at least three ways. This control is what makes the
    # apostrophe-closing in speech_tokens() load-bearing rather than decorative.
    for said in ("nine o'clock", "nine oclock", "nine o clock", "twenty one hundred"):
        if not spoken_timestamp_hit(f"it came in at {said} last night", "2026-08-19T21:00:00+00:00"):
            problems.append(
                f"positive control: an on-the-hour time spoken as {said!r} was not recognised as "
                "21:00 UTC."
            )
    if spoken_timestamp_hit("it came in at nine o'clock last night", "2026-08-19T21:07:00+00:00"):
        problems.append(
            "negative control: 'nine o'clock' was accepted for 21:07. On-the-hour idioms must "
            "still mean minute zero."
        )

    # A 12-hour rendering ("one fifty-one in the afternoon") must be accepted. It is matched as
    # the UTC+08:00 reading rather than by a 12-hour form of its own; see _hour_words.
    if not spoken_timestamp_hit("at one fifty-one in the afternoon", SPOKEN_EASTERN_INSTANT):
        problems.append(
            "positive control: a time spoken on the 12-hour clock ('one fifty-one') was not "
            "recognised as a reading of 17:51 UTC."
        )
    # Capitalisation is the transcriber's choice, not evidence. The written matcher already has a
    # control for this; the spoken one needs its own, because it tokenises separately.
    if not spoken_timestamp_hit(SPOKEN_EASTERN_REPLY.upper(), SPOKEN_EASTERN_INSTANT):
        problems.append(
            "positive control: the agent's real reply was rejected once TRANSCRIBED IN CAPITALS. "
            "The spoken matcher is comparing presentation rather than content."
        )
    # When a reply gives BOTH readings, the plain UTC one is what the report should name -- the
    # next reader should not be sent looking at a zone the message was never stored in.
    both = "thirteen fifty-one Eastern Time, which is seventeen fifty-one UTC"
    hit = spoken_timestamp_hit(both, SPOKEN_EASTERN_INSTANT)
    if not hit or "(UTC)" not in hit:
        problems.append(
            f"positive control: a reply giving BOTH readings was reported as {hit!r} rather than "
            "as the plain UTC one."
        )
    # Minutes one to nine are spoken with a filler ("thirteen oh five"), or bare.
    for said in ("thirteen oh five", "thirteen zero five", "thirteen five", "one oh five"):
        if not spoken_timestamp_hit(f"at {said} today", "2026-08-19T13:05:00+00:00"):
            problems.append(
                f"positive control: a single-digit minute spoken as {said!r} was not recognised "
                "as 13:05."
            )
    if spoken_timestamp_hit("at thirteen oh five today", "2026-08-19T13:50:00+00:00"):
        problems.append("negative control: 'thirteen oh five' was accepted for 13:50.")

    # The two remaining halves of the longer-number guard. Each blocks a spoken number that is not
    # a time but reads as one once the tokens are laid flat.
    if spoken_timestamp_hit("the rebuild cost twenty six hundred seconds", "2026-08-19T20:06:00+00:00"):
        problems.append(
            "negative control: 'twenty six hundred' was read as the time 20:06. A number followed "
            "by a scale word is a quantity, not a clock reading."
        )
    if spoken_timestamp_hit("there were twenty six thirty-second clips", "2026-08-19T06:30:00+00:00"):
        problems.append(
            "negative control: 'twenty SIX THIRTY second' was read as the time 06:30. The hour "
            "must start the number, not sit in the middle of one."
        )

    # The written matcher's digit boundaries. Contrived on purpose -- the point is that the
    # shifted-numeric branch searches for a bare "hh:mm" and must not find it inside a longer run
    # of digits, which is the one way that branch could match something that is not a time.
    if written_timestamp_hit("in trace 913:517 the retry fired", SPOKEN_EASTERN_INSTANT):
        problems.append(
            "negative control: '13:51' found INSIDE a longer digit run was accepted as the "
            "message's timestamp."
        )

    # The confabulated reply must still fail against a timestamp whose spoken form is ordinary.
    if spoken_timestamp_hit(CONFABULATED_REPLY, SPOKEN_EASTERN_INSTANT):
        problems.append(
            "negative control: the fluent confabulated reply matched a SPOKEN timestamp. The "
            "spoken matcher is finding something incidental."
        )

    # And the whole relay verdict, end to end, on a real spoken reply that also quotes the body:
    # loosening the rendering must not have loosened what is REQUIRED.
    content = "Rebuilt the container after the sparkplug refactor; digest looks right now."
    spoken_relay = (
        "The most recent message came in at " + SPOKEN_EASTERN_REPLY + ", and it reads: Rebuilt "
        "the container after the sparkplug refactor; digest looks right now."
    )
    verdict = check_relay(spoken_relay, SPOKEN_EASTERN_INSTANT, content, None)
    if not verdict.grounded:
        problems.append(
            "positive control: a spoken, zone-converted relay that ALSO quotes the body was "
            f"rejected (timestamp_hit={verdict.timestamp_hit!r} body_hit={verdict.body_hit!r})."
        )
    spoken_timestamp_only = "The most recent message came in at " + SPOKEN_EASTERN_REPLY + "."
    verdict = check_relay(spoken_timestamp_only, SPOKEN_EASTERN_INSTANT, content, None)
    if verdict.grounded:
        problems.append(
            "negative control: a SPOKEN timestamp with none of the message body was accepted as a "
            "relay. The loosening removed the body requirement, not just the notation."
        )

    return problems


def check_credential_scanner_controls() -> list[str]:
    """Run the credential scanner's own controls.

    These live here, offline, rather than only inside a paid run: a scanner that has quietly
    stopped finding anything reports a clean output forever, and the only signal would have
    been a leak nobody was told about.
    """
    problems = []
    token = "SENTINEL-WRITE-TOKEN-2c98da44"
    if secrets_found(f"prefix {token} suffix", [("write", token)]) != ["write"]:
        problems.append(
            "positive control: the credential scanner did not find a token planted in the "
            "text it was given. Every 'no credential in the output' result it produces is "
            "meaningless."
        )
    if secrets_found("nothing to see here", [("write", token)]):
        problems.append("negative control: the scanner reported a token that is absent.")
    if secrets_found("anything at all", [("blank", "")]):
        problems.append(
            "negative control: a BLANK secret matched. An empty needle is in every haystack, "
            "so an unset variable would make every run look like a leak."
        )
    return problems


#: The real payload ElevenLabs sent during the failing run of 2026-08-19, captured verbatim.
#: It is the positive control for `mcp_connection_summary`: the event's shape is undocumented, so
#: a drift would otherwise turn the summary silently into None and take the sharpest diagnostic
#: this script has with it.
REAL_MCP_STATUS = (
    '{"mcp_connection_status": {"integrations": [{"integration_id": "QGRdu0v2Ww1tPg9H2QUZ", '
    '"integration_type": "mcp_server", "is_connected": true, "tool_count": 5}]}, '
    '"type": "mcp_connection_status"}'
)


def check_diagnostics_controls() -> list[str]:
    """Run the MCP-status reader's own controls."""
    problems = []
    summary = mcp_connection_summary([REAL_MCP_STATUS])
    if not summary or "CONNECTED" not in summary or "5 tool" not in summary:
        problems.append(
            "positive control: the MCP-status reader did not understand a REAL captured "
            f"mcp_connection_status payload (got {summary!r}). The vendor does not document this "
            "event's shape, so a drift here silently removes the diagnostic that says whether the "
            "connection is the problem."
        )
    if mcp_connection_summary(['{"type": "ping"}', 'not json at all']) is not None:
        problems.append(
            "negative control: the MCP-status reader invented a summary from events that carry "
            "no connection status."
        )
    return problems


# --- talking to our own server ------------------------------------------------------------------


def http_json(
    url: str,
    token: str,
    method: str = "GET",
    payload: dict[str, JsonValue] | None = None,
    timeout: int = 30,
) -> tuple[int, JsonValue]:
    """One request to our own API. Returns (status, parsed-body-or-raw-text)."""
    data = json.dumps(payload).encode() if payload is not None else None
    request = urllib.request.Request(url, data=data, method=method)
    request.add_header("authorization", f"Bearer {token}")
    request.add_header("accept", "application/json")
    if data is not None:
        request.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read().decode("utf-8", "replace")
            status = response.status
    except urllib.error.HTTPError as error:
        body = error.read().decode("utf-8", "replace")
        status = error.code
    except (urllib.error.URLError, OSError) as error:
        raise SmokeFailure(
            EXIT_BASELINE_FAILED,
            "unreachable",
            f"could not reach {url}: {error}. Is the container running, and is --url right?",
        ) from error
    try:
        return status, cast(JsonValue, json.loads(body))
    except json.JSONDecodeError:
        return status, body


# --- reading this server's log --------------------------------------------------------------------


def read_log(command: Sequence[str]) -> str:
    """The server's log so far, as text. stderr is included: tracing writes there."""
    try:
        finished = subprocess.run(command, capture_output=True, timeout=60, check=False)
    except (OSError, subprocess.SubprocessError) as error:
        raise SmokeFailure(
            EXIT_CONTROL_FAILED,
            "log_unreadable",
            f"could not read the server log with {' '.join(command)}: {error}.\n"
            "The tool-call assertion is THE primary check here, so an unreadable log is a failure "
            "rather than something to continue past. Pass --log-command if the server is not the "
            "container this expects, or --no-log-check to run with the primary assertion off.",
        ) from error
    output = finished.stdout.decode("utf-8", "replace") + finished.stderr.decode("utf-8", "replace")
    if finished.returncode != 0 and not output.strip():
        raise SmokeFailure(
            EXIT_CONTROL_FAILED,
            "log_unreadable",
            f"{' '.join(command)} exited {finished.returncode} and printed nothing.",
        )
    return output


# --- the conversation -----------------------------------------------------------------------------


@dataclass
class Conversation:
    """What came back over the WebSocket."""

    conversation_id: str | None = None
    responses: list[str] = field(default_factory=list)
    responses_after_ask: list[str] = field(default_factory=list)
    mcp_tool_calls: list[str] = field(default_factory=list)
    #: Raw payloads of the events that say WHY a tool call did not happen. The vendor documents
    #: `mcp_connection_status` by name but not by shape, so these are kept verbatim rather than
    #: picked apart into fields that may not exist -- an unparsed payload an operator can read
    #: beats a parsed one that silently comes back empty.
    diagnostics: list[str] = field(default_factory=list)
    event_types: dict[str, int] = field(default_factory=dict)
    seconds: float = 0.0
    close_code: int | None = None

    @property
    def reply(self) -> str:
        """Everything the agent said after we asked, as one blob to assert over."""
        return "\n".join(self.responses_after_ask)


async def converse(
    signed_url: str,
    ask: str,
    max_seconds: float,
    quiet_seconds: float,
    resume: str | None = None,
) -> Conversation:
    """Hold one text-driven conversation and come straight back out of it.

    The wire protocol here is deliberately the same one web/voice.js speaks, minus the microphone:
    bare `conversation_initiation_client_data`, `pong` for every `ping`, audio ignored. The one
    addition is `user_message`, which the vendor documents as triggering the same response flow as
    speech.

    `resume` is `#46 conversation-replay`'s payload, sent as a `contextual_update` IMMEDIATELY
    after the initiation frame and before anything is asked -- exactly what web/voice.js does, and
    exactly the ordering whose effect on the FIRST agent turn nothing but a billed run can settle.
    """
    try:
        import websockets
    except ImportError as error:
        raise SmokeFailure(
            EXIT_USAGE,
            "missing_dependency",
            "the 'websockets' package is not installed, and this script cannot open a conversation "
            "without it. Install it with:\n"
            "    python3 -m pip install websockets\n"
            "(This is the only dependency beyond the standard library.)",
        ) from error

    conversation = Conversation()
    started = time.monotonic()
    deadline = started + max_seconds

    def note(event_type: str) -> None:
        conversation.event_types[event_type] = conversation.event_types.get(event_type, 0) + 1

    try:
        socket = await websockets.connect(signed_url, max_size=None, open_timeout=25, close_timeout=5)
    except Exception as error:  # noqa: BLE001 - any failure to open is the same finding
        raise SmokeFailure(
            EXIT_SOCKET_REFUSED,
            "socket_refused",
            f"the conversation WebSocket would not open: {type(error).__name__}: {error}\n"
            "The signed URL was minted, so this server and your write token are fine; the failure "
            "is between this host and ElevenLabs. A signed URL is good for 15 minutes and for one "
            "conversation.",
        ) from error

    asked_at: float | None = None
    last_response_at: float | None = None
    try:
        await socket.send(json.dumps({"type": "conversation_initiation_client_data"}))
        if resume:
            await socket.send(json.dumps({"type": "contextual_update", "text": resume}))

        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            try:
                raw = await asyncio.wait_for(socket.recv(), timeout=min(remaining, 2.0))
            except asyncio.TimeoutError:
                raw = None
            except Exception:  # noqa: BLE001 - a closed socket ends the loop, not the run
                break

            if raw is not None:
                try:
                    message = json.loads(raw) if isinstance(raw, str) else {}
                except json.JSONDecodeError:
                    message = {}
                kind = message.get("type", "")
                if kind:
                    note(kind)

                if kind == "conversation_initiation_metadata":
                    meta = message.get("conversation_initiation_metadata_event") or {}
                    conversation.conversation_id = meta.get("conversation_id")
                elif kind == "ping":
                    event_id = (message.get("ping_event") or {}).get("event_id")
                    await socket.send(json.dumps({"type": "pong", "event_id": event_id}))
                elif kind == "agent_response":
                    text = (message.get("agent_response_event") or {}).get("agent_response") or ""
                    if text.strip():
                        conversation.responses.append(text)
                        if asked_at is not None:
                            conversation.responses_after_ask.append(text)
                        last_response_at = time.monotonic()
                elif kind == "agent_response_correction":
                    event = message.get("agent_response_correction_event") or {}
                    text = event.get("corrected_agent_response") or ""
                    if text.strip():
                        conversation.responses.append(text)
                        if asked_at is not None:
                            conversation.responses_after_ask.append(text)
                        last_response_at = time.monotonic()
                elif kind in ("mcp_connection_status", "client_error", "guardrail_triggered"):
                    conversation.diagnostics.append(json.dumps(message)[:1200])
                elif kind == "mcp_tool_call":
                    call = message.get("mcp_tool_call") or {}
                    name = call.get("tool_name")
                    if name:
                        conversation.mcp_tool_calls.append(str(name))

            # Ask once the conversation is actually up. `user_activity` first, which the vendor
            # documents as resetting the turn timeout -- the agent must not start talking over the
            # question while it is being delivered.
            if asked_at is None and conversation.conversation_id is not None:
                await socket.send(json.dumps({"type": "user_activity"}))
                await socket.send(json.dumps({"type": "user_message", "text": ask}))
                asked_at = time.monotonic()
                continue

            # Leave as soon as the answer has settled. Every extra second on this socket is a
            # second of vendor time being spent to learn nothing.
            if (
                asked_at is not None
                and conversation.responses_after_ask
                and last_response_at is not None
                and time.monotonic() - last_response_at >= quiet_seconds
            ):
                break
    finally:
        # Promptly, and on every path out of here including a failure above: an abandoned socket
        # is an open conversation, and an open conversation is being billed.
        try:
            await socket.close()
        except Exception:  # noqa: BLE001
            pass
        conversation.close_code = getattr(socket, "close_code", None)
        conversation.seconds = time.monotonic() - started

    return conversation


def mcp_verdict(conversation: Conversation) -> str:
    """What the vendor's own status events add to a no-tool-call failure."""
    if not conversation.diagnostics:
        return (
            "\n  The vendor sent no mcp_connection_status or client_error event, so it has "
            "nothing to add about why."
        )
    summary = mcp_connection_summary(conversation.diagnostics)
    lines = ["\n  What ELEVENLABS said about our MCP server in this same conversation:"]
    if summary:
        lines.append(f"    {summary}")
        if "CONNECTED" in summary and "NOT connected" not in summary:
            lines.append(
                "    => THE CONNECTION IS FINE AND THE TOOLS ARE VISIBLE TO THE PLATFORM. The "
                "agent's own words about the tools being unavailable are not describing a "
                "connection problem, whatever they sound like. Do not go looking for a fault in "
                "vibe-talk or in the MCP endpoint on this evidence: look at the AGENT's "
                "configuration -- its prompt, its tool selection, and its tool-approval mode -- "
                "for why a connected tool was never invoked."
            )
    lines.append("    raw:")
    lines.extend(f"      {payload}" for payload in conversation.diagnostics)
    return "\n".join(lines)


# --- the run --------------------------------------------------------------------------------------

DEFAULT_ASK = (
    "Please read the most recent message in the channel using your tools, and relay it back to me "
    "in full. Quote the message text exactly as it is written, and give me its timestamp. Do not "
    "post anything to the channel, and do not summarise -- I want the message itself."
)


def make_nonce() -> str:
    return f"gtverify-{int(time.time())}-{random.randrange(16**6):06x}"


@dataclass
class Latest:
    """The most recent message in the channel, as WE read it, before the agent says anything."""

    author: str
    timestamp: str
    content: str

    @property
    def assertable(self) -> bool:
        """Whether there is enough here to build a check that could actually fail."""
        return bool(body_evidence(self.content) and timestamp_forms(self.timestamp))


@dataclass
class Round:
    """One conversation, and everything asserted about it."""

    name: str
    nonce: str | None
    latest: Latest
    conversation: Conversation
    tool_lines: list[ToolLine] = field(default_factory=list)
    verdict: RelayVerdict = field(default_factory=RelayVerdict)
    log_checked: bool = True


class Runner:
    """The steps of a run, sharing the connection details so main() reads as the procedure it is."""

    def __init__(self, args: argparse.Namespace, out: Transcript, read_token: str, write_token: str):
        self.args = args
        self.out = out
        self.read_token = read_token
        self.write_token = write_token
        self.base = args.url.rstrip("/")
        self.log_command = (
            args.log_command.split() if args.log_command else [args.engine, "logs", args.container]
        )
        self.signed_urls: list[str] = []
        self.rounds: list[Round] = []

    # -- pieces ----------------------------------------------------------------------------------

    def channel_path(self, suffix: str) -> str:
        return f"{self.base}/api/v1/channels/{urllib.parse.quote(self.args.channel)}/{suffix}"

    def read_latest(self) -> Latest:
        """Read the channel ourselves. This is the ground truth the agent is measured against."""
        status, body = http_json(self.channel_path("messages?limit=5"), self.read_token)
        if status != 200 or not isinstance(body, dict):
            raise SmokeFailure(
                EXIT_BASELINE_FAILED,
                "baseline_failed",
                f"reading the channel ourselves returned {status}: {body}. Without our own copy of "
                "the message there is nothing to compare the agent's reply against.",
            )
        messages = cast(list[dict[str, JsonValue]], body.get("messages") or [])
        if not messages:
            # A REFUSAL, not a verdict. With nothing to relay, the grounding assertion has nothing
            # to match: depending only on how it happens to be written it would trivially pass or
            # trivially fail, and either answer would be meaningless. So this stops before a
            # conversation is opened, with a status that is neither pass nor fail.
            raise SmokeFailure(
                EXIT_CHANNEL_UNUSABLE,
                "channel_empty",
                "the channel came back EMPTY, so there is no most-recent message for the agent to "
                "relay and nothing for the check to match. Refusing to run rather than reporting a "
                "pass or a fail that would mean nothing. No conversation was opened and nothing "
                "was billed.\n"
                "If the channel really does have messages, the usual cause is that Message Content "
                "Intent is not enabled for this bot in the Discord Developer Portal (Bot → "
                "Privileged Gateway Intents) — without it Discord delivers every message with "
                "blank content.",
            )
        latest = messages[-1]
        return Latest(
            author=str(latest.get("author") or "?"),
            timestamp=str(latest.get("timestamp") or ""),
            content=str(latest.get("content") or ""),
        )

    def post_nonce(self) -> tuple[str, Latest]:
        """Put an uninventable token in the channel, through OUR write API and never the agent."""
        nonce = make_nonce()
        status, body = http_json(
            self.channel_path("reply"),
            self.write_token,
            method="POST",
            payload={
                "text": f"vibe-talk agent smoke test {nonce} — posted by the smoke test itself, "
                "not by the agent. Safe to delete."
            },
        )
        if status != 200:
            raise SmokeFailure(
                EXIT_BASELINE_FAILED,
                "nonce_post_failed",
                f"posting the token returned {status}: {body}. The channel is probably configured "
                "read-only ('ro'); the nonce round needs 'rw'.",
            )
        time.sleep(2)  # let Discord settle before reading it back
        latest = self.read_latest()
        if nonce not in latest.content:
            raise SmokeFailure(
                EXIT_BASELINE_FAILED,
                "nonce_post_failed",
                "the token we posted is not the most recent message in the channel. Somebody else "
                f"posted in the meantime, or the post did not land. Latest is from "
                f"{latest.author!r} at {latest.timestamp}.",
            )
        self.out.emit(f"  ok    posted a unique token to the channel through OUR write API: {nonce}")
        return nonce, latest

    def tools_offered(self, token: str) -> int | None:
        """How many tools this server offers a given credential. None if it cannot be asked."""
        status, body = http_json(
            f"{self.base}/mcp",
            token,
            method="POST",
            payload={"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
        )
        if status != 200 or not isinstance(body, dict):
            return None
        result = cast(dict[str, JsonValue], body.get("result") or {})
        tools = result.get("tools")
        return len(tools) if isinstance(tools, list) else None

    def report_agent_scope(self, conversation: Conversation) -> None:
        """Say which of our credentials the agent is holding, and so whether it could post.

        This is worth the two extra requests because it is the ONE way to answer the question on
        a run where the agent invokes nothing: with no tool line in the access log there is no
        `credential=` field to read, and the credential itself lives inside the agent's
        configuration at the vendor, where we cannot see it. But ElevenLabs tells us how many
        tools it can see, and this server offers a different NUMBER of tools to each scope --
        post_reply is the difference. So the count identifies the credential.

        The counts are asked of this server rather than written down here, so adding or removing
        a tool cannot silently turn this into a confident wrong answer."""
        counts = {
            name: number
            for name, number in (
                ("read", self.tools_offered(self.read_token)),
                ("write", self.tools_offered(self.write_token)),
            )
            if number is not None
        }
        seen: int | None = None
        for raw in conversation.diagnostics:
            try:
                payload = json.loads(raw)
            except json.JSONDecodeError:
                continue
            for entry in (payload.get("mcp_connection_status") or {}).get("integrations") or []:
                if isinstance(entry, dict) and isinstance(entry.get("tool_count"), int):
                    seen = entry["tool_count"]
        if seen is None or len(counts) != 2 or counts["read"] == counts["write"]:
            return
        matches = [name for name, number in counts.items() if number == seen]
        if matches == ["write"]:
            self.out.emit(
                f"  FINDING  ElevenLabs can see {seen} tools, and this server offers "
                f"{counts['write']} to the WRITE credential and {counts['read']} to the read one. "
                "So the agent is configured with the WRITE token and post_reply IS reachable: it "
                "can post in your name. Nothing here asked it to, and the access log shows it did "
                "not. Give it the read token if you want that to be impossible rather than "
                "merely unasked-for."
            )
        elif matches == ["read"]:
            self.out.emit(
                f"  ok    ElevenLabs can see {seen} tools, which is what this server offers the "
                "READ credential. post_reply is not reachable by the agent at all."
            )
        else:
            self.out.emit(
                f"  NOTE  ElevenLabs can see {seen} tools, which matches neither what this server "
                f"offers the read credential ({counts['read']}) nor the write one "
                f"({counts['write']}). The agent may be pointed at a different server, or at a "
                "stale cached manifest."
            )

    # -- `--replay-check`: three conversations, three separate answers ---------------------------

    def run_replay_check(self) -> ReplayVerdict:
        """Prove -- or disprove -- that the vendor acts on a replayed transcript.

        Three conversations, in this order:

          A  states a nonce fact, and its turns are RECORDED through this server's own transcript
             API, exactly as web/voice.js records them. Nothing here shortcuts the store: the
             payload B receives is the one the real page would get.
          B  opens WITH the replay payload and is asked to recall the nonce. It must.
          C  the CONTROL: same question, no payload, and it must NOT be able to answer. Without
             this the run proves the agent is fluent rather than that it remembers.
        """
        nonce = make_nonce()
        fact = REPLAY_FACT.format(nonce=nonce)

        # -- refuse before billing anything, when the substrate is not there ----------------------
        status, body = http_json(f"{self.base}/api/v1/conversations", self.write_token)
        if status == 503:
            raise SmokeFailure(
                EXIT_CHANNEL_UNUSABLE,
                "storage_not_configured",
                "this server has no durable store, so there is no transcript to replay and this "
                "check has nothing to prove. Set storage.path and restart. Nothing was billed.",
            )
        if status != 200:
            raise SmokeFailure(
                EXIT_BASELINE_FAILED,
                "storage_unreadable",
                f"GET /api/v1/conversations returned {status}: {body}. Refusing to open a "
                "conversation whose result could not then be checked. Nothing was billed.",
            )

        out = self.out
        out.emit("")
        out.emit("-- conversation A: establishing something only this conversation could know")
        first = asyncio.run(
            converse(self.mint(), fact, self.args.max_seconds, self.args.quiet_seconds)
        )
        conversation_id = first.conversation_id or f"replaycheck-{nonce}"
        out.emit(f"  ok    conversation {conversation_id} ran {first.seconds:.1f}s")

        # Recorded through the SAME route the page uses. If this cannot be written there is no
        # transcript to replay and the check refuses rather than reporting a vendor failure that
        # is really a storage failure.
        for speaker, text in (("you", fact), ("agent", first.reply or "(the agent said nothing)")):
            wrote, wrote_body = http_json(
                f"{self.base}/api/v1/conversations/{urllib.parse.quote(conversation_id)}/turns",
                self.write_token,
                method="POST",
                payload={"speaker": speaker, "text": text},
            )
            if wrote != 200:
                raise SmokeFailure(
                    EXIT_BASELINE_FAILED,
                    "transcript_not_recorded",
                    f"recording the {speaker} turn returned {wrote}: {wrote_body}. One "
                    "conversation has been billed; the remaining two are not worth opening.",
                )
        out.emit("  ok    both turns recorded through /api/v1/conversations/{id}/turns")

        status, payload = http_json(
            f"{self.base}/api/v1/conversations/{urllib.parse.quote(conversation_id)}/replay",
            self.write_token,
        )
        if status != 200 or not isinstance(payload, dict):
            raise SmokeFailure(
                EXIT_BASELINE_FAILED,
                "replay_unavailable",
                f"GET .../replay returned {status}: {payload}. One conversation was billed.",
            )
        if payload.get("enabled") is not True:
            raise SmokeFailure(
                EXIT_CHANNEL_UNUSABLE,
                "replay_disabled",
                "this server has replay.enabled = false, so there is nothing to check. Turn it on "
                "and restart. One conversation was billed before this could be known -- the "
                "transcript had to exist before it could be asked for.",
            )
        text = str(payload.get("text") or "")
        if not text or not payload.get("included"):
            raise SmokeFailure(
                EXIT_CHANNEL_UNUSABLE,
                "replay_empty",
                "the replay came back EMPTY, so conversation B would be given nothing and the "
                "check could not fail. Refusing to spend two more conversations on a question "
                "that cannot be answered.",
            )
        out.emit(
            f"  ok    the replay carries {payload.get('included')} turn(s), "
            f"{payload.get('dropped')} dropped, transport {payload.get('transport')}"
        )
        if nonce not in text:
            raise SmokeFailure(
                EXIT_BASELINE_FAILED,
                "replay_lost_the_nonce",
                "the payload this server built does NOT contain the nonce, so conversation B "
                "could not answer even if the vendor honoured it perfectly. That is a fault here, "
                "not at the vendor, and the remaining conversations would prove nothing.",
            )

        out.emit("")
        out.emit("-- conversation B: a NEW call, opened with the record of A")
        resumed = asyncio.run(
            converse(
                self.mint(),
                REPLAY_QUESTION,
                self.args.max_seconds,
                self.args.quiet_seconds,
                resume=text,
            )
        )
        out.emit(f"  ok    conversation {resumed.conversation_id or '?'} ran {resumed.seconds:.1f}s")

        out.emit("")
        out.emit("-- conversation C: the CONTROL, same question and NO record")
        control = asyncio.run(
            converse(
                self.mint(),
                REPLAY_QUESTION,
                self.args.max_seconds,
                self.args.quiet_seconds,
            )
        )
        out.emit(f"  ok    conversation {control.conversation_id or '?'} ran {control.seconds:.1f}s")

        verdict = replay_verdict(nonce, resumed.reply, control.reply)
        out.emit("")
        out.emit("-- what the three conversations showed")
        out.emit(
            f"  {'ok   ' if verdict.honoured else 'FAIL '} B, given the record, "
            f"{'returned' if verdict.honoured else 'did NOT return'} the nonce"
        )
        out.emit(
            f"  {'FAIL ' if verdict.control_leaked else 'ok   '} C, the control, "
            f"{'ALSO returned it' if verdict.control_leaked else 'could not answer'}"
        )
        out.emit(f"        B said: {resumed.reply[:400] or '(nothing)'}")
        out.emit(f"        C said: {control.reply[:400] or '(nothing)'}")
        if verdict.control_leaked:
            raise SmokeFailure(
                EXIT_REPLAY_CONTROL_LEAKED,
                "replay_control_leaked",
                "the CONTROL conversation produced the nonce without ever being given it. That "
                "invalidates B rather than joining it: whatever it proves, it is not that the "
                "replay was honoured. Check that the agent has no memory feature enabled and that "
                "the nonce is not guessable.",
            )
        if not verdict.honoured:
            raise SmokeFailure(
                EXIT_REPLAY_NOT_HONOURED,
                "replay_not_honoured",
                "the vendor did NOT act on the replayed transcript for the first agent turn. This "
                "is a real, useful answer and not a broken run: it means `#46 "
                "conversation-replay` does nothing on this deployment with transport "
                f"{payload.get('transport')!r}. Try the other transport "
                "(replay.transport = \"client_data\"), and until one of them works the "
                "interface must not claim a call was resumed.",
            )
        return verdict

    def mint(self) -> str:
        status, body = http_json(f"{self.base}/api/v1/signed-url", self.write_token)
        if status != 200 or not isinstance(body, dict) or not body.get("signed_url"):
            raise SmokeFailure(
                EXIT_MINT_FAILED,
                "mint_failed",
                f"GET /api/v1/signed-url returned {status}: {body}\n"
                "503 elevenlabs_not_configured names the setting that is missing; 502 "
                "elevenlabs_error carries what ElevenLabs said. No conversation was attempted.",
            )
        signed_url = cast(str, body["signed_url"])
        self.signed_urls.append(signed_url)
        host = urllib.parse.urlsplit(signed_url).netloc
        # The URL itself is a bearer credential for the next fifteen minutes. Its HOST is what an
        # operator needs in order to debug a connection failure; the token in it is not.
        self.out.emit(f"  ok    minted a signed URL for agent {body.get('agent_id')} (host {host})")
        return signed_url

    def hold_round(self, name: str, latest: Latest, nonce: str | None) -> Round:
        """Mark the log, hold one conversation, and assert the mechanism worked."""
        log_before = 0
        if not self.args.no_log_check:
            log_before = len(read_log(self.log_command).splitlines())

        signed_url = self.mint()
        self.out.emit("")
        self.out.emit(f"-- the {name} round: holding a conversation (this is what costs money)")
        conversation = asyncio.run(
            converse(signed_url, self.args.ask or DEFAULT_ASK, self.args.max_seconds, self.args.quiet_seconds)
        )
        round_ = Round(name=name, nonce=nonce, latest=latest, conversation=conversation)
        round_.log_checked = not self.args.no_log_check
        self.rounds.append(round_)

        self.out.emit(
            f"  ok    conversation {conversation.conversation_id or '?'} ran "
            f"{conversation.seconds:.1f}s and is closed (code {conversation.close_code})"
        )
        seen = ", ".join(f"{k}x{v}" for k, v in sorted(conversation.event_types.items())) or "none"
        self.out.emit(f"        events seen: {seen}")
        self.report_agent_scope(conversation)

        if not conversation.responses_after_ask:
            raise SmokeFailure(
                EXIT_NO_REPLY,
                "no_reply",
                f"the socket opened and the question was sent, but the agent said nothing back "
                f"within {self.args.max_seconds:.0f}s. Events seen: {seen}. If the agent greeted us "
                "and then went quiet, look at the agent's own logs in the ElevenLabs dashboard for "
                f"conversation {conversation.conversation_id}.",
            )
        for index, text in enumerate(conversation.responses_after_ask, start=1):
            self.out.emit(f"        agent [{index}]: {text.strip()[:400]}")

        # -- THE PRIMARY ASSERTION -------------------------------------------------------------
        self.out.emit("")
        self.out.emit("-- did the agent actually call a tool? (the primary assertion)")
        if self.args.no_log_check:
            self.out.emit(
                "  SKIP  --no-log-check: the mechanism assertion is OFF. This run CANNOT fail the "
                "way 2026-08-19 failed."
            )
            return round_

        after = read_log(self.log_command).splitlines()[log_before:]
        round_.tool_lines = tool_lines_in("\n".join(after))
        read_calls = [t for t in round_.tool_lines if t.tool in READ_TOOLS]
        if not read_calls:
            raise SmokeFailure(
                EXIT_NO_TOOL_CALL,
                "no_tool_call",
                "the agent REPLIED BUT CALLED NO TOOL. This is exactly the 2026-08-19 failure: "
                "fluent prose, an untouched server.\n"
                f"  what it said: {conversation.reply.strip()[:300]}\n"
                f"  new log lines during the round: {len(after)}, of which tool calls: "
                f"{len(round_.tool_lines)}\n"
                "There is deliberately NO nonce escalation from here. Escalating costs a second "
                "billed conversation, and it could not tell you anything: an agent that invokes no "
                "tool at all cannot read a token either, whoever posted it.\n"
                "Look at the agent's MCP server configuration in the ElevenLabs dashboard: on "
                "2026-08-19 the handshake succeeded and the agent then reported its tools as out "
                "of date."
                + mcp_verdict(conversation),
            )
        for call in read_calls:
            self.out.emit(
                f"  ok    {call.tool} on channel {call.channel} with the {call.credential} "
                f"credential → {call.outcome}"
            )
        return round_

    def check_grounding(self, round_: Round) -> bool:
        """Did the reply carry the message back? Reported, not raised: a failure may escalate."""
        round_.verdict = check_relay(
            round_.conversation.reply, round_.latest.timestamp, round_.latest.content, round_.nonce
        )
        self.out.emit("")
        self.out.emit(f"-- is the {round_.name} round's reply grounded in the channel? (corroboration)")
        verdict = round_.verdict
        if verdict.nonce_hit:
            self.out.emit(
                f"  ok    the reply carries back the token we posted, {verdict.nonce_hit_kind} "
                f"({verdict.nonce_hit}). That string exists nowhere but this channel."
            )
        if verdict.timestamp_hit:
            self.out.emit(f"  ok    the reply carries the message's timestamp ({verdict.timestamp_hit})")
        if verdict.body_hit:
            self.out.emit(f"  ok    the reply quotes the message body ({verdict.body_hit!r})")
        if not verdict.grounded:
            self.out.emit(f"  MISS  timestamp: {verdict.timestamp_hit or 'no'}")
            self.out.emit(f"  MISS  body:      {verdict.body_hit or 'no'}")
            if round_.nonce:
                self.out.emit(f"  MISS  token:     {verdict.nonce_hit or 'no'}")
            self.out.emit(f"        what it said: {round_.conversation.reply.strip()[:400]}")
        return verdict.grounded

    def report_findings(self) -> None:
        """Things worth saying that are not pass/fail."""
        self.out.emit("")
        self.out.emit("-- findings")
        observed = [t for round_ in self.rounds for t in round_.tool_lines]
        if not observed:
            self.out.emit("  (no tool calls were observed, so there is nothing to say about scope)")
            return
        if [t for t in observed if t.credential == "write"]:
            self.out.emit(
                "  FINDING  the agent's tool calls arrived with the WRITE credential, so post_reply "
                "was reachable throughout. It was never asked to post, and the log shows it did "
                "not. If you want a read-only agent, give it the read token."
            )
        else:
            self.out.emit(
                "  ok    the agent's tool calls arrived with the READ credential; post_reply was "
                "not reachable."
            )
        posts = [t for t in observed if t.tool == "post_reply"]
        if posts:
            self.out.emit(
                f"  FINDING  post_reply was CALLED {len(posts)} time(s) during a read-only test. "
                "Nothing here asked for that."
            )
        else:
            self.out.emit("  ok    post_reply was not called")
        vendor_side = sorted({name for r in self.rounds for name in r.conversation.mcp_tool_calls})
        if vendor_side:
            self.out.emit(
                "  ok    the vendor also reported the tool call over the socket: "
                + ", ".join(vendor_side)
            )


def check_exit_status_controls() -> list[str]:
    """Does a FAILED run actually leave a non-zero PROCESS STATUS?

    This exists because the question came up and could not be answered from the transcript: the
    2026-08-19 run was invoked through a pipe, so the status the operator saw was the pipe's, not
    the script's. A script that prints "FAILED" and exits 0 is worse than one that says nothing --
    every automated caller reads it as a pass -- and it is invisible in exactly the situation where
    someone would look.

    So the escalation path is driven end to end here, offline, with the network stubbed, and the
    RETURN VALUE of main() is asserted against the documented code. `sys.exit(main(...))` at the
    bottom of the file turns that into the process status.
    """
    problems = []
    expected_latest = Latest(
        author="owner",
        timestamp=SPOKEN_EASTERN_INSTANT,
        content="Rebuilt the container after the sparkplug refactor; digest looks right now.",
    )
    expected_nonce = "gtverify-1755607000-a3f9c1"

    class StubConversation(Conversation):
        def __init__(self, reply: str) -> None:
            super().__init__(
                conversation_id="self-test",
                responses_after_ask=[reply],
                seconds=0.0,
                close_code=1000,
            )

    def stub_http_json(
        url: str,
        token: str,
        method: str = "GET",
        payload: dict[str, JsonValue] | None = None,
        timeout: int = 30,
    ) -> tuple[int, JsonValue]:
        return 200, {}

    def stub_read_latest(self: Runner) -> Latest:
        return expected_latest

    def stub_post_nonce(self: Runner) -> tuple[str, Latest]:
        return expected_nonce, expected_latest

    def stub_hold_round(self: Runner, name: str, latest: Latest, nonce: str | None) -> Round:
        reply = (
            "I had a look and it all seems fine."
            if name == "cheap"
            else f"It says {expected_nonce}."
        )
        round_ = Round(name=name, nonce=nonce, latest=latest, conversation=StubConversation(reply))
        self.rounds.append(round_)
        return round_

    environment = {
        key: os.environ.get(key)
        for key in ("VIBE_TALK_READ_TOKEN", "VIBE_TALK_WRITE_TOKEN")
    }
    try:
        os.environ["VIBE_TALK_READ_TOKEN"] = "self-test-read-credential"
        os.environ["VIBE_TALK_WRITE_TOKEN"] = "self-test-write-credential"
        buffer = io.StringIO()
        with (
            patch(f"{__name__}.http_json", stub_http_json),
            patch.object(Runner, "read_latest", stub_read_latest),
            patch.object(Runner, "post_nonce", stub_post_nonce),
            patch.object(Runner, "hold_round", stub_hold_round),
            contextlib.redirect_stdout(buffer),
        ):
            code = main(["--url", "http://self-test.invalid", "--channel", "c", "--no-log-check"])
    finally:
        for key, value in environment.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value

    if code != EXIT_RELAY_TOO_STRICT:
        problems.append(
            "control: a run whose cheap round misses and whose token round hits returned "
            f"{code}, not the documented {EXIT_RELAY_TOO_STRICT} (relay_check_too_strict). "
            + (
                "It returned SUCCESS, so every automated caller would read this failure as a pass."
                if code == EXIT_OK
                else "The exit code no longer names the failure it documents."
            )
        )
    return problems


# --- `--replay-check`: does the vendor act on a replayed transcript? -----------------------------
#
# `#46 conversation-replay` rebuilds continuity by handing a NEW conversation a written record of
# the old one. Whether the vendor puts that record in context for the FIRST agent turn is a
# question no test in this repository can answer: it is a property of the platform, reachable only
# by paying for three conversations.
#
# THREE, not two, and the third is the point. A run that only proved conversation B could answer
# would be proving the agent is fluent. The control -- same question, no payload -- has to FAIL to
# answer, or the positive result means nothing. All three outcomes are reported separately, and
# "not honoured" has an exit code of its own, because it is the most useful thing this can say.

#: What conversation A is told, and what B is asked to recall. A nonce, so nothing but the replay
#: could put it in the answer.
REPLAY_FACT = "Please remember this exactly: my build nonce for today is {nonce}."
REPLAY_QUESTION = "Earlier in our conversation I gave you my build nonce for today. What was it?"


@dataclass
class ReplayVerdict:
    """What the three conversations showed, as three separate answers."""

    #: Did B, opened WITH the payload, return the nonce?
    honoured: bool = False
    #: Did C, opened WITHOUT it, also return the nonce? If so, B proves nothing.
    control_leaked: bool = False
    #: What B actually said, for the report.
    resumed_reply: str = ""
    #: ...and what C said.
    control_reply: str = ""

    @property
    def proves_memory(self) -> bool:
        return self.honoured and not self.control_leaked


def replay_verdict(nonce: str, resumed_reply: str, control_reply: str) -> ReplayVerdict:
    """Read the two replies. Reuses the nonce matcher the relay check already trusts."""
    return ReplayVerdict(
        honoured=bool(check_relay(resumed_reply, "", "", nonce).nonce_hit),
        control_leaked=bool(check_relay(control_reply, "", "", nonce).nonce_hit),
        resumed_reply=resumed_reply,
        control_reply=control_reply,
    )


def check_replay_controls() -> list[str]:
    """The replay verdict's own controls, run before anything is billed."""
    problems: list[str] = []
    nonce = "GT-7Q2X"
    honoured = replay_verdict(nonce, f"Your build nonce was {nonce}.", "I have no record of that.")
    if not honoured.proves_memory:
        problems.append(
            "the replay verdict does not recognise a resumed conversation that returned the nonce "
            "while the control did not -- which is the only shape that means the feature works."
        )
    missed = replay_verdict(nonce, "I do not have that from earlier.", "I have no record of that.")
    if missed.honoured or missed.proves_memory:
        problems.append(
            "the replay verdict reports a reply with no nonce in it as honoured, so a vendor that "
            "ignores the payload would be reported as one that acts on it."
        )
    leaked = replay_verdict(nonce, f"It was {nonce}.", f"I believe it was {nonce}.")
    if not leaked.control_leaked or leaked.proves_memory:
        problems.append(
            "the replay verdict does not notice that the CONTROL answered too, so a run proving "
            "only fluency would be reported as proving memory."
        )
    return problems


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="smoke-agent.py",
        description="Hold a real conversation with the deployed voice agent and prove it called us.",
        epilog="COSTS VENDOR MINUTES. Opt-in and manual; never run this from CI.",
    )
    parser.add_argument("--url", help="this server's base URL, e.g. http://127.0.0.1:8080")
    parser.add_argument("--channel", help="channel snowflake to ask about")
    parser.add_argument(
        "--nonce",
        action="store_true",
        help="go straight to the token round instead of waiting for the cheap check to fail. Use "
        "this when you want proof on a run that would otherwise pass cheaply. It always writes one "
        "line to the channel.",
    )
    parser.add_argument("--ask", help="override the question put to the agent")
    parser.add_argument("--container", default="vibe-talk", help="container to read the log from")
    parser.add_argument("--engine", default=os.environ.get("VIBE_TALK_ENGINE", "podman"))
    parser.add_argument(
        "--log-command",
        help="shell-free override for reading the server log, e.g. 'journalctl -u vibe-talk'. "
        "Defaults to '<engine> logs <container>'.",
    )
    parser.add_argument(
        "--no-log-check",
        action="store_true",
        help="run WITHOUT the primary assertion. The run then cannot fail the way 2026-08-19 "
        "failed; it says so, loudly, in the report.",
    )
    parser.add_argument("--max-seconds", type=float, default=90.0, help="hard cap per conversation")
    parser.add_argument(
        "--quiet-seconds",
        type=float,
        default=8.0,
        help="hang up this long after the agent stops talking",
    )
    parser.add_argument(
        "--replay-check",
        action="store_true",
        help="instead of the relay check, prove whether the vendor acts on a replayed transcript "
        "(#46 conversation-replay). Holds THREE conversations, so it costs three times as much, "
        "and refuses to run at all when storage is not configured.",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run every control offline. No network, no vendor minutes, no conversation.",
    )
    return parser


def run_self_test(out: Transcript) -> int:
    out.emit("smoke-agent self-test — the controls only. No network, no vendor minutes.")
    out.emit("")
    problems = (
        check_log_scanner_controls()
        + check_relay_controls()
        + check_credential_scanner_controls()
        + check_diagnostics_controls()
        + check_replay_controls()
        # Only here, never in main()'s pre-flight: this one CALLS main(), and running it from
        # inside main()'s own control block would recurse.
        + check_exit_status_controls()
    )
    checks = [
        "the log scanner finds a REAL captured tool line",
        "the log scanner finds a COLOUR-CODED tool line",
        "the log scanner stays silent on the 2026-08-19 handshake-only log",
        "post_reply is not something the primary assertion can be satisfied by",
        "a real tool line counts as one of the read tools",
        "a handshake-only log matches none of the read tools",
        "the relay check accepts a reply that carries the message back",
        "the relay check REJECTS a reply carrying only the timestamp",
        "the relay check REJECTS a reply carrying only part of the body",
        "the relay check accepts a truthful relay in a different case and spacing",
        "the relay check REJECTS a fluent confabulated reply",
        "the relay check recognises a verbatim nonce",
        "the relay check recognises a nonce broken up by punctuation",
        "the relay check does not invent a nonce that is not there",
        "an undistinctive body yields no evidence to assert on",
        "the relay check accepts the agent's REAL spoken reply, 'thirteen ten and fifty-two ...'",
        "the relay check accepts a spoken time the agent CONVERTED to Eastern, and names the zone",
        "the relay check REJECTS a different time spoken in the same style",
        "the relay check REJECTS a bare quantity that happens to equal the minute",
        "the relay check REJECTS a spoken YEAR read as a time ('twenty twenty-six')",
        "the relay check accepts a WRITTEN time converted to another zone, and rejects another",
        "the relay check understands the 12-hour clock and single-digit minutes ('thirteen oh five')",
        "the relay check REJECTS spoken quantities that flatten into a clock reading",
        "the relay check accepts a spoken time TRANSCRIBED IN CAPITALS",
        "the relay check names the plain UTC reading when a reply gives two",
        "the relay check understands on-the-hour idioms (\"nine o'clock\") and holds them to :00",
        "the relay check REJECTS a spoken timestamp carrying none of the message body",
        "the credential scanner finds a planted token",
        "the credential scanner does not report a token that is absent",
        "a blank secret does not match everything",
        "the MCP-status reader understands a real captured payload",
        "the MCP-status reader invents nothing from unrelated events",
        "the replay verdict accepts memory: B answered and the control did not",
        "the replay verdict REJECTS a resumed reply that carries no nonce",
        "the replay verdict notices when the CONTROL answered too, so fluency is not memory",
        "a FAILED run returns the documented exit code, not 0",
    ]
    if problems:
        for problem in problems:
            out.emit(f"  FAIL  {problem}")
        out.emit("")
        out.emit(f"FAILED — {len(problems)} control(s) failed. The checks cannot be trusted.")
        return EXIT_CONTROL_FAILED
    for check in checks:
        out.emit(f"  ok    {check}")
    out.emit("")
    out.emit(f"PASSED — {len(checks)} controls")
    return EXIT_OK


def main(argv: list[str]) -> int:  # noqa: PLR0911, PLR0912, PLR0915 - one linear procedure
    out = Transcript()
    args = build_parser().parse_args(argv)

    if args.self_test:
        return run_self_test(out)

    read_token = os.environ.get("VIBE_TALK_READ_TOKEN", "")
    write_token = os.environ.get("VIBE_TALK_WRITE_TOKEN", "")
    elevenlabs_key = os.environ.get("VIBE_TALK_ELEVENLABS_API_KEY", "")

    if not args.url or not args.channel:
        out.emit("smoke-agent: --url and --channel are both required (or use --self-test).")
        return EXIT_USAGE
    if not write_token:
        out.emit(
            "smoke-agent: VIBE_TALK_WRITE_TOKEN is not set. Minting a signed URL requires the "
            "WRITE scope, because the conversation it opens reaches an agent that can post."
        )
        return EXIT_USAGE
    if not read_token:
        out.emit("smoke-agent: VIBE_TALK_READ_TOKEN is not set; it is needed to read the channel.")
        return EXIT_USAGE

    runner = Runner(args, out, read_token, write_token)
    wall_started = time.time()
    result, label, summary = EXIT_OK, "PASSED", ""

    out.emit("vibe-talk agent smoke test — a REAL conversation, so a REAL cost.")
    out.emit(f"   target:   {runner.base}")
    out.emit(f"   channel:  {args.channel}")
    out.emit(
        "   log:      NOT CHECKED (--no-log-check)"
        if args.no_log_check
        else f"   log:      {' '.join(runner.log_command)}"
    )
    out.emit("")

    try:
        # -- 0. the controls, before anything is spent -------------------------------------------
        # If a check cannot be trusted, that is worth knowing BEFORE a conversation is paid for.
        out.emit("-- the checks' own controls")
        problems = (
            check_log_scanner_controls()
            + check_relay_controls()
            + check_credential_scanner_controls()
            + check_diagnostics_controls()
        )
        if problems:
            for problem in problems:
                out.emit(f"   {problem}")
            raise SmokeFailure(
                EXIT_CONTROL_FAILED,
                "control_failed",
                f"{len(problems)} control(s) failed; nothing below would have meant anything.",
            )
        out.emit("  ok    the log scanner finds a real tool line and ignores a handshake-only log")
        out.emit("  ok    the relay check accepts a grounded reply and rejects a confabulated one")
        out.emit("  ok    the credential scanner finds a planted token and not an absent one")
        out.emit("  ok    the MCP-status reader understands a real payload and invents nothing")

        status, _ = http_json(f"{runner.base}/healthz", "")
        if status != 200:
            raise SmokeFailure(
                EXIT_BASELINE_FAILED, "unreachable", f"GET /healthz returned {status}, expected 200."
            )
        out.emit("  ok    the server is up")

        # -- `--replay-check` is its own run, not an extra assertion on this one ------------------
        #
        # It asks a different question (does the VENDOR act on a replayed transcript?), spends
        # three conversations rather than one, and reports three outcomes. Bolting it onto the
        # relay check would make a single "smoke failed" out of two unrelated findings.
        if args.replay_check:
            for problem in check_replay_controls():
                out.emit(f"   {problem}")
                result = EXIT_CONTROL_FAILED
            if result == EXIT_CONTROL_FAILED:
                raise SmokeFailure(
                    EXIT_CONTROL_FAILED,
                    "control_failed",
                    "the replay verdict's own controls failed; nothing below would mean anything.",
                )
            out.emit("  ok    the replay verdict tells memory from fluency, and notices a leak")
            runner.run_replay_check()
            out.emit("")
            out.emit(
                "PASSED — the vendor acted on the replayed transcript for the first agent turn, "
                "and the control could not."
            )
            out.emit(f"   wall clock: {time.time() - wall_started:.1f}s")
            leaks = secrets_found(
                out.text(),
                [
                    ("VIBE_TALK_READ_TOKEN", read_token),
                    ("VIBE_TALK_WRITE_TOKEN", write_token),
                    ("VIBE_TALK_ELEVENLABS_API_KEY", elevenlabs_key),
                ]
                + [("signed URL", url) for url in runner.signed_urls],
            )
            if leaks:
                for leak in leaks:
                    out.emit(f"   {leak}")
                return EXIT_CREDENTIAL_LEAK
            return EXIT_OK

        # -- 1. the cheap round: read the channel ourselves, then ask -----------------------------
        #
        # THE CHEAP ROUND IS PROOF, NOT MERELY CONFIDENCE, and the reason is structural rather
        # than probabilistic. There is no path by which the agent produces today's latest message
        # without invoking a tool:
        #
        #   * this opens a FRESH conversation, so there is no prior history to draw on;
        #   * the channel's recent content POSTDATES any training data;
        #   * every message in it is from the owner, from an agent of his, or from the bot -- none
        #     of which is in the model's context at turn one.
        #
        # Do not re-weaken this into a hedge. It matters because it is what makes a green run mean
        # something on its own, without spending a second conversation to confirm it.
        #
        # The escalation ladder, because getting it wrong costs the owner money:
        #
        #   cheap passes             -> done. One conversation, nothing written to the channel.
        #   cheap fails on GROUNDING -> escalate to a token round. NOT to rule out a false
        #                               POSITIVE -- those are impossible, per the argument above --
        #                               but to diagnose a false NEGATIVE: if the agent really did
        #                               read the channel and our substring matcher was simply too
        #                               strict, the token separates "the agent is not reading" from
        #                               "our check is wrong". That is a question about the quality
        #                               of THIS SCRIPT, not about the agent's honesty, and the two
        #                               have opposite fixes.
        #   cheap fails on the TOOL CALL -> STOP. No escalation (see hold_round): an agent that
        #                               invoked nothing cannot read a token either, so a second
        #                               billed conversation would buy no information.
        #   channel empty or undistinctive -> REFUSE before any conversation (see read_latest and
        #                               just below). A check with nothing to match on is not a
        #                               check, and its answer is not evidence either way.
        latest = runner.read_latest()
        out.emit(
            f"  ok    read the channel ourselves: latest message is from {latest.author!r} "
            f"at {latest.timestamp} ({len(latest.content)} chars)"
        )

        if not args.nonce and not latest.assertable:
            raise SmokeFailure(
                EXIT_CHANNEL_UNUSABLE,
                "channel_unusable",
                "the most recent message has nothing distinctive enough to match on "
                f"(body {len(latest.content)} chars, timestamp {latest.timestamp!r}), so the "
                "grounding check would pass or fail on how it happens to be written rather than on "
                "what the agent did. Refusing to run: that is neither a pass nor a fail. No "
                "conversation was opened and nothing was billed.\n"
                "Re-run with --nonce. That posts a token of our own through this server's write "
                "API first, so the check no longer depends on what happens to be in the channel.",
            )
        if args.nonce:
            out.emit("  NOTE  --nonce: going straight to the token round, as asked.")

        if args.nonce:
            nonce, latest = runner.post_nonce()
            round_ = runner.hold_round("token", latest, nonce)
            if runner.check_grounding(round_):
                summary = (
                    "the agent called a tool AND relayed back a token that exists nowhere but this "
                    "channel. It really read it."
                )
            else:
                raise SmokeFailure(
                    EXIT_UNGROUNDED,
                    "ungrounded",
                    "a tool WAS called, but the reply does not carry back the token we posted "
                    f"({nonce}). That string cannot be guessed, so the agent is invoking a tool "
                    "without using what comes back, or it is answering from something other than "
                    "this channel.",
                )
        else:
            round_ = runner.hold_round("cheap", latest, None)
            if runner.check_grounding(round_):
                summary = "the agent called a tool and relayed the channel's latest message back."
            else:
                # -- 2. ESCALATION ---------------------------------------------------------------
                # WHY THIS EXISTS, precisely, because a guard kept for a rationale that does not
                # apply is one the next reader either deletes or preserves while believing
                # something false about it:
                #
                # It is NOT here to rule out a false positive. A cheap-round PASS cannot be faked
                # (fresh conversation, post-training content, nothing of this channel in the
                # model's context) -- see the ladder comment above.
                #
                # It is here for the opposite error. A cheap-round FAIL has two causes with
                # opposite fixes: the agent is not really reading, or the agent read fine and THIS
                # SCRIPT'S substring matching was too strict. The second is a defect in our test.
                # A token nobody could guess tells the two apart, and that is worth one more
                # conversation because otherwise the next session debugs the wrong system.
                out.emit("")
                out.emit(
                    "-- ESCALATING. The cheap check did not find the message in the reply. That "
                    "has two causes with opposite fixes — the agent is not really reading, or our "
                    "substring matching was too strict — so this posts a token nobody could guess "
                    "and asks again. THIS IS A SECOND BILLED CONVERSATION."
                )
                nonce, nonce_latest = runner.post_nonce()
                nonce_round = runner.hold_round("token", nonce_latest, nonce)
                if runner.check_grounding(nonce_round):
                    raise SmokeFailure(
                        EXIT_RELAY_TOO_STRICT,
                        "relay_check_too_strict",
                        "THE AGENT IS FINE; THIS TEST IS NOT.\n"
                        "  the cheap round FAILED: the reply did not contain the timestamp and a "
                        "distinctive piece of the message body.\n"
                        f"  the token round PASSED: the agent relayed back {nonce}, which exists "
                        "nowhere but this channel, so it genuinely read it.\n"
                        "  => the defect is in this script's matching, not in the deployment. Look "
                        "at check_relay()/timestamp_forms()/body_evidence() against what the agent "
                        "actually said below, and loosen what is too fussy. Do NOT go looking for a "
                        "problem in the agent or the MCP server.\n"
                        f"  cheap round said: {round_.conversation.reply.strip()[:300]}\n"
                        f"  token round said: {nonce_round.conversation.reply.strip()[:300]}",
                    )
                raise SmokeFailure(
                    EXIT_UNGROUNDED,
                    "ungrounded",
                    "THE AGENT IS NOT READING THE CHANNEL, and this is now proven rather than "
                    "inferred.\n"
                    "  the cheap round FAILED: the reply did not carry the latest message back.\n"
                    f"  the token round ALSO FAILED: we posted {nonce} through our own write API "
                    "and the agent did not relay it back, though it did invoke a tool.\n"
                    "  => a tool is being called and its result is not reaching the answer. Check "
                    "what the tool returned in the access log, and what the agent was told to do "
                    "with it in its ElevenLabs prompt.\n"
                    f"  cheap round said: {round_.conversation.reply.strip()[:300]}\n"
                    f"  token round said: {nonce_round.conversation.reply.strip()[:300]}",
                )

        runner.report_findings()
    except SmokeFailure as failure:
        # A refusal is deliberately NOT rendered as a failure. "This run could not conclude
        # anything" and "the deployment is broken" are different facts, and collapsing them is how
        # a meaningless red sends someone hunting a bug that is not there.
        word = "REFUSED" if failure.code == EXIT_CHANNEL_UNUSABLE else "FAILED"
        out.emit("")
        out.emit(f"{word} [{failure.label}]")
        out.emit(failure.detail)
        result, label, summary = failure.code, word, failure.label

    # -- always: cost, and the credential scan over everything printed ------------------------------
    wall = time.time() - wall_started
    out.emit("")
    out.emit(f"wall time: {wall:.1f}s")
    total_seconds = sum(r.conversation.seconds for r in runner.rounds)
    if runner.rounds:
        detail = "; ".join(
            f"{r.name} round {r.conversation.seconds:.1f}s"
            + (f" (id {r.conversation.conversation_id})" if r.conversation.conversation_id else "")
            for r in runner.rounds
        )
        out.emit(
            f"conversations: {len(runner.rounds)} — {detail}. {total_seconds:.1f}s of socket time "
            "in total; that is what was billed."
        )
    else:
        out.emit("conversations: none was opened, so no vendor time was spent.")
    posted = [r.nonce for r in runner.rounds if r.nonce]
    if posted:
        out.emit(
            f"channel: this test posted {len(posted)} message(s) — {', '.join(posted)}. "
            "Delete them if you like."
        )
    else:
        out.emit("channel: nothing was written to it.")

    scanned = out.text()
    secrets = [
        ("VIBE_TALK_READ_TOKEN", read_token),
        ("VIBE_TALK_WRITE_TOKEN", write_token),
        ("VIBE_TALK_ELEVENLABS_API_KEY", elevenlabs_key),
    ]
    # A minted URL is itself a bearer credential for fifteen minutes, so every one of them is
    # scanned for exactly like a token.
    secrets += [("a minted signed URL", url) for url in runner.signed_urls]
    leaks = secrets_found(scanned, secrets)
    # Positive control, through the SAME scanner over the SAME text: "no credential found" means
    # nothing unless the scanner has just been shown finding one planted in it.
    if not secrets_found(scanned + "\n" + write_token, [("planted", write_token)]):
        out.emit("")
        out.emit(
            "FAILED [control_failed]: the credential scan is broken — it cannot find a value "
            "planted in the very text it scans, so its clean result means nothing."
        )
        return EXIT_CONTROL_FAILED
    if leaks:
        out.emit("")
        out.emit(f"FAILED [credential_leak]: {', '.join(leaks)} appeared in this script's output.")
        return EXIT_CREDENTIAL_LEAK
    out.emit("no credential value appears anywhere in this output (scanner positive-controlled).")

    out.emit("")
    out.emit(f"{label} — {summary}")
    return result


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

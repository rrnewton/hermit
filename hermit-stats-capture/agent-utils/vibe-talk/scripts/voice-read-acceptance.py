#!/usr/bin/env python3
"""Measure long spoken reads over the provider-neutral ``vibe-talk-v1`` protocol.

This is the repeatable live acceptance check for #47 read-aloud-overlong.  A run opens a fresh
WebSocket session for every trial, sends the caller's prompt byte-for-byte, and records only
content-free timing and size facts.  Live runs may consume provider time; ``--self-test`` is the
offline deterministic check used by the ordinary validation suite.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import re
import sys
import tempfile
import time
import wave
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Protocol, Sequence, TypeAlias, cast
from urllib.parse import urlsplit

JsonValue: TypeAlias = (
    str | int | float | bool | None | list["JsonValue"] | dict[str, "JsonValue"]
)
JsonObject: TypeAlias = dict[str, JsonValue]

SAMPLE_RATE = 24_000
SAMPLE_WIDTH = 2
CHANNELS = 1
BYTES_PER_SECOND = SAMPLE_RATE * SAMPLE_WIDTH * CHANNELS
DEFAULT_TRIALS = 10
DEFAULT_REANSWER_TRIALS = 2
DEFAULT_MAX_GAP_SECONDS = 15.0
DEFAULT_MAX_AUDIO_SECONDS_PER_CHAR = 0.09
DEFAULT_SESSION_TIMEOUT_SECONDS = 15.0
DEFAULT_FIRST_AUDIO_TIMEOUT_SECONDS = 15.0
DEFAULT_TURN_TIMEOUT_SECONDS = 300.0
DEFAULT_INTERRUPT_AFTER_AUDIO_SECONDS = 2.0
DEFAULT_MIN_FOLLOW_UP_COVERAGE = 0.85
DEFAULT_MAX_ORIGIN_COVERAGE = 0.15
DEFAULT_MIN_COVERAGE_MARGIN = 0.70
DEFAULT_ORIGIN_ONLY_SPAN_WORDS = 8
URL_ENV = "VIBE_TALK_VOICE_WEBSOCKET_URL"

EXIT_OK = 0
EXIT_USAGE = 2
EXIT_ACCEPTANCE = 10
EXIT_DEPENDENCY = 11
EXIT_SELF_TEST = 12


class AcceptanceFailure(Exception):
    """A content-free failure classification suitable for a saved report."""

    def __init__(self, code: str, detail: str) -> None:
        super().__init__(detail)
        self.code = code
        self.detail = detail


class Socket(Protocol):
    """The small part of a websockets client connection this runner uses."""

    async def recv(self) -> str | bytes:
        """Receive one data frame."""

    async def send(self, message: str | bytes) -> None:
        """Send one data frame."""

    async def close(self) -> None:
        """Close the connection."""


def utc_now() -> str:
    """An RFC 3339 UTC timestamp."""
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def tidy_speech(text: str) -> str:
    """Use the page's whitespace rule for one transcript hypothesis."""
    return " ".join(text.split())


def spoken_words(text: str) -> list[str]:
    """Use the page's whole-word approximation for hypothesis reconciliation."""
    cleaned = re.sub(r"[^\w\s']", " ", text.casefold(), flags=re.UNICODE).replace("_", " ")
    return cleaned.split()


def ordered_word_lcs_length(left: Sequence[str], right: Sequence[str]) -> int:
    """Length of the normalized ordered-word longest common subsequence."""
    row = [0] * (len(right) + 1)
    for left_word in left:
        next_row = [0]
        for right_index, right_word in enumerate(right, start=1):
            if left_word == right_word:
                next_row.append(row[right_index - 1] + 1)
            else:
                next_row.append(max(row[right_index], next_row[-1]))
        row = next_row
    return row[-1]


def ordered_word_coverage(source: Sequence[str], observed: Sequence[str]) -> tuple[int, float]:
    """Return source words recovered in order and their source-relative coverage."""
    if not source:
        raise ValueError("source words must not be empty")
    matched = ordered_word_lcs_length(source, observed)
    return matched, matched / len(source)


def origin_only_span_facts(
    origin: Sequence[str],
    follow_up: Sequence[str],
    observed: Sequence[str],
    width: int,
) -> tuple[int, bool]:
    """Count origin-only windows and detect whether one leaked contiguously."""
    if width < 1:
        raise ValueError("span width must be positive")
    follow_up_windows = {
        tuple(follow_up[start : start + width])
        for start in range(max(0, len(follow_up) - width + 1))
    }
    observed_windows = {
        tuple(observed[start : start + width])
        for start in range(max(0, len(observed) - width + 1))
    }
    candidates = [
        tuple(origin[start : start + width])
        for start in range(max(0, len(origin) - width + 1))
        if tuple(origin[start : start + width]) not in follow_up_windows
    ]
    return len(candidates), any(span in observed_windows for span in candidates)


def contains_words(haystack: Sequence[str], needle: Sequence[str]) -> bool:
    """Whether ``needle`` occurs contiguously in ``haystack``."""
    if not needle:
        return True
    return any(
        list(haystack[start : start + len(needle)]) == list(needle)
        for start in range(len(haystack) - len(needle) + 1)
    )


def revision_distance(before: Sequence[str], after: Sequence[str]) -> int:
    """Edit distance from all of ``before`` to the closest prefix of ``after``."""
    row = list(range(len(after) + 1))
    for before_index, before_word in enumerate(before, start=1):
        next_row = [before_index]
        for after_index, after_word in enumerate(after, start=1):
            next_row.append(
                min(
                    row[after_index] + 1,
                    next_row[after_index - 1] + 1,
                    row[after_index - 1] + (before_word != after_word),
                )
            )
        row = next_row
    return min(row)


def merge_hypothesis(held: str, frame: str) -> str:
    """Merge a final transcript frame exactly as the browser page does."""
    before = tidy_speech(held)
    after = tidy_speech(frame)
    if not before or not after:
        return before or after
    old_words = spoken_words(before)
    new_words = spoken_words(after)
    if contains_words(new_words, old_words):
        return after
    if contains_words(old_words, new_words):
        return before
    correction_limit = max(1, len(old_words) // 4)
    if (
        len(old_words) >= 3
        and len(new_words) >= 3
        and revision_distance(old_words, new_words) <= correction_limit
    ):
        return after
    return f"{before} {after}"


@dataclass
class TranscriptEntry:
    """The best text held for one protocol ``(turn, role)`` row."""

    role: str
    settled: str = ""
    pending: str = ""

    def text(self) -> str:
        """The row as it would currently appear in the page."""
        return merge_hypothesis(self.settled, self.pending) if self.pending else self.settled


@dataclass
class TranscriptBook:
    """Provider-streaming hypotheses reconciled into final protocol rows."""

    entries: dict[str, TranscriptEntry] = field(default_factory=dict)
    order: list[str] = field(default_factory=list)
    anonymous_turn: int = 0

    def role_texts(self, role: str) -> tuple[str, ...]:
        """Current rows for ``role``, in arrival order."""
        return tuple(
            self.entries[key].text()
            for key in self.order
            if self.entries[key].role == role and self.entries[key].text()
        )

    def role_characters(self, role: str) -> int:
        """Unicode characters supplied by the provider, excluding our row separators."""
        return sum(len(text) for text in self.role_texts(role))

    def apply(self, role: str, turn: int | None, text: str, is_final: bool) -> bool:
        """Apply a frame and say whether that role's visible transcript changed."""
        if not text:
            return False
        before = self.role_texts(role)
        anonymous = turn is None
        key = f"anon:{self.anonymous_turn}:{role}" if anonymous else f"turn:{turn}:{role}"
        entry = self.entries.get(key)
        if entry is None:
            entry = TranscriptEntry(role=role)
            self.entries[key] = entry
            self.order.append(key)
        if is_final:
            entry.settled = merge_hypothesis(entry.settled, text)
            entry.pending = ""
        else:
            entry.pending = text
        changed = self.role_texts(role) != before
        if anonymous and is_final:
            self.anonymous_turn += 1
        return changed


@dataclass
class TrialRecorder:
    """Content-free response facts and metric state for one prompt."""

    save_pcm: bool
    transcript: TranscriptBook = field(default_factory=TranscriptBook)
    events: list[JsonObject] = field(default_factory=list)
    audio_bytes: int = 0
    audio_frames: int = 0
    max_gap_seconds: float = 0.0
    max_wall_gap_seconds: float = 0.0
    completion_count: int = 0
    completion_at_seconds: float | None = None
    _gap_started_at: float | None = None
    _gap_pcm_seconds: float = 0.0
    _pcm: bytearray = field(default_factory=bytearray)

    def record_audio(self, data: bytes, at_seconds: float, at_utc: str) -> None:
        """Record one raw 24 kHz mono s16le frame and advance the gap metric."""
        if not data:
            raise AcceptanceFailure("invalid_pcm", "the provider sent an empty PCM frame")
        if len(data) % SAMPLE_WIDTH != 0:
            raise AcceptanceFailure(
                "invalid_pcm",
                "the provider sent an odd number of bytes for 16-bit PCM",
            )
        duration = len(data) / BYTES_PER_SECOND
        self.audio_bytes += len(data)
        self.audio_frames += 1
        if self.save_pcm:
            self._pcm.extend(data)
        if self._gap_started_at is None:
            self._gap_started_at = at_seconds
            self._gap_pcm_seconds = 0.0
        self._gap_pcm_seconds += duration
        # The acceptance metric is PCM duration accrued without transcript progress: it measures
        # how long audio actually flows and is independent of network batching. Preserve the wall
        # span too, because the timestamped diagnostic distinguishes an upstream stall from a
        # transport burst. A transcript CHANGE resets both; repeats and user recognition do not
        # pretend the assistant advanced.
        wall_span = max(0.0, at_seconds - self._gap_started_at) + duration
        self.max_gap_seconds = max(self.max_gap_seconds, self._gap_pcm_seconds)
        self.max_wall_gap_seconds = max(self.max_wall_gap_seconds, wall_span)
        self.events.append(
            {
                "kind": "audio",
                "at_seconds": round(at_seconds, 6),
                "at_utc": at_utc,
                "bytes": len(data),
                "audio_seconds": round(duration, 6),
            }
        )

    def record_transcript(
        self,
        role: str,
        text: str,
        turn: int | None,
        is_final: bool,
        at_seconds: float,
        at_utc: str,
    ) -> None:
        """Timestamp a transcript change without saving the words."""
        changed = self.transcript.apply(role, turn, text, is_final)
        if not changed:
            return
        if role == "assistant":
            self._gap_started_at = None
            self._gap_pcm_seconds = 0.0
        self.events.append(
            {
                "kind": "transcript_change",
                "at_seconds": round(at_seconds, 6),
                "at_utc": at_utc,
                "role": role,
                "turn": turn,
                "final": is_final,
                "characters": self.transcript.role_characters(role),
            }
        )

    def record_completion(
        self,
        turn: int | None,
        interrupted: bool,
        at_seconds: float,
        at_utc: str,
    ) -> None:
        """Record the response boundary."""
        self.completion_count += 1
        self.completion_at_seconds = at_seconds
        self.events.append(
            {
                "kind": "turn_complete",
                "at_seconds": round(at_seconds, 6),
                "at_utc": at_utc,
                "turn": turn,
                "interrupted": interrupted,
            }
        )

    @property
    def audio_seconds(self) -> float:
        """Duration encoded by all received PCM."""
        return self.audio_bytes / BYTES_PER_SECOND

    @property
    def assistant_characters(self) -> int:
        """Characters in the reconciled assistant transcript at completion."""
        return self.transcript.role_characters("assistant")

    @property
    def audio_seconds_per_character(self) -> float | None:
        """Encoded audio duration divided by final assistant characters."""
        if self.assistant_characters == 0:
            return None
        return self.audio_seconds / self.assistant_characters

    def write_wav(self, path: Path) -> None:
        """Write the captured PCM as a standard WAV for optional ASR replay."""
        with wave.open(str(path), "wb") as output:
            output.setnchannels(CHANNELS)
            output.setsampwidth(SAMPLE_WIDTH)
            output.setframerate(SAMPLE_RATE)
            output.writeframes(self._pcm)


@dataclass(frozen=True)
class Thresholds:
    """The two quantitative acceptance bounds."""

    max_gap_seconds: float
    max_audio_seconds_per_character: float


@dataclass(frozen=True)
class ReanswerThresholds:
    """The content-separation bounds for an interrupted read and its follow-up."""

    interrupt_after_audio_seconds: float
    min_follow_up_coverage: float
    max_origin_coverage: float
    min_coverage_margin: float
    origin_only_span_words: int


@dataclass(frozen=True)
class ReanswerComparison:
    """Content-free ordered-word evidence retained from an in-memory comparison."""

    observed_words: int
    origin_words_matched: int
    follow_up_words_matched: int
    origin_coverage: float
    follow_up_coverage: float
    coverage_margin: float
    origin_only_span_candidates: int
    origin_only_span_found: bool


def compare_reanswer(
    origin_words: Sequence[str],
    follow_up_words: Sequence[str],
    observed_text: str,
    span_words: int,
) -> ReanswerComparison:
    """Compare a follow-up transcript in memory and return only numeric evidence."""
    observed_words = spoken_words(observed_text)
    origin_matched, origin_coverage = ordered_word_coverage(origin_words, observed_words)
    follow_up_matched, follow_up_coverage = ordered_word_coverage(
        follow_up_words, observed_words
    )
    candidate_count, span_found = origin_only_span_facts(
        origin_words,
        follow_up_words,
        observed_words,
        span_words,
    )
    return ReanswerComparison(
        observed_words=len(observed_words),
        origin_words_matched=origin_matched,
        follow_up_words_matched=follow_up_matched,
        origin_coverage=origin_coverage,
        follow_up_coverage=follow_up_coverage,
        coverage_margin=follow_up_coverage - origin_coverage,
        origin_only_span_candidates=candidate_count,
        origin_only_span_found=span_found,
    )


@dataclass
class TrialOutcome:
    """One saved trial result."""

    index: int
    started_at: str
    finished_at: str
    elapsed_seconds: float
    greeting: bool | None
    completed: bool
    interrupted: bool
    completion_turn: int | None
    recorder: TrialRecorder
    thresholds: Thresholds
    failure_code: str | None = None
    failure_detail: str | None = None
    wav_file: str | None = None

    def checks(self) -> dict[str, bool]:
        """Every condition required for this trial to pass."""
        ratio = self.recorder.audio_seconds_per_character
        return {
            "completed": self.completed,
            "not_interrupted": not self.interrupted,
            "audio_present": self.recorder.audio_bytes > 0,
            "assistant_transcript_present": self.recorder.assistant_characters > 0,
            "audio_without_transcript_gap": (
                self.recorder.max_gap_seconds <= self.thresholds.max_gap_seconds
            ),
            "audio_seconds_per_transcript_character": (
                ratio is not None
                and ratio <= self.thresholds.max_audio_seconds_per_character
            ),
            "no_protocol_or_transport_failure": self.failure_code is None,
        }

    @property
    def passed(self) -> bool:
        """Whether all acceptance conditions passed."""
        return all(self.checks().values())

    def to_json(self) -> JsonObject:
        """Machine-readable, content-free representation."""
        ratio = self.recorder.audio_seconds_per_character
        checks: JsonObject = {name: value for name, value in self.checks().items()}
        events: list[JsonValue] = [event for event in self.recorder.events]
        return {
            "trial": self.index,
            "status": "pass" if self.passed else "fail",
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "elapsed_seconds": round(self.elapsed_seconds, 6),
            "session_greeting": self.greeting,
            "completed": self.completed,
            "interrupted": self.interrupted,
            "completion_turn": self.completion_turn,
            "audio_bytes": self.recorder.audio_bytes,
            "audio_frames": self.recorder.audio_frames,
            "audio_seconds": round(self.recorder.audio_seconds, 6),
            "assistant_transcript_characters": self.recorder.assistant_characters,
            "max_audio_without_transcript_seconds": round(
                self.recorder.max_gap_seconds, 6
            ),
            "max_wall_audio_without_transcript_seconds": round(
                self.recorder.max_wall_gap_seconds, 6
            ),
            "audio_seconds_per_transcript_character": (
                round(ratio, 9) if ratio is not None else None
            ),
            "checks": checks,
            "failure_code": self.failure_code,
            "failure_detail": self.failure_detail,
            "wav_file": self.wav_file,
            "events": events,
        }


@dataclass
class ReanswerTrialOutcome:
    """One two-turn result whose durable form contains no private text."""

    index: int
    started_at: str
    finished_at: str
    elapsed_seconds: float
    greeting: bool | None
    origin_recorder: TrialRecorder
    follow_up_recorder: TrialRecorder
    origin_source_words: int
    follow_up_source_words: int
    thresholds: ReanswerThresholds
    interrupt_count: int = 0
    interrupt_at_seconds: float | None = None
    interrupt_audio_seconds: float | None = None
    origin_completed: bool = False
    origin_interrupted: bool = False
    origin_turn: int | None = None
    follow_up_completed: bool = False
    follow_up_interrupted: bool = False
    follow_up_turn: int | None = None
    comparison: ReanswerComparison | None = None
    failure_code: str | None = None
    failure_detail: str | None = None

    @property
    def origin_completion_count(self) -> int:
        """Observed completions attributable to the interrupted origin turn."""
        duplicate = (
            self.origin_turn is not None
            and self.follow_up_completed
            and self.follow_up_turn == self.origin_turn
        )
        return self.origin_recorder.completion_count + int(duplicate)

    @property
    def follow_up_completion_count(self) -> int:
        """Observed completions attributable to a later turn."""
        return int(
            self.follow_up_completed
            and self.follow_up_turn is not None
            and self.follow_up_turn != self.origin_turn
        )

    def checks(self) -> dict[str, bool]:
        """Every protocol and separation requirement for a re-answer trial."""
        comparison = self.comparison
        assistant_transcript_events = [
            event
            for event in self.follow_up_recorder.events
            if event.get("kind") == "transcript_change"
            and event.get("role") == "assistant"
        ]
        return {
            "interrupt_sent_once": self.interrupt_count == 1,
            "origin_completed_once": self.origin_completion_count == 1,
            "origin_completion_interrupted": (
                self.origin_completed and self.origin_interrupted
            ),
            "follow_up_completed_once": self.follow_up_completion_count == 1,
            "follow_up_completion_natural": (
                self.follow_up_completed and not self.follow_up_interrupted
            ),
            "turn_ids_sequential": (
                self.origin_turn is not None
                and self.follow_up_turn is not None
                and self.follow_up_turn == self.origin_turn + 1
            ),
            "follow_up_audio_present": self.follow_up_recorder.audio_bytes > 0,
            "follow_up_transcript_present": (
                comparison is not None and comparison.observed_words > 0
            ),
            "follow_up_transcript_turn": (
                self.follow_up_turn is not None
                and bool(assistant_transcript_events)
                and all(
                    event.get("turn") == self.follow_up_turn
                    for event in assistant_transcript_events
                )
            ),
            "follow_up_source_coverage": (
                comparison is not None
                and comparison.follow_up_coverage
                >= self.thresholds.min_follow_up_coverage
            ),
            "origin_source_coverage": (
                comparison is not None
                and comparison.origin_coverage < self.thresholds.max_origin_coverage
            ),
            "coverage_margin": (
                comparison is not None
                and comparison.coverage_margin >= self.thresholds.min_coverage_margin
            ),
            "no_origin_only_contiguous_span": (
                comparison is not None and not comparison.origin_only_span_found
            ),
            "no_protocol_or_transport_failure": self.failure_code is None,
        }

    @property
    def passed(self) -> bool:
        """Whether the interrupted turn stayed out of the natural follow-up."""
        return all(self.checks().values())

    def to_json(self) -> JsonObject:
        """Serialize counts, ratios, timings, turn IDs, and verdict booleans only."""
        comparison = self.comparison
        checks: JsonObject = {name: value for name, value in self.checks().items()}
        return {
            "trial": self.index,
            "passed": self.passed,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "elapsed_seconds": round(self.elapsed_seconds, 6),
            "session_greeting": self.greeting,
            "interrupt_count": self.interrupt_count,
            "interrupt_at_seconds": (
                round(self.interrupt_at_seconds, 6)
                if self.interrupt_at_seconds is not None
                else None
            ),
            "interrupt_audio_seconds": (
                round(self.interrupt_audio_seconds, 6)
                if self.interrupt_audio_seconds is not None
                else None
            ),
            "origin_turn_complete_count": self.origin_completion_count,
            "origin_completed": self.origin_completed,
            "origin_interrupted": self.origin_interrupted,
            "origin_turn": self.origin_turn,
            "origin_turn_complete_at_seconds": (
                round(self.origin_recorder.completion_at_seconds, 6)
                if self.origin_recorder.completion_at_seconds is not None
                else None
            ),
            "origin_audio_bytes": self.origin_recorder.audio_bytes,
            "origin_audio_frames": self.origin_recorder.audio_frames,
            "origin_audio_seconds": round(self.origin_recorder.audio_seconds, 6),
            "follow_up_turn_complete_count": self.follow_up_completion_count,
            "follow_up_completed": self.follow_up_completed,
            "follow_up_interrupted": self.follow_up_interrupted,
            "follow_up_turn": self.follow_up_turn,
            "follow_up_turn_complete_at_seconds": (
                round(self.follow_up_recorder.completion_at_seconds, 6)
                if self.follow_up_recorder.completion_at_seconds is not None
                else None
            ),
            "follow_up_audio_bytes": self.follow_up_recorder.audio_bytes,
            "follow_up_audio_frames": self.follow_up_recorder.audio_frames,
            "follow_up_audio_seconds": round(
                self.follow_up_recorder.audio_seconds, 6
            ),
            "origin_source_words": self.origin_source_words,
            "follow_up_source_words": self.follow_up_source_words,
            "follow_up_transcript_characters": (
                self.follow_up_recorder.assistant_characters
            ),
            "follow_up_transcript_words": (
                comparison.observed_words if comparison is not None else 0
            ),
            "origin_words_matched": (
                comparison.origin_words_matched if comparison is not None else 0
            ),
            "follow_up_words_matched": (
                comparison.follow_up_words_matched if comparison is not None else 0
            ),
            "origin_coverage": (
                round(comparison.origin_coverage, 9)
                if comparison is not None
                else None
            ),
            "follow_up_coverage": (
                round(comparison.follow_up_coverage, 9)
                if comparison is not None
                else None
            ),
            "coverage_margin": (
                round(comparison.coverage_margin, 9)
                if comparison is not None
                else None
            ),
            "origin_only_span_candidates": (
                comparison.origin_only_span_candidates
                if comparison is not None
                else 0
            ),
            "origin_only_span_found": (
                comparison.origin_only_span_found
                if comparison is not None
                else False
            ),
            "checks": checks,
        }


def json_object(raw: str) -> JsonObject:
    """Parse one protocol control frame as an object."""
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise AcceptanceFailure("invalid_json", "the provider sent invalid JSON") from error
    if not isinstance(value, dict) or not all(isinstance(key, str) for key in value):
        raise AcceptanceFailure("invalid_control_frame", "a control frame was not a JSON object")
    return cast(JsonObject, value)


def string_field(message: JsonObject, name: str) -> str | None:
    """A string field, rejecting incompatible known-frame shapes."""
    value = message.get(name)
    if value is None:
        return None
    if not isinstance(value, str):
        raise AcceptanceFailure("invalid_control_frame", f"{name} was not a string")
    return value


def bool_field(message: JsonObject, name: str, default: bool) -> bool:
    """A boolean field with a protocol default."""
    value = message.get(name)
    if value is None:
        return default
    if not isinstance(value, bool):
        raise AcceptanceFailure("invalid_control_frame", f"{name} was not a boolean")
    return value


def turn_field(message: JsonObject) -> int | None:
    """The optional non-negative protocol turn number."""
    value = message.get("turn")
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise AcceptanceFailure("invalid_control_frame", "turn was not a non-negative integer")
    return value


async def receive_before(socket: Socket, deadline: float, phase: str) -> str | bytes:
    """Receive one frame before an absolute monotonic deadline."""
    timeout_code = f"{phase.replace(' ', '_')}_timeout"
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise AcceptanceFailure(timeout_code, f"timed out while waiting for {phase}")
    try:
        return await asyncio.wait_for(socket.recv(), timeout=remaining)
    except asyncio.TimeoutError as error:
        raise AcceptanceFailure(timeout_code, f"timed out while waiting for {phase}") from error
    except Exception as error:  # noqa: BLE001 - transports expose several version-specific errors
        raise AcceptanceFailure(
            "socket_closed",
            f"the socket ended while waiting for {phase} ({type(error).__name__})",
        ) from error


async def wait_for_session(socket: Socket, timeout_seconds: float) -> bool:
    """Wait for the required first protocol control frame."""
    deadline = time.monotonic() + timeout_seconds
    while True:
        raw = await receive_before(socket, deadline, "session_started")
        if isinstance(raw, bytes):
            raise AcceptanceFailure(
                "audio_before_session",
                "the provider sent audio before session_started",
            )
        message = json_object(raw)
        kind = string_field(message, "type")
        if kind == "session_started":
            return bool_field(message, "greeting", False)
        if kind == "error":
            raise AcceptanceFailure("provider_error", "the provider rejected session startup")
        # Unknown frame types are forward-compatible by contract and therefore ignored.


async def wait_for_boundary(socket: Socket, timeout_seconds: float, phase: str) -> None:
    """Drain one unmeasured turn through its non-interrupted completion."""
    deadline = time.monotonic() + timeout_seconds
    while True:
        raw = await receive_before(socket, deadline, phase)
        if isinstance(raw, bytes):
            continue
        message = json_object(raw)
        kind = string_field(message, "type")
        if kind == "error":
            raise AcceptanceFailure("provider_error", f"the provider failed during {phase}")
        if kind == "turn_complete":
            if bool_field(message, "interrupted", False):
                raise AcceptanceFailure("interrupted_boundary", f"{phase} was interrupted")
            return


async def prepare_session(socket: Socket, timeout_seconds: float) -> bool:
    """Start a session and isolate any promised greeting from the measured prompt.

    The protocol says a voice client begins with ``audio_start`` and withholds microphone frames
    during a greeting. Once that greeting completes, close the empty microphone segment and drain
    its own required completion before typing. Therefore the first boundary after the prompt can
    only belong to the prompt.
    """
    greeting = await wait_for_session(socket, timeout_seconds)
    if not greeting:
        return False
    await socket.send(json.dumps({"type": "audio_start"}))
    await wait_for_boundary(socket, timeout_seconds, "greeting completion")
    await socket.send(json.dumps({"type": "audio_end"}))
    await wait_for_boundary(socket, timeout_seconds, "audio segment closure")
    return True


async def measure_prompt(
    socket: Socket,
    prompt: str,
    recorder: TrialRecorder,
    first_audio_timeout_seconds: float,
    timeout_seconds: float,
) -> tuple[bool, bool, int | None]:
    """Send ``prompt`` byte-for-byte and collect through exactly one response boundary."""
    await socket.send(json.dumps({"type": "prompt", "text": prompt}))
    prompt_started = time.monotonic()
    first_audio_deadline = prompt_started + first_audio_timeout_seconds
    turn_deadline = prompt_started + timeout_seconds
    while True:
        waiting_for_audio = recorder.audio_frames == 0
        deadline = min(first_audio_deadline, turn_deadline) if waiting_for_audio else turn_deadline
        phase = "first audio" if waiting_for_audio else "turn completion"
        raw = await receive_before(socket, deadline, phase)
        received = time.monotonic()
        at_seconds = received - prompt_started
        at_utc = utc_now()
        if isinstance(raw, bytes):
            recorder.record_audio(raw, at_seconds, at_utc)
            continue
        message = json_object(raw)
        kind = string_field(message, "type")
        if kind == "transcript":
            role = string_field(message, "role")
            text = string_field(message, "text")
            if role not in {"user", "assistant"} or text is None:
                raise AcceptanceFailure(
                    "invalid_transcript",
                    "a transcript frame lacked a supported role or text",
                )
            recorder.record_transcript(
                role=role,
                text=text,
                turn=turn_field(message),
                is_final=bool_field(message, "final", True),
                at_seconds=at_seconds,
                at_utc=at_utc,
            )
        elif kind == "turn_complete":
            interrupted = bool_field(message, "interrupted", False)
            completion_turn = turn_field(message)
            recorder.record_completion(
                completion_turn,
                interrupted,
                at_seconds,
                at_utc,
            )
            return True, interrupted, completion_turn
        elif kind == "error":
            raise AcceptanceFailure("provider_error", "the provider returned an error frame")
        # Unknown frame types remain forward-compatible and do not affect the metrics.


@dataclass(frozen=True)
class InterruptedPromptResult:
    """The boundary and interrupt facts from the first re-answer turn."""

    completed: bool
    interrupted: bool
    completion_turn: int | None
    interrupt_count: int
    interrupt_at_seconds: float | None
    interrupt_audio_seconds: float | None


async def measure_interrupted_prompt(
    socket: Socket,
    prompt: str,
    recorder: TrialRecorder,
    interrupt_after_audio_seconds: float,
    first_audio_timeout_seconds: float,
    timeout_seconds: float,
) -> InterruptedPromptResult:
    """Interrupt one prompt exactly once after the requested amount of received PCM."""
    await socket.send(json.dumps({"type": "prompt", "text": prompt}))
    prompt_started = time.monotonic()
    first_audio_deadline = prompt_started + first_audio_timeout_seconds
    turn_deadline = prompt_started + timeout_seconds
    interrupt_count = 0
    interrupt_at_seconds: float | None = None
    interrupt_audio_seconds: float | None = None
    while True:
        waiting_for_audio = recorder.audio_frames == 0
        deadline = min(first_audio_deadline, turn_deadline) if waiting_for_audio else turn_deadline
        phase = "first audio" if waiting_for_audio else "interrupted turn completion"
        raw = await receive_before(socket, deadline, phase)
        received = time.monotonic()
        at_seconds = received - prompt_started
        at_utc = utc_now()
        if isinstance(raw, bytes):
            recorder.record_audio(raw, at_seconds, at_utc)
            if (
                interrupt_count == 0
                and recorder.audio_seconds >= interrupt_after_audio_seconds
            ):
                await socket.send(json.dumps({"type": "interrupt"}))
                interrupt_count = 1
                interrupt_at_seconds = time.monotonic() - prompt_started
                interrupt_audio_seconds = recorder.audio_seconds
            continue
        message = json_object(raw)
        kind = string_field(message, "type")
        if kind == "transcript":
            role = string_field(message, "role")
            text = string_field(message, "text")
            if role not in {"user", "assistant"} or text is None:
                raise AcceptanceFailure(
                    "invalid_transcript",
                    "a transcript frame lacked a supported role or text",
                )
            recorder.record_transcript(
                role=role,
                text=text,
                turn=turn_field(message),
                is_final=bool_field(message, "final", True),
                at_seconds=at_seconds,
                at_utc=at_utc,
            )
        elif kind == "turn_complete":
            interrupted = bool_field(message, "interrupted", False)
            completion_turn = turn_field(message)
            recorder.record_completion(
                completion_turn,
                interrupted,
                at_seconds,
                at_utc,
            )
            return InterruptedPromptResult(
                completed=True,
                interrupted=interrupted,
                completion_turn=completion_turn,
                interrupt_count=interrupt_count,
                interrupt_at_seconds=interrupt_at_seconds,
                interrupt_audio_seconds=interrupt_audio_seconds,
            )
        elif kind == "error":
            raise AcceptanceFailure("provider_error", "the provider returned an error frame")
        # Unknown frame types remain forward-compatible and do not affect the metrics.


async def close_socket(socket: Socket) -> None:
    """Best-effort bounded protocol and WebSocket shutdown."""
    try:
        await asyncio.wait_for(socket.send(json.dumps({"type": "quit"})), timeout=1.0)
    except Exception:  # noqa: BLE001 - the measured result already exists
        pass
    try:
        await asyncio.wait_for(socket.close(), timeout=2.0)
    except Exception:  # noqa: BLE001 - shutdown cannot change the measured result
        pass


async def run_trial(
    index: int,
    url: str,
    prompt: str,
    thresholds: Thresholds,
    session_timeout_seconds: float,
    first_audio_timeout_seconds: float,
    turn_timeout_seconds: float,
    save_wav: bool,
    output_dir: Path,
) -> TrialOutcome:
    """Open one fresh session, send one exact prompt, and measure its response."""
    recorder = TrialRecorder(save_pcm=save_wav)
    started_at = utc_now()
    trial_started = time.monotonic()
    socket: Socket | None = None
    greeting: bool | None = None
    completed = False
    interrupted = False
    completion_turn: int | None = None
    failure_code: str | None = None
    failure_detail: str | None = None
    wav_file: str | None = None
    try:
        try:
            import websockets
        except ImportError as error:
            raise AcceptanceFailure(
                "missing_dependency",
                "the Python websockets package is not installed",
            ) from error
        try:
            opened = await websockets.connect(
                url,
                max_size=None,
                open_timeout=session_timeout_seconds,
                close_timeout=2,
            )
            socket = cast(Socket, opened)
        except Exception as error:  # noqa: BLE001 - connection errors vary across library releases
            raise AcceptanceFailure(
                "connect_failed",
                f"the WebSocket could not be opened ({type(error).__name__})",
            ) from error

        greeting = await prepare_session(socket, session_timeout_seconds)
        completed, interrupted, completion_turn = await measure_prompt(
            socket,
            prompt,
            recorder,
            first_audio_timeout_seconds,
            turn_timeout_seconds,
        )
        if interrupted:
            failure_code = "interrupted"
            failure_detail = "the prompt completed as interrupted"
    except AcceptanceFailure as error:
        failure_code = error.code
        failure_detail = error.detail
    except Exception as error:  # noqa: BLE001 - preserve partial evidence on every unexpected path
        failure_code = "runner_error"
        failure_detail = f"the runner failed ({type(error).__name__})"
    finally:
        if socket is not None:
            await close_socket(socket)

    if save_wav and recorder.audio_bytes:
        wav_name = f"trial-{index:02d}.wav"
        recorder.write_wav(output_dir / wav_name)
        wav_file = wav_name
    return TrialOutcome(
        index=index,
        started_at=started_at,
        finished_at=utc_now(),
        elapsed_seconds=time.monotonic() - trial_started,
        greeting=greeting,
        completed=completed,
        interrupted=interrupted,
        completion_turn=completion_turn,
        recorder=recorder,
        thresholds=thresholds,
        failure_code=failure_code,
        failure_detail=failure_detail,
        wav_file=wav_file,
    )


async def run_reanswer_trial(
    index: int,
    url: str,
    origin_prompt: str,
    follow_up_prompt: str,
    origin_words: Sequence[str],
    follow_up_words: Sequence[str],
    thresholds: ReanswerThresholds,
    session_timeout_seconds: float,
    first_audio_timeout_seconds: float,
    turn_timeout_seconds: float,
) -> ReanswerTrialOutcome:
    """Run an interrupted origin read and a natural follow-up on one fresh socket."""
    origin_recorder = TrialRecorder(save_pcm=False)
    follow_up_recorder = TrialRecorder(save_pcm=False)
    started_at = utc_now()
    trial_started = time.monotonic()
    socket: Socket | None = None
    greeting: bool | None = None
    interrupted_result: InterruptedPromptResult | None = None
    follow_up_completed = False
    follow_up_interrupted = False
    follow_up_turn: int | None = None
    comparison: ReanswerComparison | None = None
    failure_code: str | None = None
    failure_detail: str | None = None
    try:
        try:
            import websockets
        except ImportError as error:
            raise AcceptanceFailure(
                "missing_dependency",
                "the Python websockets package is not installed",
            ) from error
        try:
            opened = await websockets.connect(
                url,
                max_size=None,
                open_timeout=session_timeout_seconds,
                close_timeout=2,
            )
            socket = cast(Socket, opened)
        except Exception as error:  # noqa: BLE001 - connection errors vary by release
            raise AcceptanceFailure(
                "connect_failed",
                f"the WebSocket could not be opened ({type(error).__name__})",
            ) from error

        greeting = await prepare_session(socket, session_timeout_seconds)
        interrupted_result = await measure_interrupted_prompt(
            socket,
            origin_prompt,
            origin_recorder,
            thresholds.interrupt_after_audio_seconds,
            first_audio_timeout_seconds,
            turn_timeout_seconds,
        )
        if interrupted_result.interrupt_count != 1:
            raise AcceptanceFailure(
                "origin_completed_before_interrupt",
                "the origin turn completed before the PCM interrupt threshold",
            )
        if not interrupted_result.interrupted:
            raise AcceptanceFailure(
                "origin_not_interrupted",
                "the origin completion did not acknowledge interruption",
            )
        if interrupted_result.completion_turn is None:
            raise AcceptanceFailure(
                "missing_origin_turn",
                "the interrupted origin completion had no turn ID",
            )

        follow_up_completed, follow_up_interrupted, follow_up_turn = await measure_prompt(
            socket,
            follow_up_prompt,
            follow_up_recorder,
            first_audio_timeout_seconds,
            turn_timeout_seconds,
        )
        observed_text = " ".join(
            follow_up_recorder.transcript.role_texts("assistant")
        )
        comparison = compare_reanswer(
            origin_words,
            follow_up_words,
            observed_text,
            thresholds.origin_only_span_words,
        )
        if follow_up_interrupted:
            failure_code = "follow_up_interrupted"
            failure_detail = "the follow-up completion was interrupted"
        elif follow_up_turn is None:
            failure_code = "missing_follow_up_turn"
            failure_detail = "the follow-up completion had no turn ID"
        elif follow_up_turn != interrupted_result.completion_turn + 1:
            failure_code = "unexpected_follow_up_turn"
            failure_detail = "the follow-up did not complete as the next turn"
    except AcceptanceFailure as error:
        failure_code = error.code
        failure_detail = error.detail
    except Exception as error:  # noqa: BLE001 - preserve partial numeric evidence
        failure_code = "runner_error"
        failure_detail = f"the runner failed ({type(error).__name__})"
    finally:
        if socket is not None:
            await close_socket(socket)

    return ReanswerTrialOutcome(
        index=index,
        started_at=started_at,
        finished_at=utc_now(),
        elapsed_seconds=time.monotonic() - trial_started,
        greeting=greeting,
        origin_recorder=origin_recorder,
        follow_up_recorder=follow_up_recorder,
        origin_source_words=len(origin_words),
        follow_up_source_words=len(follow_up_words),
        interrupt_count=(
            interrupted_result.interrupt_count
            if interrupted_result is not None
            else 0
        ),
        interrupt_at_seconds=(
            interrupted_result.interrupt_at_seconds
            if interrupted_result is not None
            else None
        ),
        interrupt_audio_seconds=(
            interrupted_result.interrupt_audio_seconds
            if interrupted_result is not None
            else None
        ),
        origin_completed=(
            interrupted_result.completed if interrupted_result is not None else False
        ),
        origin_interrupted=(
            interrupted_result.interrupted if interrupted_result is not None else False
        ),
        origin_turn=(
            interrupted_result.completion_turn
            if interrupted_result is not None
            else None
        ),
        follow_up_completed=follow_up_completed,
        follow_up_interrupted=follow_up_interrupted,
        follow_up_turn=follow_up_turn,
        comparison=comparison,
        thresholds=thresholds,
        failure_code=failure_code,
        failure_detail=failure_detail,
    )


def summary(outcomes: Sequence[TrialOutcome]) -> JsonObject:
    """Aggregate only observed trials, including a useful running report."""
    gaps = [outcome.recorder.max_gap_seconds for outcome in outcomes]
    wall_gaps = [outcome.recorder.max_wall_gap_seconds for outcome in outcomes]
    ratios = [
        ratio
        for outcome in outcomes
        if (ratio := outcome.recorder.audio_seconds_per_character) is not None
    ]
    return {
        "trials_finished": len(outcomes),
        "trials_passed": sum(outcome.passed for outcome in outcomes),
        "trials_failed": sum(not outcome.passed for outcome in outcomes),
        "max_audio_without_transcript_seconds": round(max(gaps), 6) if gaps else None,
        "max_wall_audio_without_transcript_seconds": (
            round(max(wall_gaps), 6) if wall_gaps else None
        ),
        "max_audio_seconds_per_transcript_character": (
            round(max(ratios), 9) if ratios else None
        ),
        "all_passed": bool(outcomes) and all(outcome.passed for outcome in outcomes),
    }


def build_report(
    status: str,
    run_started_at: str,
    finished_at: str | None,
    prompt: str,
    trial_count: int,
    thresholds: Thresholds,
    session_timeout_seconds: float,
    first_audio_timeout_seconds: float,
    turn_timeout_seconds: float,
    save_wav: bool,
    outcomes: Sequence[TrialOutcome],
) -> JsonObject:
    """Build the durable report without recording the prompt or transcript text."""
    return {
        "schema_version": 1,
        "status": status,
        "started_at": run_started_at,
        "finished_at": finished_at,
        "configuration": {
            "trials": trial_count,
            "prompt_characters": len(prompt),
            "prompt_utf8_bytes": len(prompt.encode("utf-8")),
            "pcm_sample_rate_hz": SAMPLE_RATE,
            "pcm_channels": CHANNELS,
            "pcm_sample_width_bits": SAMPLE_WIDTH * 8,
            "max_audio_without_transcript_seconds": thresholds.max_gap_seconds,
            "max_audio_seconds_per_transcript_character": (
                thresholds.max_audio_seconds_per_character
            ),
            "session_timeout_seconds": session_timeout_seconds,
            "first_audio_timeout_seconds": first_audio_timeout_seconds,
            "turn_timeout_seconds": turn_timeout_seconds,
            "wav_saved": save_wav,
        },
        "summary": summary(outcomes),
        "trials": [outcome.to_json() for outcome in outcomes],
    }


def write_report(path: Path, report: JsonObject) -> None:
    """Replace the report atomically so interruption leaves the last complete trial visible."""
    temporary = path.with_suffix(".json.tmp")
    temporary.write_text(
        json.dumps(report, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    os.replace(temporary, path)


async def run_acceptance(
    url: str,
    prompt: str,
    trial_count: int,
    thresholds: Thresholds,
    session_timeout_seconds: float,
    first_audio_timeout_seconds: float,
    turn_timeout_seconds: float,
    save_wav: bool,
    output_dir: Path,
) -> tuple[list[TrialOutcome], Path]:
    """Run every requested fresh-session trial, preserving the report after each one."""
    output_dir.mkdir(parents=True, exist_ok=False)
    report_path = output_dir / "results.json"
    run_started_at = utc_now()
    outcomes: list[TrialOutcome] = []
    write_report(
        report_path,
        build_report(
            "running",
            run_started_at,
            None,
            prompt,
            trial_count,
            thresholds,
            session_timeout_seconds,
            first_audio_timeout_seconds,
            turn_timeout_seconds,
            save_wav,
            outcomes,
        ),
    )
    for index in range(1, trial_count + 1):
        outcome = await run_trial(
            index,
            url,
            prompt,
            thresholds,
            session_timeout_seconds,
            first_audio_timeout_seconds,
            turn_timeout_seconds,
            save_wav,
            output_dir,
        )
        outcomes.append(outcome)
        ratio = outcome.recorder.audio_seconds_per_character
        ratio_text = "n/a" if ratio is None else f"{ratio:.4f}"
        print(
            f"trial {index}/{trial_count}: {'PASS' if outcome.passed else 'FAIL'}; "
            f"complete={outcome.completed}; interrupted={outcome.interrupted}; "
            f"audio={outcome.recorder.audio_seconds:.2f}s; "
            f"transcript={outcome.recorder.assistant_characters} chars; "
            f"max-gap={outcome.recorder.max_gap_seconds:.2f}s; ratio={ratio_text}",
            flush=True,
        )
        write_report(
            report_path,
            build_report(
                "running",
                run_started_at,
                None,
                prompt,
                trial_count,
                thresholds,
                session_timeout_seconds,
                first_audio_timeout_seconds,
                turn_timeout_seconds,
                save_wav,
                outcomes,
            ),
        )
    status = "pass" if all(outcome.passed for outcome in outcomes) else "fail"
    write_report(
        report_path,
        build_report(
            status,
            run_started_at,
            utc_now(),
            prompt,
            trial_count,
            thresholds,
            session_timeout_seconds,
            first_audio_timeout_seconds,
            turn_timeout_seconds,
            save_wav,
            outcomes,
        ),
    )
    return outcomes, report_path


def reanswer_summary(outcomes: Sequence[ReanswerTrialOutcome]) -> JsonObject:
    """Aggregate only counts, ratios, and pass booleans for completed trials."""
    origin_coverages = [
        outcome.comparison.origin_coverage
        for outcome in outcomes
        if outcome.comparison is not None
    ]
    follow_up_coverages = [
        outcome.comparison.follow_up_coverage
        for outcome in outcomes
        if outcome.comparison is not None
    ]
    margins = [
        outcome.comparison.coverage_margin
        for outcome in outcomes
        if outcome.comparison is not None
    ]
    return {
        "trials_finished": len(outcomes),
        "trials_passed": sum(outcome.passed for outcome in outcomes),
        "trials_failed": sum(not outcome.passed for outcome in outcomes),
        "max_origin_coverage": (
            round(max(origin_coverages), 9) if origin_coverages else None
        ),
        "min_follow_up_coverage": (
            round(min(follow_up_coverages), 9) if follow_up_coverages else None
        ),
        "min_coverage_margin": round(min(margins), 9) if margins else None,
        "all_passed": bool(outcomes) and all(outcome.passed for outcome in outcomes),
    }


def build_reanswer_report(
    finished: bool,
    run_started_at: str,
    finished_at: str | None,
    origin_prompt: str,
    follow_up_prompt: str,
    origin_source: str,
    follow_up_source: str,
    trial_count: int,
    thresholds: ReanswerThresholds,
    session_timeout_seconds: float,
    first_audio_timeout_seconds: float,
    turn_timeout_seconds: float,
    outcomes: Sequence[ReanswerTrialOutcome],
) -> JsonObject:
    """Build a re-answer report containing no input or transcript material."""
    return {
        "schema_version": 2,
        "finished": finished,
        "started_at": run_started_at,
        "finished_at": finished_at,
        "configuration": {
            "trials": trial_count,
            "origin_prompt_characters": len(origin_prompt),
            "origin_prompt_utf8_bytes": len(origin_prompt.encode("utf-8")),
            "follow_up_prompt_characters": len(follow_up_prompt),
            "follow_up_prompt_utf8_bytes": len(follow_up_prompt.encode("utf-8")),
            "origin_source_characters": len(origin_source),
            "origin_source_utf8_bytes": len(origin_source.encode("utf-8")),
            "origin_source_words": len(spoken_words(origin_source)),
            "follow_up_source_characters": len(follow_up_source),
            "follow_up_source_utf8_bytes": len(follow_up_source.encode("utf-8")),
            "follow_up_source_words": len(spoken_words(follow_up_source)),
            "interrupt_after_audio_seconds": (
                thresholds.interrupt_after_audio_seconds
            ),
            "min_follow_up_coverage": thresholds.min_follow_up_coverage,
            "max_origin_coverage": thresholds.max_origin_coverage,
            "min_coverage_margin": thresholds.min_coverage_margin,
            "origin_only_span_words": thresholds.origin_only_span_words,
            "session_timeout_seconds": session_timeout_seconds,
            "first_audio_timeout_seconds": first_audio_timeout_seconds,
            "turn_timeout_seconds": turn_timeout_seconds,
        },
        "summary": reanswer_summary(outcomes),
        "trials": [outcome.to_json() for outcome in outcomes],
    }


async def run_reanswer_acceptance(
    url: str,
    origin_prompt: str,
    follow_up_prompt: str,
    origin_source: str,
    follow_up_source: str,
    trial_count: int,
    thresholds: ReanswerThresholds,
    session_timeout_seconds: float,
    first_audio_timeout_seconds: float,
    turn_timeout_seconds: float,
    output_dir: Path,
) -> tuple[list[ReanswerTrialOutcome], Path]:
    """Run fresh two-turn sessions and atomically preserve content-free evidence."""
    output_dir.mkdir(parents=True, exist_ok=False)
    report_path = output_dir / "results.json"
    run_started_at = utc_now()
    origin_words = spoken_words(origin_source)
    follow_up_words = spoken_words(follow_up_source)
    outcomes: list[ReanswerTrialOutcome] = []

    def report(finished: bool, finished_at: str | None) -> JsonObject:
        return build_reanswer_report(
            finished,
            run_started_at,
            finished_at,
            origin_prompt,
            follow_up_prompt,
            origin_source,
            follow_up_source,
            trial_count,
            thresholds,
            session_timeout_seconds,
            first_audio_timeout_seconds,
            turn_timeout_seconds,
            outcomes,
        )

    write_report(report_path, report(False, None))
    for index in range(1, trial_count + 1):
        outcome = await run_reanswer_trial(
            index,
            url,
            origin_prompt,
            follow_up_prompt,
            origin_words,
            follow_up_words,
            thresholds,
            session_timeout_seconds,
            first_audio_timeout_seconds,
            turn_timeout_seconds,
        )
        outcomes.append(outcome)
        comparison = outcome.comparison
        origin_coverage = (
            "n/a" if comparison is None else f"{comparison.origin_coverage:.4f}"
        )
        follow_up_coverage = (
            "n/a" if comparison is None else f"{comparison.follow_up_coverage:.4f}"
        )
        margin = "n/a" if comparison is None else f"{comparison.coverage_margin:.4f}"
        print(
            f"trial {index}/{trial_count}: passed={outcome.passed}; "
            f"interrupts={outcome.interrupt_count}; "
            f"origin-turn={outcome.origin_turn}; follow-up-turn={outcome.follow_up_turn}; "
            f"origin-coverage={origin_coverage}; "
            f"follow-up-coverage={follow_up_coverage}; margin={margin}; "
            f"origin-only-span={comparison.origin_only_span_found if comparison else False}",
            flush=True,
        )
        write_report(report_path, report(False, None))
    write_report(report_path, report(True, utc_now()))
    return outcomes, report_path


def check(condition: bool, detail: str, failures: list[str]) -> None:
    """Collect one self-test failure without hiding later controls."""
    if not condition:
        failures.append(detail)


@dataclass
class ScriptedSocket:
    """In-memory protocol peer for deterministic offline controls."""

    frames: list[str | bytes]
    sent: list[str | bytes] = field(default_factory=list)
    send_receive_counts: list[int] = field(default_factory=list)
    received_count: int = 0
    closed: bool = False

    async def recv(self) -> str | bytes:
        """Return the next scripted server frame."""
        if not self.frames:
            raise RuntimeError("scripted socket exhausted")
        self.received_count += 1
        return self.frames.pop(0)

    async def send(self, message: str | bytes) -> None:
        """Remember a client frame exactly."""
        self.sent.append(message)
        self.send_receive_counts.append(self.received_count)

    async def close(self) -> None:
        """Remember closure."""
        self.closed = True


async def protocol_self_test() -> tuple[ScriptedSocket, TrialRecorder, bool, bool, int | None]:
    """Drive greeting isolation and one exact prompt without a network socket."""
    exact_prompt = "Read this exactly:\nCafé — line two.\n"
    socket = ScriptedSocket(
        frames=[
            json.dumps({"type": "session_started", "greeting": True}),
            json.dumps(
                {"type": "transcript", "role": "assistant", "text": "hello", "turn": 1}
            ),
            bytes(BYTES_PER_SECOND // 100),
            json.dumps({"type": "turn_complete", "turn": 1}),
            json.dumps({"type": "turn_complete", "turn": 2}),
            json.dumps(
                {
                    "type": "transcript",
                    "role": "assistant",
                    "text": "measured response",
                    "turn": 3,
                }
            ),
            bytes(BYTES_PER_SECOND),
            json.dumps({"type": "turn_complete", "turn": 3}),
        ]
    )
    greeting = await prepare_session(socket, 1.0)
    recorder = TrialRecorder(save_pcm=False)
    completed, interrupted, turn = await measure_prompt(
        socket,
        exact_prompt,
        recorder,
        1.0,
        1.0,
    )
    return socket, recorder, greeting and completed, interrupted, turn


@dataclass(frozen=True)
class ReanswerProtocolControl:
    """Deterministic observations from the offline two-turn protocol control."""

    socket: ScriptedSocket
    origin_recorder: TrialRecorder
    follow_up_recorder: TrialRecorder
    origin_result: InterruptedPromptResult
    follow_up_completed: bool
    follow_up_interrupted: bool
    follow_up_turn: int | None
    comparison: ReanswerComparison


async def reanswer_protocol_self_test() -> ReanswerProtocolControl:
    """Drive an interrupt and natural follow-up on one in-memory socket."""
    origin_prompt = "private origin instruction\n"
    follow_up_prompt = "different private follow-up instruction\n"
    follow_up_source = "follow zero one two three four five six seven eight nine"
    socket = ScriptedSocket(
        frames=[
            json.dumps({"type": "session_started", "greeting": False}),
            bytes(BYTES_PER_SECOND * 3 // 4),
            bytes(BYTES_PER_SECOND * 3 // 4),
            bytes(BYTES_PER_SECOND // 2),
            json.dumps({"type": "turn_complete", "turn": 10, "interrupted": True}),
            json.dumps(
                {
                    "type": "transcript",
                    "role": "assistant",
                    "text": follow_up_source,
                    "turn": 11,
                }
            ),
            bytes(BYTES_PER_SECOND),
            json.dumps({"type": "turn_complete", "turn": 11}),
        ]
    )
    await prepare_session(socket, 1.0)
    origin_recorder = TrialRecorder(save_pcm=False)
    origin_result = await measure_interrupted_prompt(
        socket,
        origin_prompt,
        origin_recorder,
        DEFAULT_INTERRUPT_AFTER_AUDIO_SECONDS,
        1.0,
        1.0,
    )
    follow_up_recorder = TrialRecorder(save_pcm=False)
    follow_up_completed, follow_up_interrupted, follow_up_turn = await measure_prompt(
        socket,
        follow_up_prompt,
        follow_up_recorder,
        1.0,
        1.0,
    )
    comparison = compare_reanswer(
        spoken_words("origin alpha beta gamma delta epsilon zeta eta theta iota"),
        spoken_words(follow_up_source),
        " ".join(follow_up_recorder.transcript.role_texts("assistant")),
        DEFAULT_ORIGIN_ONLY_SPAN_WORDS,
    )
    return ReanswerProtocolControl(
        socket=socket,
        origin_recorder=origin_recorder,
        follow_up_recorder=follow_up_recorder,
        origin_result=origin_result,
        follow_up_completed=follow_up_completed,
        follow_up_interrupted=follow_up_interrupted,
        follow_up_turn=follow_up_turn,
        comparison=comparison,
    )


def self_test() -> int:
    """Deterministic offline controls for transcript reconciliation and both metrics."""
    failures: list[str] = []
    check(merge_hypothesis("one two", "one two three") == "one two three", "cumulative hypothesis", failures)
    check(merge_hypothesis("one two three", "one two") == "one two three", "late repeat", failures)
    check(
        merge_hypothesis("read the old message", "read the new message") == "read the new message",
        "corrected hypothesis",
        failures,
    )
    check(merge_hypothesis("first fragment", "second fragment") == "first fragment second fragment", "fragment append", failures)

    book = TranscriptBook()
    check(book.apply("assistant", 1, "one two", False), "partial hypothesis changed", failures)
    check(not book.apply("assistant", 1, "one two", False), "duplicate partial changed", failures)
    check(book.apply("assistant", 1, "one new two", False), "corrected partial did not change", failures)
    check(book.apply("assistant", 1, "one new two final", True), "final hypothesis did not change", failures)
    first_turn_chars = len("one new two final")
    check(book.role_characters("assistant") == first_turn_chars, "one reconciled turn", failures)
    book.apply("assistant", 2, "one new two final", True)
    check(book.role_characters("assistant") == first_turn_chars * 2, "later repeated turn missing", failures)
    book.apply("assistant", None, "anonymous one", True)
    book.apply("assistant", None, "anonymous two", True)
    check(
        book.role_characters("assistant")
        == first_turn_chars * 2 + len("anonymous one") + len("anonymous two"),
        "anonymous final frames were not separate rows",
        failures,
    )

    recorder = TrialRecorder(save_pcm=True)
    recorder.record_audio(bytes(BYTES_PER_SECOND), 1.0, "t1")
    recorder.record_audio(bytes(BYTES_PER_SECOND), 5.0, "t5")
    recorder.record_transcript("assistant", "read the long message", 1, False, 6.0, "t6")
    recorder.record_audio(bytes(BYTES_PER_SECOND // 2), 8.0, "t8")
    recorder.record_transcript("assistant", "read the long message exactly", 1, True, 9.0, "t9")
    recorder.record_completion(1, False, 9.1, "t9")
    check(abs(recorder.audio_seconds - 2.5) < 1e-9, "PCM duration", failures)
    check(abs(recorder.max_gap_seconds - 2.0) < 1e-9, "PCM transcript gap", failures)
    check(abs(recorder.max_wall_gap_seconds - 5.0) < 1e-9, "wall-clock gap diagnostic", failures)
    check(recorder.assistant_characters == len("read the long message exactly"), "final transcript characters", failures)

    repeats = TrialRecorder(save_pcm=False)
    repeats.record_transcript("assistant", "same words", 1, False, 0.0, "t0")
    repeats.record_audio(bytes(BYTES_PER_SECOND), 1.0, "t1")
    repeats.record_transcript("assistant", "same words", 1, False, 2.0, "t2")
    repeats.record_audio(bytes(BYTES_PER_SECOND), 4.0, "t4")
    check(abs(repeats.max_gap_seconds - 2.0) < 1e-9, "repeat did not preserve gap", failures)
    repeats.record_transcript("user", "unrelated recognition", 1, True, 5.0, "t5")
    repeats.record_audio(bytes(BYTES_PER_SECOND), 6.0, "t6")
    check(abs(repeats.max_gap_seconds - 3.0) < 1e-9, "user transcript did not preserve gap", failures)

    burst = TrialRecorder(save_pcm=False)
    burst.record_audio(bytes(BYTES_PER_SECOND * 16), 0.0, "t0")
    check(abs(burst.max_gap_seconds - 16.0) < 1e-9, "buffered PCM duration", failures)
    idle = TrialRecorder(save_pcm=False)
    idle.record_audio(bytes(BYTES_PER_SECOND), 0.0, "t0")
    idle.record_audio(bytes(BYTES_PER_SECOND), 100.0, "t100")
    check(abs(idle.max_gap_seconds - 2.0) < 1e-9, "wall idle inflated PCM gap", failures)
    check(idle.max_wall_gap_seconds > 100.0, "wall diagnostic lost idle span", failures)

    ratio_at_boundary = recorder.audio_seconds_per_character
    if ratio_at_boundary is None:
        failures.append("ratio missing at threshold boundary")
        ratio_at_boundary = 0.0
    thresholds = Thresholds(
        max_gap_seconds=recorder.max_gap_seconds,
        max_audio_seconds_per_character=ratio_at_boundary,
    )
    passing = TrialOutcome(
        index=1,
        started_at="start",
        finished_at="finish",
        elapsed_seconds=9.1,
        greeting=False,
        completed=True,
        interrupted=False,
        completion_turn=1,
        recorder=recorder,
        thresholds=thresholds,
    )
    check(passing.passed, "inclusive thresholds", failures)
    passing.interrupted = True
    check(not passing.passed, "interrupted completion rejected", failures)
    passing.interrupted = False
    strict = TrialOutcome(
        index=2,
        started_at="start",
        finished_at="finish",
        elapsed_seconds=9.1,
        greeting=False,
        completed=True,
        interrupted=False,
        completion_turn=1,
        recorder=recorder,
        thresholds=Thresholds(
            max_gap_seconds=recorder.max_gap_seconds - 0.001,
            max_audio_seconds_per_character=ratio_at_boundary,
        ),
    )
    check(not strict.passed, "over-threshold gap rejected", failures)
    strict_ratio = TrialOutcome(
        index=3,
        started_at="start",
        finished_at="finish",
        elapsed_seconds=9.1,
        greeting=False,
        completed=True,
        interrupted=False,
        completion_turn=1,
        recorder=recorder,
        thresholds=Thresholds(
            max_gap_seconds=recorder.max_gap_seconds,
            max_audio_seconds_per_character=max(0.0, ratio_at_boundary - 0.000_001),
        ),
    )
    check(not strict_ratio.passed, "over-threshold ratio accepted", failures)

    try:
        TrialRecorder(save_pcm=False).record_audio(b"\0", 0.0, "t0")
        failures.append("odd PCM frame accepted")
    except AcceptanceFailure:
        pass
    try:
        TrialRecorder(save_pcm=False).record_audio(b"", 0.0, "t0")
        failures.append("empty PCM frame accepted")
    except AcceptanceFailure:
        pass

    with tempfile.TemporaryDirectory(prefix="voice-read-acceptance-") as temporary:
        wav_path = Path(temporary) / "capture.wav"
        recorder.write_wav(wav_path)
        with wave.open(str(wav_path), "rb") as captured:
            check(captured.getframerate() == SAMPLE_RATE, "WAV sample rate", failures)
            check(captured.getnchannels() == CHANNELS, "WAV channels", failures)
            check(captured.getsampwidth() == SAMPLE_WIDTH, "WAV sample width", failures)
            check(captured.getnframes() == recorder.audio_bytes // SAMPLE_WIDTH, "WAV frame count", failures)

    socket, measured, completed, interrupted, turn = asyncio.run(protocol_self_test())
    sent = [json_object(frame) for frame in socket.sent if isinstance(frame, str)]
    check(completed and not interrupted and turn == 3, "scripted prompt did not complete", failures)
    check(
        [frame.get("type") for frame in sent] == ["audio_start", "audio_end", "prompt"],
        "greeting and audio boundary were not isolated before the prompt",
        failures,
    )
    check(
        sent[-1].get("text") == "Read this exactly:\nCafé — line two.\n",
        "prompt was not sent exactly",
        failures,
    )
    check(measured.assistant_characters == len("measured response"), "greeting entered metrics", failures)
    check(abs(measured.audio_seconds - 1.0) < 1e-9, "greeting audio entered metrics", failures)

    reanswer_control = asyncio.run(reanswer_protocol_self_test())
    reanswer_sent = [
        json_object(frame)
        for frame in reanswer_control.socket.sent
        if isinstance(frame, str)
    ]
    check(
        [frame.get("type") for frame in reanswer_sent]
        == ["prompt", "interrupt", "prompt"],
        "re-answer did not use one interrupt between two prompts",
        failures,
    )
    check(
        reanswer_control.socket.send_receive_counts == [1, 4, 5],
        "interrupt was not sent after the third PCM frame and before the origin boundary",
        failures,
    )
    check(
        reanswer_sent[0].get("text") == "private origin instruction\n"
        and reanswer_sent[2].get("text")
        == "different private follow-up instruction\n",
        "re-answer prompts were not sent exactly on the same socket",
        failures,
    )
    check(
        reanswer_control.origin_result.interrupt_count == 1
        and reanswer_control.origin_result.interrupted
        and reanswer_control.origin_result.completion_turn == 10,
        "origin turn was not acknowledged as interrupted exactly once",
        failures,
    )
    check(
        reanswer_control.origin_result.interrupt_audio_seconds == 2.0,
        "interrupt did not use accumulated PCM duration",
        failures,
    )
    check(
        reanswer_control.follow_up_completed
        and not reanswer_control.follow_up_interrupted
        and reanswer_control.follow_up_turn == 11,
        "follow-up did not complete naturally as the next turn",
        failures,
    )
    check(
        reanswer_control.comparison.follow_up_coverage == 1.0
        and reanswer_control.comparison.origin_coverage == 0.0,
        "ordered-word coverage did not separate the two turns",
        failures,
    )

    origin_boundary_words = [f"origin{index:02d}" for index in range(20)]
    follow_up_boundary_words = [f"follow{index:02d}" for index in range(20)]

    def comparison_outcome(
        comparison: ReanswerComparison,
        comparison_thresholds: ReanswerThresholds,
    ) -> ReanswerTrialOutcome:
        return ReanswerTrialOutcome(
            index=1,
            started_at="start",
            finished_at="finish",
            elapsed_seconds=1.0,
            greeting=False,
            origin_recorder=reanswer_control.origin_recorder,
            follow_up_recorder=reanswer_control.follow_up_recorder,
            origin_source_words=len(origin_boundary_words),
            follow_up_source_words=len(follow_up_boundary_words),
            interrupt_count=1,
            interrupt_at_seconds=0.1,
            interrupt_audio_seconds=2.0,
            origin_completed=True,
            origin_interrupted=True,
            origin_turn=10,
            follow_up_completed=True,
            follow_up_interrupted=False,
            follow_up_turn=11,
            comparison=comparison,
            thresholds=comparison_thresholds,
        )

    default_reanswer_thresholds = ReanswerThresholds(
        interrupt_after_audio_seconds=DEFAULT_INTERRUPT_AFTER_AUDIO_SECONDS,
        min_follow_up_coverage=DEFAULT_MIN_FOLLOW_UP_COVERAGE,
        max_origin_coverage=DEFAULT_MAX_ORIGIN_COVERAGE,
        min_coverage_margin=DEFAULT_MIN_COVERAGE_MARGIN,
        origin_only_span_words=DEFAULT_ORIGIN_ONLY_SPAN_WORDS,
    )
    inclusive_comparison = compare_reanswer(
        origin_boundary_words,
        follow_up_boundary_words,
        " ".join(origin_boundary_words[:2] + follow_up_boundary_words[:17]),
        DEFAULT_ORIGIN_ONLY_SPAN_WORDS,
    )
    inclusive_outcome = comparison_outcome(
        inclusive_comparison,
        default_reanswer_thresholds,
    )
    check(
        inclusive_comparison.follow_up_coverage == 0.85
        and inclusive_outcome.checks()["follow_up_source_coverage"],
        "follow-up coverage equality was not accepted",
        failures,
    )
    margin_boundary_comparison = compare_reanswer(
        origin_boundary_words,
        follow_up_boundary_words,
        " ".join(origin_boundary_words[:3] + follow_up_boundary_words[:17]),
        DEFAULT_ORIGIN_ONLY_SPAN_WORDS,
    )
    margin_boundary_thresholds = ReanswerThresholds(
        interrupt_after_audio_seconds=DEFAULT_INTERRUPT_AFTER_AUDIO_SECONDS,
        min_follow_up_coverage=DEFAULT_MIN_FOLLOW_UP_COVERAGE,
        max_origin_coverage=0.16,
        min_coverage_margin=DEFAULT_MIN_COVERAGE_MARGIN,
        origin_only_span_words=DEFAULT_ORIGIN_ONLY_SPAN_WORDS,
    )
    margin_boundary_outcome = comparison_outcome(
        margin_boundary_comparison,
        margin_boundary_thresholds,
    )
    check(
        abs(margin_boundary_comparison.coverage_margin - 0.70) < 1e-12
        and margin_boundary_outcome.checks()["coverage_margin"],
        "coverage margin equality was not accepted",
        failures,
    )
    strict_origin_outcome = comparison_outcome(
        margin_boundary_comparison,
        default_reanswer_thresholds,
    )
    check(
        not strict_origin_outcome.checks()["origin_source_coverage"],
        "origin coverage equality was not rejected",
        failures,
    )

    long_origin = [f"origin{index:03d}" for index in range(100)]
    span_comparison = compare_reanswer(
        long_origin,
        follow_up_boundary_words,
        " ".join(follow_up_boundary_words + long_origin[10:18]),
        DEFAULT_ORIGIN_ONLY_SPAN_WORDS,
    )
    span_outcome = comparison_outcome(span_comparison, default_reanswer_thresholds)
    check(
        span_comparison.origin_coverage == 0.08
        and span_comparison.origin_only_span_found
        and not span_outcome.checks()["no_origin_only_contiguous_span"],
        "origin-only eight-word span was not rejected independently",
        failures,
    )
    _, shared_span_found = origin_only_span_facts(
        long_origin,
        long_origin[10:18] + follow_up_boundary_words,
        long_origin[10:18],
        DEFAULT_ORIGIN_ONLY_SPAN_WORDS,
    )
    check(not shared_span_found, "shared source span was treated as origin-only", failures)

    interrupted_socket = ScriptedSocket(
        frames=[
            json.dumps({"type": "session_started"}),
            json.dumps({"type": "turn_complete", "turn": 1, "interrupted": True}),
        ]
    )
    check(not asyncio.run(prepare_session(interrupted_socket, 1.0)), "false greeting changed", failures)
    interrupted_recorder = TrialRecorder(save_pcm=False)
    _, was_interrupted, _ = asyncio.run(
        measure_prompt(interrupted_socket, "exact", interrupted_recorder, 1.0, 1.0)
    )
    check(was_interrupted, "interrupted protocol completion accepted", failures)

    private_prompt = "private prompt words must not survive"
    privacy_report = build_report(
        "pass",
        "start",
        "finish",
        private_prompt,
        1,
        thresholds,
        1.0,
        1.0,
        1.0,
        False,
        [passing],
    )
    serialized = json.dumps(privacy_report)
    check(private_prompt not in serialized, "report retained prompt text", failures)
    check("read the long message exactly" not in serialized, "report retained transcript text", failures)
    check("sha256" not in serialized.casefold(), "report retained a stable prompt fingerprint", failures)

    reanswer_private_values = (
        "private origin prompt sentinel",
        "private follow-up prompt sentinel",
        "private origin source sentinel",
        "private follow-up source sentinel",
        "follow zero one two three four five six seven eight nine",
        "wss://private.invalid/secret-session",
        "sha256:stable-private-fingerprint",
    )
    reanswer_privacy_report = build_reanswer_report(
        True,
        "start",
        "finish",
        reanswer_private_values[0],
        reanswer_private_values[1],
        reanswer_private_values[2],
        reanswer_private_values[3],
        1,
        default_reanswer_thresholds,
        1.0,
        1.0,
        1.0,
        [inclusive_outcome],
    )
    reanswer_serialized = json.dumps(reanswer_privacy_report)
    for private_value in reanswer_private_values:
        check(
            private_value not in reanswer_serialized,
            "re-answer report retained private text, transcript, URL, or fingerprint",
            failures,
        )

    if failures:
        print("voice read acceptance self-test FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return EXIT_SELF_TEST
    print("voice read acceptance self-test: offline controls passed")
    return EXIT_OK


def parser() -> argparse.ArgumentParser:
    """The complete operator interface, including every live default."""
    result = argparse.ArgumentParser(
        description=(
            "Run fresh-session vibe-talk-v1 spoken-read trials and assert transcript/audio timing "
            "without saving prompt or transcript text. Supplying all follow-up/source files "
            "instead runs the same-socket interrupted re-answer gate. Live runs may consume "
            "provider time."
        )
    )
    result.add_argument(
        "--url-file",
        type=Path,
        help=(
            f"UTF-8 file containing the ws/wss endpoint (default: ${URL_ENV}); URL values are "
            "never accepted directly on the command line or saved in results"
        ),
    )
    result.add_argument(
        "--prompt-file",
        type=Path,
        help=(
            "UTF-8 file sent exactly, including its final newline when present; required for a "
            "live run so prompt text never enters the process list"
        ),
    )
    result.add_argument(
        "--follow-up-prompt-file",
        type=Path,
        help=(
            "UTF-8 prompt B file; with both source files, enables the two-turn re-answer gate "
            "without putting private text in the process list"
        ),
    )
    result.add_argument(
        "--source-file",
        type=Path,
        help=(
            "UTF-8 source A file used only for in-memory re-answer coverage; requires the "
            "follow-up prompt and source files"
        ),
    )
    result.add_argument(
        "--follow-up-source-file",
        type=Path,
        help=(
            "UTF-8 source B file used only for in-memory re-answer coverage; requires the "
            "follow-up prompt and source files"
        ),
    )
    result.add_argument(
        "--trials",
        type=int,
        help=(
            f"fresh WebSocket sessions to run (default: {DEFAULT_TRIALS} for full reads, "
            f"{DEFAULT_REANSWER_TRIALS} for re-answer)"
        ),
    )
    result.add_argument(
        "--max-gap-seconds",
        type=float,
        default=DEFAULT_MAX_GAP_SECONDS,
        help=(
            "maximum PCM audio seconds without an assistant transcript change "
            f"(default: {DEFAULT_MAX_GAP_SECONDS:g})"
        ),
    )
    result.add_argument(
        "--max-audio-seconds-per-character",
        type=float,
        default=DEFAULT_MAX_AUDIO_SECONDS_PER_CHAR,
        help=(
            "maximum PCM seconds divided by final assistant transcript characters "
            f"(default: {DEFAULT_MAX_AUDIO_SECONDS_PER_CHAR:g})"
        ),
    )
    result.add_argument(
        "--interrupt-after-audio-seconds",
        type=float,
        default=DEFAULT_INTERRUPT_AFTER_AUDIO_SECONDS,
        help=(
            "re-answer mode: accumulated origin PCM seconds before one interrupt "
            f"(default: {DEFAULT_INTERRUPT_AFTER_AUDIO_SECONDS:g})"
        ),
    )
    result.add_argument(
        "--min-follow-up-coverage",
        type=float,
        default=DEFAULT_MIN_FOLLOW_UP_COVERAGE,
        help=(
            "re-answer mode: minimum ordered-word source B coverage "
            f"(default: {DEFAULT_MIN_FOLLOW_UP_COVERAGE:g})"
        ),
    )
    result.add_argument(
        "--max-origin-coverage",
        type=float,
        default=DEFAULT_MAX_ORIGIN_COVERAGE,
        help=(
            "re-answer mode: strict upper bound on ordered-word source A coverage "
            f"(default: {DEFAULT_MAX_ORIGIN_COVERAGE:g})"
        ),
    )
    result.add_argument(
        "--min-coverage-margin",
        type=float,
        default=DEFAULT_MIN_COVERAGE_MARGIN,
        help=(
            "re-answer mode: minimum source B minus source A coverage "
            f"(default: {DEFAULT_MIN_COVERAGE_MARGIN:g})"
        ),
    )
    result.add_argument(
        "--origin-only-span-words",
        type=int,
        default=DEFAULT_ORIGIN_ONLY_SPAN_WORDS,
        help=(
            "re-answer mode: reject an origin-only contiguous span this long "
            f"(default: {DEFAULT_ORIGIN_ONLY_SPAN_WORDS})"
        ),
    )
    result.add_argument(
        "--session-timeout-seconds",
        type=float,
        default=DEFAULT_SESSION_TIMEOUT_SECONDS,
        help=(
            "connection, session_started, and promised-greeting timeout "
            f"(default: {DEFAULT_SESSION_TIMEOUT_SECONDS:g})"
        ),
    )
    result.add_argument(
        "--first-audio-timeout-seconds",
        type=float,
        default=DEFAULT_FIRST_AUDIO_TIMEOUT_SECONDS,
        help=(
            "maximum wait for a prompt's first binary PCM frame "
            f"(default: {DEFAULT_FIRST_AUDIO_TIMEOUT_SECONDS:g})"
        ),
    )
    result.add_argument(
        "--turn-timeout-seconds",
        type=float,
        default=DEFAULT_TURN_TIMEOUT_SECONDS,
        help=f"maximum wait for one prompt's turn_complete (default: {DEFAULT_TURN_TIMEOUT_SECONDS:g})",
    )
    result.add_argument(
        "--out",
        type=Path,
        help=(
            "new artifact directory (default: debug/voice-read-acceptance/<UTC timestamp>); "
            "results.json is always written"
        ),
    )
    result.add_argument(
        "--wav",
        action="store_true",
        help="also retain each trial's potentially sensitive speech as a WAV for ASR replay",
    )
    result.add_argument(
        "--self-test",
        action="store_true",
        help=(
            "run deterministic offline metric, re-answer, privacy, and WAV controls; "
            "opens no socket"
        ),
    )
    return result


def default_output() -> Path:
    """A unique ignored artifact directory inside vibe-talk."""
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return Path(__file__).resolve().parent.parent / "debug" / "voice-read-acceptance" / stamp


def read_private_text(path: Path, label: str) -> str:
    """Read one private UTF-8 input without copying its path or contents into an error."""
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise AcceptanceFailure(
            "usage",
            f"the {label} file could not be read as UTF-8 ({type(error).__name__})",
        ) from error


def main(argv: Sequence[str] | None = None) -> int:
    """Validate configuration, run offline controls or drive the live endpoint."""
    args = parser().parse_args(argv)
    if args.self_test:
        return self_test()
    if args.url_file is not None:
        try:
            url = args.url_file.read_text(encoding="utf-8").strip()
        except OSError as error:
            raise AcceptanceFailure(
                "usage",
                f"the URL file could not be read ({type(error).__name__})",
            ) from error
    else:
        url = os.environ.get(URL_ENV, "")
    if not url:
        raise AcceptanceFailure("usage", f"--url-file or {URL_ENV} is required")
    parsed = urlsplit(url)
    if parsed.scheme not in {"ws", "wss"} or not parsed.netloc:
        raise AcceptanceFailure("usage", "the WebSocket URL must be absolute ws:// or wss://")
    if args.prompt_file is None:
        raise AcceptanceFailure("usage", "--prompt-file is required")
    prompt = read_private_text(args.prompt_file, "prompt")
    if not prompt.strip():
        raise AcceptanceFailure("usage", "the exact prompt must contain non-whitespace text")
    reanswer_paths = (
        args.follow_up_prompt_file,
        args.source_file,
        args.follow_up_source_file,
    )
    reanswer_mode = any(path is not None for path in reanswer_paths)
    if reanswer_mode and not all(path is not None for path in reanswer_paths):
        raise AcceptanceFailure(
            "usage",
            "--follow-up-prompt-file, --source-file, and --follow-up-source-file are all required "
            "for re-answer mode",
        )
    trial_count = args.trials
    if trial_count is None:
        trial_count = DEFAULT_REANSWER_TRIALS if reanswer_mode else DEFAULT_TRIALS
    if trial_count < 1:
        raise AcceptanceFailure("usage", "--trials must be at least 1")
    for name, value in (
        ("--max-gap-seconds", args.max_gap_seconds),
        ("--max-audio-seconds-per-character", args.max_audio_seconds_per_character),
        ("--interrupt-after-audio-seconds", args.interrupt_after_audio_seconds),
        ("--session-timeout-seconds", args.session_timeout_seconds),
        ("--first-audio-timeout-seconds", args.first_audio_timeout_seconds),
        ("--turn-timeout-seconds", args.turn_timeout_seconds),
    ):
        if not 0 < value < float("inf"):
            raise AcceptanceFailure("usage", f"{name} must be a positive finite number")
    for name, value in (
        ("--min-follow-up-coverage", args.min_follow_up_coverage),
        ("--min-coverage-margin", args.min_coverage_margin),
    ):
        if not 0 <= value <= 1:
            raise AcceptanceFailure("usage", f"{name} must be between 0 and 1")
    if not 0 < args.max_origin_coverage <= 1:
        raise AcceptanceFailure(
            "usage",
            "--max-origin-coverage must be greater than 0 and at most 1",
        )
    if args.origin_only_span_words < 1:
        raise AcceptanceFailure("usage", "--origin-only-span-words must be at least 1")
    output_dir = args.out or default_output()
    if output_dir.exists():
        raise AcceptanceFailure("usage", f"the artifact directory already exists: {output_dir}")
    if reanswer_mode:
        if args.wav:
            raise AcceptanceFailure(
                "usage",
                "--wav is unavailable in re-answer mode because its artifacts contain content",
            )
        follow_up_prompt_path = cast(Path, args.follow_up_prompt_file)
        origin_source_path = cast(Path, args.source_file)
        follow_up_source_path = cast(Path, args.follow_up_source_file)
        follow_up_prompt = read_private_text(
            follow_up_prompt_path,
            "follow-up prompt",
        )
        origin_source = read_private_text(origin_source_path, "origin source")
        follow_up_source = read_private_text(
            follow_up_source_path,
            "follow-up source",
        )
        if not follow_up_prompt.strip():
            raise AcceptanceFailure(
                "usage",
                "the follow-up prompt must contain non-whitespace text",
            )
        if prompt == follow_up_prompt:
            raise AcceptanceFailure("usage", "the two prompt files must be different")
        origin_words = spoken_words(origin_source)
        follow_up_words = spoken_words(follow_up_source)
        if not origin_words or not follow_up_words:
            raise AcceptanceFailure(
                "usage",
                "both source files must contain at least one normalized word",
            )
        if origin_words == follow_up_words:
            raise AcceptanceFailure("usage", "the two source files must be different")
        reanswer_thresholds = ReanswerThresholds(
            interrupt_after_audio_seconds=args.interrupt_after_audio_seconds,
            min_follow_up_coverage=args.min_follow_up_coverage,
            max_origin_coverage=args.max_origin_coverage,
            min_coverage_margin=args.min_coverage_margin,
            origin_only_span_words=args.origin_only_span_words,
        )
        reanswer_outcomes, report_path = asyncio.run(
            run_reanswer_acceptance(
                url=url,
                origin_prompt=prompt,
                follow_up_prompt=follow_up_prompt,
                origin_source=origin_source,
                follow_up_source=follow_up_source,
                trial_count=trial_count,
                thresholds=reanswer_thresholds,
                session_timeout_seconds=args.session_timeout_seconds,
                first_audio_timeout_seconds=args.first_audio_timeout_seconds,
                turn_timeout_seconds=args.turn_timeout_seconds,
                output_dir=output_dir,
            )
        )
        print(f"results: {report_path.resolve()}")
        if all(outcome.passed for outcome in reanswer_outcomes):
            print(
                f"PASS: {len(reanswer_outcomes)} fresh-session re-answer trials met every "
                "protocol and separation threshold"
            )
            return EXIT_OK
        print(
            f"FAIL: {sum(not outcome.passed for outcome in reanswer_outcomes)} of "
            f"{len(reanswer_outcomes)} re-answer trials failed",
            file=sys.stderr,
        )
        return EXIT_ACCEPTANCE
    thresholds = Thresholds(
        max_gap_seconds=args.max_gap_seconds,
        max_audio_seconds_per_character=args.max_audio_seconds_per_character,
    )
    outcomes, report_path = asyncio.run(
        run_acceptance(
            url=url,
            prompt=prompt,
            trial_count=trial_count,
            thresholds=thresholds,
            session_timeout_seconds=args.session_timeout_seconds,
            first_audio_timeout_seconds=args.first_audio_timeout_seconds,
            turn_timeout_seconds=args.turn_timeout_seconds,
            save_wav=args.wav,
            output_dir=output_dir,
        )
    )
    print(f"results: {report_path.resolve()}")
    if all(outcome.passed for outcome in outcomes):
        print(f"PASS: {len(outcomes)} fresh-session reads met every threshold")
        return EXIT_OK
    print(
        f"FAIL: {sum(not outcome.passed for outcome in outcomes)} of {len(outcomes)} trials failed",
        file=sys.stderr,
    )
    return EXIT_ACCEPTANCE


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AcceptanceFailure as error:
        print(f"voice-read-acceptance: {error.detail}", file=sys.stderr)
        raise SystemExit(EXIT_DEPENDENCY if error.code == "missing_dependency" else EXIT_USAGE)
